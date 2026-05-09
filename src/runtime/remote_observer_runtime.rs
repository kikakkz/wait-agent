use crate::infra::remote_protocol::{ControlPlanePayload, ProtocolEnvelope};
use crate::runtime::remote_transport_runtime::LocalNodeMailbox;
use crate::terminal::{ScreenSnapshot, ScreenState, TerminalEngine, TerminalSize};
use std::fmt;
use std::fs::OpenOptions;
use std::io::Write;

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
    pub has_visible_output: bool,
    pub bootstrap_complete: bool,
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
    has_visible_output: bool,
    bootstrap_complete: bool,
    terminal: TerminalEngine,
    pending_raw_output: Vec<u8>,
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
            has_visible_output: false,
            bootstrap_complete: false,
            terminal: TerminalEngine::new(terminal_size(cols, rows)),
            pending_raw_output: Vec::new(),
        }
    }

    /// Sync pending envelopes and return the raw decoded output bytes from
    /// TargetOutput, MirrorBootstrapChunk, and MirrorBootstrapComplete envelopes.
    /// The raw bytes can be written directly to stdout for incremental terminal
    /// rendering (SSH-like forwarding), while the observer continues to update
    /// its TerminalEngine state for cursor tracking and input translation.
    pub fn sync_and_collect_raw(&mut self) -> Result<Vec<u8>, RemoteObserverRuntimeError> {
        self.pending_raw_output.clear();
        let _ = self.sync()?;
        Ok(std::mem::take(&mut self.pending_raw_output))
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
            has_visible_output: self.has_visible_output,
            bootstrap_complete: self.bootstrap_complete,
            screen: self.terminal.state(),
        }
    }

    pub fn begin_bootstrap(&mut self) {
        let size = self.terminal.snapshot().size;
        self.bootstrap_complete = false;
        self.has_visible_output = false;
        self.last_output_seq = None;
        self.terminal = TerminalEngine::new(size);
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
            ControlPlanePayload::MirrorBootstrapChunk(payload) => {
                self.apply_bootstrap_chunk(payload)
            }
            ControlPlanePayload::MirrorBootstrapComplete(payload) => {
                self.apply_bootstrap_complete(payload);
                Ok(())
            }
            ControlPlanePayload::TargetOutput(payload) => self.apply_target_output(payload),
            other => {
                eprintln!(
                    "[observer] ignored envelope type={} message_id={}",
                    other.message_type(),
                    envelope.message_id,
                );
                Ok(())
            }
        }
    }

    fn apply_bootstrap_chunk(
        &mut self,
        payload: &crate::infra::remote_protocol::MirrorBootstrapChunkPayload,
    ) -> Result<(), RemoteObserverRuntimeError> {
        self.session_id = Some(payload.session_id.clone());
        self.target_id = Some(payload.target_id.clone());
        self.pending_raw_output
            .extend_from_slice(&payload.output_bytes);
        self.terminal.feed(&payload.output_bytes);
        self.has_visible_output = true;
        Ok(())
    }

    fn apply_bootstrap_complete(
        &mut self,
        payload: &crate::infra::remote_protocol::MirrorBootstrapCompletePayload,
    ) {
        self.session_id = Some(payload.session_id.clone());
        self.target_id = Some(payload.target_id.clone());
        let mut suffix = String::new();
        if payload.alternate_screen_active {
            suffix.push_str("\x1b[?1049h");
        }
        if payload.application_cursor_keys {
            suffix.push_str("\x1b[?1h");
        }
        suffix.push_str(if payload.cursor_visible {
            "\x1b[?25h"
        } else {
            "\x1b[?25l"
        });
        self.terminal.feed(suffix.as_bytes());
        self.bootstrap_complete = true;
        if !suffix.is_empty() {
            self.pending_raw_output.extend_from_slice(suffix.as_bytes());
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

        self.session_id = Some(payload.session_id.clone());
        self.target_id = Some(payload.target_id.clone());

        self.pending_raw_output
            .extend_from_slice(&payload.output_bytes);
        self.terminal.feed(&payload.output_bytes);

        self.last_output_seq = Some(payload.output_seq);
        self.has_visible_output = true;
        debug_log_observer_state(
            "observer.apply_target_output",
            payload.target_id.as_str(),
            payload.output_seq,
            &self.terminal.state().active_snapshot().lines,
        );
        Ok(())
    }
}

fn debug_log_observer_state(stage: &str, target_id: &str, output_seq: u64, lines: &[String]) {
    let Ok(path) = std::env::var("WAITAGENT_REMOTE_DEBUG_LOG") else {
        return;
    };
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };

    let _ = writeln!(file, "[{stage}] target={target_id} output_seq={output_seq}");
    for (index, line) in lines.iter().take(12).enumerate() {
        let _ = writeln!(file, "L{:02}: {:?}", index + 1, line);
    }
    let _ = writeln!(file);
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
                b"hello\r\nworld".to_vec(),
            )
            .expect("target output should fan out");

        let mut observer = RemoteObserverRuntime::new(mailbox, 12, 4);
        assert_eq!(observer.sync().expect("sync should succeed"), 3);

        let snapshot = observer.snapshot();
        assert_eq!(snapshot.last_output_seq, Some(1));
        assert!(snapshot.has_visible_output);
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
            .send_target_output(&remote_target("peer-a", "shell-1"), 2, "pty", b"b".to_vec())
            .expect("first target output should fan out");
        runtime
            .send_target_output(&remote_target("peer-a", "shell-1"), 1, "pty", b"a".to_vec())
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

    #[test]
    fn sync_feeds_bootstrap_into_terminal_state_without_output_seq() {
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
            .send_mirror_bootstrap_chunk(
                &remote_target("peer-a", "shell-1"),
                1,
                "pty",
                b"hello\r\nworld".to_vec(),
            )
            .expect("bootstrap chunk should fan out");
        runtime
            .send_mirror_bootstrap_complete(
                &remote_target("peer-a", "shell-1"),
                1,
                false,
                false,
                true,
            )
            .expect("bootstrap complete should fan out");

        let mut observer = RemoteObserverRuntime::new(mailbox, 12, 4);
        assert_eq!(observer.sync().expect("sync should succeed"), 4);

        let snapshot = observer.snapshot();
        assert_eq!(snapshot.last_output_seq, None);
        assert!(snapshot.has_visible_output);
        assert!(snapshot.bootstrap_complete);
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
    fn sync_bootstrap_replay_preserves_prompt_space_and_cursor_position() {
        let runtime = RemoteMainSlotRuntime::with_registry(RemoteConnectionRegistry::new());
        let mailbox = runtime
            .ensure_local_observer_connection("observer-a")
            .expect("observer loopback registration should succeed");
        runtime.ensure_local_connection("peer-a");

        runtime
            .activate_target(
                &remote_target("peer-a", "shell-1"),
                console("console-a", "observer-a"),
                32,
                4,
            )
            .expect("remote activation should succeed");
        runtime
            .send_mirror_bootstrap_chunk(
                &remote_target("peer-a", "shell-1"),
                1,
                "pty",
                b"\x1b[2J\x1b[H\x1b[1;1Hkk@lenovo:~/wait-agent$ \x1b[1;25H".to_vec(),
            )
            .expect("bootstrap replay should fan out");
        runtime
            .send_mirror_bootstrap_complete(
                &remote_target("peer-a", "shell-1"),
                1,
                false,
                false,
                true,
            )
            .expect("bootstrap complete should fan out");

        let mut observer = RemoteObserverRuntime::new(mailbox, 32, 4);
        observer.sync().expect("sync should succeed");

        let snapshot = observer.snapshot();
        assert!(snapshot.active_screen().lines[0].starts_with("kk@lenovo:~/wait-agent$ "));
        assert_eq!(snapshot.active_screen().cursor_row, 0);
        assert_eq!(snapshot.active_screen().cursor_col, 24);
        assert!(snapshot.has_visible_output);
        assert!(snapshot.bootstrap_complete);
    }

    #[test]
    fn repeated_bootstrap_replay_replaces_screen_without_corrupting_prompt() {
        let runtime = RemoteMainSlotRuntime::with_registry(RemoteConnectionRegistry::new());
        let mailbox = runtime
            .ensure_local_observer_connection("observer-a")
            .expect("observer loopback registration should succeed");
        runtime.ensure_local_connection("peer-a");

        runtime
            .activate_target(
                &remote_target("peer-a", "shell-1"),
                console("console-a", "observer-a"),
                32,
                4,
            )
            .expect("remote activation should succeed");
        let replay = b"\x1b[2J\x1b[H\x1b[1;1Hkk@lenovo:~/wait-agent$ \x1b[1;25H";
        runtime
            .send_mirror_bootstrap_chunk(
                &remote_target("peer-a", "shell-1"),
                1,
                "pty",
                replay.to_vec(),
            )
            .expect("first bootstrap replay should fan out");
        runtime
            .send_mirror_bootstrap_complete(
                &remote_target("peer-a", "shell-1"),
                1,
                false,
                false,
                true,
            )
            .expect("first bootstrap complete should fan out");
        runtime
            .send_mirror_bootstrap_chunk(
                &remote_target("peer-a", "shell-1"),
                2,
                "pty",
                replay.to_vec(),
            )
            .expect("second bootstrap replay should fan out");
        runtime
            .send_mirror_bootstrap_complete(
                &remote_target("peer-a", "shell-1"),
                2,
                false,
                false,
                true,
            )
            .expect("second bootstrap complete should fan out");

        let mut observer = RemoteObserverRuntime::new(mailbox, 32, 4);
        observer.sync().expect("sync should succeed");

        let snapshot = observer.snapshot();
        assert!(snapshot.active_screen().lines[0].starts_with("kk@lenovo:~/wait-agent$ "));
        assert_eq!(snapshot.active_screen().cursor_row, 0);
        assert_eq!(snapshot.active_screen().cursor_col, 24);
        assert!(snapshot.has_visible_output);
        assert!(snapshot.bootstrap_complete);
    }

    #[test]
    fn observe_codex_update_menu_bootstrap_plus_live_down_redraw_state() {
        let runtime = RemoteMainSlotRuntime::with_registry(RemoteConnectionRegistry::new());
        let mailbox = runtime
            .ensure_local_observer_connection("observer-a")
            .expect("observer loopback registration should succeed");
        runtime.ensure_local_connection("peer-a");

        runtime
            .activate_target(
                &remote_target("peer-a", "shell-1"),
                console("console-a", "observer-a"),
                47,
                21,
            )
            .expect("remote activation should succeed");

        let bootstrap_screen = concat!(
            "\n",
            "  ✨\u{200a}Update available! \x1b[2m0.125.0 -> 0.128.0\x1b[0m      \n",
            "\n",
            "  \x1b[2mRelease notes: \x1b[4mhttps://github.com/openai/code\n",
            "\n",
            "\x1b[0m› 1. Update now (runs `npm install -g          \n",
            "     @openai/codex`)   \n",
            "  2. Skip  \n",
            "  3. Skip until next version                  \n",
            "\n",
            "  \x1b[2mPress enter to continue\x1b[0m                    \n",
            "\n\n\n\n\n\n\n\n\n\n",
        );
        let mut bootstrap = String::from("\x1b[2J\x1b[H");
        for (index, line) in bootstrap_screen.lines().enumerate() {
            bootstrap.push_str(&format!("\x1b[{};1H{}", index + 1, line));
        }
        bootstrap.push_str("\x1b[11;26H");
        let redraw = b"\x1b[?2026h\x1b[1;2H\x1b[0m\x1b[m\x1b[K\x1b[2;42H\x1b[0m\x1b[m\x1b[K\x1b[3;2H\x1b[0m\x1b[m\x1b[K\x1b[5;2H\x1b[0m\x1b[m\x1b[K\x1b[6;38H\x1b[0m\x1b[m\x1b[K\x1b[7;21H\x1b[0m\x1b[m\x1b[K\x1b[8;10H\x1b[0m\x1b[m\x1b[K\x1b[9;29H\x1b[0m\x1b[m\x1b[K\x1b[10;2H\x1b[0m\x1b[m\x1b[K\x1b[11;26H\x1b[0m\x1b[m\x1b[K\x1b[12;2H\x1b[0m\x1b[m\x1b[K\x1b[13;2H\x1b[0m\x1b[m\x1b[K\x1b[14;2H\x1b[0m\x1b[m\x1b[K\x1b[15;2H\x1b[0m\x1b[m\x1b[K\x1b[16;2H\x1b[0m\x1b[m\x1b[K\x1b[17;2H\x1b[0m\x1b[m\x1b[K\x1b[18;2H\x1b[0m\x1b[m\x1b[K\x1b[19;2H\x1b[0m\x1b[m\x1b[K\x1b[20;2H\x1b[0m\x1b[m\x1b[K\x1b[21;2H\x1b[0m\x1b[m\x1b[K\x1b[6;1H  1. Update now (runs `npm install -g\x1b[7;6H@openai/codex`)\x1b[8;1H\x1b[;m\xe2\x80\xba 2. Skip\x1b[m\x1b[m\x1b[0m\x1b[?25l\x1b[?2026l";

        runtime
            .send_mirror_bootstrap_chunk(
                &remote_target("peer-a", "shell-1"),
                1,
                "pty",
                bootstrap.into_bytes(),
            )
            .expect("bootstrap replay should fan out");
        runtime
            .send_mirror_bootstrap_complete(
                &remote_target("peer-a", "shell-1"),
                1,
                false,
                false,
                false,
            )
            .expect("bootstrap complete should fan out");
        runtime
            .send_target_output(
                &remote_target("peer-a", "shell-1"),
                1,
                "pty",
                redraw.to_vec(),
            )
            .expect("redraw should fan out");

        let mut observer = RemoteObserverRuntime::new(mailbox, 47, 21);
        observer.sync().expect("observer sync should succeed");
        let snapshot = observer.snapshot();

        eprintln!("observer line2={:?}", snapshot.active_screen().lines[1]);
        eprintln!("observer line6={:?}", snapshot.active_screen().lines[5]);
        eprintln!("observer line7={:?}", snapshot.active_screen().lines[6]);
        eprintln!("observer line8={:?}", snapshot.active_screen().lines[7]);
        eprintln!("observer line9={:?}", snapshot.active_screen().lines[8]);
        eprintln!(
            "observer cursor=({}, {})",
            snapshot.active_screen().cursor_row,
            snapshot.active_screen().cursor_col
        );

        assert!(
            snapshot.active_screen().lines[1]
                .starts_with("  ✨ Update available! 0.125.0 -> 0.128.0"),
            "unexpected observer line2: {:?}",
            snapshot.active_screen().lines[1]
        );
        assert_eq!(
            snapshot.active_screen().lines[5],
            "  1. Update now (runs `npm install -g          "
        );
        assert_eq!(
            snapshot.active_screen().lines[6],
            "     @openai/codex`)                           "
        );
        assert_eq!(
            snapshot.active_screen().lines[7],
            "› 2. Skip                                      "
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
