//! Cross-platform process monitoring for waitagent sessions.
//!
//! The public entry point is [`ProcessMonitor`]. On Linux it uses netlink proc
//! events; other platforms currently fall back to no-op sources and should be
//! implemented later (Windows: ETW, macOS: kqueue + libproc).

use crate::domain::session_catalog::ManagedSessionTaskState;
use crate::infra::error_log::ERROR_LOG;
use crate::lifecycle::LifecycleError;
use crate::process_monitor::event::ProcessEvent;
use crate::process_monitor::state_machine::{derive_session_state, foreground_pgid};
use crate::process_monitor::tree::SessionProcessTree;
use crate::ratatui_node::runtime::SharedState;
use crate::ratatui_node::state_event::StateEvent;
use std::collections::HashMap;
use std::os::unix::io::RawFd;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

pub mod event;
pub mod state_machine;
pub mod tree;

#[cfg(target_os = "linux")]
use linux::LinuxProcessEventSource as PlatformSource;
#[cfg(target_os = "macos")]
use macos::MacOsProcessEventSource as PlatformSource;
#[cfg(target_os = "windows")]
use windows::WindowsProcessEventSource as PlatformSource;

/// Per-session tracking state used by the process monitor.
struct SessionState {
    tree: SessionProcessTree,
    pty_master_fd: RawFd,
    last_command_name: Option<String>,
    last_task_state: ManagedSessionTaskState,
    pane_text: Box<dyn Fn() -> String + Send>,
}

/// Global process monitor that listens to system process events and updates
/// session metadata accordingly.
#[derive(Clone)]
pub(crate) struct ProcessMonitor {
    sessions: Arc<Mutex<HashMap<String, SessionState>>>,
    pid_to_session: Arc<Mutex<HashMap<u32, String>>>,
}

impl ProcessMonitor {
    /// Start the platform-specific process event source and the dispatcher loop.
    pub fn start(shared: Arc<SharedState>) -> Result<Self, LifecycleError> {
        let (tx, rx) = mpsc::channel::<ProcessEvent>();
        let _source = PlatformSource::start(tx)?;

        let sessions = Arc::new(Mutex::new(HashMap::<String, SessionState>::new()));
        let pid_to_session = Arc::new(Mutex::new(HashMap::<u32, String>::new()));

        let monitor = Self {
            sessions: sessions.clone(),
            pid_to_session: pid_to_session.clone(),
        };

        ERROR_LOG.log("[process-monitor] dispatcher starting".to_string());

        std::thread::spawn(move || {
            dispatcher_loop(rx, sessions, pid_to_session, shared);
        });

        Ok(monitor)
    }

    /// Register a new shell session with the monitor.
    pub fn register_session(
        &self,
        target_id: String,
        shell_pid: u32,
        pty_master_fd: RawFd,
        pane_text: Box<dyn Fn() -> String + Send>,
    ) {
        let (argv0, argv) = read_proc_cmdline(shell_pid);
        let argv0 = argv0.unwrap_or_else(|| "bash".to_string());
        ERROR_LOG.log(format!(
            "[process-monitor] register target_id={target_id} shell_pid={shell_pid} argv0={argv0}"
        ));
        let tree = SessionProcessTree::new(shell_pid, argv0, argv.unwrap_or_default());

        {
            let mut pid_map = self
                .pid_to_session
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            pid_map.insert(shell_pid, target_id.clone());
        }

        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        sessions.insert(
            target_id,
            SessionState {
                tree,
                pty_master_fd,
                last_command_name: None,
                last_task_state: ManagedSessionTaskState::Unknown,
                pane_text,
            },
        );
    }

    /// Unregister a session and stop tracking its processes.
    pub fn unregister_session(&self, target_id: &str) {
        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        let Some(state) = sessions.remove(target_id) else {
            return;
        };
        ERROR_LOG.log(format!(
            "[process-monitor] unregister target_id={target_id} shell_pid={}",
            state.tree.shell_pid()
        ));
        drop(sessions);

        let mut pid_map = self
            .pid_to_session
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        pid_map.remove(&state.tree.shell_pid());
        for pid in state.tree.pids() {
            pid_map.remove(&pid);
        }
    }
}

#[cfg(unix)]
pub(crate) fn read_proc_cmdline(pid: u32) -> (Option<String>, Option<Vec<String>>) {
    let path = format!("/proc/{pid}/cmdline");
    let contents = std::fs::read_to_string(&path).unwrap_or_default();
    let parts: Vec<String> = contents
        .split('\0')
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .collect();
    let argv0 = parts.first().cloned();
    if parts.is_empty() {
        (None, None)
    } else {
        (argv0, Some(parts))
    }
}

#[cfg(not(unix))]
pub(crate) fn read_proc_cmdline(_pid: u32) -> (Option<String>, Option<Vec<String>>) {
    (None, None)
}

/// Read the command line of the process currently in the PTY foreground.
///
/// This is a one-shot fallback for code paths that need the foreground command
/// at a specific instant (e.g. paste-time agent detection for authority-host
/// sessions). The event-driven process monitor keeps its own tree and should be
/// preferred for continuous tracking.
#[cfg(unix)]
pub(crate) fn read_foreground_process_cmdline(
    pty_master_fd: RawFd,
) -> (Option<String>, Option<Vec<String>>) {
    // SAFETY: `tcgetpgrp` and `getpgid` are async-signal-safe POSIX calls; the
    // fd is owned by the session/IO loop and is only read here.
    let pgid = unsafe { libc::tcgetpgrp(pty_master_fd) };
    if pgid < 0 {
        return (None, None);
    }
    let leader_pid = unsafe { libc::getpgid(pgid) };
    if leader_pid <= 0 {
        return (None, None);
    }
    read_proc_cmdline(leader_pid as u32)
}

#[cfg(not(unix))]
pub(crate) fn read_foreground_process_cmdline(
    _pty_master_fd: RawFd,
) -> (Option<String>, Option<Vec<String>>) {
    (None, None)
}

fn dispatcher_loop(
    rx: mpsc::Receiver<ProcessEvent>,
    sessions: Arc<Mutex<HashMap<String, SessionState>>>,
    pid_to_session: Arc<Mutex<HashMap<u32, String>>>,
    shared: Arc<SharedState>,
) {
    while let Ok(event) = rx.recv() {
        match event {
            ProcessEvent::Fork {
                parent_pid,
                child_pid,
            } => {
                let target_id = {
                    let pid_map = pid_to_session.lock().unwrap_or_else(|e| e.into_inner());
                    pid_map.get(&parent_pid).cloned()
                };
                let Some(target_id) = target_id else {
                    continue;
                };

                let (argv0, argv) = read_proc_cmdline(child_pid);
                let argv0 = argv0.unwrap_or_default();
                let argv = argv.unwrap_or_default();

                {
                    let mut pid_map = pid_to_session.lock().unwrap_or_else(|e| e.into_inner());
                    pid_map.insert(child_pid, target_id.clone());
                }

                let mut sessions = sessions.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(state) = sessions.get_mut(&target_id) {
                    state.tree.add_child(parent_pid, child_pid, argv0, argv);
                    recompute_and_emit(&target_id, state, &shared);
                }
            }
            ProcessEvent::Exec { pid, .. } => {
                let target_id = {
                    let pid_map = pid_to_session.lock().unwrap_or_else(|e| e.into_inner());
                    pid_map.get(&pid).cloned()
                };
                let Some(target_id) = target_id else {
                    continue;
                };

                let (argv0, argv) = read_proc_cmdline(pid);
                let argv0 = argv0.unwrap_or_default();
                let argv = argv.unwrap_or_default();

                let mut sessions = sessions.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(state) = sessions.get_mut(&target_id) {
                    state.tree.update_exec(pid, argv0, argv);
                    recompute_and_emit(&target_id, state, &shared);
                }
            }
            ProcessEvent::Exit { pid, .. } => {
                let target_id = {
                    let pid_map = pid_to_session.lock().unwrap_or_else(|e| e.into_inner());
                    pid_map.get(&pid).cloned()
                };
                let Some(target_id) = target_id else {
                    continue;
                };

                {
                    let mut pid_map = pid_to_session.lock().unwrap_or_else(|e| e.into_inner());
                    pid_map.remove(&pid);
                }

                let mut sessions = sessions.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(state) = sessions.get_mut(&target_id) {
                    let shell_exited = pid == state.tree.shell_pid();
                    state.tree.remove(pid);
                    if shell_exited {
                        sessions.remove(&target_id);
                    } else {
                        recompute_and_emit(&target_id, state, &shared);
                    }
                }
            }
        }
    }
}

fn recompute_and_emit(target_id: &str, state: &mut SessionState, shared: &Arc<SharedState>) {
    let pane_text = (state.pane_text)();
    let fg_pgid = foreground_pgid(state.pty_master_fd);
    let (command_name, task_state) = derive_session_state(&state.tree, fg_pgid, &pane_text);

    if state.last_task_state != task_state {
        ERROR_LOG.log(format!(
            "[process-monitor] task_state target_id={target_id} task_state={task_state:?}"
        ));
        state.last_task_state = task_state;
        let _ = shared
            .state_sender()
            .send(StateEvent::SessionTaskStateChanged {
                target_id: target_id.to_string(),
                task_state,
            });
    }

    if state.last_command_name != command_name {
        match command_name {
            Some(ref command_name) if !command_name.is_empty() => {
                ERROR_LOG.log(format!(
                    "[process-monitor] command_name target_id={target_id} command_name={command_name}"
                ));
                state.last_command_name = Some(command_name.clone());
                // For agents that accept @-references, set agent_command_name
                // proactively so paste formatting works even before hooks fire.
                let _ = shared
                    .state_sender()
                    .send(StateEvent::SessionCommandNameChanged {
                        target_id: target_id.to_string(),
                        command_name: command_name.clone(),
                    });
            }
            _ => {
                // Only clear the displayed command name when we are back at the
                // shell prompt. While a command is still running (even if we do
                // not know its name yet), keep the last known name to avoid
                // sidebar flicker.
                if task_state == ManagedSessionTaskState::Input && state.last_command_name.is_some()
                {
                    ERROR_LOG.log(format!(
                        "[process-monitor] command_name target_id={target_id} cleared"
                    ));
                    state.last_command_name = None;
                    let _ = shared
                        .state_sender()
                        .send(StateEvent::SessionCommandNameCleared {
                            target_id: target_id.to_string(),
                        });
                }
            }
        }
    }
}
