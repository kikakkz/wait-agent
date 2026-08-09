use crate::domain::agent_detector::{first_argv_token, SHELL_NAMES};
use std::collections::HashMap;

/// A single process node inside a session's process tree.
#[derive(Debug, Clone)]
pub(crate) struct ProcessNode {
    pub parent_pid: Option<u32>,
    pub command_name: String,
    pub argv: Vec<String>,
    pub children: Vec<u32>,
}

impl ProcessNode {
    /// Return true if this process is a known shell.
    pub fn is_shell(&self) -> bool {
        SHELL_NAMES.contains(&self.command_name.as_str())
    }
}

/// Process tree rooted at a session's shell pid.
#[derive(Debug, Default)]
pub(crate) struct SessionProcessTree {
    shell_pid: u32,
    processes: HashMap<u32, ProcessNode>,
}

impl SessionProcessTree {
    /// Create a new tree rooted at `shell_pid`.
    pub fn new(shell_pid: u32, argv0: String, argv: Vec<String>) -> Self {
        let mut processes = HashMap::new();
        processes.insert(
            shell_pid,
            ProcessNode {
                parent_pid: None,
                command_name: first_argv_token(&argv0).to_string(),
                argv,
                children: Vec::new(),
            },
        );
        Self {
            shell_pid,
            processes,
        }
    }

    /// Register a forked child under its parent.
    pub fn add_child(&mut self, parent_pid: u32, child_pid: u32, argv0: String, argv: Vec<String>) {
        if !self.processes.contains_key(&parent_pid) && parent_pid != self.shell_pid {
            // Unknown parent; ignore.
            return;
        }

        let command_name = first_argv_token(&argv0).to_string();
        let node = ProcessNode {
            parent_pid: Some(parent_pid),
            command_name,
            argv,
            children: Vec::new(),
        };

        if let Some(parent) = self.processes.get_mut(&parent_pid) {
            if !parent.children.contains(&child_pid) {
                parent.children.push(child_pid);
            }
        }
        self.processes.insert(child_pid, node);
    }

    /// Update a process's command after execve.
    pub fn update_exec(&mut self, pid: u32, argv0: String, argv: Vec<String>) {
        if let Some(node) = self.processes.get_mut(&pid) {
            node.command_name = first_argv_token(&argv0).to_string();
            node.argv = argv;
        }
    }

    /// Remove a process and detach its children.
    pub fn remove(&mut self, pid: u32) {
        let Some(node) = self.processes.remove(&pid) else {
            return;
        };
        if let Some(parent_pid) = node.parent_pid {
            if let Some(parent) = self.processes.get_mut(&parent_pid) {
                parent.children.retain(|c| *c != pid);
            }
        }
    }

    /// Return the shell pid for this tree.
    pub fn shell_pid(&self) -> u32 {
        self.shell_pid
    }

    /// Return the process node for `pid`, if known.
    #[allow(dead_code)]
    pub fn get(&self, pid: u32) -> Option<&ProcessNode> {
        self.processes.get(&pid)
    }

    /// Return true if `pid` is known in this tree.
    #[allow(dead_code)]
    pub fn contains(&self, pid: u32) -> bool {
        self.processes.contains_key(&pid)
    }

    /// Return all pids currently tracked in this tree.
    pub fn pids(&self) -> Vec<u32> {
        self.processes.keys().copied().collect()
    }

    /// Find the most significant non-shell process in the tree.
    ///
    /// Priority:
    /// 1. The foreground process group leader, if it is a known non-shell node.
    /// 2. A direct non-shell child of the shell, preferring processes with children.
    /// 3. A non-shell grandchild (handles `bash -c "sleep 100"` wrappers).
    pub fn primary_process(&self, foreground_pgid: Option<u32>) -> Option<&ProcessNode> {
        // 1. Foreground process group leader.
        if let Some(pgid) = foreground_pgid {
            if let Some(node) = self.processes.get(&pgid) {
                if !node.command_name.is_empty() && !node.is_shell() {
                    return Some(node);
                }
            }
        }

        // 2 & 3. Scan shell descendants.
        let shell = self.processes.get(&self.shell_pid)?;
        let mut best: Option<(usize, &ProcessNode)> = None;

        for direct_pid in &shell.children {
            let direct = match self.processes.get(direct_pid) {
                Some(n) => n,
                None => continue,
            };

            if direct.command_name.is_empty() || direct.is_shell() {
                // Shell wrapper (e.g. bash -c "..."): look at grandchildren.
                for grandchild_pid in &direct.children {
                    let grandchild = match self.processes.get(grandchild_pid) {
                        Some(n) => n,
                        None => continue,
                    };
                    if grandchild.command_name.is_empty() || grandchild.is_shell() {
                        continue;
                    }
                    let score = grandchild.children.len();
                    if best.is_none_or(|(best_score, _)| score > best_score) {
                        best = Some((score, grandchild));
                    }
                }
                continue;
            }

            // Direct non-shell child is strongly preferred.
            let score = 100 + direct.children.len();
            if best.is_none_or(|(best_score, _)| score > best_score) {
                best = Some((score, direct));
            }
        }

        best.map(|(_, node)| node)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_tracks_fork_exec_exit() {
        let mut tree =
            SessionProcessTree::new(1, "/bin/bash".to_string(), vec!["/bin/bash".to_string()]);
        tree.add_child(
            1,
            10,
            "/usr/bin/vi".to_string(),
            vec!["/usr/bin/vi".to_string()],
        );
        assert!(tree.contains(10));
        assert_eq!(tree.get(10).unwrap().command_name, "vi");

        tree.update_exec(
            10,
            "/usr/bin/vim".to_string(),
            vec!["/usr/bin/vim".to_string()],
        );
        assert_eq!(tree.get(10).unwrap().command_name, "vim");

        tree.remove(10);
        assert!(!tree.contains(10));
    }

    #[test]
    fn primary_prefers_foreground() {
        let mut tree =
            SessionProcessTree::new(1, "/bin/bash".to_string(), vec!["/bin/bash".to_string()]);
        tree.add_child(
            1,
            10,
            "/usr/bin/chrome".to_string(),
            vec!["/usr/bin/chrome".to_string()],
        );
        tree.add_child(
            1,
            11,
            "/usr/bin/vi".to_string(),
            vec!["/usr/bin/vi".to_string()],
        );

        let primary = tree.primary_process(Some(11));
        assert_eq!(primary.unwrap().command_name, "vi");
    }

    #[test]
    fn primary_prefers_child_with_descendants() {
        let mut tree =
            SessionProcessTree::new(1, "/bin/bash".to_string(), vec!["/bin/bash".to_string()]);
        tree.add_child(
            1,
            10,
            "/usr/bin/chrome".to_string(),
            vec!["/usr/bin/chrome".to_string()],
        );
        tree.add_child(
            10,
            11,
            "/usr/bin/chrome-renderer".to_string(),
            vec!["/usr/bin/chrome-renderer".to_string()],
        );
        tree.add_child(
            1,
            12,
            "/usr/bin/sleep".to_string(),
            vec!["/usr/bin/sleep".to_string()],
        );

        let primary = tree.primary_process(None);
        assert_eq!(primary.unwrap().command_name, "chrome");
    }

    #[test]
    fn primary_ignores_shell_children_and_uses_grandchild() {
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

        let primary = tree.primary_process(None);
        assert_eq!(primary.unwrap().command_name, "sleep");
    }

    #[test]
    fn primary_returns_none_for_shell_only() {
        let mut tree =
            SessionProcessTree::new(1, "/bin/bash".to_string(), vec!["/bin/bash".to_string()]);
        tree.add_child(
            1,
            10,
            "/bin/bash".to_string(),
            vec!["/bin/bash".to_string()],
        );
        assert!(tree.primary_process(None).is_none());
    }
}
