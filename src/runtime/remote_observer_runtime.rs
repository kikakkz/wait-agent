use crate::infra::remote_protocol::{ControlPlanePayload, ProtocolEnvelope};
use crate::runtime::remote_transport_runtime::LocalNodeMailbox;
use crate::terminal::{ScreenSnapshot, ScreenState, TerminalEngine, TerminalSize};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteObserverSnapshot {
    pub session_id: Option<String>,
    pub target_id: Option<String>,
    pub attachment_id: Option<String>,
    pub console_id: Option<String>,
    pub availability: Option<&'static str>,
    pub resize_epoch: Option<u64>,
    pub resize_authority_console_id: Option<String>,
    pub resize_authority_host_id: Option<String>,
    pub last_output_seq: Option<u64>,
    pub screen: ScreenState,
}

impl RemoteObserverSnapshot {
    pub fn active_screen(&self) -> &ScreenSnapshot {
        self.screen.active_snapshot()
    }
}

pub struct RemoteObserverRuntime {
    mailbox: LocalNodeMailbox,
    processed_envelopes: usize,
    session_id: Option<String>,
    target_id: Option<String>,
    attachment_id: Option<String>,
    console_id: Option<String>,
    availability: Option<&'static str>,
    resize_epoch: Option<u64>,
    resize_authority_console_id: Option<String>,
    resize_authority_host_id: Option<String>,
    last_output_seq: Option<u64>,
    terminal: TerminalEngine,
}

impl RemoteObserverRuntime {
    pub fn new(mailbox: LocalNodeMailbox, cols: usize, rows: usize) -> Self {
        Self {
            mailbox,
            processed_envelopes: 0,
            session_id: None,
            target_id: None,
            attachment_id: None,
            console_id: None,
            availability: None,
            resize_epoch: None,
            resize_authority_console_id: None,
            resize_authority_host_id: None,
            last_output_seq: None,
            terminal: TerminalEngine::new(terminal_size(cols, rows)),
        }
    }

    pub fn sync(&mut self) -> Result<usize, RemoteObserverRuntimeError> {
        let envelopes = self.mailbox.snapshot_from(self.processed_envelopes);
        for envelope in &envelopes {
            self.apply_envelope(envelope)?;
        }
        self.processed_envelopes += envelopes.len();
        Ok(envelopes.len())
    }

    pub fn snapshot(&self) -> RemoteObserverSnapshot {
        RemoteObserverSnapshot {
            session_id: self.session_id.clone(),
            target_id: self.target_id.clone(),
            attachment_id: self.attachment_id.clone(),
            console_id: self.console_id.clone(),
            availability: self.availability,
            resize_epoch: self.resize_epoch,
            resize_authority_console_id: self.resize_authority_console_id.clone(),
            resize_authority_host_id: self.resize_authority_host_id.clone(),
            last_output_seq: self.last_output_seq,
            screen: self.terminal.state(),
        }
    }

    fn apply_envelope(
        &mut self,
        envelope: &ProtocolEnvelope<ControlPlanePayload>,
    ) -> Result<(), RemoteObserverRuntimeError> {
        match &envelope.payload {
            ControlPlanePayload::OpenTargetOk(payload) => {
                self.session_id = Some(payload.session_id.clone());
                self.target_id = Some(payload.target_id.clone());
                self.attachment_id = Some(payload.attachment_id.clone());
                self.console_id = Some(payload.console_id.clone());
                self.availability = Some(payload.availability);
                self.resize_epoch = Some(payload.resize_epoch);
                self.resize_authority_console_id =
                    Some(payload.resize_authority_console_id.clone());
                self.resize_authority_host_id = Some(payload.resize_authority_host_id.clone());
                Ok(())
            }
            ControlPlanePayload::ResizeAuthorityChanged(payload) => {
                self.session_id = Some(payload.session_id.clone());
                self.target_id = Some(payload.target_id.clone());
                self.resize_epoch = Some(payload.resize_epoch);
                self.resize_authority_console_id =
                    Some(payload.resize_authority_console_id.clone());
                self.resize_authority_host_id = Some(payload.resize_authority_host_id.clone());
                Ok(())
            }
            ControlPlanePayload::TargetOutput(payload) => self.apply_target_output(payload),
            _ => Ok(()),
        }
    }

    fn apply_target_output(
        &mut self,
        payload: &crate::infra::remote_protocol::TargetOutputPayload,
    ) -> Result<(), RemoteObserverRuntimeError> {
        if let Some(last_output_seq) = self.last_output_seq {
            if payload.output_seq <= last_output_seq {
                return Err(RemoteObserverRuntimeError::new(format!(
                    "remote observer received out-of-order target_output for `{}`: {} after {}",
                    payload.target_id, payload.output_seq, last_output_seq
                )));
            }
        }

        let decoded = decode_base64(&payload.bytes_base64).map_err(|error| {
            RemoteObserverRuntimeError::new(format!(
                "failed to decode target_output for `{}`: {error}",
                payload.target_id
            ))
        })?;
        self.session_id = Some(payload.session_id.clone());
        self.target_id = Some(payload.target_id.clone());
        self.terminal.feed(&decoded);
        self.last_output_seq = Some(payload.output_seq);
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteObserverRuntimeError {
    message: String,
}

impl RemoteObserverRuntimeError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for RemoteObserverRuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RemoteObserverRuntimeError {}

fn terminal_size(cols: usize, rows: usize) -> TerminalSize {
    TerminalSize {
        cols: cols.clamp(1, u16::MAX as usize) as u16,
        rows: rows.clamp(1, u16::MAX as usize) as u16,
        pixel_width: 0,
        pixel_height: 0,
    }
}

fn decode_base64(input: &str) -> Result<Vec<u8>, &'static str> {
    let bytes = input.as_bytes();
    if bytes.len() % 4 != 0 {
        return Err("invalid base64 length");
    }

    let mut decoded = Vec::with_capacity((bytes.len() / 4) * 3);
    for chunk in bytes.chunks(4) {
        let mut values = [0u8; 4];
        let mut padding = 0usize;
        for (index, byte) in chunk.iter().enumerate() {
            match *byte {
                b'=' => {
                    values[index] = 0;
                    padding += 1;
                }
                _ => {
                    if padding > 0 {
                        return Err("invalid base64 padding");
                    }
                    values[index] = decode_base64_value(*byte)?;
                }
            }
        }

        decoded.push((values[0] << 2) | (values[1] >> 4));
        if padding < 2 {
            decoded.push((values[1] << 4) | (values[2] >> 2));
        }
        if padding == 0 {
            decoded.push((values[2] << 6) | values[3]);
        }
    }

    Ok(decoded)
}

fn decode_base64_value(byte: u8) -> Result<u8, &'static str> {
    match byte {
        b'A'..=b'Z' => Ok(byte - b'A'),
        b'a'..=b'z' => Ok(byte - b'a' + 26),
        b'0'..=b'9' => Ok(byte - b'0' + 52),
        b'+' => Ok(62),
        b'/' => Ok(63),
        _ => Err("invalid base64 character"),
    }
}

#[cfg(test)]
mod tests {
    use super::RemoteObserverRuntime;
    use crate::domain::session_catalog::{
        ConsoleLocation, ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState,
        SessionAvailability,
    };
    use crate::infra::remote_protocol::RemoteConsoleDescriptor;
    use crate::runtime::remote_main_slot_runtime::RemoteMainSlotRuntime;
    use crate::runtime::remote_transport_runtime::RemoteConnectionRegistry;

    #[test]
    fn sync_captures_remote_attachment_and_resize_authority_state() {
        let runtime = RemoteMainSlotRuntime::with_registry(RemoteConnectionRegistry::new());
        let mailbox = runtime
            .ensure_local_observer_connection("observer-a")
            .expect("observer loopback registration should succeed");
        runtime.ensure_local_connection("peer-a");

        runtime
            .activate_target(
                &remote_target("peer-a", "shell-1"),
                console("console-a", "observer-a"),
                120,
                40,
            )
            .expect("remote activation should succeed");

        let mut observer = RemoteObserverRuntime::new(mailbox, 120, 40);
        assert_eq!(observer.sync().expect("sync should succeed"), 2);

        let snapshot = observer.snapshot();
        assert_eq!(
            snapshot.target_id.as_deref(),
            Some("remote-peer:peer-a:shell-1")
        );
        assert_eq!(snapshot.attachment_id.as_deref(), Some("attach-1"));
        assert_eq!(snapshot.console_id.as_deref(), Some("console-a"));
        assert_eq!(snapshot.availability, Some("online"));
        assert_eq!(snapshot.resize_epoch, Some(1));
        assert_eq!(
            snapshot.resize_authority_console_id.as_deref(),
            Some("console-a")
        );
        assert_eq!(
            snapshot.resize_authority_host_id.as_deref(),
            Some("observer-a")
        );
        assert_eq!(snapshot.last_output_seq, None);
    }

    #[test]
    fn sync_feeds_target_output_into_terminal_state() {
        let runtime = RemoteMainSlotRuntime::with_registry(RemoteConnectionRegistry::new());
        let mailbox = runtime
            .ensure_local_observer_connection("observer-a")
            .expect("observer loopback registration should succeed");
        runtime.ensure_local_connection("peer-a");

        runtime
            .activate_target(
                &remote_target("peer-a", "shell-1"),
                console("console-a", "observer-a"),
                12,
                4,
            )
            .expect("remote activation should succeed");
        runtime
            .send_target_output(
                &remote_target("peer-a", "shell-1"),
                1,
                "pty",
                "aGVsbG8NCndvcmxk",
            )
            .expect("target output should fan out");

        let mut observer = RemoteObserverRuntime::new(mailbox, 12, 4);
        assert_eq!(observer.sync().expect("sync should succeed"), 3);

        let snapshot = observer.snapshot();
        assert_eq!(snapshot.last_output_seq, Some(1));
        assert_eq!(
            snapshot.active_screen().lines,
            vec![
                "hello       ".to_string(),
                "world       ".to_string(),
                "            ".to_string(),
                "            ".to_string(),
            ]
        );
    }

    #[test]
    fn sync_rejects_out_of_order_target_output() {
        let runtime = RemoteMainSlotRuntime::with_registry(RemoteConnectionRegistry::new());
        let mailbox = runtime
            .ensure_local_observer_connection("observer-a")
            .expect("observer loopback registration should succeed");
        runtime.ensure_local_connection("peer-a");

        runtime
            .activate_target(
                &remote_target("peer-a", "shell-1"),
                console("console-a", "observer-a"),
                12,
                4,
            )
            .expect("remote activation should succeed");
        runtime
            .send_target_output(&remote_target("peer-a", "shell-1"), 2, "pty", "Yg==")
            .expect("first target output should fan out");
        runtime
            .send_target_output(&remote_target("peer-a", "shell-1"), 1, "pty", "YQ==")
            .expect("second target output still routes through control plane");

        let mut observer = RemoteObserverRuntime::new(mailbox, 12, 4);
        let error = observer
            .sync()
            .expect_err("observer should reject out-of-order target output");

        assert_eq!(
            error.to_string(),
            "remote observer received out-of-order target_output for `remote-peer:peer-a:shell-1`: 1 after 2"
        );
    }

    fn console(console_id: &str, host_id: &str) -> RemoteConsoleDescriptor {
        RemoteConsoleDescriptor {
            console_id: console_id.to_string(),
            console_host_id: host_id.to_string(),
            location: ConsoleLocation::LocalWorkspace,
        }
    }

    fn remote_target(authority_id: &str, session_id: &str) -> ManagedSessionRecord {
        ManagedSessionRecord {
            address: ManagedSessionAddress::remote_peer(authority_id, session_id),
            selector: None,
            availability: SessionAvailability::Online,
            workspace_dir: None,
            workspace_key: None,
            session_role: None,
            opened_by: Vec::new(),
            attached_clients: 0,
            window_count: 1,
            command_name: Some("bash".to_string()),
            current_path: None,
            task_state: ManagedSessionTaskState::Running,
        }
    }
}
