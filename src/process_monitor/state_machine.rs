use crate::domain::agent_detector::DetectorRegistry;
use crate::domain::session_catalog::ManagedSessionTaskState;
use crate::process_monitor::tree::SessionProcessTree;
use std::os::unix::io::RawFd;

/// Read the foreground process group of the PTY master fd.
///
/// Returns `None` if the fd is not a TTY or the call fails.
#[cfg(target_os = "linux")]
pub(crate) fn foreground_pgid(pty_master_fd: RawFd) -> Option<u32> {
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
pub(crate) fn foreground_pgid(_pty_master_fd: RawFd) -> Option<u32> {
    None
}

/// Derive the display command name and task state for a session from its
/// process tree, PTY foreground group, and pane text.
pub(crate) fn derive_session_state(
    tree: &SessionProcessTree,
    fg_pgid: Option<u32>,
    pane_text: &str,
) -> (Option<String>, ManagedSessionTaskState) {
    let registry = DetectorRegistry::default();

    // 1. The foreground process group leader is authoritative. If it points to
    //    a known non-shell process, the session is Running regardless of pane
    //    text.  An empty command name is treated as an unknown running process
    //    so that transient /proc races do not flicker to Input.
    if let Some(pgid) = fg_pgid {
        if let Some(node) = tree.get(pgid) {
            if !node.is_shell() {
                let command_name =
                    registry.detect_command_name(&node.command_name, Some(&node.argv), pane_text);
                if command_name.is_empty() {
                    return (None, ManagedSessionTaskState::Running);
                }
                return (Some(command_name), ManagedSessionTaskState::Running);
            }
        }
    }

    // 2. The foreground is the shell itself (or we cannot read it). Use the
    //    pane text to decide whether the shell is at a prompt or executing
    //    commands. This handles sourced scripts, shell functions, and any case
    //    where the foreground process group points back to the shell.
    let shell_state = registry.infer_task_state(None, pane_text);
    if shell_state == ManagedSessionTaskState::Input
        || shell_state == ManagedSessionTaskState::Confirm
    {
        return (None, shell_state);
    }

    // 3. Not at a prompt: fall back to the most significant non-shell descendant.
    if let Some(primary) = tree.primary_process(None) {
        let command_name =
            registry.detect_command_name(&primary.command_name, Some(&primary.argv), pane_text);
        if command_name.is_empty() {
            return (None, ManagedSessionTaskState::Running);
        }
        return (Some(command_name), ManagedSessionTaskState::Running);
    }

    // 4. No evidence of a running command.
    (
        None,
        if shell_state == ManagedSessionTaskState::Unknown {
            ManagedSessionTaskState::Input
        } else {
            shell_state
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_tree_yields_input() {
        let tree =
            SessionProcessTree::new(1, "/bin/bash".to_string(), vec!["/bin/bash".to_string()]);
        let (name, state) = derive_session_state(&tree, None, "");
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
        let (name, state) = derive_session_state(&tree, None, "");
        assert_eq!(name.as_deref(), Some("vi"));
        assert_eq!(state, ManagedSessionTaskState::Running);
    }

    #[test]
    fn foreground_script_yields_running() {
        let mut tree =
            SessionProcessTree::new(1, "/bin/bash".to_string(), vec!["/bin/bash".to_string()]);
        tree.add_child(
            1,
            10,
            "/home/user/deploy.sh".to_string(),
            vec!["/home/user/deploy.sh".to_string()],
        );
        let (name, state) = derive_session_state(&tree, Some(10), "");
        assert_eq!(name.as_deref(), Some("deploy.sh"));
        assert_eq!(state, ManagedSessionTaskState::Running);
    }

    #[test]
    fn foreground_unknown_process_yields_running_without_clearing_name() {
        let mut tree =
            SessionProcessTree::new(1, "/bin/bash".to_string(), vec!["/bin/bash".to_string()]);
        tree.add_child(1, 10, "".to_string(), vec![]);
        let (name, state) = derive_session_state(&tree, Some(10), "");
        assert!(name.is_none());
        assert_eq!(state, ManagedSessionTaskState::Running);
    }

    #[test]
    fn shell_prompt_yields_input() {
        let tree =
            SessionProcessTree::new(1, "/bin/bash".to_string(), vec!["/bin/bash".to_string()]);
        let (name, state) = derive_session_state(&tree, None, "user@host:~$ ");
        assert!(name.is_none());
        assert_eq!(state, ManagedSessionTaskState::Input);
    }

    #[test]
    fn shell_output_without_prompt_yields_running() {
        let tree =
            SessionProcessTree::new(1, "/bin/bash".to_string(), vec!["/bin/bash".to_string()]);
        let (name, state) = derive_session_state(&tree, None, "Building...\nCompiling foo");
        assert!(name.is_none());
        assert_eq!(state, ManagedSessionTaskState::Running);
    }

    #[test]
    fn script_exit_returns_to_input() {
        let mut tree =
            SessionProcessTree::new(1, "/bin/bash".to_string(), vec!["/bin/bash".to_string()]);
        tree.add_child(
            1,
            10,
            "/usr/bin/vi".to_string(),
            vec!["/usr/bin/vi".to_string()],
        );
        tree.remove(10);
        let (name, state) = derive_session_state(&tree, None, "user@host:~$ ");
        assert!(name.is_none());
        assert_eq!(state, ManagedSessionTaskState::Input);
    }
}
