use crate::domain::workspace::stable_workspace_key;
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonStatusRecord {
    pub key: String,
    pub workspace_dir: PathBuf,
    pub socket_path: PathBuf,
    pub node_id: String,
    pub child_pid: String,
    pub ready: bool,
    pub attached_clients: usize,
    pub rows: u16,
    pub cols: u16,
}

impl DaemonStatusRecord {
    pub fn parse(status: &str, socket_path: PathBuf) -> Option<Self> {
        let mut fields = HashMap::<String, String>::new();
        for line in status.lines() {
            let (key, value) = line.split_once(": ")?;
            fields.insert(key.to_string(), value.to_string());
        }

        let workspace_dir = PathBuf::from(fields.get("workspace")?.clone());
        let key = fields
            .get("key")
            .cloned()
            .unwrap_or_else(|| stable_workspace_key(&workspace_dir));
        let node_id = fields.get("node")?.clone();
        let child_pid = fields.get("child_pid")?.clone();
        let ready = fields.get("ready").map(|value| value == "yes")?;
        let attached_clients = fields.get("attached_clients")?.parse::<usize>().ok()?;
        let (rows, cols) = parse_screen_size(fields.get("screen_size")?)?;

        Some(Self {
            key,
            workspace_dir,
            socket_path,
            node_id,
            child_pid,
            ready,
            attached_clients,
            rows,
            cols,
        })
    }

    pub fn summary_line(&self) -> String {
        format!(
            "{}: {} | node={} | ready={} | attached={} | pid={} | size={}x{} | socket={}",
            self.key,
            self.workspace_dir.display(),
            self.node_id,
            if self.ready { "yes" } else { "no" },
            self.attached_clients,
            self.child_pid,
            self.cols,
            self.rows,
            self.socket_path.display()
        )
    }
}

pub fn daemon_status_ready(status: &str) -> bool {
    status
        .lines()
        .find_map(|line| line.strip_prefix("ready: "))
        .map(|value| value == "yes")
        .unwrap_or(false)
}

fn parse_screen_size(value: &str) -> Option<(u16, u16)> {
    let (rows, cols) = value.split_once('x')?;
    Some((rows.parse().ok()?, cols.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::{daemon_status_ready, DaemonStatusRecord};
    use std::path::PathBuf;

    #[test]
    fn daemon_status_requires_ready_yes() {
        assert!(daemon_status_ready(
            "workspace: /tmp/demo\nready: yes\nattached: no"
        ));
        assert!(!daemon_status_ready("workspace: /tmp/demo\nattached: no"));
        assert!(!daemon_status_ready(
            "workspace: /tmp/demo\nready: no\nattached: no"
        ));
    }

    #[test]
    fn parses_daemon_status_fields() {
        let status = "\
workspace: /tmp/demo\n\
socket: /run/user/1000/waitagent/demo.sock\n\
key: abc123\n\
node: local\n\
child_pid: 4242\n\
ready: yes\n\
attached_clients: 2\n\
screen_size: 24x80\n\
initial_size: 24x80\n\
alternate_screen: yes";
        let parsed =
            DaemonStatusRecord::parse(status, PathBuf::from("/run/user/1000/waitagent/demo.sock"))
                .expect("status should parse");
        assert_eq!(parsed.key, "abc123");
        assert_eq!(parsed.workspace_dir, PathBuf::from("/tmp/demo"));
        assert_eq!(parsed.attached_clients, 2);
        assert_eq!(parsed.rows, 24);
        assert_eq!(parsed.cols, 80);
        assert!(parsed.ready);
    }
}
