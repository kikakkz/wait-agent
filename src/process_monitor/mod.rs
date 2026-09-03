//! Cross-platform process monitoring for waitagent sessions.
//!
//! The public entry point is [`ProcessMonitor`]. On Linux it uses netlink proc
//! events; on Windows it polls Toolhelp32 snapshots and diffs them into
//! Fork/Exec/Exit events; macOS currently falls back to a no-op source and
//! should be implemented later (kqueue + libproc).
//!
//! The Linux implementation uses a single unified event loop that polls the
//! netlink socket (primary event source) and a timerfd (fallback refresh). When
//! netlink events are delivered they update the process tree directly. When the
//! kernel proc connector is silent, the timerfd triggers a refresh from
//! `/proc/<pid>/task/<pid>/children` so sessions still show the correct command
//! name and task state.

#[cfg(target_os = "linux")]
use crate::domain::agent_detector::{first_argv_token, SHELL_NAMES};
use crate::domain::session_catalog::ManagedSessionTaskState;
use crate::infra::error_log::ERROR_LOG;
use crate::lifecycle::LifecycleError;
use crate::process_monitor::event::ProcessEvent;
use crate::process_monitor::state_machine::derive_session_state;
use crate::process_monitor::tree::SessionProcessTree;
use crate::ratatui_node::runtime::SharedState;
use crate::ratatui_node::state_event::StateEvent;
use std::collections::HashMap;
#[cfg(unix)]
use std::fs;
#[cfg(target_os = "linux")]
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
#[cfg(any(target_os = "linux", target_os = "windows"))]
use std::time::Duration;
use std::time::Instant;

/// PTY master handle type used by the process monitor.
///
/// On Unix this is the raw fd of the PTY master. Windows local sessions use
/// anonymous pipes without a PTY master (ConPTY is not implemented yet), so
/// this is a plain placeholder that callers set to `0`; it is never
/// dereferenced on Windows. A `usize` placeholder (rather than `c_void`) keeps
/// the type `Send + Sync` and constructible.
#[cfg(unix)]
pub(crate) type PlatformPtyFd = std::os::fd::RawFd;
/// See the Unix documentation above.
#[cfg(windows)]
pub(crate) type PlatformPtyFd = usize;

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

/// Platform implementation of `read_proc_cmdline`; the Unix version lives in
/// this module below, the Windows Toolhelp32-based one in `windows.rs`.
#[cfg(target_os = "windows")]
pub(crate) use windows::read_proc_cmdline;

/// Token used by the polling loop for the netlink socket.
#[cfg(target_os = "linux")]
const NETLINK_TOKEN: usize = 0;
/// Token used by the polling loop for the fallback timerfd.
#[cfg(target_os = "linux")]
const TIMER_TOKEN: usize = 1;

/// How often the fallback timer fires.
#[cfg(target_os = "linux")]
const FALLBACK_INTERVAL: Duration = Duration::from_secs(1);
/// If a session has not received a netlink event in this time, refresh it from
/// `/proc`. This is intentionally shorter than the timer interval so a single
/// missed event does not immediately trigger a scan.
#[cfg(target_os = "linux")]
const NETLINK_STALE_THRESHOLD: Duration = Duration::from_millis(500);

/// How often the Windows event loop diffs Toolhelp32 snapshots.
#[cfg(target_os = "windows")]
const WINDOWS_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Per-session tracking state used by the process monitor.
struct SessionState {
    tree: SessionProcessTree,
    pty_master_fd: PlatformPtyFd,
    last_command_name: Option<String>,
    last_task_state: ManagedSessionTaskState,
    // Only read by the Linux fallback refresh; Windows/macOS write it from
    // event handlers but never read it.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    last_netlink_event: Option<Instant>,
    pane_text: Box<dyn Fn() -> String + Send>,
}

/// Global process monitor that listens to system process events and updates
/// session metadata accordingly.
#[derive(Clone)]
pub(crate) struct ProcessMonitor {
    sessions: Arc<Mutex<HashMap<String, SessionState>>>,
    pid_to_session: Arc<Mutex<HashMap<u32, String>>>,
    // Keep the event source and timerfd alive for the lifetime of the monitor.
    // Dropping them would close the kernel sockets and stop the event loop.
    // The netlink source is optional so the monitor can fall back to /proc-only
    // refreshes when the process lacks CAP_NET_ADMIN.
    #[allow(dead_code)]
    source: Option<Arc<PlatformSource>>,
    // Keep the timerfd alive for the lifetime of the monitor. Dropping it would
    // close the kernel socket and stop the fallback timer.
    #[cfg(target_os = "linux")]
    #[allow(dead_code)]
    timer_fd: Arc<OwnedFd>,
}

impl ProcessMonitor {
    /// Start the unified process monitor event loop.
    ///
    /// On Linux this creates a single poller that waits on the netlink proc
    /// connector socket and a periodic timerfd. Netlink events drive the process
    /// tree; the timerfd refreshes sessions from `/proc` when netlink is silent.
    pub fn start(shared: Arc<SharedState>) -> Result<Self, LifecycleError> {
        let source = match PlatformSource::new() {
            Ok(source) => {
                ERROR_LOG.log("[process-monitor] netlink proc connector source ready".to_string());
                Some(Arc::new(source))
            }
            Err(error) => {
                ERROR_LOG.log_error(format!(
                    "[process-monitor] netlink source unavailable, using /proc fallback: {error}"
                ));
                None
            }
        };
        #[cfg(target_os = "linux")]
        let timer_fd = Arc::new(start_fallback_timer()?);

        let sessions = Arc::new(Mutex::new(HashMap::<String, SessionState>::new()));
        let pid_to_session = Arc::new(Mutex::new(HashMap::<u32, String>::new()));

        let monitor = Self {
            sessions: sessions.clone(),
            pid_to_session: pid_to_session.clone(),
            source: source.clone(),
            #[cfg(target_os = "linux")]
            timer_fd: timer_fd.clone(),
        };

        ERROR_LOG.log("[process-monitor] unified event loop starting".to_string());

        let running = Arc::new(AtomicBool::new(true));
        std::thread::spawn(move || {
            #[cfg(target_os = "linux")]
            event_loop(source, timer_fd, sessions, pid_to_session, shared, running);
            #[cfg(not(target_os = "linux"))]
            event_loop(source, sessions, pid_to_session, shared, running);
        });

        Ok(monitor)
    }

    /// Register a new shell session with the monitor.
    pub fn register_session(
        &self,
        target_id: String,
        shell_pid: u32,
        pty_master_fd: PlatformPtyFd,
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
                last_netlink_event: None,
                pane_text,
            },
        );
    }

    /// Unregister a session and stop tracking its processes.
    ///
    /// Lock order: `pid_to_session` is acquired before `sessions` to match the
    /// order used by netlink event handlers and [`Self::register_session`].
    pub fn unregister_session(&self, target_id: &str) {
        let mut pid_map = self
            .pid_to_session
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let pids_for_target: Vec<u32> = pid_map
            .iter()
            .filter(|(_, session_id)| *session_id == target_id)
            .map(|(&pid, _)| pid)
            .collect();
        for pid in pids_for_target {
            pid_map.remove(&pid);
        }

        let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(state) = sessions.remove(target_id) {
            ERROR_LOG.log(format!(
                "[process-monitor] unregister target_id={target_id} shell_pid={}",
                state.tree.shell_pid()
            ));
        }
    }
}

#[cfg(unix)]
pub(crate) fn read_proc_cmdline(pid: u32) -> (Option<String>, Option<Vec<String>>) {
    let path = format!("/proc/{pid}/cmdline");
    let contents = fs::read_to_string(&path).unwrap_or_default();
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

#[cfg(target_os = "macos")]
pub(crate) fn read_proc_cmdline(_pid: u32) -> (Option<String>, Option<Vec<String>>) {
    (None, None)
}

/// Read the direct children of `pid` from `/proc/<pid>/task/<pid>/children`.
#[cfg(target_os = "linux")]
fn read_proc_children(pid: u32) -> Vec<u32> {
    let path = format!("/proc/{pid}/task/{pid}/children");
    let contents = fs::read_to_string(&path).unwrap_or_default();
    contents
        .split_whitespace()
        .filter_map(|s| s.parse::<u32>().ok())
        .collect()
}

/// Read the command line of the process currently in the PTY foreground.
///
/// This is a one-shot fallback for code paths that need the foreground command
/// at a specific instant (e.g. paste-time agent detection for authority-host
/// sessions). The event-driven process monitor keeps its own tree and should be
/// preferred for continuous tracking.
#[cfg(unix)]
pub(crate) fn read_foreground_process_cmdline(
    pty_master_fd: PlatformPtyFd,
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

/// Create a periodic timerfd that fires every [`FALLBACK_INTERVAL`].
#[cfg(target_os = "linux")]
fn start_fallback_timer() -> Result<OwnedFd, LifecycleError> {
    // SAFETY: `timerfd_create` is a standard libc call with well-defined args.
    let fd = unsafe {
        libc::timerfd_create(
            libc::CLOCK_MONOTONIC,
            libc::TFD_NONBLOCK | libc::TFD_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(LifecycleError::Io(
            "failed to create fallback timerfd".to_string(),
            std::io::Error::last_os_error(),
        ));
    }
    // SAFETY: fd was just returned by a successful timerfd_create() call.
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };

    let interval = libc::itimerspec {
        it_interval: libc::timespec {
            tv_sec: FALLBACK_INTERVAL.as_secs() as libc::time_t,
            tv_nsec: FALLBACK_INTERVAL.subsec_nanos() as libc::c_long,
        },
        it_value: libc::timespec {
            tv_sec: FALLBACK_INTERVAL.as_secs() as libc::time_t,
            tv_nsec: FALLBACK_INTERVAL.subsec_nanos() as libc::c_long,
        },
    };
    // SAFETY: `timerfd_settime` on a valid timerfd with a valid itimerspec.
    let rc =
        unsafe { libc::timerfd_settime(owned.as_raw_fd(), 0, &interval, std::ptr::null_mut()) };
    if rc < 0 {
        return Err(LifecycleError::Io(
            "failed to arm fallback timerfd".to_string(),
            std::io::Error::last_os_error(),
        ));
    }

    Ok(owned)
}

/// Drain the timerfd so subsequent polls do not immediately return readable.
#[cfg(target_os = "linux")]
fn drain_timerfd(timer_fd: &OwnedFd) {
    let mut buf = [0u8; 8];
    // SAFETY: read on a valid non-blocking timerfd into a stack buffer.
    unsafe {
        libc::read(
            timer_fd.as_raw_fd(),
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len(),
        );
    }
}

#[cfg(target_os = "linux")]
fn event_loop(
    source: Option<Arc<PlatformSource>>,
    timer_fd: Arc<OwnedFd>,
    sessions: Arc<Mutex<HashMap<String, SessionState>>>,
    pid_to_session: Arc<Mutex<HashMap<u32, String>>>,
    shared: Arc<SharedState>,
    running: Arc<AtomicBool>,
) {
    let poller = match polling::Poller::new() {
        Ok(p) => p,
        Err(error) => {
            ERROR_LOG.log_error(format!(
                "[process-monitor] failed to create poller: {error}"
            ));
            return;
        }
    };

    // SAFETY: `add_with_mode` requires that the fd outlives the poller. Both
    // `source` and `timer_fd` are owned by `Arc`s that live until this function
    // returns, and the poller is dropped before them.
    if let Some(source) = source.as_ref() {
        unsafe {
            if let Err(error) = poller.add_with_mode(
                &**source,
                polling::Event::readable(NETLINK_TOKEN),
                polling::PollMode::Level,
            ) {
                ERROR_LOG.log_error(format!(
                    "[process-monitor] failed to add netlink socket to poller: {error}"
                ));
                return;
            }
        }
    }

    // SAFETY: Same lifetime guarantee as above; `timer_fd` outlives the poller.
    unsafe {
        if let Err(error) = poller.add_with_mode(
            &*timer_fd,
            polling::Event::readable(TIMER_TOKEN),
            polling::PollMode::Level,
        ) {
            ERROR_LOG.log_error(format!(
                "[process-monitor] failed to add timerfd to poller: {error}"
            ));
            return;
        }
    }

    let mut events = polling::Events::new();

    while running.load(Ordering::Relaxed) {
        events.clear();
        match poller.wait(&mut events, Some(Duration::from_millis(100))) {
            Ok(0) => continue,
            Ok(_) => {}
            Err(error) => {
                ERROR_LOG.log(format!("[process-monitor] poller wait error: {error}"));
                continue;
            }
        }

        for event in events.iter() {
            match event.key {
                NETLINK_TOKEN => {
                    if let Some(source) = source.as_ref() {
                        let mut netlink_events = Vec::new();
                        match source.read_events(&mut netlink_events) {
                            Ok(count) => {
                                if count > 0 {
                                    handle_process_events(
                                        netlink_events,
                                        &sessions,
                                        &pid_to_session,
                                        &shared,
                                    );
                                }
                            }
                            Err(error) => {
                                ERROR_LOG
                                    .log(format!("[process-monitor] netlink read error: {error}"));
                            }
                        }
                    }
                }
                TIMER_TOKEN => {
                    drain_timerfd(&timer_fd);
                    fallback_refresh(&sessions, &pid_to_session, &shared);
                }
                _ => {}
            }
        }
    }

    ERROR_LOG.log("[process-monitor] unified event loop stopped".to_string());
}

/// macOS no-op event loop until a native source (kqueue EVFILT_PROC) is added.
#[cfg(target_os = "macos")]
fn event_loop(
    _source: Option<Arc<PlatformSource>>,
    _sessions: Arc<Mutex<HashMap<String, SessionState>>>,
    _pid_to_session: Arc<Mutex<HashMap<u32, String>>>,
    _shared: Arc<SharedState>,
    _running: Arc<AtomicBool>,
) {
    // Non-Linux platforms fall back to no-op until a native source is added.
}

/// Windows polling event loop driven by Toolhelp32 snapshot diffs.
///
/// Every [`WINDOWS_POLL_INTERVAL`] the source snapshots all processes and
/// appends Fork/Exec/Exit diffs, which feed the same shared event handler as
/// netlink events on Linux. The snapshot diff is both the event source and the
/// refresh source: `fallback_refresh` is Linux-only because it rebuilds trees
/// from `/proc/<pid>/task/<pid>/children`, which has no equivalent here.
///
/// Cost note: a full snapshot is typically a few hundred processes, so diffing
/// every 500 ms is acceptable. Processes that start and exit within a single
/// poll interval are missed; that matches the accuracy of a 500 ms poll.
#[cfg(target_os = "windows")]
fn event_loop(
    source: Option<Arc<PlatformSource>>,
    sessions: Arc<Mutex<HashMap<String, SessionState>>>,
    pid_to_session: Arc<Mutex<HashMap<u32, String>>>,
    shared: Arc<SharedState>,
    running: Arc<AtomicBool>,
) {
    ERROR_LOG.log("[process-monitor] windows polling loop starting".to_string());
    while running.load(Ordering::Relaxed) {
        std::thread::sleep(WINDOWS_POLL_INTERVAL);
        let Some(source) = source.as_ref() else {
            continue;
        };
        let mut process_events = Vec::new();
        match source.read_events(&mut process_events) {
            Ok(count) => {
                if count > 0 {
                    handle_process_events(process_events, &sessions, &pid_to_session, &shared);
                }
            }
            Err(error) => {
                ERROR_LOG.log(format!(
                    "[process-monitor] toolhelp snapshot diff error: {error}"
                ));
            }
        }
    }
    ERROR_LOG.log("[process-monitor] windows polling loop stopped".to_string());
}

/// Apply normalized process events to the affected session trees and emit any
/// resulting command name / task state changes.
///
/// Lock order: `pid_to_session` is acquired before `sessions`, matching
/// [`ProcessMonitor::register_session`] and `fallback_refresh`.
// On macOS no event source calls this yet; the allow is scoped to that stub.
#[cfg_attr(target_os = "macos", allow(dead_code))]
fn handle_process_events(
    events: Vec<ProcessEvent>,
    sessions: &Arc<Mutex<HashMap<String, SessionState>>>,
    pid_to_session: &Arc<Mutex<HashMap<u32, String>>>,
    shared: &Arc<SharedState>,
) {
    for event in events {
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
                    state.last_netlink_event = Some(Instant::now());
                    state.tree.add_child(parent_pid, child_pid, argv0, argv);
                    recompute_and_emit(&target_id, state, shared);
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
                    state.last_netlink_event = Some(Instant::now());
                    state.tree.update_exec(pid, argv0, argv);
                    recompute_and_emit(&target_id, state, shared);
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
                    state.last_netlink_event = Some(Instant::now());
                    let shell_exited = pid == state.tree.shell_pid();
                    state.tree.remove(pid);
                    if shell_exited {
                        sessions.remove(&target_id);
                    } else {
                        recompute_and_emit(&target_id, state, shared);
                    }
                }
            }
        }
    }
}

/// Refresh sessions whose netlink event stream appears stale by rebuilding the
/// process tree from `/proc`. This is the fallback path used when the kernel
/// proc connector is silent.
///
/// Lock order: `pid_to_session` is acquired before `sessions` for each target.
#[cfg(target_os = "linux")]
fn fallback_refresh(
    sessions: &Arc<Mutex<HashMap<String, SessionState>>>,
    pid_to_session: &Arc<Mutex<HashMap<u32, String>>>,
    shared: &Arc<SharedState>,
) {
    let now = Instant::now();
    let stale_ids: Vec<String> = {
        let sessions = sessions.lock().unwrap_or_else(|e| e.into_inner());
        sessions
            .iter()
            .filter(|(_, state)| {
                state
                    .last_netlink_event
                    .map(|t| now.duration_since(t) >= NETLINK_STALE_THRESHOLD)
                    .unwrap_or(true)
            })
            .map(|(id, _)| id.clone())
            .collect()
    };

    for target_id in stale_ids {
        // Acquire pid_to_session first to maintain the module lock order.
        let mut pid_map = pid_to_session.lock().unwrap_or_else(|e| e.into_inner());
        let mut sessions = sessions.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(state) = sessions.get_mut(&target_id) {
            refresh_session_from_proc(&target_id, state, &mut pid_map, shared);
        }
    }
}

/// Rebuild the process tree for a single session from `/proc` and emit any
/// resulting command name / task state changes.
///
/// The caller must hold both locks: `pid_to_session` first, then `sessions`.
#[cfg(target_os = "linux")]
fn refresh_session_from_proc(
    target_id: &str,
    state: &mut SessionState,
    pid_map: &mut HashMap<u32, String>,
    shared: &Arc<SharedState>,
) {
    let shell_pid = state.tree.shell_pid();

    // Remove stale children so the rebuild reflects the current /proc state.
    let old_children: Vec<u32> = state
        .tree
        .pids()
        .into_iter()
        .filter(|&p| p != shell_pid)
        .collect();
    for pid in old_children {
        state.tree.remove(pid);
        pid_map.remove(&pid);
    }

    // Rebuild one level of descendants. If a direct child is a shell wrapper
    // (e.g. `bash -c "..."`), recurse one level to find the real process.
    rebuild_children(target_id, shell_pid, shell_pid, state, pid_map, 0);

    recompute_and_emit(target_id, state, shared);
}

/// Recursively rebuild children of `parent_pid` into `state.tree`.
#[cfg(target_os = "linux")]
fn rebuild_children(
    target_id: &str,
    parent_pid: u32,
    shell_pid: u32,
    state: &mut SessionState,
    pid_map: &mut HashMap<u32, String>,
    depth: usize,
) {
    if depth > 1 {
        return;
    }

    let children = read_proc_children(parent_pid);
    for child_pid in children {
        if child_pid == shell_pid {
            continue;
        }

        let (argv0, argv) = read_proc_cmdline(child_pid);
        let argv0 = argv0.unwrap_or_default();
        let argv = argv.unwrap_or_default();
        let command_name = first_argv_token(&argv0).to_string();

        state.tree.add_child(parent_pid, child_pid, argv0, argv);
        pid_map.insert(child_pid, target_id.to_string());

        if command_name.is_empty() || SHELL_NAMES.contains(&command_name.as_str()) {
            rebuild_children(target_id, child_pid, shell_pid, state, pid_map, depth + 1);
        }
    }
}

fn recompute_and_emit(target_id: &str, state: &mut SessionState, shared: &Arc<SharedState>) {
    let pane_text = (state.pane_text)();
    let (command_name, task_state) =
        derive_session_state(&state.tree, state.pty_master_fd, &pane_text);

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
        state.last_command_name = command_name.clone();
        ERROR_LOG.log(format!(
            "[process-monitor] command_name target_id={target_id} command_name={command_name:?}"
        ));
        match command_name {
            Some(command_name) => {
                // For agents that accept @-references, set agent_command_name
                // proactively so paste formatting works even before hooks fire.
                let _ = shared
                    .state_sender()
                    .send(StateEvent::SessionCommandNameChanged {
                        target_id: target_id.to_string(),
                        command_name,
                    });
            }
            None => {
                let _ = shared
                    .state_sender()
                    .send(StateEvent::SessionCommandNameCleared {
                        target_id: target_id.to_string(),
                    });
            }
        }
    }
}
