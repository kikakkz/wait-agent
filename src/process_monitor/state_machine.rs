use crate::domain::agent_detector::DetectorRegistry;
use crate::domain::session_catalog::ManagedSessionTaskState;
use crate::process_monitor::tree::SessionProcessTree;
use crate::process_monitor::PlatformPtyFd;

/// Read the foreground process group of the PTY master fd.
///
/// Returns `None` if the fd is not a TTY or the call fails.
#[cfg(target_os = "linux")]
pub(crate) fn foreground_pgid(pty_master_fd: PlatformPtyFd) -> Option<u32> {
    // SAFETY: `tcgetpgrp` is an async-signal-safe POSIX call; the fd is owned
    // by the session/IO loop and is only read here.
    let pgid = unsafe { libc::tcgetpgrp(pty_master_fd) };
    if pgid < 0 {
        None
    } else {
        Some(pgid as u32)
    }
}

/// Non-Unix stub: foreground process groups are not available.
#[cfg(not(target_os = "linux"))]
pub(crate) fn foreground_pgid(_pty_master_fd: PlatformPtyFd) -> Option<u32> {
    None
}

/// Derive the display command name and task state for a session from its
/// process tree and PTY foreground group.
pub(crate) fn derive_session_state(
    tree: &SessionProcessTree,
    pty_master_fd: PlatformPtyFd,
    pane_text: &str,
) -> (Option<String>, ManagedSessionTaskState) {
    let fg_pgid = foreground_pgid(pty_master_fd);
    let Some(primary) = tree.primary_process(fg_pgid) else {
        return (None, ManagedSessionTaskState::Input);
    };

    let registry = DetectorRegistry::default();
    let command_name =
        registry.detect_command_name(&primary.command_name, Some(&primary.argv), pane_text);

    let task_state = if registry.is_registered_agent(&command_name) {
        // Known agent: use agent-specific inference from pane text; fall back
        // to Input when inconclusive, because hook signals will switch to
        // Running once the agent actually starts working.
        let detected = registry.infer_task_state(Some(&command_name), pane_text);
        if detected == ManagedSessionTaskState::Unknown {
            ManagedSessionTaskState::Input
        } else {
            detected
        }
    } else {
        // Non-agent foreground/child command is always Running.
        ManagedSessionTaskState::Running
    };

    (Some(command_name), task_state)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn empty_tree_yields_input() {
        let tree =
            SessionProcessTree::new(1, "/bin/bash".to_string(), vec!["/bin/bash".to_string()]);
        let (name, state) = derive_session_state(&tree, -1, "");
        assert!(name.is_none());
        assert_eq!(state, ManagedSessionTaskState::Input);
    }

    #[test]
    fn vi_tree_yields_running() {
        let mut tree =
            SessionProcessTree::new(1, "/bin/bash".to_string(), vec!["/bin/bash".to_string()]);
        tree.add_child(
            1,
            10,
            "/usr/bin/vi".to_string(),
            vec!["/usr/bin/vi".to_string()],
        );
        let (name, state) = derive_session_state(&tree, -1, "");
        assert_eq!(name.as_deref(), Some("vi"));
        assert_eq!(state, ManagedSessionTaskState::Running);
    }

    #[test]
    fn shell_script_yields_script_name() {
        let mut tree =
            SessionProcessTree::new(1, "/bin/bash".to_string(), vec!["/bin/bash".to_string()]);
        tree.add_child(
            1,
            10,
            "/bin/bash".to_string(),
            vec![
                "/bin/bash".to_string(),
                "./scripts/run_local.sh".to_string(),
            ],
        );
        let (name, state) = derive_session_state(&tree, -1, "");
        assert_eq!(name.as_deref(), Some("run_local.sh"));
        assert_eq!(state, ManagedSessionTaskState::Running);
    }

    #[test]
    fn bash_c_wrapper_yields_grandchild_name() {
        let mut tree =
            SessionProcessTree::new(1, "/bin/bash".to_string(), vec!["/bin/bash".to_string()]);
        tree.add_child(
            1,
            10,
            "/bin/bash".to_string(),
            vec![
                "/bin/bash".to_string(),
                "-c".to_string(),
                "sleep 100".to_string(),
            ],
        );
        tree.add_child(
            10,
            11,
            "/usr/bin/sleep".to_string(),
            vec!["/usr/bin/sleep".to_string(), "100".to_string()],
        );
        let (name, state) = derive_session_state(&tree, -1, "");
        assert_eq!(name.as_deref(), Some("sleep"));
        assert_eq!(state, ManagedSessionTaskState::Running);
    }
}
