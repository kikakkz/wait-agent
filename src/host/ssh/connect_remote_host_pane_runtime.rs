// Legacy tmux-era remote-host pane runtime kept during the ratatui migration; most items are currently unused.

use crate::cli::{prepend_global_network_args, ConnectRemoteHostPaneCommand, RemoteNetworkConfig};
use crate::host::ssh::remote_host_history_store::{
    RemoteHostAuthProfile, RemoteHostHistoryStore, RemoteHostKind, RemoteHostProfile,
    RemotePortPreference,
};
use crate::host::ssh::remote_host_secret_store::{
    KeyringRemoteHostSecretStore, RemoteHostSecretId, RemoteHostSecretStore, RemoteHostSecretValue,
};
use crate::host::ssh::remote_install_proxy_store::{
    no_proxy_for_install, RemoteInstallProxyProfile, RemoteInstallProxySettings,
    RemoteInstallProxyStore,
};
use crate::host::ssh::remote_shell::RemoteShellKind;
use crate::infra::relay_toml_store::RelayTomlConfig;
use crate::infra::remote_grpc_transport::RemoteNodeVia;
use crate::lifecycle::LifecycleError;
use crate::process::current_executable::current_waitagent_executable;
use crate::ratatui_node::clipboard_reader::{read_clipboard, ClipboardReadResult};
use crate::ratatui_node::node_runtime::ServerMessageJson;
use base64::{engine::general_purpose, Engine as _};
use crossbeam_channel::{unbounded, Receiver as CrossbeamReceiver, Sender as CrossbeamSender};
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Row, Table, Wrap};
use ratatui::{Frame, Terminal};
use std::io::{self, Write};
use std::process::{Command, Stdio};
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Clone)]
pub struct ConnectRemoteHostPaneRuntime {
    network: RemoteNetworkConfig,
    ratatui_port: Option<u16>,
}

impl ConnectRemoteHostPaneRuntime {
    pub fn new(network: RemoteNetworkConfig) -> Self {
        Self {
            network,
            ratatui_port: None,
        }
    }

    pub fn with_ratatui_port(mut self, port: u16) -> Self {
        self.ratatui_port = Some(port);
        self
    }

    // TODO(cleanup): transitional remote code, kept for Phase 8 wiring.
    #[allow(dead_code)]
    pub fn run(&self, command: ConnectRemoteHostPaneCommand) -> Result<(), LifecycleError> {
        enable_raw_mode().map_err(write_error)?;
        crossterm::execute!(io::stdout(), crossterm::event::EnableMouseCapture)
            .map_err(write_error)?;
        let backend = CrosstermBackend::new(io::stdout());
        let mut terminal = Terminal::new(backend).map_err(write_error)?;
        terminal.clear().map_err(write_error)?;

        let (crossterm_tx, crossterm_rx) = unbounded::<Event>();
        std::thread::spawn(move || {
            while let Ok(event) = event::read() {
                if crossterm_tx.send(event).is_err() {
                    break;
                }
            }
        });

        let (mut state, initial_secret_request) =
            ConnectRemoteHostState::load_with_initial_secret_request();
        let mut render_background = |_frame: &mut Frame| {};
        let result = self.run_event_loop(
            &mut terminal,
            &mut state,
            command,
            initial_secret_request,
            &mut render_background,
            &crossterm_rx,
        );

        crossterm::execute!(io::stdout(), crossterm::event::DisableMouseCapture)
            .map_err(write_error)?;
        disable_raw_mode().map_err(write_error)?;
        terminal.show_cursor().map_err(write_error)?;
        result
    }

    /// Run the popup inside an existing ratatui terminal without taking over
    /// raw mode or the alternate screen. Used by the ratatui client for Ctrl+W.
    ///
    /// Events are read from `crossterm_rx` so the popup cooperates with the
    /// external event-driven TUI loop instead of polling crossterm.
    pub fn run_embedded<F>(
        &self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
        command: ConnectRemoteHostPaneCommand,
        mut render_background: F,
        crossterm_rx: &CrossbeamReceiver<Event>,
    ) -> Result<(), LifecycleError>
    where
        F: FnMut(&mut Frame),
    {
        crossterm::execute!(io::stdout(), crossterm::event::EnableMouseCapture)
            .map_err(write_error)?;
        let (mut state, initial_secret_request) =
            ConnectRemoteHostState::load_with_initial_secret_request();
        let result = self.run_event_loop(
            terminal,
            &mut state,
            command,
            initial_secret_request,
            &mut render_background,
            crossterm_rx,
        );
        let _ = crossterm::execute!(io::stdout(), crossterm::event::DisableMouseCapture);
        result
    }

    fn run_event_loop(
        &self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
        state: &mut ConnectRemoteHostState,
        command: ConnectRemoteHostPaneCommand,
        initial_secret_request: Option<SecretLoadRequest>,
        render_background: &mut dyn FnMut(&mut Frame),
        crossterm_rx: &CrossbeamReceiver<Event>,
    ) -> Result<(), LifecycleError> {
        let (secret_tx, secret_rx) = unbounded::<SecretLoadResult>();
        if let Some(request) = initial_secret_request {
            spawn_secret_loader(request, secret_tx.clone());
        }
        let (clipboard_tx, clipboard_rx) = unbounded::<Result<ClipboardReadResult, String>>();
        terminal
            .draw(|frame| {
                render_background(frame);
                render(frame, state);
            })
            .map_err(write_error)?;
        loop {
            crossbeam_channel::select! {
                recv(crossterm_rx) -> result => {
                    let event = match result {
                        Ok(event) => event,
                        Err(_) => return Ok(()),
                    };
                    let action = match event {
                        // Windows consoles deliver a Release event for every
                        // key; applying both Press and Release would insert
                        // each character twice.
                        Event::Key(key) if key.kind == KeyEventKind::Press => {
                            state.apply_key(key)
                        }
                        Event::Key(_) | Event::FocusGained | Event::FocusLost => PaneAction::None,
                        Event::Mouse(mouse) => {
                            state.apply_mouse(mouse, crossterm::terminal::size().unwrap_or((96, 24)))
                        }
                        Event::Paste(text) => state.apply_paste(&text),
                        Event::Resize(_, _) => PaneAction::Redraw,
                    };
                    match action {
                        PaneAction::None | PaneAction::Redraw => {}
                        PaneAction::ReadClipboard => {
                            let tx = clipboard_tx.clone();
                            std::thread::spawn(move || {
                                let _ = tx.send(read_clipboard());
                            });
                        }
                        PaneAction::Close => return Ok(()),
                        PaneAction::LoadSecrets(request) => {
                            if let Some(request) = request {
                                spawn_secret_loader(request, secret_tx.clone());
                            }
                        }
                        PaneAction::SaveProxyConfig => match state.save_proxy_settings() {
                            Ok(()) => {
                                state.status = Status::Hint("Saved proxy profile.".to_string());
                            }
                            Err(message) => {
                                state.status = Status::Error(message);
                            }
                        },
                        PaneAction::ActivateProxyConfig => match state.activate_proxy_profile() {
                            Ok(()) => {
                                state.status = Status::Hint("Activated proxy profile.".to_string());
                            }
                            Err(message) => {
                                state.status = Status::Error(message);
                            }
                        },
                        PaneAction::DeleteProxyConfig => match state.delete_proxy_profile() {
                            Ok(()) => {
                                state.status = Status::Hint("Deleted proxy profile.".to_string());
                            }
                            Err(message) => {
                                state.status = Status::Error(message);
                            }
                        },
                        PaneAction::RelayJoin {
                            address,
                            token,
                            force,
                        } => {
                            if matches!(state.status, Status::Working(_)) {
                                continue;
                            }
                            state.status = Status::Working("Joining relay...".to_string());
                            terminal
                                .draw(|frame| {
                                    render_background(frame);
                                    render(frame, state);
                                })
                                .map_err(write_error)?;
                            match run_relay_join_command(
                                self.ratatui_port,
                                &address,
                                &token,
                                force,
                            ) {
                                RelayNodeAnswer::Ok(message) => {
                                    state.relay = load_relay_pin();
                                    state.relay_draft_token.clear();
                                    state.relay_mismatch = RelayMismatchState::Idle;
                                    state.status = Status::Hint(message);
                                }
                                RelayNodeAnswer::PinMismatch(message) => {
                                    state.relay_mismatch = RelayMismatchState::Prompt {
                                        message,
                                        focus: RelayMismatchFocus::Abort,
                                    };
                                    state.status = default_hint_status();
                                }
                                RelayNodeAnswer::Err(message) => {
                                    state.status = Status::Error(message);
                                }
                            }
                        }
                        PaneAction::RelayRemove => {
                            if matches!(state.status, Status::Working(_)) {
                                continue;
                            }
                            state.status = Status::Working("Removing relay...".to_string());
                            terminal
                                .draw(|frame| {
                                    render_background(frame);
                                    render(frame, state);
                                })
                                .map_err(write_error)?;
                            match run_relay_remove_command(self.ratatui_port) {
                                Ok(message) => {
                                    state.relay = None;
                                    state.relay_mismatch = RelayMismatchState::Idle;
                                    state.status = Status::Hint(message);
                                }
                                Err(message) => {
                                    state.status = Status::Error(message);
                                }
                            }
                        }
                        PaneAction::DeleteSelectedHost { profile_name } => {
                            match delete_selected_host(state, &profile_name) {
                                Ok(request) => {
                                    if let Some(request) = request {
                                        spawn_secret_loader(request, secret_tx.clone());
                                    }
                                }
                                Err(message) => {
                                    state.delete_confirm = DeleteConfirmState::Idle;
                                    state.status = Status::Error(message);
                                }
                            }
                        }
                        PaneAction::Connect => {
                            if matches!(state.status, Status::Working(_)) || state.credentials_loading() {
                                continue;
                            }
                            state.status = Status::Working("Connecting...".to_string());
                            terminal
                                .draw(|frame| {
                                    render_background(frame);
                                    render(frame, state);
                                })
                                .map_err(write_error)?;
                            match run_connect(
                                state,
                                &command,
                                &self.network,
                                self.ratatui_port,
                            ) {
                                Ok(_) => return Ok(()),
                                Err(message) => state.status = Status::Error(message),
                            }
                        }
                    }
                }
                recv(secret_rx) -> result => {
                    if let Ok(result) = result {
                        state.apply_secret_result(result);
                    }
                    // Drain any additional results that arrived while we were
                    // blocked so the UI reflects the final state.
                    while let Ok(result) = secret_rx.try_recv() {
                        state.apply_secret_result(result);
                    }
                }
                recv(clipboard_rx) -> result => {
                    match result {
                        Ok(Ok(ClipboardReadResult::Text(text))) => {
                            state.apply_paste(&text);
                        }
                        Ok(Ok(_)) => {
                            state.status =
                                Status::Hint("clipboard does not contain text".to_string());
                        }
                        Ok(Err(message)) => {
                            state.status =
                                Status::Hint(format!("clipboard read failed: {message}"));
                        }
                        Err(_) => {}
                    }
                }
            }
            terminal
                .draw(|frame| {
                    render_background(frame);
                    render(frame, state);
                })
                .map_err(write_error)?;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ConnectRemoteHostState {
    profiles: Vec<RemoteHostProfile>,
    selected: usize,
    focus: Focus,
    host: String,
    ssh_user: String,
    /// Port the remote `sshd` listens on ("22" by default). Entered either
    /// directly or via `host:port` input in the Host field.
    ssh_port: String,
    remote_port_preference: String,
    last_remote_port: Option<u16>,
    host_kind: RemoteHostKind,
    /// Dial-path choice for the saved profile: auto (direct first, relay
    /// fallback), direct, or relay (issue #156 slice 2).
    via: RemoteNodeVia,
    auth: AuthChoice,
    key_path: String,
    ssh_password: String,
    sudo_password: String,
    password_mode: PasswordMode,
    sudo_mode: SudoMode,
    show_ssh_password: bool,
    show_sudo_password: bool,
    remember: bool,
    use_install_proxy: bool,
    proxy_settings: RemoteInstallProxySettings,
    proxy_draft: RemoteInstallProxyProfile,
    proxy_all_proxy_autofilled: bool,
    proxy_https_proxy_autofilled: bool,
    /// The pinned relay as the popup opened; the join flow compares the
    /// learned fingerprint against this anchor (issue #156 slice 1).
    relay: Option<RelayTomlConfig>,
    relay_draft_address: String,
    relay_draft_token: String,
    relay_mismatch: RelayMismatchState,
    editing: Option<EditField>,
    edit_cursor: usize,
    status: Status,
    delete_confirm: DeleteConfirmState,
    secret_load: SecretLoadState,
    next_secret_request_id: u64,
}

impl ConnectRemoteHostState {
    #[cfg(test)]
    fn load() -> Self {
        Self::load_with_initial_secret_request().0
    }

    fn load_with_initial_secret_request() -> (Self, Option<SecretLoadRequest>) {
        let profiles = load_profiles();
        let mut state = Self {
            profiles,
            selected: 0,
            focus: Focus::Hosts,
            host: String::new(),
            ssh_user: std::env::var("USER").unwrap_or_default(),
            ssh_port: "22".to_string(),
            remote_port_preference: "auto".to_string(),
            last_remote_port: None,
            host_kind: RemoteHostKind::Lan,
            via: RemoteNodeVia::Auto,
            auth: AuthChoice::Password,
            key_path: String::new(),
            ssh_password: String::new(),
            sudo_password: String::new(),
            password_mode: PasswordMode::Enter,
            sudo_mode: SudoMode::SameAsSsh,
            show_ssh_password: false,
            show_sudo_password: false,
            remember: true,
            use_install_proxy: true,
            proxy_settings: load_proxy_settings(),
            proxy_draft: RemoteInstallProxyProfile {
                name: String::new(),
                all_proxy: String::new(),
                https_proxy: String::new(),
            },
            proxy_all_proxy_autofilled: false,
            proxy_https_proxy_autofilled: false,
            relay: load_relay_pin(),
            relay_draft_address: String::new(),
            relay_draft_token: String::new(),
            relay_mismatch: RelayMismatchState::Idle,
            editing: None,
            edit_cursor: 0,
            status: default_hint_status(),
            delete_confirm: DeleteConfirmState::Idle,
            secret_load: SecretLoadState::Idle,
            next_secret_request_id: 1,
        };
        let initial_secret_request = state.sync_selected_profile();
        (state, initial_secret_request)
    }

    fn sync_selected_profile(&mut self) -> Option<SecretLoadRequest> {
        self.delete_confirm = DeleteConfirmState::Idle;
        self.secret_load = SecretLoadState::Idle;
        if self.selected >= self.profiles.len() {
            self.host.clear();
            self.ssh_user = std::env::var("USER").unwrap_or_default();
            self.ssh_port = "22".to_string();
            self.remote_port_preference = "auto".to_string();
            self.last_remote_port = None;
            self.host_kind = RemoteHostKind::Lan;
            self.via = RemoteNodeVia::Auto;
            self.auth = AuthChoice::Password;
            self.key_path.clear();
            self.ssh_password.clear();
            self.sudo_password.clear();
            self.password_mode = PasswordMode::Enter;
            self.sudo_mode = SudoMode::SameAsSsh;
            self.show_ssh_password = false;
            self.show_sudo_password = false;
            self.remember = true;
            self.use_install_proxy = true;
            self.status = default_hint_status();
            return None;
        }
        let profile = self.profiles.get(self.selected).cloned()?;
        self.host = profile.host.clone();
        self.ssh_user = profile.ssh_user.clone();
        self.remote_port_preference = match profile.preferred_remote_port {
            RemotePortPreference::Auto => "auto".to_string(),
            RemotePortPreference::Port(port) => port.to_string(),
        };
        self.ssh_port = profile.ssh_port().to_string();
        self.last_remote_port = profile.last_remote_port;
        self.host_kind = profile.host_kind;
        self.via = profile.via();
        let mut request = SecretLoadRequest {
            id: self.next_secret_request_id,
            selected: self.selected,
            ssh_secret_id: None,
            sudo_secret_id: None,
        };
        self.next_secret_request_id = self.next_secret_request_id.saturating_add(1);
        match &profile.auth {
            RemoteHostAuthProfile::Password { password_secret_id } => {
                self.auth = AuthChoice::Password;
                self.key_path.clear();
                self.ssh_password.clear();
                if let Some(id) = password_secret_id {
                    request.ssh_secret_id = Some(id.clone());
                    self.password_mode = PasswordMode::Loading;
                } else {
                    self.password_mode = PasswordMode::Enter;
                }
            }
            RemoteHostAuthProfile::Key { key_path } => {
                self.auth = AuthChoice::Key;
                self.key_path = key_path.to_string_lossy().into_owned();
                self.ssh_password.clear();
                self.password_mode = PasswordMode::Enter;
            }
        }
        self.sudo_password.clear();
        if let Some(id) = &profile.sudo_password_secret_id {
            request.sudo_secret_id = Some(id.clone());
            self.sudo_mode = SudoMode::Loading;
        } else {
            self.sudo_mode = if self.auth == AuthChoice::Password {
                SudoMode::SameAsSsh
            } else {
                SudoMode::None
            };
        }
        self.show_ssh_password = false;
        self.show_sudo_password = false;
        self.remember = true;
        self.use_install_proxy = profile.use_install_proxy;
        if request.has_work() {
            self.status = Status::Loading("Loading saved credentials...".to_string());
            self.secret_load = SecretLoadState::Loading {
                id: request.id,
                selected: request.selected,
            };
            Some(request)
        } else {
            self.status = default_hint_status();
            None
        }
    }

    fn apply_secret_result(&mut self, result: SecretLoadResult) {
        if self.secret_load
            != (SecretLoadState::Loading {
                id: result.id,
                selected: result.selected,
            })
            || self.selected != result.selected
        {
            return;
        }
        self.secret_load = SecretLoadState::Idle;
        let mut load_errors = Vec::new();
        if let Some(outcome) = result.ssh {
            match outcome {
                Ok(value) => {
                    self.ssh_password = value;
                    self.password_mode = PasswordMode::Saved;
                }
                Err(error) => {
                    self.ssh_password.clear();
                    self.password_mode = PasswordMode::Enter;
                    load_errors.push(format!("SSH password: {error}"));
                }
            }
        }
        if let Some(outcome) = result.sudo {
            match outcome {
                Ok(value) => {
                    self.sudo_password = value;
                    self.sudo_mode = SudoMode::Saved;
                }
                Err(error) => {
                    self.sudo_password.clear();
                    self.sudo_mode = SudoMode::Replace;
                    load_errors.push(format!("sudo password: {error}"));
                }
            }
        }
        if load_errors.is_empty() {
            self.status = default_hint_status();
        } else {
            self.status = Status::Error(format!(
                "Failed to load saved secret: {}",
                load_errors.join("; ")
            ));
        }
        self.set_focus(self.focus);
    }

    fn apply_key(&mut self, key: KeyEvent) -> PaneAction {
        if !matches!(self.delete_confirm, DeleteConfirmState::Idle) {
            return self.apply_delete_confirm_key(key);
        }
        if !matches!(self.relay_mismatch, RelayMismatchState::Idle) {
            return self.apply_relay_mismatch_key(key);
        }
        if matches!(self.status, Status::Error(_)) {
            return self.apply_error_popup_key(key);
        }
        if let Some(field) = self.editing {
            return self.apply_edit_key(key, field);
        }
        match key.code {
            KeyCode::Esc => {
                if self.focus == Focus::Hosts {
                    PaneAction::Close
                } else {
                    self.set_focus(Focus::Hosts);
                    PaneAction::None
                }
            }
            KeyCode::Char('q') => PaneAction::Close,
            KeyCode::Tab => {
                self.set_focus(self.next_focus());
                PaneAction::None
            }
            KeyCode::BackTab => {
                self.set_focus(self.prev_focus());
                PaneAction::None
            }
            KeyCode::Up => self.move_up(),
            KeyCode::Down => self.move_down(),
            KeyCode::Left => {
                match self.focus {
                    Focus::Delete => self.set_focus(Focus::Connect),
                    Focus::ProxySave => self.set_focus(Focus::ProxyActive),
                    Focus::ProxyDelete => self.set_focus(Focus::ProxySave),
                    Focus::RelayRemove => self.set_focus(Focus::RelayJoin),
                    Focus::Auth if self.auth == AuthChoice::Password => {
                        self.set_focus(Focus::HostKind);
                    }
                    _ if self.focus.uses_horizontal_choice() => self.adjust_choice(-1),
                    _ if self.focus != Focus::Hosts => self.set_focus(Focus::Hosts),
                    _ => {}
                }
                PaneAction::None
            }
            KeyCode::Right => {
                match self.focus {
                    Focus::Connect => self.set_focus(Focus::Delete),
                    Focus::ProxyActive => self.set_focus(Focus::ProxySave),
                    Focus::ProxySave => self.set_focus(Focus::ProxyDelete),
                    Focus::RelayJoin if self.relay.is_some() => {
                        self.set_focus(Focus::RelayRemove);
                    }
                    Focus::Hosts => self.set_focus(self.default_detail_focus()),
                    Focus::Auth if self.auth == AuthChoice::Key => {}
                    _ if self.focus.uses_horizontal_choice() => self.adjust_choice(1),
                    _ => {}
                }
                PaneAction::None
            }
            KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.toggle_password_visibility();
                PaneAction::None
            }
            KeyCode::Enter => self.activate_focus(),
            KeyCode::Char(' ') => {
                if self.focus == Focus::Remember {
                    self.remember = !self.remember;
                } else if self.focus == Focus::InstallProxy {
                    self.use_install_proxy = !self.use_install_proxy;
                } else if self.focus == Focus::Password || self.focus == Focus::Sudo {
                    self.toggle_password_visibility();
                } else if self.focus == Focus::HostKind || self.focus == Focus::Via {
                    self.adjust_choice(1);
                }
                PaneAction::None
            }
            _ => PaneAction::None,
        }
    }

    fn apply_error_popup_key(&mut self, key: KeyEvent) -> PaneAction {
        match key.code {
            KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q') => {
                self.dismiss_error_popup();
                PaneAction::None
            }
            _ => PaneAction::None,
        }
    }

    fn apply_delete_confirm_key(&mut self, key: KeyEvent) -> PaneAction {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.delete_confirm = DeleteConfirmState::Idle;
                PaneAction::None
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
                self.shift_delete_confirm_focus();
                PaneAction::None
            }
            KeyCode::Enter => self.activate_delete_confirm_focus(),
            _ => PaneAction::None,
        }
    }

    fn apply_delete_confirm_mouse(
        &mut self,
        mouse: crossterm::event::MouseEvent,
        size: (u16, u16),
    ) -> PaneAction {
        let layout = DeleteConfirmGeometry::from_terminal_size(size);
        if point_in_rect(mouse.column, mouse.row, layout.cancel_button) {
            self.delete_confirm = DeleteConfirmState::Idle;
            return PaneAction::None;
        }
        if point_in_rect(mouse.column, mouse.row, layout.delete_button) {
            return self.confirm_delete_action();
        }
        PaneAction::None
    }

    fn apply_relay_mismatch_mouse(
        &mut self,
        mouse: crossterm::event::MouseEvent,
        size: (u16, u16),
    ) -> PaneAction {
        let layout = RelayMismatchGeometry::from_terminal_size(size);
        if point_in_rect(mouse.column, mouse.row, layout.abort_button) {
            self.relay_mismatch = RelayMismatchState::Idle;
            return PaneAction::None;
        }
        if point_in_rect(mouse.column, mouse.row, layout.switch_button) {
            self.relay_mismatch = RelayMismatchState::Idle;
            return self.relay_join_action(true);
        }
        PaneAction::None
    }

    fn shift_delete_confirm_focus(&mut self) {
        if let DeleteConfirmState::Prompt { focus, .. } = &mut self.delete_confirm {
            *focus = match focus {
                DeleteConfirmFocus::Cancel => DeleteConfirmFocus::Delete,
                DeleteConfirmFocus::Delete => DeleteConfirmFocus::Cancel,
            };
        }
    }

    fn activate_delete_confirm_focus(&mut self) -> PaneAction {
        match self.delete_confirm_focus() {
            Some(DeleteConfirmFocus::Cancel) => {
                self.delete_confirm = DeleteConfirmState::Idle;
                PaneAction::None
            }
            Some(DeleteConfirmFocus::Delete) => self.confirm_delete_action(),
            None => PaneAction::None,
        }
    }

    fn delete_confirm_focus(&self) -> Option<DeleteConfirmFocus> {
        match &self.delete_confirm {
            DeleteConfirmState::Prompt { focus, .. } => Some(*focus),
            DeleteConfirmState::Idle => None,
        }
    }

    fn confirm_delete_action(&mut self) -> PaneAction {
        let DeleteConfirmState::Prompt { profile_name, .. } = &self.delete_confirm else {
            return PaneAction::None;
        };
        PaneAction::DeleteSelectedHost {
            profile_name: profile_name.clone(),
        }
    }

    fn apply_relay_mismatch_key(&mut self, key: KeyEvent) -> PaneAction {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.relay_mismatch = RelayMismatchState::Idle;
                PaneAction::None
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
                self.shift_relay_mismatch_focus();
                PaneAction::None
            }
            KeyCode::Enter => self.activate_relay_mismatch_focus(),
            _ => PaneAction::None,
        }
    }

    fn shift_relay_mismatch_focus(&mut self) {
        if let RelayMismatchState::Prompt { focus, .. } = &mut self.relay_mismatch {
            *focus = match focus {
                RelayMismatchFocus::Abort => RelayMismatchFocus::SwitchAnyway,
                RelayMismatchFocus::SwitchAnyway => RelayMismatchFocus::Abort,
            };
        }
    }

    fn activate_relay_mismatch_focus(&mut self) -> PaneAction {
        match self.relay_mismatch_focus() {
            Some(RelayMismatchFocus::Abort) => {
                self.relay_mismatch = RelayMismatchState::Idle;
                PaneAction::None
            }
            Some(RelayMismatchFocus::SwitchAnyway) => {
                self.relay_mismatch = RelayMismatchState::Idle;
                self.relay_join_action(true)
            }
            None => PaneAction::None,
        }
    }

    fn relay_mismatch_focus(&self) -> Option<RelayMismatchFocus> {
        match &self.relay_mismatch {
            RelayMismatchState::Prompt { focus, .. } => Some(*focus),
            RelayMismatchState::Idle => None,
        }
    }

    fn apply_edit_key(&mut self, key: KeyEvent, field: EditField) -> PaneAction {
        if matches!(key.code, KeyCode::Char('v') | KeyCode::Char('V'))
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            return PaneAction::ReadClipboard;
        }
        if key.code == KeyCode::Insert && key.modifiers.contains(KeyModifiers::SHIFT) {
            return PaneAction::ReadClipboard;
        }
        if field == EditField::Host
            && matches!(
                key.code,
                KeyCode::Esc
                    | KeyCode::Enter
                    | KeyCode::Tab
                    | KeyCode::BackTab
                    | KeyCode::Up
                    | KeyCode::Down
            )
        {
            // Accept `host:port` (e.g. pasted) and move the port into the
            // dedicated port field before focus leaves the host edit.
            self.normalize_host_port();
        }
        if matches!(field, EditField::SshPassword | EditField::SudoPassword)
            && key.code == KeyCode::Char(' ')
        {
            self.toggle_password_visibility();
            return PaneAction::None;
        }
        if matches!(
            (field, key.code),
            (
                EditField::SshPassword | EditField::SudoPassword,
                KeyCode::Char('r')
            )
        ) && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            self.toggle_password_visibility();
            return PaneAction::None;
        }
        if field == EditField::SudoPassword {
            return self.apply_sudo_password_edit_key(key);
        }
        if field == EditField::SshPassword && self.password_mode == PasswordMode::Saved {
            self.password_mode = PasswordMode::Enter;
        }
        match key.code {
            KeyCode::Esc => self.set_focus(Focus::Hosts),
            KeyCode::Left => self.move_edit_cursor_left_or_leave(),
            KeyCode::Right => {
                self.move_edit_cursor_right(field);
            }
            KeyCode::Tab => self.set_focus(self.next_focus()),
            KeyCode::BackTab => self.set_focus(self.prev_focus()),
            KeyCode::Up => return self.move_up(),
            KeyCode::Down => return self.move_down(),
            KeyCode::Enter => self.set_focus(self.next_focus()),
            code if is_backspace_key(code, key.modifiers) => {
                self.edit_field_backspace(field);
            }
            KeyCode::Char(ch) if !ch.is_control() => self.edit_field_push(field, ch),
            _ => {}
        }
        PaneAction::None
    }

    fn apply_sudo_password_edit_key(&mut self, key: KeyEvent) -> PaneAction {
        match key.code {
            KeyCode::Esc => self.set_focus(Focus::Sudo),
            KeyCode::Tab => self.set_focus(self.next_focus()),
            KeyCode::BackTab => self.set_focus(self.prev_focus()),
            KeyCode::Up => return self.move_up(),
            KeyCode::Down => return self.move_down(),
            KeyCode::Left => self.move_edit_cursor_left_or_leave(),
            KeyCode::Right => {
                self.move_edit_cursor_right(EditField::SudoPassword);
            }
            KeyCode::Enter => self.set_focus(self.next_focus()),
            code if is_backspace_key(code, key.modifiers) => {
                if self.sudo_mode == SudoMode::Saved {
                    self.sudo_mode = SudoMode::Replace;
                }
                self.edit_field_backspace(EditField::SudoPassword);
            }
            KeyCode::Char(ch) if !ch.is_control() => {
                if self.sudo_mode == SudoMode::Saved {
                    self.sudo_mode = SudoMode::Replace;
                }
                self.edit_field_push(EditField::SudoPassword, ch);
            }
            _ => {}
        }
        PaneAction::None
    }

    /// Insert pasted text into the currently edited field.
    ///
    /// Only the first line is used so multi-line clipboard content (e.g.
    /// `echo secret | xclip`) cannot smuggle newlines into a single-line
    /// field. Password fields follow the same Saved→Enter/Replace transitions
    /// as typed input. A `host:port` paste into the host field is split into
    /// the host and port fields immediately.
    fn apply_paste(&mut self, text: &str) -> PaneAction {
        if !matches!(self.delete_confirm, DeleteConfirmState::Idle)
            || matches!(self.status, Status::Error(_))
        {
            return PaneAction::None;
        }
        let Some(field) = self.editing else {
            return PaneAction::None;
        };
        let Some(line) = text.lines().next() else {
            return PaneAction::None;
        };
        if line.trim().is_empty() {
            return PaneAction::None;
        }
        if field == EditField::SshPassword && self.password_mode == PasswordMode::Saved {
            self.password_mode = PasswordMode::Enter;
        }
        if field == EditField::SudoPassword && self.sudo_mode == SudoMode::Saved {
            self.sudo_mode = SudoMode::Replace;
        }
        for ch in line.chars().filter(|ch| !ch.is_control()) {
            self.edit_field_push(field, ch);
        }
        if field == EditField::Host {
            self.normalize_host_port();
        }
        PaneAction::None
    }

    fn normalize_host_port(&mut self) {
        if let Some((host, port)) = split_host_port(&self.host) {
            self.host = host;
            self.ssh_port = port.to_string();
        }
    }

    fn apply_mouse(&mut self, mouse: crossterm::event::MouseEvent, size: (u16, u16)) -> PaneAction {
        if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            return PaneAction::None;
        }
        if !matches!(self.delete_confirm, DeleteConfirmState::Idle) {
            return self.apply_delete_confirm_mouse(mouse, size);
        }
        if !matches!(self.relay_mismatch, RelayMismatchState::Idle) {
            return self.apply_relay_mismatch_mouse(mouse, size);
        }
        let x = mouse.column;
        let y = mouse.row;
        if matches!(self.status, Status::Error(_)) {
            let layout = ConnectErrorGeometry::from_terminal_size(size);
            if point_in_rect(x, y, layout.ok_button) {
                self.dismiss_error_popup();
            }
            return PaneAction::None;
        }
        let layout = PopupGeometry::from_terminal_size(size, self);
        if !point_in_rect(x, y, layout.dialog) {
            return PaneAction::None;
        }
        if point_in_rect(x, y, layout.sidebar.saved_list) {
            let row = y.saturating_sub(layout.sidebar.saved_list.y) as usize;
            if row < self.profiles.len() {
                self.selected = row;
                self.set_focus(Focus::Hosts);
                return PaneAction::LoadSecrets(self.sync_selected_profile());
            }
            return PaneAction::None;
        }
        if point_in_rect(x, y, layout.sidebar.new_host) {
            self.selected = self.profiles.len();
            self.set_focus(Focus::Hosts);
            return PaneAction::LoadSecrets(self.sync_selected_profile());
        }
        if point_in_rect(x, y, layout.sidebar.proxy_list) {
            let row = y.saturating_sub(layout.sidebar.proxy_list.y) as usize;
            if row < self.proxy_settings.profiles.len() {
                self.selected = self.proxy_selection_index().saturating_add(row);
                self.set_focus(Focus::Hosts);
                self.sync_selected_proxy();
                return PaneAction::None;
            }
            return PaneAction::None;
        }
        if point_in_rect(x, y, layout.sidebar.new_proxy) {
            self.selected = self.new_proxy_selection_index();
            self.set_focus(Focus::Hosts);
            self.sync_selected_proxy();
            return PaneAction::None;
        }
        if point_in_rect(x, y, layout.sidebar.relay_list) {
            self.selected = self.relay_selection_index();
            self.set_focus(Focus::Hosts);
            return PaneAction::None;
        }
        if !point_in_rect(x, y, layout.details) {
            return PaneAction::None;
        }
        let row = y.saturating_sub(layout.details.y);
        if self.selected_proxy_config() {
            let details = ProxyDetailsGeometry::from_area(layout.details);
            match row {
                row if row == details.rows.name => self.set_focus(Focus::ProxyName),
                row if row == details.rows.all_proxy => self.set_focus(Focus::AllProxy),
                row if row == details.rows.https_proxy => self.set_focus(Focus::HttpsProxy),
                row if row == details.rows.action => {
                    return proxy_action_from_x(x, details.buttons, self);
                }
                _ => {}
            }
            return PaneAction::None;
        }
        if self.selected_relay_config() {
            let details = RelayDetailsGeometry::from_area(layout.details);
            match row {
                row if row == details.rows.address => self.set_focus(Focus::RelayAddress),
                row if row == details.rows.token => self.set_focus(Focus::RelayToken),
                row if row == details.rows.action => {
                    if let Some(focus) = relay_action_from_x(x, details.action, self) {
                        self.set_focus(focus);
                        return match focus {
                            Focus::RelayJoin => self.relay_join_action(false),
                            Focus::RelayRemove => PaneAction::RelayRemove,
                            _ => PaneAction::None,
                        };
                    }
                }
                _ => {}
            }
            return PaneAction::None;
        }
        let details = DetailsGeometry::from_area(layout.details, self);
        match row {
            row if row == details.rows.host && point_in_rect(x, y, details.connection) => {
                self.set_focus(Focus::Host)
            }
            row if row == details.rows.port && point_in_rect(x, y, details.connection) => {
                self.set_focus(Focus::Port)
            }
            row if row == details.rows.user && point_in_rect(x, y, details.connection) => {
                self.set_focus(Focus::User)
            }
            row if row == details.rows.host_kind && point_in_rect(x, y, details.connection) => {
                self.set_focus(Focus::HostKind)
            }
            row if row == details.rows.via && point_in_rect(x, y, details.connection) => {
                self.set_focus(Focus::Via)
            }
            row if row == details.rows.auth && point_in_rect(x, y, details.authentication) => {
                self.set_focus(Focus::Auth)
            }
            row if row == details.rows.password && point_in_rect(x, y, details.authentication) => {
                self.set_focus(Focus::Password)
            }
            row if row == details.rows.sudo
                && point_in_rect(x, y, details.authentication)
                && !self.selected_profile_is_windows_shell() =>
            {
                self.set_focus(Focus::Sudo)
            }
            row if row == details.rows.remember => {
                self.set_focus(Focus::Remember);
                self.delete_confirm = DeleteConfirmState::Idle;
                self.remember = !self.remember;
            }
            row if row == details.rows.install_proxy => {
                self.set_focus(Focus::InstallProxy);
                self.delete_confirm = DeleteConfirmState::Idle;
                self.use_install_proxy = !self.use_install_proxy;
            }
            _ if point_in_rect(x, y, details.buttons) => {
                if let Some(focus) = button_action_from_x(x, details.buttons, self) {
                    self.set_focus(focus);
                    return match focus {
                        Focus::Connect => self.connect_action(),
                        Focus::Delete => self.delete_action(),
                        _ => PaneAction::None,
                    };
                }
            }
            _ => {}
        }
        PaneAction::None
    }

    fn move_up(&mut self) -> PaneAction {
        if self.focus == Focus::Hosts {
            if self.selected > 0 {
                self.selected -= 1;
                if self.selected_proxy_config() {
                    self.sync_selected_proxy();
                    return PaneAction::None;
                }
                return PaneAction::LoadSecrets(self.sync_selected_profile());
            }
        } else {
            let next = match self.focus {
                Focus::ProxySave | Focus::ProxyDelete => Focus::HttpsProxy,
                Focus::Delete => Focus::InstallProxy,
                _ => {
                    let candidate = self.prev_focus();
                    if candidate == Focus::Hosts {
                        self.default_detail_focus()
                    } else {
                        candidate
                    }
                }
            };
            self.set_focus(next);
        }
        PaneAction::None
    }

    fn move_down(&mut self) -> PaneAction {
        if self.focus == Focus::Hosts {
            if self.selected < self.relay_selection_index() {
                self.selected += 1;
                if self.selected_proxy_config() {
                    self.sync_selected_proxy();
                    return PaneAction::None;
                }
                if self.selected_relay_config() {
                    return PaneAction::None;
                }
                return PaneAction::LoadSecrets(self.sync_selected_profile());
            }
        } else {
            let mut next = self.next_focus();
            if next == Focus::Hosts {
                next = if self.selected_proxy_config() {
                    Focus::ProxySave
                } else if self.selected_relay_config() {
                    Focus::RelayJoin
                } else {
                    Focus::Connect
                };
            }
            self.set_focus(next);
        }
        PaneAction::None
    }

    fn default_detail_focus(&self) -> Focus {
        if self.selected_proxy_config() {
            Focus::ProxyActive
        } else if self.selected_relay_config() {
            Focus::RelayAddress
        } else if self.selected >= self.profiles.len() {
            Focus::Host
        } else {
            Focus::Connect
        }
    }

    fn set_focus(&mut self, focus: Focus) {
        if self.focus != focus {
            self.delete_confirm = DeleteConfirmState::Idle;
        }
        self.focus = focus;
        self.editing = focus.edit_field(self.auth);
        self.sync_edit_cursor_to_end();
        if focus == Focus::Password
            && self.auth == AuthChoice::Password
            && self.ssh_password.is_empty()
        {
            self.password_mode = PasswordMode::Enter;
        }
        if focus == Focus::Sudo {
            self.start_sudo_password_edit();
        }
    }

    fn start_edit(&mut self, field: EditField) {
        self.focus = edit_focus(field);
        self.editing = Some(field);
        self.sync_edit_cursor_to_end();
    }

    fn start_sudo_password_edit(&mut self) {
        if self.sudo_mode == SudoMode::None || self.selected_profile_is_windows_shell() {
            return;
        }
        if self.sudo_mode == SudoMode::SameAsSsh {
            self.sudo_password = self.ssh_password.clone();
            self.sudo_mode = SudoMode::Replace;
        }
        self.start_edit(EditField::SudoPassword);
    }

    fn toggle_password_visibility(&mut self) {
        self.toggle_password_visibility_for(self.focus);
    }

    fn toggle_password_visibility_for(&mut self, focus: Focus) {
        match focus {
            Focus::Password if self.auth == AuthChoice::Password => {
                self.show_ssh_password = !self.show_ssh_password;
            }
            Focus::Sudo if self.sudo_mode != SudoMode::None => {
                self.show_sudo_password = !self.show_sudo_password;
            }
            _ => {}
        }
    }

    fn activate_focus(&mut self) -> PaneAction {
        match self.focus {
            Focus::Hosts => {
                self.set_focus(self.default_detail_focus());
                PaneAction::None
            }
            Focus::Host
            | Focus::Port
            | Focus::User
            | Focus::HostKind
            | Focus::ProxyName
            | Focus::AllProxy
            | Focus::HttpsProxy
            | Focus::RelayAddress
            | Focus::RelayToken => PaneAction::None,
            Focus::Auth => {
                self.adjust_choice(1);
                PaneAction::None
            }
            Focus::Via => {
                self.adjust_choice(1);
                PaneAction::None
            }
            Focus::Password => PaneAction::None,
            Focus::Sudo => {
                self.start_sudo_password_edit();
                PaneAction::None
            }
            Focus::Remember => {
                self.delete_confirm = DeleteConfirmState::Idle;
                self.remember = !self.remember;
                PaneAction::None
            }
            Focus::InstallProxy => {
                self.delete_confirm = DeleteConfirmState::Idle;
                self.use_install_proxy = !self.use_install_proxy;
                PaneAction::None
            }
            Focus::Delete => self.delete_action(),
            Focus::Connect => self.connect_action(),
            Focus::ProxyActive => PaneAction::ActivateProxyConfig,
            Focus::ProxySave => PaneAction::SaveProxyConfig,
            Focus::ProxyDelete => PaneAction::DeleteProxyConfig,
            Focus::RelayJoin => self.relay_join_action(false),
            Focus::RelayRemove => PaneAction::RelayRemove,
        }
    }

    /// Validates the relay draft and dispatches the join. `force` carries
    /// the explicit operator confirmation after a pin-mismatch warning.
    fn relay_join_action(&mut self, force: bool) -> PaneAction {
        if matches!(self.status, Status::Working(_)) {
            return PaneAction::None;
        }
        let address = self.relay_draft_address.trim().to_string();
        let token = self.relay_draft_token.trim().to_string();
        if address.is_empty() {
            self.status = Status::Error("Relay address is required.".to_string());
            return PaneAction::None;
        }
        if token.is_empty() {
            self.status = Status::Error("Invite token is required.".to_string());
            return PaneAction::None;
        }
        self.relay_draft_address = address.clone();
        PaneAction::RelayJoin {
            address,
            token,
            force,
        }
    }

    fn delete_action(&mut self) -> PaneAction {
        let Some(profile) = self.selected_profile() else {
            self.delete_confirm = DeleteConfirmState::Idle;
            return PaneAction::None;
        };
        self.delete_confirm = DeleteConfirmState::Prompt {
            profile_name: profile.name.clone(),
            profile_label: saved_host_label(profile),
            focus: DeleteConfirmFocus::Cancel,
        };
        PaneAction::None
    }

    fn dismiss_error_popup(&mut self) {
        if matches!(self.status, Status::Error(_)) {
            self.status = default_hint_status();
        }
    }

    fn connect_action(&mut self) -> PaneAction {
        // Safety net: the user may have typed `host:port` and clicked Connect
        // without ever leaving the host field.
        self.normalize_host_port();
        if matches!(self.status, Status::Working(_)) || self.credentials_loading() {
            PaneAction::None
        } else {
            PaneAction::Connect
        }
    }

    fn adjust_choice(&mut self, step: i32) {
        if self.focus != Focus::Delete {
            self.delete_confirm = DeleteConfirmState::Idle;
        }
        match self.focus {
            Focus::HostKind => {
                self.host_kind = self.host_kind.shift(step);
                if self.host_kind == RemoteHostKind::Cloud && self.auth != AuthChoice::Key {
                    self.auth = AuthChoice::Key;
                    if self.sudo_mode == SudoMode::SameAsSsh {
                        self.sudo_mode = SudoMode::None;
                    }
                }
                self.set_focus(Focus::HostKind);
            }
            Focus::Via => {
                self.via = self.via.shift(step);
                self.set_focus(Focus::Via);
            }
            Focus::Auth => {
                if self.host_kind == RemoteHostKind::Cloud {
                    self.auth = AuthChoice::Key;
                } else {
                    self.auth = self.auth.shift(step);
                }
                if self.auth == AuthChoice::Password && self.sudo_mode == SudoMode::None {
                    self.sudo_mode = SudoMode::SameAsSsh;
                }
                if self.auth != AuthChoice::Password && self.sudo_mode == SudoMode::SameAsSsh {
                    self.sudo_mode = SudoMode::None;
                }
                self.set_focus(Focus::Auth);
            }
            Focus::Password if self.auth == AuthChoice::Password => {
                self.password_mode = self.password_mode.shift(step, self.saved_ssh_password());
                if self.password_mode == PasswordMode::Enter {
                    self.start_edit(EditField::SshPassword);
                }
            }
            _ => {}
        }
    }

    fn move_edit_cursor_left_or_leave(&mut self) {
        if self.edit_cursor > 0 {
            self.edit_cursor -= 1;
        } else {
            self.set_focus(Focus::Hosts);
        }
    }

    fn move_edit_cursor_right(&mut self, field: EditField) -> bool {
        let len = self.edit_field_len(field);
        if self.edit_cursor < len {
            self.edit_cursor += 1;
            true
        } else {
            false
        }
    }

    fn sync_edit_cursor_to_end(&mut self) {
        self.edit_cursor = self
            .editing
            .map(|field| self.edit_field_len(field))
            .unwrap_or(0);
    }

    fn edit_field_len(&self, field: EditField) -> usize {
        edit_buffer_ref(self, field).chars().count()
    }

    fn edit_field_backspace(&mut self, field: EditField) {
        if self.edit_cursor == 0 {
            return;
        }
        let cursor = self.edit_cursor;
        let buffer = edit_buffer(self, field);
        let start = char_to_byte_index(buffer, cursor - 1);
        let end = char_to_byte_index(buffer, cursor);
        buffer.replace_range(start..end, "");
        self.edit_cursor -= 1;
        self.after_field_edit(field);
    }

    fn edit_field_push(&mut self, field: EditField, ch: char) {
        let cursor = self.edit_cursor;
        let buffer = edit_buffer(self, field);
        let index = char_to_byte_index(buffer, cursor);
        buffer.insert(index, ch);
        self.edit_cursor += 1;
        self.after_field_edit(field);
    }

    fn after_field_edit(&mut self, field: EditField) {
        match field {
            EditField::AllProxy => {
                self.proxy_all_proxy_autofilled = false;
                self.apply_proxy_default_from_all_proxy();
            }
            EditField::HttpsProxy => {
                self.proxy_https_proxy_autofilled = false;
                self.apply_proxy_default_from_https_proxy();
            }
            _ => {}
        }
    }

    fn apply_proxy_default_from_all_proxy(&mut self) {
        if !self.proxy_draft.https_proxy.trim().is_empty() && !self.proxy_https_proxy_autofilled {
            return;
        }
        let Some(host) = proxy_host_part(&self.proxy_draft.all_proxy) else {
            if self.proxy_https_proxy_autofilled {
                self.proxy_draft.https_proxy.clear();
                self.proxy_https_proxy_autofilled = false;
            }
            return;
        };
        self.proxy_draft.https_proxy = format!("http://{host}");
        self.proxy_https_proxy_autofilled = true;
    }

    fn apply_proxy_default_from_https_proxy(&mut self) {
        if !self.proxy_draft.all_proxy.trim().is_empty() && !self.proxy_all_proxy_autofilled {
            return;
        }
        let Some(host) = proxy_host_part(&self.proxy_draft.https_proxy) else {
            if self.proxy_all_proxy_autofilled {
                self.proxy_draft.all_proxy.clear();
                self.proxy_all_proxy_autofilled = false;
            }
            return;
        };
        self.proxy_draft.all_proxy = format!("socks5://{host}");
        self.proxy_all_proxy_autofilled = true;
    }

    fn credentials_loading(&self) -> bool {
        matches!(self.secret_load, SecretLoadState::Loading { .. })
            || self.password_mode == PasswordMode::Loading
            || self.sudo_mode == SudoMode::Loading
    }

    fn next_focus(&self) -> Focus {
        let focus = self.focus.next(
            self.auth,
            self.has_saved_selection(),
            self.selected_proxy_config(),
            self.selected_relay_config(),
        );
        if focus == Focus::Sudo && self.selected_profile_is_windows_shell() {
            return focus.next(
                self.auth,
                self.has_saved_selection(),
                self.selected_proxy_config(),
                self.selected_relay_config(),
            );
        }
        focus
    }

    fn prev_focus(&self) -> Focus {
        let focus = self.focus.prev(
            self.auth,
            self.has_saved_selection(),
            self.selected_proxy_config(),
            self.selected_relay_config(),
        );
        if focus == Focus::Sudo && self.selected_profile_is_windows_shell() {
            return focus.prev(
                self.auth,
                self.has_saved_selection(),
                self.selected_proxy_config(),
                self.selected_relay_config(),
            );
        }
        focus
    }

    fn proxy_selection_index(&self) -> usize {
        self.profiles.len().saturating_add(1)
    }

    fn selected_proxy_config(&self) -> bool {
        self.selected >= self.proxy_selection_index()
            && self.selected <= self.new_proxy_selection_index()
    }

    fn proxy_profile_selection_start(&self) -> usize {
        self.proxy_selection_index()
    }

    fn new_proxy_selection_index(&self) -> usize {
        self.proxy_profile_selection_start()
            .saturating_add(self.proxy_settings.profiles.len())
    }

    /// Sidebar selection index of the single relay entry. The relay.toml
    /// data model pins at most one relay; a multi-relay list would extend
    /// this range (issue #156 keeps single-relay switch as the flow).
    // TODO(issue #156 multi-relay): replace the single entry with a relay
    // list once the data model supports more than one pinned relay.
    fn relay_selection_index(&self) -> usize {
        self.new_proxy_selection_index().saturating_add(1)
    }

    fn selected_relay_config(&self) -> bool {
        self.selected == self.relay_selection_index()
    }

    fn selected_proxy_profile_index(&self) -> Option<usize> {
        if !self.selected_proxy_config()
            || self.selected < self.proxy_profile_selection_start()
            || self.selected >= self.new_proxy_selection_index()
        {
            return None;
        }
        Some(
            self.selected
                .saturating_sub(self.proxy_profile_selection_start()),
        )
    }

    fn sync_selected_proxy(&mut self) {
        if let Some(index) = self.selected_proxy_profile_index() {
            if let Some(profile) = self.proxy_settings.profiles.get(index).cloned() {
                self.proxy_draft = profile;
            }
        } else {
            self.proxy_draft = RemoteInstallProxyProfile {
                name: String::new(),
                all_proxy: String::new(),
                https_proxy: String::new(),
            };
        }
        self.proxy_all_proxy_autofilled = false;
        self.proxy_https_proxy_autofilled = false;
    }

    fn save_proxy_settings(&mut self) -> Result<(), String> {
        self.proxy_draft
            .config()
            .validate()
            .map_err(|error| error.to_string())?;
        let name = proxy_profile_name(&self.proxy_draft);
        if name.is_empty() {
            return Err("Proxy profile name is required.".to_string());
        }
        self.proxy_draft.name = name.clone();
        let mut settings = self.proxy_settings.clone();
        if let Some(index) = self.selected_proxy_profile_index() {
            if settings
                .profiles
                .iter()
                .enumerate()
                .any(|(other, profile)| other != index && profile.name == name)
            {
                return Err(format!("Proxy profile `{name}` already exists."));
            }
            if settings.active.as_deref()
                == self
                    .proxy_settings
                    .profiles
                    .get(index)
                    .map(|profile| profile.name.as_str())
                && settings.active.as_deref() != Some(name.as_str())
            {
                settings.active = Some(name.clone());
            }
            if let Some(profile) = settings.profiles.get_mut(index) {
                *profile = self.proxy_draft.clone();
            }
        } else {
            if settings.profiles.iter().any(|profile| profile.name == name) {
                return Err(format!("Proxy profile `{name}` already exists."));
            }
            settings.profiles.push(self.proxy_draft.clone());
            self.selected = self
                .proxy_profile_selection_start()
                .saturating_add(settings.profiles.len().saturating_sub(1));
        }
        RemoteInstallProxyStore::default()
            .save_settings(&settings)
            .map_err(|error| error.to_string())?;
        self.proxy_settings = settings;
        Ok(())
    }

    fn activate_proxy_profile(&mut self) -> Result<(), String> {
        self.save_proxy_settings()?;
        self.proxy_settings.active = Some(self.proxy_draft.name.clone());
        RemoteInstallProxyStore::default()
            .save_settings(&self.proxy_settings)
            .map_err(|error| error.to_string())
    }

    fn delete_proxy_profile(&mut self) -> Result<(), String> {
        let Some(index) = self.selected_proxy_profile_index() else {
            return Ok(());
        };
        let mut settings = self.proxy_settings.clone();
        let removed = settings.profiles.remove(index);
        if settings.active.as_deref() == Some(removed.name.as_str()) {
            settings.active = None;
        }
        RemoteInstallProxyStore::default()
            .save_settings(&settings)
            .map_err(|error| error.to_string())?;
        self.proxy_settings = settings;
        self.selected = if self.proxy_settings.profiles.is_empty() {
            self.new_proxy_selection_index()
        } else {
            self.proxy_profile_selection_start()
                .saturating_add(index.min(self.proxy_settings.profiles.len().saturating_sub(1)))
        };
        self.sync_selected_proxy();
        Ok(())
    }

    fn selected_profile(&self) -> Option<&RemoteHostProfile> {
        self.profiles.get(self.selected)
    }

    /// True when the selected saved profile is a known Windows SSH target.
    ///
    /// The shell family is cached in the profile by the first connect, so
    /// brand-new (unsaved) hosts are unclassified and keep the editable sudo
    /// field. The sudo password only exists for the POSIX installer
    /// (`allow_sudo` in `ssh_remote_host_bootstrapper.rs`); the Windows
    /// installer is per-user and needs no elevation
    /// (`docs/windows-ssh-target-design.md` §6.5), so on Windows targets the
    /// sudo row is annotated instead of editable.
    fn selected_profile_is_windows_shell(&self) -> bool {
        self.selected_profile()
            .and_then(|profile| profile.remote_shell)
            == Some(RemoteShellKind::Windows)
    }

    fn has_saved_selection(&self) -> bool {
        self.selected < self.profiles.len()
    }

    fn saved_ssh_password(&self) -> bool {
        matches!(
            self.selected_profile().map(|profile| &profile.auth),
            Some(RemoteHostAuthProfile::Password {
                password_secret_id: Some(_),
            })
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Hosts,
    Host,
    Port,
    User,
    HostKind,
    Via,
    Auth,
    Password,
    Sudo,
    Remember,
    InstallProxy,
    Delete,
    Connect,
    ProxyName,
    AllProxy,
    HttpsProxy,
    ProxyActive,
    ProxySave,
    ProxyDelete,
    RelayAddress,
    RelayToken,
    RelayJoin,
    RelayRemove,
}

impl Focus {
    fn uses_horizontal_choice(self) -> bool {
        matches!(self, Self::HostKind | Self::Via | Self::Auth)
    }

    fn edit_field(self, auth: AuthChoice) -> Option<EditField> {
        match self {
            Self::Host => Some(EditField::Host),
            Self::Port => Some(EditField::RemotePort),
            Self::User => Some(EditField::SshUser),
            Self::ProxyName => Some(EditField::ProxyName),
            Self::AllProxy => Some(EditField::AllProxy),
            Self::HttpsProxy => Some(EditField::HttpsProxy),
            Self::RelayAddress => Some(EditField::RelayAddress),
            Self::RelayToken => Some(EditField::RelayToken),
            Self::Password if auth == AuthChoice::Key => Some(EditField::KeyPath),
            Self::Password if auth == AuthChoice::Password => Some(EditField::SshPassword),
            _ => None,
        }
    }

    fn ordered(
        _auth: AuthChoice,
        has_saved_selection: bool,
        proxy_page: bool,
        relay_page: bool,
    ) -> Vec<Self> {
        if proxy_page {
            return vec![
                Self::Hosts,
                Self::ProxyName,
                Self::AllProxy,
                Self::HttpsProxy,
                Self::ProxyActive,
                Self::ProxySave,
                Self::ProxyDelete,
            ];
        }
        if relay_page {
            return vec![
                Self::Hosts,
                Self::RelayAddress,
                Self::RelayToken,
                Self::RelayJoin,
                Self::RelayRemove,
            ];
        }
        let mut ordered = vec![
            Self::Hosts,
            Self::Host,
            Self::Port,
            Self::User,
            Self::HostKind,
            Self::Via,
            Self::Auth,
            Self::Password,
            Self::Sudo,
            Self::Remember,
            Self::InstallProxy,
        ];
        ordered.push(Self::Connect);
        if has_saved_selection {
            ordered.push(Self::Delete);
        }
        ordered
    }

    fn next(
        self,
        auth: AuthChoice,
        has_saved_selection: bool,
        proxy_page: bool,
        relay_page: bool,
    ) -> Self {
        let ordered = Self::ordered(auth, has_saved_selection, proxy_page, relay_page);
        let index = ordered.iter().position(|field| *field == self).unwrap_or(0);
        ordered[(index + 1) % ordered.len()]
    }

    fn prev(
        self,
        auth: AuthChoice,
        has_saved_selection: bool,
        proxy_page: bool,
        relay_page: bool,
    ) -> Self {
        let ordered = Self::ordered(auth, has_saved_selection, proxy_page, relay_page);
        let index = ordered.iter().position(|field| *field == self).unwrap_or(0);
        ordered[(index + ordered.len() - 1) % ordered.len()]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditField {
    Host,
    RemotePort,
    SshUser,
    KeyPath,
    SshPassword,
    SudoPassword,
    ProxyName,
    AllProxy,
    HttpsProxy,
    RelayAddress,
    RelayToken,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthChoice {
    Password,
    Key,
}

impl AuthChoice {
    fn shift(self, step: i32) -> Self {
        let values = [Self::Password, Self::Key];
        shift_value(&values, self, step)
    }

    fn as_arg(self) -> &'static str {
        match self {
            Self::Password => "password",
            Self::Key => "key",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PasswordMode {
    Loading,
    Saved,
    Enter,
}

impl PasswordMode {
    fn shift(self, step: i32, saved: bool) -> Self {
        let values = if saved {
            vec![Self::Saved, Self::Enter]
        } else {
            vec![Self::Enter]
        };
        shift_value(&values, self, step)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SudoMode {
    SameAsSsh,
    Loading,
    Saved,
    Replace,
    None,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Status {
    Hint(String),
    Loading(String),
    Working(String),
    Error(String),
}

fn default_hint_status() -> Status {
    Status::Hint(String::new())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DeleteConfirmState {
    Idle,
    Prompt {
        profile_name: String,
        profile_label: String,
        focus: DeleteConfirmFocus,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeleteConfirmFocus {
    Cancel,
    Delete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PaneAction {
    None,
    Redraw,
    Close,
    Connect,
    ReadClipboard,
    DeleteSelectedHost {
        profile_name: String,
    },
    LoadSecrets(Option<SecretLoadRequest>),
    SaveProxyConfig,
    ActivateProxyConfig,
    DeleteProxyConfig,
    /// Enroll at / switch to the relay `address` with the invite `token`;
    /// `force` confirms a pin mismatch after the explicit operator warning
    /// (issue #156 slice 1).
    RelayJoin {
        address: String,
        token: String,
        force: bool,
    },
    /// Remove the pinned relay: clear relay.toml and stop the link.
    RelayRemove,
}

/// Pin-mismatch confirmation shown when a join presented a different relay
/// fingerprint than the pin it replaced. Abort is the default; switching
/// requires an explicit second confirmation (no silent continue).
#[derive(Debug, Clone, PartialEq, Eq)]
enum RelayMismatchState {
    Idle,
    Prompt {
        message: String,
        focus: RelayMismatchFocus,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RelayMismatchFocus {
    Abort,
    SwitchAnyway,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SecretLoadRequest {
    id: u64,
    selected: usize,
    ssh_secret_id: Option<crate::host::ssh::remote_host_secret_store::RemoteHostSecretId>,
    sudo_secret_id: Option<crate::host::ssh::remote_host_secret_store::RemoteHostSecretId>,
}

impl SecretLoadRequest {
    fn has_work(&self) -> bool {
        self.ssh_secret_id.is_some() || self.sudo_secret_id.is_some()
    }
}

#[derive(Debug)]
struct SecretLoadResult {
    id: u64,
    selected: usize,
    ssh: Option<Result<String, String>>,
    sudo: Option<Result<String, String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SecretLoadState {
    Idle,
    Loading { id: u64, selected: usize },
}

#[derive(Debug, Clone, Copy)]
struct PopupGeometry {
    dialog: Rect,
    hosts: Rect,
    details: Rect,
    sidebar: HostSidebarGeometry,
}

#[derive(Debug, Clone, Copy)]
struct HostSidebarGeometry {
    saved_header: Rect,
    saved_list: Rect,
    new_host: Rect,
    proxy_header: Rect,
    proxy_list: Rect,
    new_proxy: Rect,
    relay_header: Rect,
    relay_list: Rect,
}

#[derive(Debug, Clone, Copy)]
struct DetailsGeometry {
    header: Rect,
    connection: Rect,
    authentication: Rect,
    options: Rect,
    info: Rect,
    buttons: Rect,
    hint: Rect,
    rows: DetailsRows,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DetailsRows {
    host: u16,
    port: u16,
    user: u16,
    host_kind: u16,
    via: u16,
    auth: u16,
    password: u16,
    sudo: u16,
    remember: u16,
    install_proxy: u16,
}

#[derive(Debug, Clone, Copy)]
struct ProxyDetailsGeometry {
    header: Rect,
    proxy: Rect,
    no_proxy: Rect,
    info: Rect,
    buttons: Rect,
    hint: Rect,
    status: Rect,
    rows: ProxyDetailsRows,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProxyDetailsRows {
    name: u16,
    all_proxy: u16,
    https_proxy: u16,
    no_proxy: u16,
    action: u16,
}

#[derive(Debug, Clone, Copy)]
struct DeleteConfirmGeometry {
    dialog: Rect,
    cancel_button: Rect,
    delete_button: Rect,
}

#[derive(Debug, Clone, Copy)]
struct ConnectingGeometry {
    dialog: Rect,
    message: Rect,
}

struct ConnectErrorGeometry {
    dialog: Rect,
    message: Rect,
    ok_button: Rect,
}

impl DeleteConfirmGeometry {
    fn from_terminal_size((cols, rows): (u16, u16)) -> Self {
        let width = cols.clamp(36, 56);
        let height = 7.min(rows.max(1));
        let x = cols.saturating_sub(width) / 2;
        let y = rows.saturating_sub(height) / 2;
        let dialog = Rect::new(x, y, width, height);
        let button_y = y.saturating_add(height.saturating_sub(2));
        let delete_button = Rect::new(x.saturating_add(width.saturating_sub(18)), button_y, 14, 1);
        let cancel_button = Rect::new(delete_button.x.saturating_sub(13), button_y, 10, 1);
        Self {
            dialog,
            cancel_button,
            delete_button,
        }
    }
}

impl ConnectingGeometry {
    fn from_terminal_size((cols, rows): (u16, u16)) -> Self {
        let width = cols.clamp(24, 36);
        let height = 5.min(rows.max(1));
        let x = cols.saturating_sub(width) / 2;
        let y = rows.saturating_sub(height) / 2;
        let dialog = Rect::new(x, y, width, height);
        let message = Rect::new(
            x.saturating_add(2),
            y.saturating_add(height / 2),
            width.saturating_sub(4),
            1,
        );
        Self { dialog, message }
    }
}

impl ConnectErrorGeometry {
    fn from_terminal_size((cols, rows): (u16, u16)) -> Self {
        let width = cols.clamp(36, 76);
        let height = rows.clamp(7, 14);
        let x = cols.saturating_sub(width) / 2;
        let y = rows.saturating_sub(height) / 2;
        let dialog = Rect::new(x, y, width, height);
        let message = Rect::new(
            x.saturating_add(2),
            y.saturating_add(2),
            width.saturating_sub(4),
            height.saturating_sub(5),
        );
        let ok_button = Rect::new(
            x.saturating_add(width.saturating_sub(12) / 2),
            y.saturating_add(height.saturating_sub(2)),
            12,
            1,
        );
        Self {
            dialog,
            message,
            ok_button,
        }
    }
}

/// Pin-mismatch confirmation dialog: taller than the delete confirm so the
/// refusal text (both fingerprints) fits without scrolling.
#[derive(Debug, Clone, Copy)]
struct RelayMismatchGeometry {
    dialog: Rect,
    message: Rect,
    abort_button: Rect,
    switch_button: Rect,
}

impl RelayMismatchGeometry {
    fn from_terminal_size((cols, rows): (u16, u16)) -> Self {
        let width = cols.clamp(52, 84);
        let height = 11.min(rows.max(1));
        let x = cols.saturating_sub(width) / 2;
        let y = rows.saturating_sub(height) / 2;
        let dialog = Rect::new(x, y, width, height);
        let message = Rect::new(
            x.saturating_add(2),
            y.saturating_add(2),
            width.saturating_sub(4),
            height.saturating_sub(5),
        );
        let button_y = y.saturating_add(height.saturating_sub(2));
        let switch_button = Rect::new(x.saturating_add(width.saturating_sub(19)), button_y, 15, 1);
        let abort_button = Rect::new(switch_button.x.saturating_sub(11), button_y, 8, 1);
        Self {
            dialog,
            message,
            abort_button,
            switch_button,
        }
    }
}

impl PopupGeometry {
    fn from_terminal_size((cols, rows): (u16, u16), state: &ConnectRemoteHostState) -> Self {
        let width = popup_preferred_width(state).min(cols);
        let x = cols.saturating_sub(width) / 2;

        // Keep the popup compact and centered, like the original popup,
        // instead of stretching to the full terminal height. The height is
        // fixed so it does not jump when the selected menu item changes
        // (e.g. Saved Host vs New Host). It shrinks only on very small
        // terminals to keep a visible margin; 28 rows fit the framed
        // sidebar and detail sections including the Via choice row.
        const POPUP_HEIGHT: u16 = 28;
        let dialog_height = POPUP_HEIGHT.min(rows.saturating_sub(2)).max(14);
        let body_height = dialog_height.saturating_sub(2);
        let y = rows.saturating_sub(dialog_height) / 2;
        let dialog = Rect::new(x, y, width, dialog_height);
        // Leave one column/row on each side for the border.
        let body = Rect::new(
            dialog.x.saturating_add(1),
            dialog.y.saturating_add(1),
            dialog.width.saturating_sub(2),
            body_height,
        );
        let host_width = host_list_width(state, body.width);
        let separator_width = u16::from(body.width > host_width);
        let right_padding = DETAIL_RIGHT_PADDING;
        let details_x = body
            .x
            .saturating_add(host_width)
            .saturating_add(separator_width);
        let details_width = body
            .width
            .saturating_sub(host_width)
            .saturating_sub(separator_width)
            .saturating_sub(right_padding);
        let hosts = Rect::new(body.x, body.y, host_width, body.height);
        let details = Rect::new(details_x, body.y, details_width, body.height);
        let sidebar = HostSidebarGeometry::from_area(hosts, state);
        Self {
            dialog,
            hosts,
            details,
            sidebar,
        }
    }
}

impl HostSidebarGeometry {
    fn from_area(area: Rect, state: &ConnectRemoteHostState) -> Self {
        const HEADER_HEIGHT: u16 = 3;
        const BUTTON_HEIGHT: u16 = 3;
        // Lists sit directly under their headers/buttons; only the
        // sections (Saved Hosts / Proxy Configuration / Relay) are
        // separated.
        const SECTION_GAP: u16 = 0;
        // The relay section is a single always-present row (pinned relay or
        // a "no relay" placeholder).
        const RELAY_LIST_HEIGHT: u16 = 1;
        const FIXED_ROWS: u16 =
            HEADER_HEIGHT * 3 + BUTTON_HEIGHT * 2 + SECTION_GAP + RELAY_LIST_HEIGHT;

        let saved_content = state.profiles.len() as u16;
        let proxy_content = state.proxy_settings.profiles.len() as u16;
        let available_for_lists = area.height.saturating_sub(FIXED_ROWS);
        let total_content = saved_content.saturating_add(proxy_content);

        // Keep lists at their natural content height so action buttons sit
        // directly underneath. If content does not fit, cap each list and let
        // the List widget scroll the selected item into view.
        let (saved_list_height, proxy_list_height) = if total_content <= available_for_lists {
            (saved_content, proxy_content)
        } else {
            let min_each = 2_u16.min(available_for_lists / 2);
            if available_for_lists <= min_each.saturating_mul(2) {
                (min_each, available_for_lists.saturating_sub(min_each))
            } else {
                let extra = available_for_lists.saturating_sub(min_each.saturating_mul(2));
                let saved_extra = if total_content == 0 {
                    0
                } else {
                    (saved_content as u32 * extra as u32 / total_content as u32) as u16
                };
                (
                    min_each.saturating_add(saved_extra),
                    available_for_lists
                        .saturating_sub(min_each)
                        .saturating_sub(saved_extra),
                )
            }
        };

        let saved_header = Rect::new(area.x, area.y, area.width, HEADER_HEIGHT);
        let saved_list_y = saved_header.y.saturating_add(saved_header.height);
        let saved_list = Rect::new(area.x, saved_list_y, area.width, saved_list_height);
        let new_host_y = saved_list.y.saturating_add(saved_list.height);
        let new_host = Rect::new(area.x, new_host_y, area.width, BUTTON_HEIGHT);
        let proxy_header_y = new_host
            .y
            .saturating_add(new_host.height)
            .saturating_add(SECTION_GAP);
        let proxy_header = Rect::new(area.x, proxy_header_y, area.width, HEADER_HEIGHT);
        let proxy_list_y = proxy_header.y.saturating_add(proxy_header.height);
        let proxy_list = Rect::new(area.x, proxy_list_y, area.width, proxy_list_height);
        let new_proxy_y = proxy_list.y.saturating_add(proxy_list.height);
        let new_proxy = Rect::new(area.x, new_proxy_y, area.width, BUTTON_HEIGHT);
        let relay_header_y = new_proxy
            .y
            .saturating_add(new_proxy.height)
            .saturating_add(SECTION_GAP);
        let relay_header_height = HEADER_HEIGHT.min(
            area.y
                .saturating_add(area.height)
                .saturating_sub(relay_header_y),
        );
        let relay_header = Rect::new(area.x, relay_header_y, area.width, relay_header_height);
        let relay_list_y = relay_header.y.saturating_add(relay_header.height);
        let relay_list_height = RELAY_LIST_HEIGHT.min(
            area.y
                .saturating_add(area.height)
                .saturating_sub(relay_list_y),
        );
        let relay_list = Rect::new(area.x, relay_list_y, area.width, relay_list_height);

        Self {
            saved_header,
            saved_list,
            new_host,
            proxy_header,
            proxy_list,
            new_proxy,
            relay_header,
            relay_list,
        }
    }
}

impl DetailsGeometry {
    fn from_area(area: Rect, _state: &ConnectRemoteHostState) -> Self {
        // Layout inspired by the UI mock: header bar, then Connection and
        // Authentication stacked as full-width bordered cards, then Options,
        // an info box, a spacer, action buttons, and a bottom hint bar.
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // framed header bar
                Constraint::Length(8), // Connection card (Host/Port/Last Port/User/Host Kind/Via)
                Constraint::Length(5), // Authentication card
                Constraint::Length(4), // Options card
                Constraint::Length(3), // info box card
                Constraint::Length(1), // spacer above buttons
                Constraint::Length(1), // buttons
                Constraint::Length(1), // hint
            ])
            .split(area);
        let rows = DetailsRows {
            host: sections[1].y.saturating_add(1).saturating_sub(area.y),
            port: sections[1].y.saturating_add(2).saturating_sub(area.y),
            user: sections[1].y.saturating_add(3).saturating_sub(area.y),
            host_kind: sections[1].y.saturating_add(4).saturating_sub(area.y),
            via: sections[1].y.saturating_add(5).saturating_sub(area.y),
            auth: sections[2].y.saturating_add(1).saturating_sub(area.y),
            password: sections[2].y.saturating_add(2).saturating_sub(area.y),
            sudo: sections[2].y.saturating_add(3).saturating_sub(area.y),
            remember: sections[3].y.saturating_add(1).saturating_sub(area.y),
            install_proxy: sections[3].y.saturating_add(2).saturating_sub(area.y),
        };
        Self {
            header: sections[0],
            connection: sections[1],
            authentication: sections[2],
            options: sections[3],
            info: sections[4],
            buttons: sections[6],
            hint: sections[7],
            rows,
        }
    }
}

impl ProxyDetailsGeometry {
    fn from_area(area: Rect) -> Self {
        // Same skeleton as the host detail page (DetailsGeometry): framed
        // header bar, bordered cards, info box, spacer, buttons, and hint;
        // the status line fills whatever rows remain.
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // framed header bar
                Constraint::Length(5), // Proxy card (Name / all_proxy / https_proxy)
                Constraint::Length(3), // No Proxy card
                Constraint::Length(3), // info box card
                Constraint::Length(1), // spacer above buttons
                Constraint::Length(1), // buttons
                Constraint::Length(1), // hint
                Constraint::Min(0),    // status
            ])
            .split(area);
        let rows = ProxyDetailsRows {
            name: sections[1].y.saturating_add(1).saturating_sub(area.y),
            all_proxy: sections[1].y.saturating_add(2).saturating_sub(area.y),
            https_proxy: sections[1].y.saturating_add(3).saturating_sub(area.y),
            no_proxy: sections[2].y.saturating_add(1).saturating_sub(area.y),
            action: sections[5].y.saturating_sub(area.y),
        };
        Self {
            header: sections[0],
            proxy: sections[1],
            no_proxy: sections[2],
            info: sections[3],
            buttons: sections[5],
            hint: sections[6],
            status: sections[7],
            rows,
        }
    }
}

fn render(frame: &mut Frame<'_>, state: &ConnectRemoteHostState) {
    let geometry =
        PopupGeometry::from_terminal_size((frame.size().width, frame.size().height), state);

    // The background is rendered with a dim modifier behind the popup. Reset
    // the dialog area to the default style so the popup text and borders are
    // drawn at full brightness.
    frame.render_widget(Clear, geometry.dialog);

    frame.render_widget(
        Block::default()
            .title("Connect Remote Host")
            .title_alignment(Alignment::Center)
            .title_style(Style::default().add_modifier(Modifier::BOLD))
            .borders(Borders::ALL)
            .style(Style::default().fg(Color::White).bg(DIALOG_BG)),
        geometry.dialog,
    );

    render_hosts(frame, geometry.hosts, state);
    render_details(frame, geometry.details, state);
    render_cursor(frame, geometry.details, state);
    render_connecting_popup(frame, state);
    render_connect_error_popup(frame, state);
    render_delete_confirm(frame, state);
    render_relay_mismatch(frame, state);
}

fn render_hosts(frame: &mut Frame<'_>, area: Rect, state: &ConnectRemoteHostState) {
    let geometry = HostSidebarGeometry::from_area(area, state);
    let hosts_focused = state.focus == Focus::Hosts;

    let new_host_selected = state.selected == state.profiles.len();
    let new_proxy_selected = state.selected == state.new_proxy_selection_index();

    render_framed_block(
        frame,
        geometry.saved_header,
        header_block_content(
            "▤",
            "Saved Hosts",
            state.profiles.len(),
            geometry.saved_header.width,
        ),
        Alignment::Left,
        false,
    );
    render_host_list(
        frame,
        geometry.saved_list,
        state,
        hosts_focused,
        0,
        state.profiles.len(),
    );
    render_framed_block(
        frame,
        geometry.new_host,
        button_block_content("+", "New Host", geometry.new_host.width, new_host_selected),
        Alignment::Left,
        new_host_selected,
    );

    render_framed_block(
        frame,
        geometry.proxy_header,
        header_block_content(
            "⛓",
            "Proxy Configuration",
            state.proxy_settings.profiles.len(),
            geometry.proxy_header.width,
        ),
        Alignment::Left,
        false,
    );
    render_host_list(
        frame,
        geometry.proxy_list,
        state,
        hosts_focused,
        state.proxy_selection_index(),
        state.proxy_settings.profiles.len(),
    );
    render_framed_block(
        frame,
        geometry.new_proxy,
        button_block_content(
            "+",
            "New Proxy",
            geometry.new_proxy.width,
            new_proxy_selected,
        ),
        Alignment::Left,
        new_proxy_selected,
    );

    render_framed_block(
        frame,
        geometry.relay_header,
        header_block_content(
            "⇄",
            "Relay",
            usize::from(state.relay.is_some()),
            geometry.relay_header.width,
        ),
        Alignment::Left,
        false,
    );
    let relay_selected = state.selected_relay_config();
    let relay_item = match &state.relay {
        Some(pin) => ListItem::new(Line::from(vec![
            Span::styled(" ●  ", Style::default().fg(Color::Green)),
            Span::styled(pin.address.clone(), Style::default().fg(Color::White)),
        ])),
        None => ListItem::new(Line::from(vec![
            Span::styled(" ○  ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                "no relay pinned".to_string(),
                Style::default().fg(Color::DarkGray),
            ),
        ])),
    };
    let relay_list = List::new(vec![relay_item])
        .highlight_symbol("")
        .highlight_style(if hosts_focused && relay_selected {
            active_focus_style()
        } else if relay_selected {
            selected_host_style()
        } else {
            Style::default()
        });
    let mut relay_list_state = ratatui::widgets::ListState::default().with_selected(Some(0));
    frame.render_stateful_widget(relay_list, geometry.relay_list, &mut relay_list_state);
}

fn render_framed_block(
    frame: &mut Frame<'_>,
    area: Rect,
    content: Line<'static>,
    alignment: Alignment,
    selected: bool,
) {
    if area.height < 3 {
        return;
    }
    let border_color = SECTION_BORDER;
    let (block_bg, content_bg) = if selected {
        (SECTION_BG, Color::Blue)
    } else {
        (SECTION_BG, SECTION_BG)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .style(Style::default().bg(block_bg));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    frame.render_widget(
        Paragraph::new(content)
            .alignment(alignment)
            .style(Style::default().bg(content_bg)),
        Rect::new(inner.x, inner.y, inner.width, inner.height),
    );
}

fn header_block_content(icon: &str, title: &str, count: usize, width: u16) -> Line<'static> {
    let count_text = format!("{count}");
    let left = format!("{icon}  {title}");
    let right = count_text.to_string();
    let inner_width = width.saturating_sub(2) as usize;
    let left_width = left.width();
    let right_width = right.width();
    let padding = inner_width.saturating_sub(left_width + right_width);
    let spaces = " ".repeat(padding);

    Line::from(vec![
        Span::styled(icon.to_string(), Style::default().fg(Color::White)),
        Span::raw("  "),
        Span::styled(
            title.to_string(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(spaces),
        Span::styled(
            count_text,
            Style::default()
                .fg(Color::White)
                .bg(Color::Rgb(50, 55, 65))
                .add_modifier(Modifier::BOLD),
        ),
    ])
}

fn button_block_content(icon: &str, label: &str, width: u16, selected: bool) -> Line<'static> {
    let left = format!("{icon} {label}");
    let icon_width = icon.width();
    let left_width = left.width();
    let inner_width = width.saturating_sub(2) as usize;
    let padding = inner_width.saturating_sub(left_width + icon_width);
    let fg = if selected { Color::White } else { Color::Gray };

    Line::from(vec![
        Span::styled(icon.to_string(), Style::default().fg(fg)),
        Span::raw(" "),
        Span::styled(label.to_string(), Style::default().fg(fg)),
        Span::raw(" ".repeat(padding)),
        Span::styled(icon.to_string(), Style::default().fg(fg)),
    ])
}

fn render_host_list(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &ConnectRemoteHostState,
    hosts_focused: bool,
    selection_offset: usize,
    item_count: usize,
) {
    if area.height == 0 || item_count == 0 {
        return;
    }
    let items: Vec<ListItem<'static>> = (0..item_count)
        .map(|index| host_list_item(state, selection_offset + index))
        .collect();
    let selected =
        if state.selected >= selection_offset && state.selected < selection_offset + item_count {
            Some(state.selected - selection_offset)
        } else {
            None
        };
    let visible_height = area.height as usize;
    let selected_row = selected.unwrap_or(0);
    let max_offset = items.len().saturating_sub(visible_height);
    let offset = if selected_row >= visible_height {
        (selected_row - visible_height + 1).min(max_offset)
    } else {
        0
    };
    let list = List::new(items)
        .highlight_symbol("")
        .highlight_style(if hosts_focused {
            active_focus_style()
        } else {
            selected_host_style()
        });
    let mut list_state = ratatui::widgets::ListState::default()
        .with_selected(selected)
        .with_offset(offset);
    frame.render_stateful_widget(list, area, &mut list_state);
}

fn host_list_item(state: &ConnectRemoteHostState, selection: usize) -> ListItem<'static> {
    if selection < state.profiles.len() {
        let profile = &state.profiles[selection];
        ListItem::new(Line::from(vec![
            Span::styled(" ●  ", Style::default().fg(Color::Green)),
            Span::styled(saved_host_label(profile), Style::default().fg(Color::White)),
        ]))
    } else {
        let index = selection - state.proxy_selection_index();
        let profile = &state.proxy_settings.profiles[index];
        let active = state.proxy_settings.active.as_deref() == Some(profile.name.as_str());
        let (prefix, color) = if active {
            ("★", Color::Yellow)
        } else {
            ("●", Color::Green)
        };
        ListItem::new(Line::from(vec![
            Span::styled(format!(" {prefix}  "), Style::default().fg(color)),
            Span::styled(profile.name.clone(), Style::default().fg(Color::White)),
        ]))
    }
}

fn saved_host_label(profile: &RemoteHostProfile) -> String {
    format!("{}@{}", profile.ssh_user, profile.host)
}

const POPUP_WIDTH: u16 = 100;
const HOST_LIST_WIDTH: u16 = 29;
const DETAIL_RIGHT_PADDING: u16 = 0;
const SECTION_CONTENT_INDENT: u16 = 0;
const LABEL_WIDTH: u16 = 16;
const DETAIL_VALUE_START: u16 = SECTION_CONTENT_INDENT + LABEL_WIDTH + 1;
const PROXY_VALUE_START: u16 = LABEL_WIDTH + 1;

fn popup_preferred_width(_state: &ConnectRemoteHostState) -> u16 {
    POPUP_WIDTH
}

fn host_list_width(_state: &ConnectRemoteHostState, body_width: u16) -> u16 {
    HOST_LIST_WIDTH.min(body_width)
}

fn status_message(state: &ConnectRemoteHostState) -> &str {
    match &state.status {
        Status::Hint(message)
        | Status::Loading(message)
        | Status::Working(message)
        | Status::Error(message) => message,
    }
}

fn render_details(frame: &mut Frame<'_>, area: Rect, state: &ConnectRemoteHostState) {
    if state.selected_proxy_config() {
        render_proxy_details(frame, area, state);
        return;
    }
    if state.selected_relay_config() {
        render_relay_details(frame, area, state);
        return;
    }
    let geometry = DetailsGeometry::from_area(area, state);
    render_header(frame, geometry.header, state);
    render_connection(frame, geometry.connection, state);
    render_authentication(frame, geometry.authentication, state);
    render_options(frame, geometry.options, state);
    render_info_box(frame, geometry.info, state);
    render_action_buttons(frame, geometry.buttons, state);
    render_hint(frame, geometry.hint, state);
}

const DIALOG_BG: Color = Color::Rgb(22, 24, 30);
const SECTION_BG: Color = Color::Rgb(28, 31, 38);
const SECTION_BORDER: Color = Color::Rgb(60, 65, 75);

fn render_header(frame: &mut Frame<'_>, area: Rect, state: &ConnectRemoteHostState) {
    let host = if state.host.is_empty() {
        "New Host".to_string()
    } else {
        format!("{}@{}", state.ssh_user, state.host)
    };
    let status_color = if state.selected >= state.profiles.len() {
        Color::Yellow
    } else {
        Color::Green
    };
    let mut content = vec![
        Span::styled("●", Style::default().fg(status_color)),
        Span::raw(" "),
        Span::styled(
            host,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if state.selected < state.profiles.len() {
        content.push(Span::raw("  "));
        content.push(Span::styled(
            "SSH",
            Style::default().bg(Color::Cyan).fg(Color::Black),
        ));
        content.push(Span::raw(" "));
        content.push(Span::styled(
            "Saved",
            Style::default().bg(Color::Green).fg(Color::Black),
        ));
        if let Some(badge) = via_badge(state.selected_profile()) {
            content.push(Span::raw(" "));
            content.push(badge);
        }
    }

    let star = if state.selected >= state.profiles.len() {
        "☆"
    } else {
        "★"
    };
    let star_width = star.width();
    let content_width: usize = content.iter().map(|span| span.content.width()).sum();
    let inner_width = area.width.saturating_sub(2) as usize;
    let padding = inner_width
        .saturating_sub(content_width)
        .saturating_sub(star_width);
    content.push(Span::raw(" ".repeat(padding)));
    content.push(Span::styled(star, Style::default().fg(Color::Yellow)));

    render_framed_block(frame, area, Line::from(content), Alignment::Left, false);
}

/// Header badge for the selected profile's dial path (issue #156 slice 2):
/// the explicit choice, or — for `auto` — the choice annotated with the
/// path that last took effect, so the effective route is visible without
/// ever rewriting the user's choice.
fn via_badge(profile: Option<&RemoteHostProfile>) -> Option<Span<'static>> {
    let profile = profile?;
    let badge = match profile.via() {
        RemoteNodeVia::Relay => Span::styled(
            "via relay",
            Style::default().bg(Color::Magenta).fg(Color::Black),
        ),
        RemoteNodeVia::Direct => Span::styled(
            "via direct",
            Style::default().bg(Color::Cyan).fg(Color::Black),
        ),
        RemoteNodeVia::Auto => {
            let text = match profile.last_via_used() {
                Some(RemoteNodeVia::Direct) => "via auto → direct",
                Some(RemoteNodeVia::Relay) => "via auto → relay",
                // No successful auto connect yet (or a hand-edited value).
                _ => "via auto",
            };
            Span::styled(text, Style::default().bg(Color::Green).fg(Color::Black))
        }
    };
    Some(badge)
}

/// Proxy detail header (issue #166): the same framed identity bar as the
/// host page — profile icon and name, a Saved/Draft badge, and the active
/// marker right-aligned (★ only when this profile is the active one; the
/// New Proxy draft shows no marker).
fn render_proxy_header(frame: &mut Frame<'_>, area: Rect, state: &ConnectRemoteHostState) {
    let saved = state.selected_proxy_profile_index().is_some();
    let name = if saved {
        state.proxy_draft.name.clone()
    } else {
        "New Proxy".to_string()
    };
    let badge = if saved {
        Span::styled("Saved", Style::default().bg(Color::Green).fg(Color::Black))
    } else {
        Span::styled("Draft", Style::default().bg(Color::Yellow).fg(Color::Black))
    };
    let mut content = vec![
        Span::styled("⛓", Style::default().fg(SECTION_COLOR_CONNECTION)),
        Span::raw(" "),
        Span::styled(
            name,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        badge,
    ];
    let star = if !saved {
        None
    } else if state.proxy_settings.active.as_deref() == Some(state.proxy_draft.name.as_str()) {
        Some(Span::styled("★", Style::default().fg(Color::Yellow)))
    } else {
        Some(Span::styled("☆", Style::default().fg(Color::DarkGray)))
    };
    let star_width = star.as_ref().map_or(0, |span| span.content.width());
    let content_width: usize = content.iter().map(|span| span.content.width()).sum();
    let inner_width = area.width.saturating_sub(2) as usize;
    let padding = inner_width
        .saturating_sub(content_width)
        .saturating_sub(star_width);
    content.push(Span::raw(" ".repeat(padding)));
    if let Some(star) = star {
        content.push(star);
    }
    render_framed_block(frame, area, Line::from(content), Alignment::Left, false);
}

fn render_info_box(frame: &mut Frame<'_>, area: Rect, _state: &ConnectRemoteHostState) {
    if area.height == 0 {
        return;
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(SECTION_BORDER_UNIFIED))
        .style(Style::default().bg(SECTION_BG_UNIFIED));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let text = "ⓘ Connect to the selected remote host via SSH. All session settings will be applied after connection.";
    frame.render_widget(
        Paragraph::new(text)
            .style(Style::default().fg(Color::Gray))
            .wrap(Wrap { trim: true }),
        inner,
    );
}

fn render_proxy_info_box(frame: &mut Frame<'_>, area: Rect) {
    if area.height == 0 {
        return;
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(SECTION_BORDER_UNIFIED))
        .style(Style::default().bg(SECTION_BG_UNIFIED));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let text =
        "ⓘ Environment used when the remote host downloads and installs waitagent during connect.";
    frame.render_widget(
        Paragraph::new(text)
            .style(Style::default().fg(Color::Gray))
            .wrap(Wrap { trim: true }),
        inner,
    );
}

fn render_proxy_details(frame: &mut Frame<'_>, area: Rect, state: &ConnectRemoteHostState) {
    let geometry = ProxyDetailsGeometry::from_area(area);
    render_proxy_header(frame, geometry.header, state);

    let proxy_block = section_block("Proxy", "⛓", SECTION_COLOR_CONNECTION);
    let proxy_inner = proxy_block.inner(geometry.proxy);
    frame.render_widget(proxy_block, geometry.proxy);
    let rows = [
        icon_detail_row(
            "●",
            "Name",
            &state.proxy_draft.name,
            state,
            Focus::ProxyName,
        ),
        icon_detail_row(
            "◆",
            "all_proxy",
            &proxy_input_display(&state.proxy_draft.all_proxy),
            state,
            Focus::AllProxy,
        ),
        icon_detail_row(
            "◇",
            "https_proxy",
            &proxy_input_display(&state.proxy_draft.https_proxy),
            state,
            Focus::HttpsProxy,
        ),
    ];
    render_detail_table(frame, proxy_inner, rows);

    let no_proxy_block = section_block("No Proxy", "⊘", SECTION_COLOR_OPTIONS);
    let no_proxy_inner = no_proxy_block.inner(geometry.no_proxy);
    frame.render_widget(no_proxy_block, geometry.no_proxy);
    render_no_proxy(frame, no_proxy_inner, state);

    render_proxy_info_box(frame, geometry.info);
    render_proxy_save(frame, geometry.buttons, state);
    render_hint(frame, geometry.hint, state);
    render_status(frame, geometry.status, state);
}

#[derive(Debug, Clone, Copy)]
struct RelayDetailsGeometry {
    relay: Rect,
    action: Rect,
    status: Rect,
    rows: RelayDetailsRows,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RelayDetailsRows {
    address: u16,
    token: u16,
    fingerprint: u16,
    action: u16,
}

impl RelayDetailsGeometry {
    fn from_area(area: Rect) -> Self {
        let bottom = area.y.saturating_add(area.height);
        // Card: title + Address + Invite Token + Fingerprint rows + borders.
        let relay_height = area.height.min(6);
        let relay = Rect::new(area.x, area.y, area.width, relay_height);
        let action_y = relay
            .y
            .saturating_add(relay_height)
            .saturating_add(1)
            .min(bottom);
        let action_height = u16::from(action_y < bottom);
        let action = Rect::new(area.x, action_y, area.width, action_height);
        let status_y = action_y.saturating_add(action_height).min(bottom);
        let status = Rect::new(
            area.x,
            status_y,
            area.width,
            bottom.saturating_sub(status_y),
        );
        let rows = RelayDetailsRows {
            address: relay.y.saturating_add(1).saturating_sub(area.y),
            token: relay.y.saturating_add(2).saturating_sub(area.y),
            fingerprint: relay.y.saturating_add(3).saturating_sub(area.y),
            action: action_y.saturating_sub(area.y),
        };
        Self {
            relay,
            action,
            status,
            rows,
        }
    }
}

fn render_relay_details(frame: &mut Frame<'_>, area: Rect, state: &ConnectRemoteHostState) {
    let geometry = RelayDetailsGeometry::from_area(area);
    let fingerprint = state
        .relay
        .as_ref()
        .map(|pin| pin.relay_fingerprint.clone())
        .unwrap_or_else(|| "—".to_string());
    let rows = [
        detail_row(
            "Address",
            &relay_input_display(&state.relay_draft_address),
            state,
            Focus::RelayAddress,
        ),
        detail_row(
            "Invite Token",
            &relay_input_display(&state.relay_draft_token),
            state,
            Focus::RelayToken,
        ),
        readonly_detail_row("Fingerprint", &fingerprint),
    ];
    let relay_block = section_block("Relay", "⇄", SECTION_COLOR_CONNECTION);
    let relay_inner = relay_block.inner(geometry.relay);
    frame.render_widget(relay_block, geometry.relay);
    render_detail_table(frame, relay_inner, rows);

    render_relay_actions(frame, geometry.action, state);
    render_status(frame, geometry.status, state);
}

fn relay_button_layout(area: Rect, state: &ConnectRemoteHostState) -> (Rect, Option<Rect>) {
    let join_text = " ⇄  Join Relay ";
    let join_width = join_text.width() as u16;
    let remove_text = " 🗑  Remove Relay ";
    let remove_width = remove_text.width() as u16;
    let gap = 2;
    let total_width = if state.relay.is_some() {
        join_width + gap + remove_width
    } else {
        join_width
    };
    let start_x = area.x + (area.width.saturating_sub(total_width)) / 2;
    let join = Rect::new(start_x, area.y, join_width, 1);
    let remove = if state.relay.is_some() {
        Some(Rect::new(
            start_x + join_width + gap,
            area.y,
            remove_width,
            1,
        ))
    } else {
        None
    };
    (join, remove)
}

fn render_relay_actions(frame: &mut Frame<'_>, area: Rect, state: &ConnectRemoteHostState) {
    if area.height == 0 {
        return;
    }
    let join_text = " ⇄  Join Relay ";
    let remove_text = " 🗑  Remove Relay ";
    let (join_area, remove_area) = relay_button_layout(area, state);
    let join_style = if state.focus == Focus::RelayJoin {
        Style::default()
            .bg(Color::Blue)
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().bg(Color::Rgb(40, 44, 52)).fg(Color::Gray)
    };
    frame.render_widget(
        Paragraph::new(join_text)
            .style(join_style)
            .alignment(Alignment::Center),
        join_area,
    );
    if let Some(remove_area) = remove_area {
        let remove_style = if state.focus == Focus::RelayRemove {
            delete_focus_style()
        } else {
            Style::default().bg(Color::Rgb(40, 44, 52)).fg(Color::Red)
        };
        frame.render_widget(
            Paragraph::new(remove_text)
                .style(remove_style)
                .alignment(Alignment::Center),
            remove_area,
        );
    }
}

const PROXY_EMPTY_PLACEHOLDER: &str = "________________";

fn proxy_input_display(value: &str) -> String {
    if value.is_empty() {
        PROXY_EMPTY_PLACEHOLDER.to_string()
    } else {
        value.to_string()
    }
}

fn relay_input_display(value: &str) -> String {
    if value.is_empty() {
        PROXY_EMPTY_PLACEHOLDER.to_string()
    } else {
        value.to_string()
    }
}

fn render_no_proxy(frame: &mut Frame<'_>, area: Rect, state: &ConnectRemoteHostState) {
    if area.height == 0 {
        return;
    }
    // Icon-style label like the other card rows, but the value stays
    // left-aligned: the auto-computed CIDR list is longer than the value
    // column, and right-aligning it would clip the `auto:` prefix.
    frame.render_widget(
        Paragraph::new(format!(
            "{:<width$}",
            "⊘  no_proxy",
            width = LABEL_WIDTH as usize
        )),
        Rect::new(area.x, area.y, LABEL_WIDTH, 1),
    );
    frame.render_widget(
        Paragraph::new(format!("auto: {}", no_proxy_for_install(&state.host, "")))
            .alignment(Alignment::Left)
            .wrap(Wrap { trim: false }),
        Rect::new(
            area.x.saturating_add(LABEL_WIDTH + 1),
            area.y,
            area.width.saturating_sub(LABEL_WIDTH + 1),
            area.height,
        ),
    );
}

fn proxy_button_texts(state: &ConnectRemoteHostState) -> (String, String, String) {
    let active = state.proxy_settings.active.as_deref() == Some(state.proxy_draft.name.as_str());
    let active_label = if active {
        "✓  Active"
    } else {
        "✓  Set Active"
    };
    (
        format!(" {active_label} "),
        " 💾  Save ".to_string(),
        " 🗑  Delete ".to_string(),
    )
}

fn proxy_button_layout(area: Rect, state: &ConnectRemoteHostState) -> (Rect, Rect, Rect) {
    let (active_text, save_text, delete_text) = proxy_button_texts(state);
    let gap = 2;
    let active_width = active_text.width() as u16;
    let save_width = save_text.width() as u16;
    let delete_width = delete_text.width() as u16;
    let total_width = active_width + gap + save_width + gap + delete_width;
    let start_x = area.x + (area.width.saturating_sub(total_width)) / 2;

    let active = Rect::new(start_x, area.y, active_width, 1);
    let save_x = start_x + active_width + gap;
    let save = Rect::new(save_x, area.y, save_width, 1);
    let delete_x = save_x + save_width + gap;
    let delete = Rect::new(delete_x, area.y, delete_width, 1);
    (active, save, delete)
}

fn render_proxy_save(frame: &mut Frame<'_>, area: Rect, state: &ConnectRemoteHostState) {
    if area.height == 0 {
        return;
    }
    let (active_text, save_text, delete_text) = proxy_button_texts(state);
    let (active_area, save_area, delete_area) = proxy_button_layout(area, state);
    let primary_style = |focused: bool| {
        if focused {
            Style::default()
                .bg(Color::Blue)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().bg(Color::Rgb(40, 44, 52)).fg(Color::Gray)
        }
    };
    frame.render_widget(
        Paragraph::new(active_text)
            .style(primary_style(state.focus == Focus::ProxyActive))
            .alignment(Alignment::Center),
        active_area,
    );
    frame.render_widget(
        Paragraph::new(save_text)
            .style(primary_style(state.focus == Focus::ProxySave))
            .alignment(Alignment::Center),
        save_area,
    );
    let delete_style = if state.focus == Focus::ProxyDelete {
        delete_focus_style()
    } else {
        Style::default().bg(Color::Rgb(40, 44, 52)).fg(Color::Red)
    };
    frame.render_widget(
        Paragraph::new(delete_text)
            .style(delete_style)
            .alignment(Alignment::Center),
        delete_area,
    );
}

fn proxy_action_from_x(x: u16, area: Rect, state: &ConnectRemoteHostState) -> PaneAction {
    let (active, save, delete) = proxy_button_layout(area, state);
    if point_in_rect(x, area.y, active) {
        PaneAction::ActivateProxyConfig
    } else if point_in_rect(x, area.y, save) {
        PaneAction::SaveProxyConfig
    } else if point_in_rect(x, area.y, delete) {
        PaneAction::DeleteProxyConfig
    } else {
        PaneAction::None
    }
}

fn relay_action_from_x(x: u16, area: Rect, state: &ConnectRemoteHostState) -> Option<Focus> {
    let (join, remove) = relay_button_layout(area, state);
    if point_in_rect(x, area.y, join) {
        Some(Focus::RelayJoin)
    } else if let Some(remove) = remove {
        if point_in_rect(x, area.y, remove) {
            return Some(Focus::RelayRemove);
        }
        None
    } else {
        None
    }
}

fn render_connection(frame: &mut Frame<'_>, area: Rect, state: &ConnectRemoteHostState) {
    let block = section_block("Connection", "◎", SECTION_COLOR_CONNECTION);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let table_area = Rect::new(
        inner.x.saturating_add(SECTION_CONTENT_INDENT),
        inner.y,
        inner.width.saturating_sub(SECTION_CONTENT_INDENT),
        inner.height,
    );
    let mut rows = vec![
        icon_detail_row("●", "Host", &host_display(state), state, Focus::Host),
        icon_detail_row(
            "◆",
            "Listen Port",
            &state.remote_port_preference,
            state,
            Focus::Port,
        ),
    ];
    if let Some(port) = state.last_remote_port {
        rows.push(readonly_icon_detail_row(
            "→",
            "Last Port",
            &port.to_string(),
        ));
    }
    rows.push(icon_detail_row(
        "◇",
        "SSH User",
        &state.ssh_user,
        state,
        Focus::User,
    ));
    rows.push(icon_choice_row(
        "◎",
        "Host Kind",
        host_kind_tabs(state),
        state,
        Focus::HostKind,
    ));
    rows.push(icon_choice_row(
        "⇄",
        "Via",
        via_tabs(state),
        state,
        Focus::Via,
    ));
    render_detail_table(frame, table_area, rows);
}

fn render_authentication(frame: &mut Frame<'_>, area: Rect, state: &ConnectRemoteHostState) {
    let block = section_block("Authentication", "◼", SECTION_COLOR_AUTH);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let table_area = Rect::new(
        inner.x.saturating_add(SECTION_CONTENT_INDENT),
        inner.y,
        inner.width.saturating_sub(SECTION_CONTENT_INDENT),
        inner.height,
    );
    let mut rows = vec![icon_choice_row(
        "○",
        "Auth Method",
        auth_tabs(state),
        state,
        Focus::Auth,
    )];
    if state.auth == AuthChoice::Key || state.host_kind == RemoteHostKind::Cloud {
        rows.push(icon_detail_row(
            "■",
            "Key",
            &password_display(state),
            state,
            Focus::Password,
        ));
    } else {
        rows.push(icon_password_row(
            "■",
            "Password",
            PasswordField::Ssh,
            state,
        ));
    }
    if state.host_kind == RemoteHostKind::Lan {
        if state.selected_profile_is_windows_shell() {
            // The sudo password only exists for the POSIX installer; on a
            // known Windows target the row is an annotation, not a field.
            rows.push(
                readonly_detail_row("▲  Sudo", "Not needed on Windows hosts")
                    .style(Style::default().fg(Color::DarkGray)),
            );
        } else {
            rows.push(icon_password_row("▲", "Sudo", PasswordField::Sudo, state));
        }
    }
    render_detail_table(frame, table_area, rows);
}

fn render_options(frame: &mut Frame<'_>, area: Rect, state: &ConnectRemoteHostState) {
    let block = section_block("Options", "⚙", SECTION_COLOR_OPTIONS);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let row_width = inner
        .width
        .saturating_sub(SECTION_CONTENT_INDENT)
        .saturating_sub(1);
    let remember_area = Rect::new(
        inner.x.saturating_add(SECTION_CONTENT_INDENT),
        inner.y,
        row_width,
        1,
    );
    let proxy_area = Rect::new(
        inner.x.saturating_add(SECTION_CONTENT_INDENT),
        inner.y.saturating_add(1),
        row_width,
        1,
    );
    render_checkbox_row(
        frame,
        remember_area,
        state.remember,
        "Remember host",
        state,
        Focus::Remember,
    );
    render_checkbox_row(
        frame,
        proxy_area,
        state.use_install_proxy,
        "Use proxy",
        state,
        Focus::InstallProxy,
    );
}

fn render_checkbox_row(
    frame: &mut Frame<'_>,
    area: Rect,
    checked: bool,
    label: &str,
    state: &ConnectRemoteHostState,
    focus: Focus,
) {
    let box_symbol = if checked { "☑" } else { "☐" };
    let style = if state.focus == focus {
        active_focus_style()
    } else {
        Style::default()
    };
    frame.render_widget(
        Paragraph::new(format!("{box_symbol}  {label}")).style(style),
        area,
    );
}

const SECTION_COLOR_CONNECTION: Color = Color::Cyan;
const SECTION_COLOR_AUTH: Color = Color::Magenta;
const SECTION_COLOR_OPTIONS: Color = Color::Yellow;
const SECTION_BG_UNIFIED: Color = Color::Rgb(32, 36, 43);
const SECTION_BORDER_UNIFIED: Color = Color::Rgb(70, 75, 85);

fn section_block(title: &str, icon: &str, title_color: Color) -> Block<'static> {
    Block::default()
        .title(section_title(title, icon, title_color))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(SECTION_BORDER_UNIFIED))
        .style(Style::default().bg(SECTION_BG_UNIFIED))
}

fn section_title(title: &str, icon: &str, icon_color: Color) -> Line<'static> {
    Line::from(vec![
        Span::raw(" "),
        Span::styled(icon.to_string(), Style::default().fg(icon_color)),
        Span::raw("  "),
        Span::styled(
            title.to_string(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
    ])
}

fn modal_title(title: &str) -> Line<'static> {
    Line::from(format!(" {title}")).style(
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )
}

fn render_detail_table<I>(frame: &mut Frame<'_>, area: Rect, rows: I)
where
    I: IntoIterator<Item = Row<'static>>,
{
    let table =
        Table::new(rows, [Constraint::Length(LABEL_WIDTH), Constraint::Min(1)]).column_spacing(1);
    frame.render_widget(table, area);
}

fn render_action_buttons(frame: &mut Frame<'_>, area: Rect, state: &ConnectRemoteHostState) {
    let connect_label = connect_label(state);
    let connect_text = format!(" ▶  {connect_label} ");
    let connect_width = connect_text.width() as u16;
    let delete_text = " 🗑  Delete ".to_string();
    let delete_width = delete_text.width() as u16;
    let gap = 2;
    let total_width = if state.has_saved_selection() {
        connect_width + gap + delete_width
    } else {
        connect_width
    };
    let start_x = area.x + (area.width.saturating_sub(total_width)) / 2;

    let connect_style = if state.focus == Focus::Connect {
        Style::default()
            .bg(Color::Blue)
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    } else if state.focus == Focus::Hosts {
        Style::default().bg(Color::Rgb(40, 44, 52)).fg(Color::Gray)
    } else {
        Style::default().bg(Color::Blue).fg(Color::White)
    };
    frame.render_widget(
        Paragraph::new(connect_text)
            .style(connect_style)
            .alignment(Alignment::Center),
        Rect::new(start_x, area.y, connect_width, 1),
    );
    if state.has_saved_selection() {
        let delete_x = start_x + connect_width + gap;
        let delete_style = if state.focus == Focus::Delete {
            delete_focus_style()
        } else {
            Style::default().bg(Color::Rgb(40, 44, 52)).fg(Color::Red)
        };
        frame.render_widget(
            Paragraph::new(delete_text)
                .style(delete_style)
                .alignment(Alignment::Center),
            Rect::new(delete_x, area.y, delete_width, 1),
        );
    }
}

fn button_action_from_x(
    x: u16,
    buttons_area: Rect,
    state: &ConnectRemoteHostState,
) -> Option<Focus> {
    let connect_text = format!(" ▶  {} ", connect_label(state));
    let connect_width = connect_text.width() as u16;
    let delete_text = " 🗑  Delete ";
    let delete_width = delete_text.width() as u16;
    let gap = 2;
    let total_width = if state.has_saved_selection() {
        connect_width + gap + delete_width
    } else {
        connect_width
    };
    let start_x = buttons_area.x + (buttons_area.width.saturating_sub(total_width)) / 2;
    if x >= start_x && x < start_x + connect_width {
        return Some(Focus::Connect);
    }
    if state.has_saved_selection() {
        let delete_start = start_x + connect_width + gap;
        if x >= delete_start && x < delete_start + delete_width {
            return Some(Focus::Delete);
        }
    }
    None
}

fn render_hint(frame: &mut Frame<'_>, area: Rect, state: &ConnectRemoteHostState) {
    frame.render_widget(
        Paragraph::new(bottom_hint_text(state))
            .alignment(Alignment::Center)
            .style(Style::default().fg(Color::DarkGray)),
        area,
    );
}

fn bottom_hint_text(state: &ConnectRemoteHostState) -> String {
    match state.focus {
        Focus::Password | Focus::Sudo => "Enter: edit · Space: show/hide · Tab: next".to_string(),
        Focus::HostKind | Focus::Via | Focus::Auth => {
            "←/→: switch · Space: toggle · Tab: next".to_string()
        }
        Focus::Remember | Focus::InstallProxy => "Space: toggle · Tab: next".to_string(),
        Focus::Connect => "Enter: connect · Tab: next".to_string(),
        Focus::Delete => "Enter: delete · Tab: next".to_string(),
        Focus::ProxyName | Focus::AllProxy | Focus::HttpsProxy => {
            "Enter: edit · Ctrl-V: paste · Tab: next".to_string()
        }
        Focus::ProxyActive => "Enter: set active · Tab: next".to_string(),
        Focus::ProxySave => "Enter: save · Tab: next".to_string(),
        Focus::ProxyDelete => "Enter: delete · Tab: next".to_string(),
        Focus::RelayAddress | Focus::RelayToken => {
            "Enter: edit · Ctrl-V: paste · Tab: next".to_string()
        }
        Focus::RelayJoin => "Enter: join relay · Tab: next".to_string(),
        Focus::RelayRemove => "Enter: remove relay · Tab: next".to_string(),
        _ => {
            let base = "↑/↓ Select · Tab Switch · Enter Connect";
            if state.has_saved_selection() {
                format!("{base} · D Delete · Esc Back")
            } else {
                format!("{base} · Esc Back")
            }
        }
    }
}

fn detail_row(
    label: &str,
    value: &str,
    state: &ConnectRemoteHostState,
    focus: Focus,
) -> Row<'static> {
    let style = detail_focus_style(state, focus);
    Row::new(vec![
        Line::from(format!("{label:<width$}", width = LABEL_WIDTH as usize)).style(style),
        Line::from(value.to_string())
            .style(style)
            .alignment(Alignment::Right),
    ])
}

fn readonly_detail_row(label: &str, value: &str) -> Row<'static> {
    Row::new(vec![
        Line::from(format!("{label:<width$}", width = LABEL_WIDTH as usize)),
        Line::from(value.to_string()).alignment(Alignment::Right),
    ])
}

fn detail_focus_style(state: &ConnectRemoteHostState, focus: Focus) -> Style {
    if state.focus == focus {
        active_focus_style()
    } else {
        Style::default()
    }
}

fn password_row(label: &str, field: PasswordField, state: &ConnectRemoteHostState) -> Row<'static> {
    Row::new(vec![
        Line::from(format!("{label:<width$}", width = LABEL_WIDTH as usize)),
        password_control_line(field, state).alignment(Alignment::Right),
    ])
}

fn password_control_line(field: PasswordField, state: &ConnectRemoteHostState) -> Line<'static> {
    let value = password_control_value(field, state);
    let value_style = if state.focus == password_field_focus(field) {
        active_focus_style()
    } else {
        Style::default()
    };
    Line::from(vec![Span::styled(value, value_style)])
}

fn password_field_focus(field: PasswordField) -> Focus {
    match field {
        PasswordField::Ssh => Focus::Password,
        PasswordField::Sudo => Focus::Sudo,
    }
}

fn password_mask_preserve_length(state: &ConnectRemoteHostState, field: PasswordField) -> bool {
    matches!(
        (field, state.editing),
        (PasswordField::Ssh, Some(EditField::SshPassword))
            | (PasswordField::Sudo, Some(EditField::SudoPassword))
    )
}

fn password_control_value(field: PasswordField, state: &ConnectRemoteHostState) -> String {
    match field {
        PasswordField::Ssh if state.auth == AuthChoice::Password => password_field_display(
            &state.ssh_password,
            state.password_mode == PasswordMode::Loading,
            state.show_ssh_password,
            PASSWORD_EMPTY_PLACEHOLDER,
            password_mask_preserve_length(state, PasswordField::Ssh),
        ),
        PasswordField::Sudo if state.sudo_mode != SudoMode::None => password_field_display(
            sudo_password_value(state),
            state.sudo_mode == SudoMode::Loading,
            state.show_sudo_password,
            PASSWORD_EMPTY_PLACEHOLDER,
            password_mask_preserve_length(state, PasswordField::Sudo),
        ),
        PasswordField::Sudo => "No sudo password".to_string(),
        PasswordField::Ssh => password_display(state),
    }
}

fn choice_row(
    label: &str,
    value: Vec<ChoiceSegment>,
    state: &ConnectRemoteHostState,
    focus: Focus,
) -> Row<'static> {
    let focused = state.focus == focus;
    let label_style = if focused {
        active_focus_style()
    } else {
        Style::default()
    };
    Row::new(vec![
        Line::from(format!("{label:<width$}", width = LABEL_WIDTH as usize)).style(label_style),
        choice_line(value, focused).alignment(Alignment::Right),
    ])
}

fn icon_detail_row(
    icon: &str,
    label: &str,
    value: &str,
    state: &ConnectRemoteHostState,
    focus: Focus,
) -> Row<'static> {
    detail_row(&format!("{icon}  {label}"), value, state, focus)
}

fn readonly_icon_detail_row(icon: &str, label: &str, value: &str) -> Row<'static> {
    readonly_detail_row(&format!("{icon}  {label}"), value)
}

fn icon_password_row(
    icon: &str,
    label: &str,
    field: PasswordField,
    state: &ConnectRemoteHostState,
) -> Row<'static> {
    password_row(&format!("{icon}  {label}"), field, state)
}

fn icon_choice_row(
    icon: &str,
    label: &str,
    value: Vec<ChoiceSegment>,
    state: &ConnectRemoteHostState,
    focus: Focus,
) -> Row<'static> {
    choice_row(&format!("{icon}  {label}"), value, state, focus)
}

fn active_focus_style() -> Style {
    Style::default()
        .bg(Color::Blue)
        .fg(Color::White)
        .add_modifier(Modifier::BOLD)
}

fn delete_focus_style() -> Style {
    Style::default()
        .bg(Color::Red)
        .fg(Color::White)
        .add_modifier(Modifier::BOLD)
}

fn selected_host_style() -> Style {
    Style::default().bg(Color::Gray).fg(Color::Black)
}

fn inactive_selected_style() -> Style {
    selected_host_style()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ChoiceSegment {
    label: &'static str,
    selected: bool,
}

fn auth_tabs(state: &ConnectRemoteHostState) -> Vec<ChoiceSegment> {
    vec![
        ChoiceSegment {
            label: "Password",
            selected: state.auth == AuthChoice::Password,
        },
        ChoiceSegment {
            label: "Key",
            selected: state.auth == AuthChoice::Key,
        },
    ]
}

fn host_kind_tabs(state: &ConnectRemoteHostState) -> Vec<ChoiceSegment> {
    vec![
        ChoiceSegment {
            label: RemoteHostKind::Lan.label(),
            selected: state.host_kind == RemoteHostKind::Lan,
        },
        ChoiceSegment {
            label: RemoteHostKind::Cloud.label(),
            selected: state.host_kind == RemoteHostKind::Cloud,
        },
    ]
}

fn via_tabs(state: &ConnectRemoteHostState) -> Vec<ChoiceSegment> {
    vec![
        ChoiceSegment {
            label: RemoteNodeVia::Auto.label(),
            selected: state.via == RemoteNodeVia::Auto,
        },
        ChoiceSegment {
            label: RemoteNodeVia::Direct.label(),
            selected: state.via == RemoteNodeVia::Direct,
        },
        ChoiceSegment {
            label: RemoteNodeVia::Relay.label(),
            selected: state.via == RemoteNodeVia::Relay,
        },
    ]
}

fn choice_line(segments: Vec<ChoiceSegment>, focused: bool) -> Line<'static> {
    let mut spans = Vec::new();
    for (index, segment) in segments.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("  "));
        }
        let style = if segment.selected && focused {
            active_focus_style()
        } else if segment.selected {
            inactive_selected_style()
        } else {
            Style::default()
        };
        spans.push(Span::styled(segment.label, style));
    }
    Line::from(spans)
}

#[cfg(test)]
fn segmented_for_test(segments: &[ChoiceSegment]) -> String {
    segments
        .iter()
        .map(|segment| segment.label)
        .collect::<Vec<_>>()
        .join("  ")
}

const HOST_EMPTY_PLACEHOLDER: &str = "__________";
const PASSWORD_EMPTY_PLACEHOLDER: &str = "________";

fn host_display(state: &ConnectRemoteHostState) -> String {
    if state.host.is_empty() {
        HOST_EMPTY_PLACEHOLDER.to_string()
    } else if state_ssh_port(state) == 22 {
        state.host.clone()
    } else {
        format!("{}:{}", state.host, state_ssh_port(state))
    }
}

fn password_display(state: &ConnectRemoteHostState) -> String {
    match state.auth {
        AuthChoice::Password => password_field_display(
            &state.ssh_password,
            state.password_mode == PasswordMode::Loading,
            state.show_ssh_password,
            PASSWORD_EMPTY_PLACEHOLDER,
            password_mask_preserve_length(state, PasswordField::Ssh),
        ),
        AuthChoice::Key => {
            if state.key_path.is_empty() {
                "Key file path".to_string()
            } else {
                state.key_path.clone()
            }
        }
    }
}

fn sudo_password_display(state: &ConnectRemoteHostState) -> String {
    if state.sudo_mode == SudoMode::None {
        return "No sudo password".to_string();
    }
    password_field_display(
        sudo_password_value(state),
        state.sudo_mode == SudoMode::Loading,
        state.show_sudo_password,
        PASSWORD_EMPTY_PLACEHOLDER,
        password_mask_preserve_length(state, PasswordField::Sudo),
    )
}

fn password_field_display(
    value: &str,
    loading: bool,
    show_plaintext: bool,
    empty_label: &str,
    preserve_mask_length: bool,
) -> String {
    if loading {
        "Loading saved...".to_string()
    } else if value.is_empty() {
        empty_label.to_string()
    } else if show_plaintext {
        value.to_string()
    } else {
        password_mask(value, preserve_mask_length)
    }
}

fn password_mask(value: &str, preserve_length: bool) -> String {
    let len = value.chars().count();
    "*".repeat(if preserve_length { len } else { len.max(6) })
}

fn sudo_password_value(state: &ConnectRemoteHostState) -> &str {
    match state.sudo_mode {
        SudoMode::SameAsSsh => &state.ssh_password,
        SudoMode::Saved | SudoMode::Replace => &state.sudo_password,
        SudoMode::Loading | SudoMode::None => "",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PasswordField {
    Ssh,
    Sudo,
}

#[cfg(test)]
fn delete_label(_state: &ConnectRemoteHostState) -> String {
    "Delete".to_string()
}

fn connect_label(state: &ConnectRemoteHostState) -> String {
    let label = if matches!(state.status, Status::Working(_)) {
        "Connect"
    } else if state.credentials_loading() {
        "Loading..."
    } else {
        "Connect"
    };
    label.to_string()
}

fn render_status(frame: &mut Frame<'_>, area: Rect, state: &ConnectRemoteHostState) {
    let color = match &state.status {
        Status::Hint(_) | Status::Loading(_) | Status::Working(_) | Status::Error(_) => {
            Color::DarkGray
        }
    };
    let message = match &state.status {
        Status::Error(_) => "Press Enter to close the error message.",
        _ => status_message(state),
    };
    frame.render_widget(
        Paragraph::new(message)
            .style(Style::default().fg(color))
            .alignment(Alignment::Left),
        area,
    );
}

fn render_dim_overlay(frame: &mut Frame<'_>) {
    // Darken the area behind a modal popup so the modal stands out the same
    // way the main Connect Remote Host popup stands out against the workspace.
    frame.render_widget(
        Paragraph::new("").style(
            Style::default()
                .bg(Color::Black)
                .add_modifier(Modifier::DIM),
        ),
        frame.size(),
    );
}

fn render_connecting_popup(frame: &mut Frame<'_>, state: &ConnectRemoteHostState) {
    let Status::Working(message) = &state.status else {
        return;
    };
    render_dim_overlay(frame);
    let geometry =
        ConnectingGeometry::from_terminal_size((frame.size().width, frame.size().height));
    frame.render_widget(Clear, geometry.dialog);
    let block = Block::default()
        .title(modal_title("Connecting"))
        .borders(Borders::ALL);
    frame.render_widget(block, geometry.dialog);
    frame.render_widget(
        Paragraph::new(message.as_str())
            .style(Style::default().fg(Color::White))
            .alignment(Alignment::Center),
        geometry.message,
    );
}

fn render_connect_error_popup(frame: &mut Frame<'_>, state: &ConnectRemoteHostState) {
    let Status::Error(message) = &state.status else {
        return;
    };
    render_dim_overlay(frame);
    let geometry =
        ConnectErrorGeometry::from_terminal_size((frame.size().width, frame.size().height));
    frame.render_widget(Clear, geometry.dialog);
    let block = Block::default()
        .title(modal_title("Connect failed"))
        .borders(Borders::ALL);
    frame.render_widget(block, geometry.dialog);
    frame.render_widget(
        Paragraph::new(message.as_str())
            .style(Style::default().fg(Color::White))
            .wrap(Wrap { trim: false })
            .alignment(Alignment::Left),
        geometry.message,
    );
    render_modal_button(frame, geometry.ok_button, "OK", true, false);
}

fn render_delete_confirm(frame: &mut Frame<'_>, state: &ConnectRemoteHostState) {
    let DeleteConfirmState::Prompt {
        profile_label,
        focus,
        ..
    } = &state.delete_confirm
    else {
        return;
    };
    render_dim_overlay(frame);
    let geometry =
        DeleteConfirmGeometry::from_terminal_size((frame.size().width, frame.size().height));
    frame.render_widget(Clear, geometry.dialog);
    let block = Block::default()
        .title(modal_title("Delete saved host"))
        .borders(Borders::ALL);
    frame.render_widget(block, geometry.dialog);
    let message_area = Rect::new(
        geometry.dialog.x.saturating_add(2),
        geometry.dialog.y.saturating_add(2),
        geometry.dialog.width.saturating_sub(4),
        2,
    );
    frame.render_widget(
        Paragraph::new(format!("Delete saved host {profile_label}?"))
            .style(Style::default().fg(Color::White))
            .alignment(Alignment::Left),
        message_area,
    );
    render_modal_button(
        frame,
        geometry.cancel_button,
        "Cancel",
        *focus == DeleteConfirmFocus::Cancel,
        false,
    );
    render_modal_button(
        frame,
        geometry.delete_button,
        "Delete",
        *focus == DeleteConfirmFocus::Delete,
        true,
    );
}

fn render_relay_mismatch(frame: &mut Frame<'_>, state: &ConnectRemoteHostState) {
    let RelayMismatchState::Prompt { message, focus } = &state.relay_mismatch else {
        return;
    };
    render_dim_overlay(frame);
    let geometry =
        RelayMismatchGeometry::from_terminal_size((frame.size().width, frame.size().height));
    frame.render_widget(Clear, geometry.dialog);
    let block = Block::default()
        .title(modal_title("Relay pin mismatch"))
        .borders(Borders::ALL);
    frame.render_widget(block, geometry.dialog);
    frame.render_widget(
        Paragraph::new(message.as_str())
            .style(Style::default().fg(Color::White))
            .wrap(Wrap { trim: false })
            .alignment(Alignment::Left),
        geometry.message,
    );
    render_modal_button(
        frame,
        geometry.abort_button,
        "Abort",
        *focus == RelayMismatchFocus::Abort,
        false,
    );
    render_modal_button(
        frame,
        geometry.switch_button,
        "Switch anyway",
        *focus == RelayMismatchFocus::SwitchAnyway,
        true,
    );
}

fn render_modal_button(
    frame: &mut Frame<'_>,
    area: Rect,
    label: &str,
    focused: bool,
    destructive: bool,
) {
    let style = if focused {
        if destructive {
            delete_focus_style()
        } else {
            active_focus_style()
        }
    } else if destructive {
        Style::default().fg(Color::Red)
    } else {
        Style::default()
    };
    frame.render_widget(
        Paragraph::new(label.to_string())
            .style(style)
            .alignment(Alignment::Center),
        area,
    );
}

fn render_cursor(frame: &mut Frame<'_>, details: Rect, state: &ConnectRemoteHostState) {
    if let Some((x, y)) = cursor_position(details, state) {
        frame.set_cursor(x, y);
    }
}

fn cursor_position(details: Rect, state: &ConnectRemoteHostState) -> Option<(u16, u16)> {
    let field = state.editing?;
    if state.selected_proxy_config() {
        let rows = ProxyDetailsGeometry::from_area(details).rows;
        let row = match field {
            EditField::ProxyName => rows.name,
            EditField::AllProxy => rows.all_proxy,
            EditField::HttpsProxy => rows.https_proxy,
            _ => return None,
        };
        let value_area_x = details.x.saturating_add(PROXY_VALUE_START);
        let value_area_width = details.width.saturating_sub(PROXY_VALUE_START);
        let desired_x = right_aligned_cursor_x(
            value_area_x,
            value_area_width,
            &edit_field_display_text(state, field),
            state.edit_cursor,
        );
        let max_x = details.x.saturating_add(details.width.saturating_sub(1));
        return Some((desired_x.min(max_x), details.y.saturating_add(row)));
    }
    if state.selected_relay_config() {
        let rows = RelayDetailsGeometry::from_area(details).rows;
        let row = match field {
            EditField::RelayAddress => rows.address,
            EditField::RelayToken => rows.token,
            _ => return None,
        };
        let value_area_x = details.x.saturating_add(DETAIL_VALUE_START);
        let value_area_width = details.width.saturating_sub(DETAIL_VALUE_START);
        let desired_x = right_aligned_cursor_x(
            value_area_x,
            value_area_width,
            &edit_field_display_text(state, field),
            state.edit_cursor,
        );
        let max_x = details.x.saturating_add(details.width.saturating_sub(1));
        return Some((desired_x.min(max_x), details.y.saturating_add(row)));
    }
    let rows = DetailsGeometry::from_area(details, state).rows;
    let row = match field {
        EditField::Host => rows.host,
        EditField::RemotePort => rows.port,
        EditField::SshUser => rows.user,
        EditField::KeyPath | EditField::SshPassword => rows.password,
        EditField::SudoPassword => rows.sudo,
        EditField::ProxyName
        | EditField::AllProxy
        | EditField::HttpsProxy
        | EditField::RelayAddress
        | EditField::RelayToken => return None,
    };
    let value_area_x = details.x.saturating_add(DETAIL_VALUE_START);
    let value_area_width = details.width.saturating_sub(DETAIL_VALUE_START);
    let desired_x = right_aligned_cursor_x(
        value_area_x,
        value_area_width,
        &edit_field_display_text(state, field),
        state.edit_cursor,
    );
    let max_x = details.x.saturating_add(details.width.saturating_sub(1));
    Some((desired_x.min(max_x), details.y.saturating_add(row)))
}

fn edit_field_display_text(state: &ConnectRemoteHostState, field: EditField) -> String {
    match field {
        EditField::Host => host_display(state),
        EditField::RemotePort => state.remote_port_preference.clone(),
        EditField::SshUser => state.ssh_user.clone(),
        EditField::KeyPath | EditField::SshPassword => password_display(state),
        EditField::SudoPassword => sudo_password_display(state),
        EditField::ProxyName => state.proxy_draft.name.clone(),
        EditField::AllProxy => proxy_input_display(&state.proxy_draft.all_proxy),
        EditField::HttpsProxy => proxy_input_display(&state.proxy_draft.https_proxy),
        EditField::RelayAddress => relay_input_display(&state.relay_draft_address),
        EditField::RelayToken => relay_input_display(&state.relay_draft_token),
    }
}

fn right_aligned_cursor_x(
    value_area_x: u16,
    value_area_width: u16,
    display_text: &str,
    cursor_chars: usize,
) -> u16 {
    let text_width = display_text.width() as u16;
    let value_area_right = value_area_x
        .saturating_add(value_area_width)
        .saturating_sub(1);
    let text_start = value_area_right
        .saturating_sub(text_width.saturating_sub(1))
        .max(value_area_x);
    text_start
        .saturating_add(cursor_chars as u16)
        .min(value_area_right)
}

fn point_in_rect(x: u16, y: u16, rect: Rect) -> bool {
    x >= rect.x && x < rect.x + rect.width && y >= rect.y && y < rect.y + rect.height
}

fn shift_value<T: Copy + Eq>(values: &[T], current: T, step: i32) -> T {
    if values.is_empty() {
        return current;
    }
    let index = values
        .iter()
        .position(|value| *value == current)
        .unwrap_or(0) as i32;
    let len = values.len() as i32;
    let shifted = (index + step).rem_euclid(len) as usize;
    values[shifted]
}

fn is_backspace_key(code: KeyCode, modifiers: KeyModifiers) -> bool {
    code == KeyCode::Backspace
        || matches!(code, KeyCode::Char('h') if modifiers.contains(KeyModifiers::CONTROL))
        || matches!(code, KeyCode::Char('\u{7f}'))
}

fn proxy_host_part(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let value = value
        .strip_prefix("http://")
        .or_else(|| value.strip_prefix("https://"))
        .or_else(|| value.strip_prefix("socks5://"))
        .or_else(|| value.strip_prefix("socks5h://"))
        .unwrap_or(value);
    if let Some(stripped) = value.strip_prefix('[') {
        return stripped
            .split_once(']')
            .map(|(host, rest)| format!("[{host}]{rest}"));
    }
    let authority = value.split('/').next().unwrap_or(value);
    (!authority.trim().is_empty()).then(|| authority.to_string())
}

fn proxy_profile_name(profile: &RemoteInstallProxyProfile) -> String {
    let name = profile.name.trim();
    if !name.is_empty() && name != "Default" && name != "Proxy" && name != "New Proxy" {
        return name.to_string();
    }
    proxy_host_part(&profile.all_proxy)
        .or_else(|| proxy_host_part(&profile.https_proxy))
        .unwrap_or_else(|| name.to_string())
}

fn edit_buffer(state: &mut ConnectRemoteHostState, field: EditField) -> &mut String {
    match field {
        EditField::Host => &mut state.host,
        EditField::RemotePort => &mut state.remote_port_preference,
        EditField::SshUser => &mut state.ssh_user,
        EditField::KeyPath => &mut state.key_path,
        EditField::SshPassword => &mut state.ssh_password,
        EditField::SudoPassword => &mut state.sudo_password,
        EditField::ProxyName => &mut state.proxy_draft.name,
        EditField::AllProxy => &mut state.proxy_draft.all_proxy,
        EditField::HttpsProxy => &mut state.proxy_draft.https_proxy,
        EditField::RelayAddress => &mut state.relay_draft_address,
        EditField::RelayToken => &mut state.relay_draft_token,
    }
}

fn edit_buffer_ref(state: &ConnectRemoteHostState, field: EditField) -> &str {
    match field {
        EditField::Host => &state.host,
        EditField::RemotePort => &state.remote_port_preference,
        EditField::SshUser => &state.ssh_user,
        EditField::KeyPath => &state.key_path,
        EditField::SshPassword => &state.ssh_password,
        EditField::SudoPassword => &state.sudo_password,
        EditField::ProxyName => &state.proxy_draft.name,
        EditField::AllProxy => &state.proxy_draft.all_proxy,
        EditField::HttpsProxy => &state.proxy_draft.https_proxy,
        EditField::RelayAddress => &state.relay_draft_address,
        EditField::RelayToken => &state.relay_draft_token,
    }
}

fn char_to_byte_index(value: &str, char_index: usize) -> usize {
    value
        .char_indices()
        .map(|(index, _)| index)
        .nth(char_index)
        .unwrap_or(value.len())
}

fn edit_focus(field: EditField) -> Focus {
    match field {
        EditField::Host => Focus::Host,
        EditField::RemotePort => Focus::Port,
        EditField::SshUser => Focus::User,
        EditField::KeyPath | EditField::SshPassword => Focus::Password,
        EditField::SudoPassword => Focus::Sudo,
        EditField::ProxyName => Focus::ProxyName,
        EditField::AllProxy => Focus::AllProxy,
        EditField::HttpsProxy => Focus::HttpsProxy,
        EditField::RelayAddress => Focus::RelayAddress,
        EditField::RelayToken => Focus::RelayToken,
    }
}

fn spawn_secret_loader(request: SecretLoadRequest, tx: CrossbeamSender<SecretLoadResult>) {
    std::thread::spawn(move || {
        let ssh = request.ssh_secret_id.as_ref().map(load_secret_value);
        let sudo = request.sudo_secret_id.as_ref().map(load_secret_value);
        let _ = tx.send(SecretLoadResult {
            id: request.id,
            selected: request.selected,
            ssh,
            sudo,
        });
    });
}

fn load_proxy_settings() -> RemoteInstallProxySettings {
    RemoteInstallProxyStore::default()
        .load_settings()
        .unwrap_or_default()
}

/// Reads the pinned relay for the sidebar listing. An unreadable pin is
/// logged and treated as "no relay": enrollment must never be blocked by a
/// stale file (the join path surfaces parse errors with actionable text).
fn load_relay_pin() -> Option<RelayTomlConfig> {
    match RelayTomlConfig::load(&RelayTomlConfig::default_path()) {
        Ok(pin) => pin,
        Err(error) => {
            crate::infra::error_log::ERROR_LOG.log(format!(
                "[connect-popup] relay.toml unreadable; treating as unpinned: {error}"
            ));
            None
        }
    }
}

fn load_profiles() -> Vec<RemoteHostProfile> {
    RemoteHostHistoryStore::new(RemoteHostHistoryStore::default_path())
        .load()
        .map(|history| history.hosts)
        .unwrap_or_default()
}

fn delete_selected_host(
    state: &mut ConnectRemoteHostState,
    profile_name: &str,
) -> Result<Option<SecretLoadRequest>, String> {
    let deleted_index = state
        .profiles
        .iter()
        .position(|profile| profile.name == profile_name)
        .ok_or_else(|| format!("saved host profile `{profile_name}` is no longer selected"))?;
    let history_store = RemoteHostHistoryStore::new(RemoteHostHistoryStore::default_path());
    let removed = history_store
        .remove_profile(profile_name)
        .map_err(|error| error.to_string())?;
    let Some(removed) = removed else {
        state.delete_confirm = DeleteConfirmState::Idle;
        return Err(format!("saved host profile `{profile_name}` was not found"));
    };

    let secret_store = KeyringRemoteHostSecretStore;
    let mut delete_errors = Vec::new();
    if let RemoteHostAuthProfile::Password {
        password_secret_id: Some(id),
    } = &removed.auth
    {
        if let Err(error) = secret_store.delete_secret(id) {
            delete_errors.push(format!("SSH password: {error}"));
        }
    }
    if let Some(id) = &removed.sudo_password_secret_id {
        if let Err(error) = secret_store.delete_secret(id) {
            delete_errors.push(format!("sudo password: {error}"));
        }
    }

    let deleted_label = saved_host_label(&removed);
    state.profiles = load_profiles();
    state.selected = deleted_index.min(state.profiles.len());
    let request = state.sync_selected_profile();
    if delete_errors.is_empty() {
        state.status = Status::Hint(format!("Deleted saved host {deleted_label}."));
    } else {
        state.status = Status::Error(format!(
            "Deleted saved host {deleted_label}, but failed to delete secret: {}",
            delete_errors.join("; ")
        ));
    }
    Ok(request)
}

fn ensure_connectable_profile<S: RemoteHostSecretStore>(
    state: &ConnectRemoteHostState,
    secret_store: &S,
    history_store: &RemoteHostHistoryStore,
) -> Result<RemoteHostProfile, String>
where
    S::Error: std::fmt::Display,
{
    if let Some(profile) = state
        .selected_profile()
        .filter(|profile| saved_profile_can_connect_by_id(state, profile))
    {
        return Ok(profile.clone());
    }

    let profile_name = save_profile_name_for_state(state);

    let ssh_password_secret_id = if state.auth == AuthChoice::Password {
        let id = secret_id_for_profile_name(&profile_name, "ssh-password")?;
        secret_store
            .put_secret(&id, RemoteHostSecretValue::new(state.ssh_password.clone()))
            .map_err(|error| format!("failed to save SSH password: {error}"))?;
        Some(id)
    } else {
        None
    };

    let sudo_password_secret_id = match state.sudo_mode {
        SudoMode::None => None,
        SudoMode::SameAsSsh => ssh_password_secret_id.clone(),
        SudoMode::Loading => {
            return Err("Saved credentials are still loading.".to_string());
        }
        SudoMode::Replace | SudoMode::Saved => {
            let id = secret_id_for_profile_name(&profile_name, "sudo-password")?;
            let password = if state.sudo_mode == SudoMode::SameAsSsh {
                state.ssh_password.clone()
            } else {
                state.sudo_password.clone()
            };
            secret_store
                .put_secret(&id, RemoteHostSecretValue::new(password))
                .map_err(|error| format!("failed to save sudo password: {error}"))?;
            Some(id)
        }
    };

    let auth = if state.auth == AuthChoice::Password {
        RemoteHostAuthProfile::Password {
            password_secret_id: ssh_password_secret_id,
        }
    } else {
        RemoteHostAuthProfile::Key {
            key_path: std::path::PathBuf::from(state.key_path.clone()),
        }
    };

    // Preserve the stored connection metadata from the existing profile so that
    // a reuse-dial can still be attempted when credentials are being re-entered.
    // Without this, tls_pin_sha256 is cleared and try_reuse_existing_connection
    // skips the existing remote waitagent, causing a new one to be bootstrapped.
    let (last_remote_port, last_endpoint, tls_pin_sha256) = state
        .selected_profile()
        .map(|profile| {
            (
                profile.last_remote_port,
                profile.last_endpoint.clone(),
                profile.tls_pin_sha256.clone(),
            )
        })
        .unwrap_or((state.last_remote_port, None, None));

    let profile = RemoteHostProfile {
        name: profile_name,
        host: state.host.clone(),
        ssh_user: state.ssh_user.clone(),
        auth,
        sudo_password_secret_id,
        preferred_remote_port: remote_port_preference_from_state(state),
        ssh_port: ssh_port_from_state(state),
        last_remote_port,
        last_endpoint,
        last_connected_at: None,
        use_install_proxy: state.use_install_proxy,
        tls_pin_sha256,
        host_kind: state.host_kind,
        remote_shell: state
            .selected_profile()
            .and_then(|profile| profile.remote_shell),
        // Persist the popup's three-way choice; auto is the default for new
        // entries. The effective-path memory is preserved from the stored
        // profile and only ever updated by the connect runtime after a
        // successful auto connect — never rewritten here (issue #156
        // slice 2).
        via: Some(state.via.as_str().to_string()),
        last_via_used: state
            .selected_profile()
            .and_then(|profile| profile.last_via_used.clone()),
    };

    history_store
        .upsert_profile(profile.clone())
        .map_err(|error| format!("failed to save host profile: {error}"))?;

    Ok(profile)
}

fn run_ratatui_connect(state: &ConnectRemoteHostState, port: u16) -> Result<String, String> {
    validate(state)?;
    let profile = ensure_connectable_profile(
        state,
        &KeyringRemoteHostSecretStore,
        &RemoteHostHistoryStore::new(RemoteHostHistoryStore::default_path()),
    )?;

    // The node server answers CONNECT_REMOTE_HOST only after the full remote
    // bootstrap finishes (SSH probe, waitagent install/start), which routinely
    // exceeds the 2-second default used by quick control commands. The pane
    // shows a modal "Connecting..." while blocked here, matching the
    // `__connect-remote-host` sidecar path, which waits without any timeout.
    const CONNECT_RESPONSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);
    let line = crate::platform::local_ipc::send_node_command_with_timeout(
        port,
        &format!("CONNECT_REMOTE_HOST {}", profile.name),
        CONNECT_RESPONSE_TIMEOUT,
    )
    .map_err(|error| format!("failed to connect to ratatui node on port {port}: {error}"))?;
    let response = line.trim();
    if response.is_empty() {
        return Err("empty response from ratatui node".to_string());
    }
    // The node server wraps command replies in `ServerMessageJson::Response`.
    if let Ok(message) = serde_json::from_str::<ServerMessageJson>(response) {
        match message {
            ServerMessageJson::Response(resp) => {
                if resp.ok {
                    return Ok("Connected. Press Esc to close.".to_string());
                }
                return Err(resp.message.unwrap_or_default());
            }
            ServerMessageJson::Snapshot(_) | ServerMessageJson::History(_) => {
                return Err("unexpected non-response from ratatui node".to_string());
            }
        }
    }
    // Plain-text fallback for older/simple replies.
    if response.starts_with("OK") {
        return Ok("Connected. Press Esc to close.".to_string());
    }
    Err(response
        .strip_prefix("ERR ")
        .unwrap_or(response)
        .to_string())
}

/// What the node answered to a relay-management command. A pin-mismatch
/// refusal is its own variant: the caller must show the explicit
/// Switch-anyway confirmation (default abort) instead of a plain error.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RelayNodeAnswer {
    Ok(String),
    PinMismatch(String),
    Err(String),
}

/// Sends `RELAY_JOIN` over the node control channel and interprets the
/// answer. Enrollment can block on TLS + enrollment IO, so the timeout is
/// generous; the popup shows a modal "Joining relay..." while blocked here.
fn run_relay_join_command(
    port: Option<u16>,
    address: &str,
    token: &str,
    force: bool,
) -> RelayNodeAnswer {
    let Some(port) = port else {
        return RelayNodeAnswer::Err(
            "relay management requires the embedded console (no node socket)".to_string(),
        );
    };
    const JOIN_RESPONSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
    let force_arg = if force { " FORCE" } else { "" };
    let command = format!(
        "RELAY_JOIN {} {}{}",
        general_purpose::STANDARD.encode(address.as_bytes()),
        general_purpose::STANDARD.encode(token.as_bytes()),
        force_arg
    );
    let line = match crate::platform::local_ipc::send_node_command_with_timeout(
        port,
        &command,
        JOIN_RESPONSE_TIMEOUT,
    ) {
        Ok(line) => line,
        Err(error) => return RelayNodeAnswer::Err(error.to_string()),
    };
    relay_node_answer(&line, "pin mismatch:")
}

/// Sends `RELAY_REMOVE` over the node control channel.
fn run_relay_remove_command(port: Option<u16>) -> Result<String, String> {
    let Some(port) = port else {
        return Err("relay management requires the embedded console (no node socket)".to_string());
    };
    const REMOVE_RESPONSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
    let line = crate::platform::local_ipc::send_node_command_with_timeout(
        port,
        "RELAY_REMOVE",
        REMOVE_RESPONSE_TIMEOUT,
    )
    .map_err(|error| error.to_string())?;
    match relay_node_answer(&line, "pin mismatch:") {
        RelayNodeAnswer::Ok(message) => Ok(message),
        RelayNodeAnswer::PinMismatch(message) | RelayNodeAnswer::Err(message) => Err(message),
    }
}

/// Parses the node's reply like `run_ratatui_connect` does; answers whose
/// refusal carries `mismatch_marker` become the explicit-confirmation
/// variant instead of a plain error.
fn relay_node_answer(line: &str, mismatch_marker: &str) -> RelayNodeAnswer {
    let response = line.trim();
    if response.is_empty() {
        return RelayNodeAnswer::Err("empty response from ratatui node".to_string());
    }
    if let Ok(ServerMessageJson::Response(resp)) = serde_json::from_str(response) {
        return match (resp.ok, resp.message) {
            (true, message) => RelayNodeAnswer::Ok(message.unwrap_or_else(|| "ok".to_string())),
            (false, Some(message)) if message.contains(mismatch_marker) => {
                RelayNodeAnswer::PinMismatch(message)
            }
            (false, message) => {
                RelayNodeAnswer::Err(message.unwrap_or_else(|| "command failed".to_string()))
            }
        };
    }
    if response.starts_with("OK") {
        return RelayNodeAnswer::Ok(response.to_string());
    }
    RelayNodeAnswer::Err(
        response
            .strip_prefix("ERR ")
            .unwrap_or(response)
            .to_string(),
    )
}

fn run_connect(
    state: &ConnectRemoteHostState,
    command: &ConnectRemoteHostPaneCommand,
    network: &RemoteNetworkConfig,
    ratatui_port: Option<u16>,
) -> Result<String, String> {
    if let Some(port) = ratatui_port {
        return run_ratatui_connect(state, port);
    }

    validate(state)?;
    let executable = current_waitagent_executable()
        .map_err(|error| error.to_string())?
        .to_string_lossy()
        .into_owned();
    let mut args = vec![
        "__connect-remote-host".to_string(),
        "--current-socket-name".to_string(),
        command.current_socket_name.clone(),
        "--current-session-name".to_string(),
        command.current_session_name.clone(),
    ];
    let mut stdin_payload = None;
    args.push("--use-install-proxy".to_string());
    args.push(state.use_install_proxy.to_string());
    let selected_profile = state.selected_profile();
    if let Some(profile) =
        selected_profile.filter(|profile| saved_profile_can_connect_by_id(state, profile))
    {
        args.push("--profile".to_string());
        args.push(profile.name.clone());
    } else {
        args.extend([
            "--host".to_string(),
            state.host.clone(),
            "--ssh-user".to_string(),
            state.ssh_user.clone(),
            "--host-kind".to_string(),
            state.host_kind.as_str().to_string(),
            "--auth".to_string(),
            state.auth.as_arg().to_string(),
            "--remote-port".to_string(),
            normalized_port(&state.remote_port_preference),
        ]);
        if let Some(port) = ssh_port_from_state(state) {
            args.push("--ssh-port".to_string());
            args.push(port.to_string());
        }
        if state.remember {
            args.push("--save-profile".to_string());
            args.push(save_profile_name_for_state(state));
            if let Some(profile) = selected_profile {
                args.push("--replace-profile".to_string());
                args.push(profile.name.clone());
            }
        }
        match state.auth {
            AuthChoice::Password => match state.password_mode {
                PasswordMode::Loading => {
                    return Err("Saved credentials are still loading.".to_string())
                }
                PasswordMode::Saved => {
                    if let Some(id) = saved_ssh_secret_id(state) {
                        args.push("--ssh-password-secret-id".to_string());
                        args.push(id);
                    }
                }
                PasswordMode::Enter => args.push("--ssh-password-stdin".to_string()),
            },
            AuthChoice::Key => {
                args.push("--key-path".to_string());
                args.push(state.key_path.clone());
            }
        }
        match state.sudo_mode {
            SudoMode::SameAsSsh | SudoMode::Replace => {
                args.push("--sudo-password-stdin".to_string())
            }
            SudoMode::Loading => return Err("Saved credentials are still loading.".to_string()),
            SudoMode::Saved => {
                if let Some(id) = saved_sudo_secret_id(state) {
                    args.push("--sudo-password-secret-id".to_string());
                    args.push(id);
                }
            }
            SudoMode::None => {}
        }
        if state.auth == AuthChoice::Password
            || matches!(state.sudo_mode, SudoMode::SameAsSsh | SudoMode::Replace)
        {
            let ssh = if state.auth == AuthChoice::Password
                && state.password_mode == PasswordMode::Enter
            {
                state.ssh_password.clone()
            } else {
                String::new()
            };
            let sudo = match state.sudo_mode {
                SudoMode::SameAsSsh => state.ssh_password.clone(),
                SudoMode::Replace => state.sudo_password.clone(),
                SudoMode::Loading => return Err("Saved credentials are still loading.".to_string()),
                _ => String::new(),
            };
            stdin_payload = Some(format!("{ssh}\n{sudo}\n"));
        }
    }
    let args = prepend_global_network_args(args, network);
    let mut child = Command::new(executable)
        .args(args)
        .stdin(if stdin_payload.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| error.to_string())?;
    if let Some(payload) = stdin_payload {
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(payload.as_bytes())
                .map_err(|error| error.to_string())?;
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|error| error.to_string())?;
    if output.status.success() {
        Ok("Connected. Press Esc to close.".to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let detail = if !stderr.trim().is_empty() {
            stderr.trim()
        } else {
            stdout.trim()
        };
        Err(format!(
            "Connect failed: {}{}",
            output.status,
            if detail.is_empty() {
                String::new()
            } else {
                format!(" - {detail}")
            }
        ))
    }
}

fn saved_profile_can_connect_by_id(
    state: &ConnectRemoteHostState,
    profile: &RemoteHostProfile,
) -> bool {
    let auth_ready = match &profile.auth {
        RemoteHostAuthProfile::Password { .. } => state.password_mode == PasswordMode::Saved,
        RemoteHostAuthProfile::Key { .. } => true,
    };
    auth_ready
        && matches!(state.sudo_mode, SudoMode::Saved | SudoMode::None)
        && profile_matches_state(profile, state)
}

fn save_profile_name_for_state(state: &ConnectRemoteHostState) -> String {
    let default_name = default_profile_name_for(&state.ssh_user, &state.host);
    let Some(profile) = state.selected_profile() else {
        return default_name;
    };
    let previous_default_name = default_profile_name_for(&profile.ssh_user, &profile.host);
    if profile.name == previous_default_name {
        default_name
    } else {
        profile.name.clone()
    }
}

fn default_profile_name_for(ssh_user: &str, host: &str) -> String {
    format!("{ssh_user}@{host}")
}

fn secret_id_for_profile_name(
    profile_name: &str,
    purpose: &str,
) -> Result<RemoteHostSecretId, String> {
    let segment = profile_name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    let collapsed = segment
        .split('-')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let segment = if collapsed.is_empty() {
        "remote".to_string()
    } else {
        collapsed
    };
    RemoteHostSecretId::new(format!("waitagent.remote-host.{segment}.{purpose}"))
        .map_err(|error| error.to_string())
}

/// Port the remote `sshd` listens on as entered in the pane; defaults to 22
/// when the field is empty or unparsable.
fn state_ssh_port(state: &ConnectRemoteHostState) -> u16 {
    let trimmed = state.ssh_port.trim();
    if trimmed.is_empty() {
        return 22;
    }
    trimmed.parse::<u16>().unwrap_or(22)
}

/// SSH port to persist on the profile: `None` unless the user set a
/// non-default port, keeping the history file free of noise.
fn ssh_port_from_state(state: &ConnectRemoteHostState) -> Option<u16> {
    let port = state_ssh_port(state);
    (port != 22).then_some(port)
}

fn remote_port_preference_from_state(state: &ConnectRemoteHostState) -> RemotePortPreference {
    let trimmed = state.remote_port_preference.trim();
    if trimmed.is_empty() || trimmed == "auto" {
        RemotePortPreference::Auto
    } else {
        trimmed
            .parse::<u16>()
            .map(RemotePortPreference::Port)
            .unwrap_or(RemotePortPreference::Auto)
    }
}

fn profile_matches_state(profile: &RemoteHostProfile, state: &ConnectRemoteHostState) -> bool {
    profile.host == state.host
        && profile.ssh_user == state.ssh_user
        && profile.ssh_port() == state_ssh_port(state)
        && normalized_port_matches_profile(&state.remote_port_preference, profile)
        && profile.host_kind == state.host_kind
        && profile.via() == state.via
        && auth_matches_state(&profile.auth, state)
        && profile.use_install_proxy == state.use_install_proxy
}

fn normalized_port_matches_profile(value: &str, profile: &RemoteHostProfile) -> bool {
    normalized_port(value) == profile_preferred_port(profile)
}

fn profile_preferred_port(profile: &RemoteHostProfile) -> String {
    match profile.preferred_remote_port {
        RemotePortPreference::Auto => "auto".to_string(),
        RemotePortPreference::Port(port) => port.to_string(),
    }
}

fn auth_matches_state(auth: &RemoteHostAuthProfile, state: &ConnectRemoteHostState) -> bool {
    match (auth, state.auth) {
        (RemoteHostAuthProfile::Password { .. }, AuthChoice::Password) => true,
        (RemoteHostAuthProfile::Key { key_path }, AuthChoice::Key) => {
            key_path.to_string_lossy() == state.key_path
        }
        _ => false,
    }
}

fn validate(state: &ConnectRemoteHostState) -> Result<(), String> {
    if state.credentials_loading() {
        return Err("Saved credentials are still loading.".to_string());
    }
    if state.host.trim().is_empty() {
        return Err("Host is required.".to_string());
    }
    if state.ssh_user.trim().is_empty() {
        return Err("SSH user is required.".to_string());
    }
    if state.host_kind == RemoteHostKind::Cloud && state.auth != AuthChoice::Key {
        return Err("Cloud hosts require key authentication.".to_string());
    }
    if state.auth == AuthChoice::Password
        && state.password_mode == PasswordMode::Enter
        && state.ssh_password.is_empty()
    {
        return Err("SSH password is required.".to_string());
    }
    if state.auth == AuthChoice::Key && state.key_path.trim().is_empty() {
        return Err("Key path is required.".to_string());
    }
    if state.sudo_mode == SudoMode::Replace && state.sudo_password.is_empty() {
        return Err("Sudo password is required.".to_string());
    }
    Ok(())
}

fn normalized_port(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        "auto".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Split a `host:port` / `[ipv6]:port` string into host and port.
///
/// Returns `None` when the input has no port suffix, so plain hostnames and
/// bare IPv6 addresses pass through unchanged. The port must be all digits
/// and fit in `u16`; anything else is treated as part of the host.
fn split_host_port(raw: &str) -> Option<(String, u16)> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let (host, port) = if let Some(rest) = trimmed.strip_prefix('[') {
        let (host, port) = rest.split_once("]:")?;
        (host, port)
    } else {
        let (host, port) = trimmed.rsplit_once(':')?;
        if host.contains(':') {
            // Bare IPv6 address without brackets; the colons belong to the
            // address, not a port separator.
            return None;
        }
        (host, port)
    };
    if host.is_empty() || !port.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    let port = port.parse::<u16>().ok()?;
    Some((host.to_string(), port))
}

fn saved_ssh_secret_id(state: &ConnectRemoteHostState) -> Option<String> {
    match state.selected_profile().map(|profile| &profile.auth) {
        Some(RemoteHostAuthProfile::Password {
            password_secret_id: Some(id),
        }) => Some(id.as_str().to_string()),
        _ => None,
    }
}

fn saved_sudo_secret_id(state: &ConnectRemoteHostState) -> Option<String> {
    state
        .selected_profile()?
        .sudo_password_secret_id
        .as_ref()
        .map(|id| id.as_str().to_string())
}

fn load_secret_value(
    id: &crate::host::ssh::remote_host_secret_store::RemoteHostSecretId,
) -> Result<String, String> {
    KeyringRemoteHostSecretStore
        .get_secret(id)
        .map_err(|error| error.to_string())?
        .map(|value| value.expose_secret().to_string())
        .ok_or_else(|| "saved secret is missing".to_string())
}

fn write_error(error: io::Error) -> LifecycleError {
    LifecycleError::Io(
        "failed to render connect remote host popup".to_string(),
        error,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use unicode_width::UnicodeWidthStr;

    fn display_width(text: &str) -> usize {
        UnicodeWidthStr::width(text)
    }

    fn saved_password_profile() -> RemoteHostProfile {
        RemoteHostProfile {
            name: "k.0.0.1".to_string(),
            host: "127.0.0.1".to_string(),
            ssh_user: "k".to_string(),
            auth: RemoteHostAuthProfile::Password {
                password_secret_id: None,
            },
            sudo_password_secret_id: None,
            preferred_remote_port: RemotePortPreference::Auto,
            ssh_port: None,
            last_remote_port: Some(7575),
            last_endpoint: None,
            last_connected_at: None,
            use_install_proxy: true,
            ..RemoteHostProfile::default()
        }
    }

    fn saved_key_profile() -> RemoteHostProfile {
        RemoteHostProfile {
            name: "k@127.0.0.1".to_string(),
            host: "127.0.0.1".to_string(),
            ssh_user: "k".to_string(),
            auth: RemoteHostAuthProfile::Key {
                key_path: std::path::PathBuf::from("~/.ssh/id_rsa"),
            },
            sudo_password_secret_id: None,
            preferred_remote_port: RemotePortPreference::Auto,
            ssh_port: None,
            last_remote_port: Some(7575),
            last_endpoint: None,
            last_connected_at: None,
            use_install_proxy: true,
            ..RemoteHostProfile::default()
        }
    }

    fn blank_proxy_profile() -> RemoteInstallProxyProfile {
        RemoteInstallProxyProfile {
            name: String::new(),
            all_proxy: String::new(),
            https_proxy: String::new(),
        }
    }

    fn rendered_text(width: u16, height: u16, state: &ConnectRemoteHostState) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, state)).unwrap();
        let buffer = terminal.backend().buffer();
        let mut output = String::new();
        for y in 0..height {
            for x in 0..width {
                output.push_str(buffer.get(x, y).symbol());
            }
            output.push('\n');
        }
        output
    }

    #[test]
    fn proxy_configuration_save_is_centered_in_detail_area() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles = vec![saved_password_profile()];
        state.selected = state.proxy_selection_index();
        let popup = PopupGeometry::from_terminal_size((100, 26), &state);
        let details = ProxyDetailsGeometry::from_area(popup.details);
        let output = rendered_text(100, 26, &state);
        let save_row = output
            .lines()
            .nth(details.buttons.y as usize)
            .expect("save row should render");
        let save_col = save_row
            .find("Save")
            .map(|index| display_width(&save_row[..index]))
            .expect("Save should render") as u16;

        assert!(save_col > details.buttons.x + 8);
        assert!(save_col + 4 < details.buttons.x + details.buttons.width);
    }

    #[test]
    fn proxy_details_geometry_follows_host_page_layout() {
        let state = ConnectRemoteHostState::load();
        let popup = PopupGeometry::from_terminal_size((100, 30), &state);
        let details = ProxyDetailsGeometry::from_area(popup.details);

        assert_eq!(details.header.height, 3);
        assert_eq!(details.proxy.height, 5);
        assert_eq!(details.no_proxy.height, 3);
        assert_eq!(details.info.height, 3);
        assert_eq!(details.buttons.height, 1);
        assert_eq!(details.hint.height, 1);
        assert_eq!(details.hint.y, details.buttons.y + 1);
        assert!(details.status.y > details.hint.y);
        assert_eq!(
            details.rows.action,
            details.buttons.y.saturating_sub(popup.details.y)
        );
    }

    fn proxy_settings_fixture() -> RemoteInstallProxySettings {
        RemoteInstallProxySettings {
            active: Some("Office".to_string()),
            profiles: vec![
                RemoteInstallProxyProfile {
                    name: "Home".to_string(),
                    all_proxy: "socks5://192.168.31.1:7897".to_string(),
                    https_proxy: "http://192.168.31.1:7897".to_string(),
                },
                RemoteInstallProxyProfile {
                    name: "Office".to_string(),
                    all_proxy: "socks5://127.0.0.1:7897".to_string(),
                    https_proxy: "http://127.0.0.1:7897".to_string(),
                },
            ],
        }
    }

    #[test]
    fn proxy_details_match_host_page_skeleton_for_saved_profile() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles = vec![saved_password_profile()];
        state.proxy_settings = proxy_settings_fixture();
        state.selected = state.proxy_profile_selection_start() + 1;
        state.sync_selected_proxy();
        state.set_focus(Focus::ProxyName);

        let popup = PopupGeometry::from_terminal_size((100, 30), &state);
        let details = ProxyDetailsGeometry::from_area(popup.details);
        let output = rendered_text(100, 30, &state);

        let header_row = output
            .lines()
            .nth(details.header.y as usize + 1)
            .expect("proxy header row renders");
        assert!(
            header_row.contains('⛓') && header_row.contains("Office"),
            "header shows the proxy identity: {header_row}"
        );
        assert!(
            header_row.contains("Saved"),
            "saved profile badge renders: {header_row}"
        );
        assert!(
            header_row.contains('★'),
            "active profile shows the yellow star: {header_row}"
        );

        assert!(output.contains("all_proxy"), "proxy card row renders");
        assert!(output.contains("https_proxy"), "proxy card row renders");
        assert!(
            output.contains("no_proxy"),
            "no proxy card row renders: {output}"
        );
        assert!(
            output.contains("auto:"),
            "no proxy auto-computed value renders: {output}"
        );
        assert!(
            output.contains("ⓘ Environment used when the remote host downloads"),
            "info box renders: {output}"
        );
        assert!(output.contains("Active"), "active button renders");
        assert!(output.contains("Save"), "save button renders");
        assert!(output.contains("Delete"), "delete button renders");

        let hint_row = output
            .lines()
            .nth(details.hint.y as usize)
            .expect("proxy hint row renders");
        assert!(
            hint_row.contains(&bottom_hint_text(&state)),
            "proxy hint arm renders on the proxy page: {hint_row}"
        );
    }

    #[test]
    fn proxy_details_match_host_page_skeleton_for_new_proxy_draft() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles = vec![saved_password_profile()];
        state.proxy_settings = proxy_settings_fixture();
        state.selected = state.new_proxy_selection_index();
        state.sync_selected_proxy();
        state.set_focus(Focus::ProxySave);

        let popup = PopupGeometry::from_terminal_size((100, 30), &state);
        let details = ProxyDetailsGeometry::from_area(popup.details);
        let output = rendered_text(100, 30, &state);

        let header_row = output
            .lines()
            .nth(details.header.y as usize + 1)
            .expect("proxy header row renders");
        assert!(
            header_row.contains("New Proxy"),
            "draft header shows the New Proxy identity: {header_row}"
        );
        assert!(
            header_row.contains("Draft"),
            "draft badge renders: {header_row}"
        );
        assert!(
            !header_row.contains('★') && !header_row.contains('☆'),
            "draft shows no active marker: {header_row}"
        );

        let hint_row = output
            .lines()
            .nth(details.hint.y as usize)
            .expect("proxy hint row renders");
        assert!(
            hint_row.contains("Enter: save · Tab: next"),
            "proxy hint arm renders on the draft page: {hint_row}"
        );
    }

    #[test]
    fn proxy_page_hint_text_matches_focused_control() {
        let mut state = ConnectRemoteHostState::load();
        for (focus, expected) in [
            (Focus::ProxyName, "Enter: edit · Ctrl-V: paste · Tab: next"),
            (Focus::AllProxy, "Enter: edit · Ctrl-V: paste · Tab: next"),
            (Focus::HttpsProxy, "Enter: edit · Ctrl-V: paste · Tab: next"),
            (Focus::ProxyActive, "Enter: set active · Tab: next"),
            (Focus::ProxySave, "Enter: save · Tab: next"),
            (Focus::ProxyDelete, "Enter: delete · Tab: next"),
        ] {
            state.set_focus(focus);
            assert_eq!(bottom_hint_text(&state), expected, "hint for {focus:?}");
        }
    }

    #[test]
    fn proxy_configuration_autofills_https_proxy_from_all_proxy_when_empty() {
        let mut state = ConnectRemoteHostState::load();
        state.proxy_draft = blank_proxy_profile();
        state.set_focus(Focus::AllProxy);

        for ch in "socks5://127.0.0.1:7897".chars() {
            state.apply_key(KeyEvent::from(KeyCode::Char(ch)));
        }

        assert_eq!(state.proxy_draft.all_proxy, "socks5://127.0.0.1:7897");
        assert_eq!(state.proxy_draft.https_proxy, "http://127.0.0.1:7897");
    }

    #[test]
    fn proxy_configuration_autofills_all_proxy_from_https_proxy_when_empty() {
        let mut state = ConnectRemoteHostState::load();
        state.proxy_draft = blank_proxy_profile();
        state.set_focus(Focus::HttpsProxy);

        for ch in "http://10.0.0.1:8080".chars() {
            state.apply_key(KeyEvent::from(KeyCode::Char(ch)));
        }

        assert_eq!(state.proxy_draft.https_proxy, "http://10.0.0.1:8080");
        assert_eq!(state.proxy_draft.all_proxy, "socks5://10.0.0.1:8080");
    }

    #[test]
    fn proxy_configuration_does_not_overwrite_user_edited_counterpart() {
        let mut state = ConnectRemoteHostState::load();
        state.proxy_draft = blank_proxy_profile();
        state.set_focus(Focus::AllProxy);
        for ch in "socks5://127.0.0.1:7897".chars() {
            state.apply_key(KeyEvent::from(KeyCode::Char(ch)));
        }
        state.set_focus(Focus::HttpsProxy);
        while !state.proxy_draft.https_proxy.is_empty() {
            state.apply_key(KeyEvent::from(KeyCode::Backspace));
        }
        for ch in "http://proxy.example:443".chars() {
            state.apply_key(KeyEvent::from(KeyCode::Char(ch)));
        }

        state.set_focus(Focus::AllProxy);
        state.apply_key(KeyEvent::from(KeyCode::Char('8')));

        assert_eq!(state.proxy_draft.https_proxy, "http://proxy.example:443");
    }

    #[test]
    fn saved_host_keeps_port_preference_separate_from_last_port() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles = vec![saved_key_profile()];
        state.selected = 0;

        let _ = state.sync_selected_profile();

        assert_eq!(state.remote_port_preference, "auto");
        assert_eq!(state.last_remote_port, Some(7575));
        assert!(saved_profile_can_connect_by_id(
            &state,
            state.selected_profile().unwrap()
        ));
    }

    #[test]
    fn saved_host_dirty_check_ignores_observed_last_port() {
        let mut state = ConnectRemoteHostState::load();
        let mut profile = saved_key_profile();
        profile.last_remote_port = Some(7474);
        state.profiles = vec![profile];
        state.selected = 0;

        let _ = state.sync_selected_profile();
        state.last_remote_port = Some(7575);

        assert!(profile_matches_state(
            state.selected_profile().unwrap(),
            &state
        ));
    }

    fn blank_state() -> ConnectRemoteHostState {
        let mut state = ConnectRemoteHostState::load();
        state.profiles = Vec::new();
        state.selected = 0;
        state.host.clear();
        state.ssh_user.clear();
        state.remote_port_preference = "auto".to_string();
        state.password_mode = PasswordMode::Enter;
        state.sudo_mode = SudoMode::None;
        state.secret_load = SecretLoadState::Idle;
        state
    }

    #[test]
    fn split_host_port_parses_ipv4_with_port() {
        assert_eq!(
            split_host_port("117.157.77.4:50045"),
            Some(("117.157.77.4".to_string(), 50045))
        );
    }

    #[test]
    fn split_host_port_parses_hostname_and_bracketed_ipv6() {
        assert_eq!(
            split_host_port("example.com:22"),
            Some(("example.com".to_string(), 22))
        );
        assert_eq!(split_host_port("[::1]:22"), Some(("::1".to_string(), 22)));
    }

    #[test]
    fn split_host_port_leaves_plain_hosts_and_bare_ipv6_unchanged() {
        assert_eq!(split_host_port("plain-host"), None);
        assert_eq!(split_host_port("fe80::1"), None);
        assert_eq!(split_host_port("host:"), None);
        assert_eq!(split_host_port(":22"), None);
        assert_eq!(split_host_port("host:abc"), None);
        assert_eq!(split_host_port("host:99999"), None);
        assert_eq!(split_host_port(""), None);
    }

    #[test]
    fn host_edit_leaving_field_splits_host_and_port() {
        let mut state = blank_state();
        state.set_focus(Focus::Host);
        for ch in "117.157.77.4:50045".chars() {
            state.apply_key(KeyEvent::from(KeyCode::Char(ch)));
        }
        state.apply_key(KeyEvent::from(KeyCode::Enter));

        assert_eq!(state.host, "117.157.77.4");
        assert_eq!(state.ssh_port, "50045");
        assert_eq!(state.remote_port_preference, "auto");
    }

    #[test]
    fn connect_action_splits_host_and_port_without_leaving_field() {
        let mut state = blank_state();
        state.set_focus(Focus::Host);
        for ch in "117.157.77.4:50045".chars() {
            state.apply_key(KeyEvent::from(KeyCode::Char(ch)));
        }

        assert_eq!(state.connect_action(), PaneAction::Connect);
        assert_eq!(state.host, "117.157.77.4");
        assert_eq!(state.ssh_port, "50045");
        assert_eq!(state.remote_port_preference, "auto");
    }

    #[test]
    fn paste_into_host_field_splits_host_and_port() {
        let mut state = blank_state();
        state.set_focus(Focus::Host);

        state.apply_paste("117.157.77.4:50045\n");

        assert_eq!(state.host, "117.157.77.4");
        assert_eq!(state.ssh_port, "50045");
        assert_eq!(state.remote_port_preference, "auto");
    }

    #[test]
    fn paste_into_ssh_password_uses_first_line_and_switches_saved_to_enter() {
        let mut state = ConnectRemoteHostState::load();
        state.set_focus(Focus::Password);
        state.password_mode = PasswordMode::Saved;

        state.apply_paste("s3\tcret\nignored-second-line");

        assert_eq!(state.ssh_password, "s3cret");
        assert_eq!(state.password_mode, PasswordMode::Enter);
    }

    #[test]
    fn paste_into_sudo_password_switches_saved_to_replace() {
        let mut state = ConnectRemoteHostState::load();
        state.set_focus(Focus::Sudo);
        state.sudo_mode = SudoMode::Saved;

        state.apply_paste("sudos3cret\n");

        assert_eq!(state.sudo_password, "sudos3cret");
        assert_eq!(state.sudo_mode, SudoMode::Replace);
    }

    #[test]
    fn paste_without_edit_focus_is_ignored() {
        let mut state = blank_state();
        state.set_focus(Focus::Hosts);

        state.apply_paste("117.157.77.4:50045");

        assert_eq!(state.host, "");
        assert_eq!(state.remote_port_preference, "auto");
    }

    #[test]
    fn edited_saved_host_uses_replace_profile_and_updates_default_name() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles = vec![RemoteHostProfile {
            name: "k@127.0.0.1".to_string(),
            host: "127.0.0.1".to_string(),
            ssh_user: "k".to_string(),
            auth: RemoteHostAuthProfile::Password {
                password_secret_id: None,
            },
            sudo_password_secret_id: None,
            preferred_remote_port: RemotePortPreference::Auto,
            ssh_port: None,
            last_remote_port: Some(7575),
            last_endpoint: None,
            last_connected_at: None,
            use_install_proxy: true,
            ..RemoteHostProfile::default()
        }];
        state.selected = 0;
        let _ = state.sync_selected_profile();
        state.host = "127.0.0.2".to_string();

        assert!(!saved_profile_can_connect_by_id(
            &state,
            state.selected_profile().unwrap()
        ));
        assert_eq!(save_profile_name_for_state(&state), "k@127.0.0.2");
    }

    #[test]
    fn edited_saved_host_preserves_custom_profile_name() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles = vec![RemoteHostProfile {
            name: "prod".to_string(),
            host: "127.0.0.1".to_string(),
            ssh_user: "k".to_string(),
            auth: RemoteHostAuthProfile::Password {
                password_secret_id: None,
            },
            sudo_password_secret_id: None,
            preferred_remote_port: RemotePortPreference::Auto,
            ssh_port: None,
            last_remote_port: Some(7575),
            last_endpoint: None,
            last_connected_at: None,
            use_install_proxy: true,
            ..RemoteHostProfile::default()
        }];
        state.selected = 0;
        let _ = state.sync_selected_profile();
        state.host = "127.0.0.2".to_string();

        assert_eq!(save_profile_name_for_state(&state), "prod");
    }

    #[test]
    fn connect_popup_renders_saved_host_and_profile_fields() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles = vec![RemoteHostProfile {
            name: "k@127.0.0.1".to_string(),
            host: "127.0.0.1".to_string(),
            ssh_user: "k".to_string(),
            auth: RemoteHostAuthProfile::Password {
                password_secret_id: None,
            },
            sudo_password_secret_id: None,
            preferred_remote_port: RemotePortPreference::Auto,
            ssh_port: None,
            last_remote_port: Some(7575),
            last_endpoint: None,
            last_connected_at: None,
            use_install_proxy: true,
            ..RemoteHostProfile::default()
        }];
        state.selected = 0;
        let _ = state.sync_selected_profile();
        assert_eq!(state.host, "127.0.0.1");
        assert_eq!(state.ssh_user, "k");
        assert_eq!(segmented_for_test(&auth_tabs(&state)), "Password  Key");
    }

    fn state_with_shell_profile(remote_shell: Option<RemoteShellKind>) -> ConnectRemoteHostState {
        let mut state = ConnectRemoteHostState::load();
        state.profiles = vec![RemoteHostProfile {
            name: "win".to_string(),
            host: "192.168.1.6".to_string(),
            ssh_user: "jj".to_string(),
            auth: RemoteHostAuthProfile::Password {
                password_secret_id: None,
            },
            remote_shell,
            ..RemoteHostProfile::default()
        }];
        state.selected = 0;
        let _ = state.sync_selected_profile();
        state
    }

    #[test]
    fn windows_shell_profile_skips_sudo_focus_and_blocks_editing() {
        let mut state = state_with_shell_profile(Some(RemoteShellKind::Windows));

        assert!(state.selected_profile_is_windows_shell());
        state.set_focus(Focus::Password);
        assert_eq!(state.next_focus(), Focus::Remember);
        state.set_focus(Focus::Remember);
        assert_eq!(state.prev_focus(), Focus::Password);

        state.focus = Focus::Sudo;
        state.start_sudo_password_edit();
        assert_eq!(state.editing, None);
    }

    #[test]
    fn posix_or_unclassified_profile_keeps_sudo_field() {
        let mut posix = state_with_shell_profile(Some(RemoteShellKind::Posix));
        assert!(!posix.selected_profile_is_windows_shell());
        posix.set_focus(Focus::Password);
        assert_eq!(posix.next_focus(), Focus::Sudo);
        posix.focus = Focus::Sudo;
        posix.start_sudo_password_edit();
        assert_eq!(posix.editing, Some(EditField::SudoPassword));

        let mut unknown = state_with_shell_profile(None);
        assert!(!unknown.selected_profile_is_windows_shell());
        unknown.set_focus(Focus::Password);
        assert_eq!(unknown.next_focus(), Focus::Sudo);
    }

    #[test]
    fn connect_popup_initial_secret_load_request_only_targets_selected_profile() {
        let ssh_id = crate::host::ssh::remote_host_secret_store::RemoteHostSecretId::new(
            "waitagent.remote-host.first.ssh-password",
        )
        .unwrap();
        let sudo_id = crate::host::ssh::remote_host_secret_store::RemoteHostSecretId::new(
            "waitagent.remote-host.first.sudo-password",
        )
        .unwrap();
        let second_id = crate::host::ssh::remote_host_secret_store::RemoteHostSecretId::new(
            "waitagent.remote-host.second.ssh-password",
        )
        .unwrap();
        let mut state = ConnectRemoteHostState::load();
        state.profiles = vec![
            RemoteHostProfile {
                name: "first".to_string(),
                host: "127.0.0.1".to_string(),
                ssh_user: "k".to_string(),
                auth: RemoteHostAuthProfile::Password {
                    password_secret_id: Some(ssh_id.clone()),
                },
                sudo_password_secret_id: Some(sudo_id.clone()),
                preferred_remote_port: RemotePortPreference::Auto,
                ssh_port: None,
                last_remote_port: Some(7575),
                last_endpoint: None,
                last_connected_at: None,
                use_install_proxy: true,
                ..RemoteHostProfile::default()
            },
            RemoteHostProfile {
                name: "second".to_string(),
                host: "127.0.0.2".to_string(),
                ssh_user: "k".to_string(),
                auth: RemoteHostAuthProfile::Password {
                    password_secret_id: Some(second_id),
                },
                sudo_password_secret_id: None,
                preferred_remote_port: RemotePortPreference::Auto,
                ssh_port: None,
                last_remote_port: Some(7575),
                last_endpoint: None,
                last_connected_at: None,
                use_install_proxy: true,
                ..RemoteHostProfile::default()
            },
        ];
        state.selected = 0;

        let request = state.sync_selected_profile().unwrap();

        assert_eq!(request.selected, 0);
        assert_eq!(request.ssh_secret_id, Some(ssh_id));
        assert_eq!(request.sudo_secret_id, Some(sudo_id));
    }

    #[test]
    fn connect_popup_initial_saved_host_creates_current_profile_load_request() {
        let mut state = ConnectRemoteHostState::load();
        let ssh_id = crate::host::ssh::remote_host_secret_store::RemoteHostSecretId::new(
            "waitagent.remote-host.k-127-0-0-1.ssh-password",
        )
        .unwrap();
        state.profiles = vec![RemoteHostProfile {
            name: "k@127.0.0.1".to_string(),
            host: "127.0.0.1".to_string(),
            ssh_user: "k".to_string(),
            auth: RemoteHostAuthProfile::Password {
                password_secret_id: Some(ssh_id),
            },
            sudo_password_secret_id: None,
            preferred_remote_port: RemotePortPreference::Auto,
            ssh_port: None,
            last_remote_port: Some(7575),
            last_endpoint: None,
            last_connected_at: None,
            use_install_proxy: true,
            ..RemoteHostProfile::default()
        }];
        state.selected = 0;

        let initial_request = state.sync_selected_profile();

        assert!(initial_request.is_some());
        assert!(state.credentials_loading());
        assert_eq!(connect_label(&state), "Loading...");
    }

    #[test]
    fn connect_popup_loads_saved_passwords_through_event_loop_result() {
        let ssh_id = crate::host::ssh::remote_host_secret_store::RemoteHostSecretId::new(
            "waitagent.remote-host.k-127-0-0-1.ssh-password",
        )
        .unwrap();
        let sudo_id = crate::host::ssh::remote_host_secret_store::RemoteHostSecretId::new(
            "waitagent.remote-host.k-127-0-0-1.sudo-password",
        )
        .unwrap();

        let mut state = ConnectRemoteHostState::load();
        state.profiles = vec![RemoteHostProfile {
            name: "k@127.0.0.1".to_string(),
            host: "127.0.0.1".to_string(),
            ssh_user: "k".to_string(),
            auth: RemoteHostAuthProfile::Password {
                password_secret_id: Some(ssh_id.clone()),
            },
            sudo_password_secret_id: Some(sudo_id.clone()),
            preferred_remote_port: RemotePortPreference::Auto,
            ssh_port: None,
            last_remote_port: Some(7575),
            last_endpoint: None,
            last_connected_at: None,
            use_install_proxy: true,
            ..RemoteHostProfile::default()
        }];
        state.selected = 0;
        let request = state
            .sync_selected_profile()
            .expect("saved host loads secrets");

        assert_eq!(request.ssh_secret_id, Some(ssh_id));
        assert_eq!(request.sudo_secret_id, Some(sudo_id));
        assert_eq!(state.password_mode, PasswordMode::Loading);
        assert_eq!(state.sudo_mode, SudoMode::Loading);
        assert_eq!(password_display(&state), "Loading saved...");
        assert_eq!(
            state.apply_key(KeyEvent::from(KeyCode::Enter)),
            PaneAction::None
        );

        state.apply_secret_result(SecretLoadResult {
            id: request.id,
            selected: request.selected,
            ssh: Some(Ok("ssh-secret".to_string())),
            sudo: Some(Ok("sudo-secret".to_string())),
        });

        assert_eq!(state.password_mode, PasswordMode::Saved);
        assert_eq!(state.sudo_mode, SudoMode::Saved);
        assert_eq!(state.ssh_password, "ssh-secret");
        assert_eq!(state.sudo_password, "sudo-secret");
        assert_eq!(password_display(&state), "**********");
        assert_eq!(sudo_password_display(&state), "***********");
        state.set_focus(Focus::Password);
        assert_eq!(state.ssh_password, "ssh-secret");
    }

    #[test]
    fn saved_host_label_hides_remote_waitagent_port_and_auth_kind() {
        let profile = RemoteHostProfile {
            name: "k@127.0.0.1".to_string(),
            host: "127.0.0.1".to_string(),
            ssh_user: "k".to_string(),
            auth: RemoteHostAuthProfile::Password {
                password_secret_id: None,
            },
            sudo_password_secret_id: None,
            preferred_remote_port: RemotePortPreference::Auto,
            ssh_port: None,
            last_remote_port: Some(7575),
            last_endpoint: None,
            last_connected_at: None,
            use_install_proxy: true,
            ..RemoteHostProfile::default()
        };

        assert_eq!(saved_host_label(&profile), "k@127.0.0.1");
    }

    #[test]
    fn popup_geometry_uses_terminal_sized_dialog_independent_of_selection() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles = vec![RemoteHostProfile {
            name: "k@127.0.0.1".to_string(),
            host: "127.0.0.1".to_string(),
            ssh_user: "k".to_string(),
            auth: RemoteHostAuthProfile::Password {
                password_secret_id: None,
            },
            sudo_password_secret_id: None,
            preferred_remote_port: RemotePortPreference::Auto,
            ssh_port: None,
            last_remote_port: Some(7575),
            last_endpoint: None,
            last_connected_at: None,
            use_install_proxy: true,
            ..RemoteHostProfile::default()
        }];
        state.selected = 1;
        let _ = state.sync_selected_profile();

        let geometry = PopupGeometry::from_terminal_size((140, 26), &state);

        assert_eq!(geometry.dialog.x, 20);
        assert_eq!(geometry.dialog.width, 100);
        // Height is fixed relative to the terminal, not to the selected item.
        assert_eq!(geometry.dialog.height, 24);
        assert_eq!(geometry.hosts.y, 2);
        assert_eq!(geometry.details.y, 2);
        assert_eq!(geometry.hosts.height, 22);
        assert_eq!(geometry.hosts.width, 29);
        assert_eq!(geometry.details.width, 68);
        assert_eq!(
            geometry.details.x + geometry.details.width + DETAIL_RIGHT_PADDING,
            geometry.dialog.x + geometry.dialog.width - 1
        );

        // Switching to New Host should not change the popup geometry.
        state.selected = state.profiles.len();
        let _ = state.sync_selected_profile();
        let new_host_geometry = PopupGeometry::from_terminal_size((140, 26), &state);
        assert_eq!(new_host_geometry.dialog.height, geometry.dialog.height);
        assert_eq!(new_host_geometry.hosts.height, geometry.hosts.height);
    }

    #[test]
    fn popup_geometry_keeps_host_list_width_stable_for_saved_host_selection() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles = vec![RemoteHostProfile {
            name: "k@127.0.0.1".to_string(),
            host: "127.0.0.1".to_string(),
            ssh_user: "k".to_string(),
            auth: RemoteHostAuthProfile::Password {
                password_secret_id: None,
            },
            sudo_password_secret_id: None,
            preferred_remote_port: RemotePortPreference::Auto,
            ssh_port: None,
            last_remote_port: Some(7575),
            last_endpoint: None,
            last_connected_at: None,
            use_install_proxy: true,
            ..RemoteHostProfile::default()
        }];
        state.selected = 0;
        let _ = state.sync_selected_profile();

        let geometry = PopupGeometry::from_terminal_size((140, 26), &state);

        assert_eq!(geometry.dialog.x, 20);
        assert_eq!(geometry.dialog.width, 100);
        assert_eq!(geometry.hosts.width, 29);
        assert_eq!(geometry.details.width, 68);
        assert_eq!(
            geometry.details.x + geometry.details.width + DETAIL_RIGHT_PADDING,
            geometry.dialog.x + geometry.dialog.width - 1
        );
    }

    #[test]
    fn host_list_width_uses_compact_width_for_short_saved_hosts() {
        let mut state = ConnectRemoteHostState::load();
        state.proxy_settings = RemoteInstallProxySettings::default();
        state.profiles = vec![RemoteHostProfile {
            name: "k@127.0.0.1".to_string(),
            host: "127.0.0.1".to_string(),
            ssh_user: "k".to_string(),
            auth: RemoteHostAuthProfile::Password {
                password_secret_id: None,
            },
            sudo_password_secret_id: None,
            preferred_remote_port: RemotePortPreference::Auto,
            ssh_port: None,
            last_remote_port: Some(7575),
            last_endpoint: None,
            last_connected_at: None,
            use_install_proxy: true,
            ..RemoteHostProfile::default()
        }];

        assert_eq!(host_list_width(&state, 98), 29);
    }

    #[test]
    fn host_list_width_caps_content_at_proxy_host_budget() {
        let mut state = ConnectRemoteHostState::load();
        state.proxy_settings = RemoteInstallProxySettings::default();
        state.profiles = vec![RemoteHostProfile {
            name: "deploy@very-long-host-name.example.internal".to_string(),
            host: "very-long-host-name.example.internal".to_string(),
            ssh_user: "deploy".to_string(),
            auth: RemoteHostAuthProfile::Key {
                key_path: std::path::PathBuf::from("~/.ssh/id_rsa"),
            },
            sudo_password_secret_id: None,
            preferred_remote_port: RemotePortPreference::Auto,
            ssh_port: None,
            last_remote_port: Some(7575),
            last_endpoint: None,
            last_connected_at: None,
            use_install_proxy: true,
            ..RemoteHostProfile::default()
        }];

        assert_eq!(host_list_width(&state, 98), 29);
    }

    #[test]
    fn host_list_width_expands_for_proxy_profile_hosts() {
        let mut state = ConnectRemoteHostState::load();
        state.proxy_settings = RemoteInstallProxySettings {
            active: Some("10.1.29.96:7897".to_string()),
            profiles: vec![RemoteInstallProxyProfile {
                name: "10.1.29.96:7897".to_string(),
                all_proxy: "socks5://10.1.29.96:7897".to_string(),
                https_proxy: String::new(),
            }],
        };

        assert_eq!(host_list_width(&state, 98), 29);
    }

    #[test]
    fn host_list_width_fits_max_ipv4_proxy_profile_host() {
        let mut state = ConnectRemoteHostState::load();
        state.proxy_settings = RemoteInstallProxySettings {
            active: Some("255.255.255.255:65535".to_string()),
            profiles: vec![RemoteInstallProxyProfile {
                name: "255.255.255.255:65535".to_string(),
                all_proxy: "socks5://255.255.255.255:65535".to_string(),
                https_proxy: String::new(),
            }],
        };

        assert_eq!(host_list_width(&state, 98), 29);
    }

    #[test]
    fn proxy_configuration_geometry_keeps_details_complete_with_right_padding() {
        let mut state = ConnectRemoteHostState::load();
        state.proxy_settings = RemoteInstallProxySettings {
            active: Some("192.168.31.178:7897".to_string()),
            profiles: vec![RemoteInstallProxyProfile {
                name: "192.168.31.178:7897".to_string(),
                all_proxy: "socks5://192.168.31.178:7897".to_string(),
                https_proxy: String::new(),
            }],
        };
        state.selected = state.proxy_profile_selection_start();
        state.sync_selected_proxy();

        let geometry = PopupGeometry::from_terminal_size((140, 26), &state);

        assert_eq!(geometry.dialog.x, 20);
        assert_eq!(geometry.dialog.width, 100);
        assert_eq!(geometry.hosts.width, 29);
        assert_eq!(geometry.details.width, 68);
        assert_eq!(
            geometry.details.x + geometry.details.width + DETAIL_RIGHT_PADDING,
            geometry.dialog.x + geometry.dialog.width - 1
        );
    }

    #[test]
    fn popup_geometry_clips_to_terminal_when_allocated_less_than_requested_width() {
        let state = ConnectRemoteHostState::load();

        let geometry = PopupGeometry::from_terminal_size((66, 18), &state);

        assert_eq!(geometry.dialog.x, 0);
        assert_eq!(geometry.dialog.width, 66);
        assert_eq!(geometry.hosts.width, 29);
        assert_eq!(geometry.details.width, 34);
    }

    #[test]
    fn connect_popup_keyboard_contract_matches_popup_navigation() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles = vec![
            RemoteHostProfile {
                name: "a@127.0.0.1".to_string(),
                host: "127.0.0.1".to_string(),
                ssh_user: "a".to_string(),
                auth: RemoteHostAuthProfile::Password {
                    password_secret_id: None,
                },
                sudo_password_secret_id: None,
                preferred_remote_port: RemotePortPreference::Auto,
                ssh_port: None,
                last_remote_port: None,
                last_endpoint: None,
                last_connected_at: None,
                use_install_proxy: true,
                ..RemoteHostProfile::default()
            },
            RemoteHostProfile {
                name: "b@127.0.0.2".to_string(),
                host: "127.0.0.2".to_string(),
                ssh_user: "b".to_string(),
                auth: RemoteHostAuthProfile::Password {
                    password_secret_id: None,
                },
                sudo_password_secret_id: None,
                preferred_remote_port: RemotePortPreference::Auto,
                ssh_port: None,
                last_remote_port: None,
                last_endpoint: None,
                last_connected_at: None,
                use_install_proxy: true,
                ..RemoteHostProfile::default()
            },
        ];
        state.set_focus(Focus::Hosts);

        assert_eq!(state.focus, Focus::Hosts);
        assert_eq!(state.selected, 0);
        assert!(matches!(
            state.apply_key(KeyEvent::from(KeyCode::Down)),
            PaneAction::LoadSecrets(_)
        ));
        assert_eq!(state.selected, 1);
        assert!(matches!(
            state.apply_key(KeyEvent::from(KeyCode::Down)),
            PaneAction::LoadSecrets(_)
        ));
        assert_eq!(state.selected, 2);
        assert!(matches!(
            state.apply_key(KeyEvent::from(KeyCode::Up)),
            PaneAction::LoadSecrets(_)
        ));
        assert_eq!(state.selected, 1);

        assert_eq!(
            state.apply_key(KeyEvent::from(KeyCode::Right)),
            PaneAction::None
        );
        assert_eq!(state.focus, Focus::Connect);
        assert_eq!(
            state.apply_key(KeyEvent::from(KeyCode::Up)),
            PaneAction::None
        );
        assert_eq!(state.focus, Focus::InstallProxy);
        assert_eq!(
            state.apply_key(KeyEvent::from(KeyCode::Down)),
            PaneAction::None
        );
        assert_eq!(state.focus, Focus::Connect);
        assert_eq!(
            state.apply_key(KeyEvent::from(KeyCode::Down)),
            PaneAction::None
        );
        assert_eq!(state.focus, Focus::Delete);
        assert_eq!(
            state.apply_key(KeyEvent::from(KeyCode::Up)),
            PaneAction::None
        );
        assert_eq!(state.focus, Focus::InstallProxy);

        state.set_focus(Focus::Remember);
        assert_eq!(
            state.apply_key(KeyEvent::from(KeyCode::Up)),
            PaneAction::None
        );
        assert_eq!(state.focus, Focus::Sudo);

        state.set_focus(Focus::Auth);
        assert_eq!(state.auth, AuthChoice::Password);
        assert_eq!(
            state.apply_key(KeyEvent::from(KeyCode::Right)),
            PaneAction::None
        );
        assert_eq!(state.auth, AuthChoice::Key);
        assert_eq!(state.focus, Focus::Auth);
        assert_eq!(
            state.apply_key(KeyEvent::from(KeyCode::Left)),
            PaneAction::None
        );
        assert_eq!(state.auth, AuthChoice::Password);
        assert_eq!(state.focus, Focus::Auth);

        state.set_focus(Focus::Host);
        assert_eq!(state.editing, Some(EditField::Host));
        assert_eq!(state.edit_cursor, state.host.chars().count());
        assert_eq!(
            state.apply_key(KeyEvent::from(KeyCode::Left)),
            PaneAction::None
        );
        assert_eq!(state.focus, Focus::Host);
        assert_eq!(state.edit_cursor, state.host.chars().count() - 1);
        while state.edit_cursor > 0 {
            state.apply_key(KeyEvent::from(KeyCode::Left));
        }
        assert_eq!(
            state.apply_key(KeyEvent::from(KeyCode::Left)),
            PaneAction::None
        );
        assert_eq!(state.focus, Focus::Hosts);

        state.set_focus(Focus::Host);
        assert_eq!(
            state.apply_key(KeyEvent::from(KeyCode::Esc)),
            PaneAction::None
        );
        assert_eq!(state.focus, Focus::Hosts);
        assert_eq!(
            state.apply_key(KeyEvent::from(KeyCode::Esc)),
            PaneAction::Close
        );
    }

    #[test]
    fn proxy_configuration_is_global_left_nav_entry() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles = vec![saved_password_profile()];
        state.proxy_settings = RemoteInstallProxySettings::default();

        let popup = PopupGeometry::from_terminal_size((100, 30), &state);
        assert!(popup.sidebar.saved_list.height >= 1);
        assert!(popup.sidebar.proxy_header.height >= 1);
        assert_eq!(state.proxy_selection_index(), state.profiles.len() + 1);

        state.selected = state.proxy_selection_index();
        state.set_focus(Focus::Hosts);
        assert_eq!(state.default_detail_focus(), Focus::ProxyActive);
        assert!(state.selected_profile().is_none());
    }

    #[test]
    fn proxy_configuration_lists_profiles_and_new_proxy_entry() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles = vec![saved_password_profile()];
        state.proxy_settings = RemoteInstallProxySettings {
            active: Some("Office".to_string()),
            profiles: vec![
                RemoteInstallProxyProfile {
                    name: "Home".to_string(),
                    all_proxy: "socks5://192.168.31.1:7897".to_string(),
                    https_proxy: "http://192.168.31.1:7897".to_string(),
                },
                RemoteInstallProxyProfile {
                    name: "Office".to_string(),
                    all_proxy: "socks5://127.0.0.1:7897".to_string(),
                    https_proxy: "http://127.0.0.1:7897".to_string(),
                },
            ],
        };

        let popup = PopupGeometry::from_terminal_size((100, 30), &state);
        assert!(popup.sidebar.proxy_list.height >= 2);

        state.selected = state.proxy_profile_selection_start();
        state.sync_selected_proxy();
        assert_eq!(state.proxy_draft.name, "Home");

        state.selected = state.proxy_profile_selection_start() + 1;
        state.sync_selected_proxy();
        assert_eq!(state.proxy_draft.name, "Office");

        state.selected = state.new_proxy_selection_index();
        state.sync_selected_proxy();
        assert!(state.proxy_draft.name.is_empty());
    }

    #[test]
    fn proxy_configuration_selection_syncs_existing_and_new_drafts() {
        let mut state = ConnectRemoteHostState::load();
        state.proxy_settings = RemoteInstallProxySettings {
            active: Some("Office".to_string()),
            profiles: vec![RemoteInstallProxyProfile {
                name: "Office".to_string(),
                all_proxy: "socks5://127.0.0.1:7897".to_string(),
                https_proxy: "http://127.0.0.1:7897".to_string(),
            }],
        };

        state.selected = state.proxy_profile_selection_start();
        state.sync_selected_proxy();
        assert_eq!(state.proxy_draft.name, "Office");
        assert_eq!(state.proxy_draft.all_proxy, "socks5://127.0.0.1:7897");

        state.selected = state.new_proxy_selection_index();
        state.sync_selected_proxy();
        assert!(state.proxy_draft.name.is_empty());
        assert!(state.proxy_draft.all_proxy.is_empty());
    }

    #[test]
    fn proxy_configuration_derives_default_profile_name_from_all_proxy_host() {
        let profile = RemoteInstallProxyProfile {
            name: String::new(),
            all_proxy: "socks5://10.1.29.96:7897".to_string(),
            https_proxy: "http://192.168.31.1:7897".to_string(),
        };

        assert_eq!(proxy_profile_name(&profile), "10.1.29.96:7897");
    }

    #[test]
    fn proxy_configuration_replaces_placeholder_profile_names_from_proxy_host() {
        let profile = RemoteInstallProxyProfile {
            name: "Default".to_string(),
            all_proxy: String::new(),
            https_proxy: "http://proxy.example:7897".to_string(),
        };

        assert_eq!(proxy_profile_name(&profile), "proxy.example:7897");
    }

    #[test]
    fn install_proxy_toggle_is_host_detail_state() {
        let mut state = ConnectRemoteHostState::load();
        assert!(state.use_install_proxy);
        state.set_focus(Focus::InstallProxy);
        assert_eq!(
            state.apply_key(KeyEvent::from(KeyCode::Char(' '))),
            PaneAction::None
        );
        assert!(!state.use_install_proxy);
    }

    #[test]
    fn connect_popup_tab_cycles_focus() {
        let mut state = ConnectRemoteHostState::load();
        assert_eq!(state.focus, Focus::Hosts);
        state.apply_key(KeyEvent::from(KeyCode::Tab));
        assert_eq!(state.focus, Focus::Host);
        state.apply_key(KeyEvent::from(KeyCode::BackTab));
        assert_eq!(state.focus, Focus::Hosts);
    }

    #[test]
    fn connect_popup_renders_delete_in_ctrl_w_popup_size() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles = vec![saved_password_profile()];
        state.selected = 0;
        let _ = state.sync_selected_profile();
        let popup = PopupGeometry::from_terminal_size((100, 26), &state);
        let details = DetailsGeometry::from_area(popup.details, &state);

        assert_eq!(popup.details.y, 2);
        assert_eq!(popup.details.height, 22);
        assert_eq!(details.buttons.height, 1);
        assert_eq!(details.hint.height, 1);
        assert_eq!(details.hint.y, details.buttons.y + 1);
        assert!(
            details.buttons.y + details.buttons.height <= popup.details.y + popup.details.height
        );

        let output = rendered_text(100, 26, &state);
        assert!(output.contains("Connect Remote Host"));
        assert!(output.contains("Remember host"));
        assert!(output.contains("Use proxy"));
        assert!(output.contains("Connect"));
        assert!(output.contains("Delete"));
        assert!(output.contains(&bottom_hint_text(&state)));
    }

    #[test]
    fn connect_popup_shows_connecting_as_modal_without_renaming_connect_button() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles.clear();
        state.selected = 0;
        let _ = state.sync_selected_profile();
        state.status = Status::Working("Connecting...".to_string());

        assert_eq!(connect_label(&state), "Connect");
        let output = rendered_text(100, 26, &state);
        assert!(output.contains("Connecting"));
        assert!(output.contains("Connecting..."));
        assert!(output.contains("Connect"));
    }

    #[test]
    fn connect_popup_delete_saved_host_opens_confirmation_popup() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles = vec![saved_password_profile()];
        state.selected = 0;
        let _ = state.sync_selected_profile();
        state.set_focus(Focus::Delete);

        assert_eq!(delete_label(&state), "Delete");
        assert_eq!(
            state.apply_key(KeyEvent::from(KeyCode::Enter)),
            PaneAction::None
        );
        assert_eq!(delete_label(&state), "Delete");
        assert_eq!(
            state.delete_confirm_focus(),
            Some(DeleteConfirmFocus::Cancel)
        );

        assert_eq!(
            state.apply_key(KeyEvent::from(KeyCode::Enter)),
            PaneAction::None
        );
        assert_eq!(state.delete_confirm, DeleteConfirmState::Idle);
    }

    #[test]
    fn connect_popup_delete_confirmation_requires_delete_choice() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles = vec![saved_password_profile()];
        state.selected = 0;
        let _ = state.sync_selected_profile();
        state.set_focus(Focus::Delete);
        assert_eq!(
            state.apply_key(KeyEvent::from(KeyCode::Enter)),
            PaneAction::None
        );

        assert_eq!(
            state.delete_confirm_focus(),
            Some(DeleteConfirmFocus::Cancel)
        );
        assert_eq!(
            state.apply_key(KeyEvent::from(KeyCode::Right)),
            PaneAction::None
        );
        assert_eq!(
            state.delete_confirm_focus(),
            Some(DeleteConfirmFocus::Delete)
        );
        assert_eq!(
            state.apply_key(KeyEvent::from(KeyCode::Enter)),
            PaneAction::DeleteSelectedHost {
                profile_name: "k.0.0.1".to_string()
            }
        );
    }

    #[test]
    fn connect_popup_delete_confirmation_escape_cancels_popup() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles = vec![saved_password_profile()];
        state.selected = 0;
        let _ = state.sync_selected_profile();
        state.set_focus(Focus::Delete);
        let _ = state.apply_key(KeyEvent::from(KeyCode::Enter));

        assert_eq!(
            state.apply_key(KeyEvent::from(KeyCode::Esc)),
            PaneAction::None
        );
        assert_eq!(state.delete_confirm, DeleteConfirmState::Idle);
    }

    #[test]
    fn connect_popup_enters_connect_for_saved_host_and_host_for_new_host() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles = vec![RemoteHostProfile {
            name: "k@127.0.0.1".to_string(),
            host: "127.0.0.1".to_string(),
            ssh_user: "k".to_string(),
            auth: RemoteHostAuthProfile::Password {
                password_secret_id: None,
            },
            sudo_password_secret_id: None,
            preferred_remote_port: RemotePortPreference::Auto,
            ssh_port: None,
            last_remote_port: Some(7575),
            last_endpoint: None,
            last_connected_at: None,
            use_install_proxy: true,
            ..RemoteHostProfile::default()
        }];

        state.selected = 0;
        state.set_focus(Focus::Hosts);
        state.apply_key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(state.focus, Focus::Connect);

        state.selected = state.profiles.len();
        state.set_focus(Focus::Hosts);
        state.apply_key(KeyEvent::from(KeyCode::Right));
        assert_eq!(state.focus, Focus::Host);
    }

    #[test]
    fn connect_popup_keyboard_can_return_from_detail_area_to_host_list() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles.clear();
        state.selected = 0;
        let _ = state.sync_selected_profile();
        state.host.clear();
        assert_eq!(state.focus, Focus::Hosts);

        state.apply_key(KeyEvent::from(KeyCode::Right));
        assert_eq!(state.focus, Focus::Host);
        assert_eq!(
            state.apply_key(KeyEvent::from(KeyCode::Left)),
            PaneAction::None
        );
        assert_eq!(state.focus, Focus::Hosts);

        state.apply_key(KeyEvent::from(KeyCode::Right));
        assert_eq!(state.focus, Focus::Host);
        assert_eq!(
            state.apply_key(KeyEvent::from(KeyCode::Esc)),
            PaneAction::None
        );
        assert_eq!(state.focus, Focus::Hosts);
        assert_eq!(
            state.apply_key(KeyEvent::from(KeyCode::Esc)),
            PaneAction::Close
        );
    }

    #[test]
    fn connect_popup_mouse_hits_visible_password_row() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles.clear();
        state.selected = 0;
        let _ = state.sync_selected_profile();
        let geometry = PopupGeometry::from_terminal_size((80, 24), &state);

        let details = DetailsGeometry::from_area(geometry.details, &state);
        state.apply_mouse(
            crossterm::event::MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: geometry.details.x + geometry.details.width - 10,
                row: geometry.details.y + details.rows.password,
                modifiers: crossterm::event::KeyModifiers::empty(),
            },
            (80, 24),
        );

        assert_eq!(state.focus, Focus::Password);
        assert_eq!(state.editing, Some(EditField::SshPassword));
    }

    #[test]
    fn password_rows_style_only_the_focused_value() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles.clear();
        state.selected = 0;
        let _ = state.sync_selected_profile();
        state.ssh_password = "secret".to_string();

        state.set_focus(Focus::Password);
        assert_password_control_styles(
            password_control_line(PasswordField::Ssh, &state),
            active_focus_style(),
        );

        state.set_focus(Focus::Sudo);
        state.sudo_mode = SudoMode::Replace;
        assert_password_control_styles(
            password_control_line(PasswordField::Sudo, &state),
            active_focus_style(),
        );
    }

    fn assert_password_control_styles(line: Line<'static>, value_style: Style) {
        assert_eq!(line.spans.len(), 1);
        assert_eq!(line.spans[0].content.as_ref(), "******");
        assert_eq!(line.spans[0].style, value_style);
    }

    #[test]
    fn empty_host_uses_placeholder_only_for_display() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles.clear();
        state.selected = 0;
        let _ = state.sync_selected_profile();

        assert_eq!(host_display(&state), HOST_EMPTY_PLACEHOLDER);

        state.host = "example.internal".to_string();

        assert_eq!(host_display(&state), "example.internal");
    }

    #[test]
    fn password_and_sudo_empty_states_use_placeholder_only_for_display() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles.clear();
        state.selected = 0;
        let _ = state.sync_selected_profile();
        state.set_focus(Focus::Password);

        let password_line = password_control_line(PasswordField::Ssh, &state);
        assert_eq!(password_line.spans.len(), 1);
        assert_eq!(
            password_line.spans[0].content.as_ref(),
            PASSWORD_EMPTY_PLACEHOLDER
        );
        assert_eq!(password_display(&state), PASSWORD_EMPTY_PLACEHOLDER);

        state.set_focus(Focus::Sudo);
        let sudo_line = password_control_line(PasswordField::Sudo, &state);
        assert_eq!(sudo_line.spans.len(), 1);
        assert_eq!(
            sudo_line.spans[0].content.as_ref(),
            PASSWORD_EMPTY_PLACEHOLDER
        );
        assert_eq!(sudo_password_display(&state), PASSWORD_EMPTY_PLACEHOLDER);
    }

    #[test]
    fn empty_host_cursor_starts_at_input_origin() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles.clear();
        state.selected = 0;
        let _ = state.sync_selected_profile();
        state.set_focus(Focus::Host);
        let geometry = PopupGeometry::from_terminal_size((80, 24), &state);

        let (x, y) = cursor_position(geometry.details, &state).unwrap();
        let details = DetailsGeometry::from_area(geometry.details, &state);

        assert_eq!(y, geometry.details.y + details.rows.host);
        assert_eq!(x, geometry.details.x + 38);
    }

    #[test]
    fn empty_password_cursor_starts_at_input_origin() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles.clear();
        state.selected = 0;
        let _ = state.sync_selected_profile();
        state.set_focus(Focus::Password);
        let geometry = PopupGeometry::from_terminal_size((80, 24), &state);

        let (x, y) = cursor_position(geometry.details, &state).unwrap();
        let details = DetailsGeometry::from_area(geometry.details, &state);

        assert_eq!(y, geometry.details.y + details.rows.password);
        assert_eq!(x, geometry.details.x + 40);
    }

    #[test]
    fn editing_password_mask_tracks_short_password_length() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles.clear();
        state.selected = 0;
        let _ = state.sync_selected_profile();
        state.ssh_password = "abc".to_string();
        state.password_mode = PasswordMode::Enter;

        assert_eq!(password_display(&state), "******");
        state.set_focus(Focus::Password);
        assert_eq!(password_display(&state), "***");

        let geometry = PopupGeometry::from_terminal_size((80, 24), &state);
        let (x, _) = cursor_position(geometry.details, &state).unwrap();
        assert_eq!(x, geometry.details.x + 47);

        assert_eq!(
            state.apply_key(KeyEvent::from(KeyCode::Backspace)),
            PaneAction::None
        );
        assert_eq!(password_display(&state), "**");
    }

    #[test]
    fn editing_sudo_password_mask_tracks_short_password_length() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles.clear();
        state.selected = 0;
        let _ = state.sync_selected_profile();
        state.sudo_password = "abc".to_string();
        state.sudo_mode = SudoMode::Replace;

        assert_eq!(sudo_password_display(&state), "******");
        state.set_focus(Focus::Sudo);
        assert_eq!(sudo_password_display(&state), "***");

        let geometry = PopupGeometry::from_terminal_size((80, 24), &state);
        let (x, _) = cursor_position(geometry.details, &state).unwrap();
        assert_eq!(x, geometry.details.x + 47);

        assert_eq!(
            state.apply_key(KeyEvent::from(KeyCode::Backspace)),
            PaneAction::None
        );
        assert_eq!(sudo_password_display(&state), "**");
    }

    #[test]
    fn edit_backspace_accepts_terminal_control_h_encoding() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles.clear();
        state.selected = 0;
        let _ = state.sync_selected_profile();

        state.host = "abc".to_string();
        state.set_focus(Focus::Host);
        assert_eq!(
            state.apply_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL)),
            PaneAction::None
        );
        assert_eq!(state.host, "ab");

        state.ssh_password = "secret".to_string();
        state.set_focus(Focus::Password);
        assert_eq!(
            state.apply_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL)),
            PaneAction::None
        );
        assert_eq!(state.ssh_password, "secre");

        state.sudo_password = "rootpw".to_string();
        state.sudo_mode = SudoMode::Replace;
        state.set_focus(Focus::Sudo);
        assert_eq!(
            state.apply_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL)),
            PaneAction::None
        );
        assert_eq!(state.sudo_password, "rootp");
    }

    #[test]
    fn edit_backspace_accepts_terminal_del_character_encoding() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles.clear();
        state.selected = 0;
        let _ = state.sync_selected_profile();
        state.host = "abc".to_string();
        state.set_focus(Focus::Host);

        assert_eq!(
            state.apply_key(KeyEvent::from(KeyCode::Char('\u{7f}'))),
            PaneAction::None
        );

        assert_eq!(state.host, "ab");
    }

    #[test]
    fn proxy_input_left_right_moves_cursor_before_focus_navigation() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles.clear();
        state.selected = state.proxy_selection_index();
        state.proxy_draft.all_proxy = "socks5://10.1.29.96:7897".to_string();
        state.set_focus(Focus::AllProxy);

        assert_eq!(state.editing, Some(EditField::AllProxy));
        assert_eq!(
            state.edit_cursor,
            state.proxy_draft.all_proxy.chars().count()
        );

        assert_eq!(
            state.apply_key(KeyEvent::from(KeyCode::Left)),
            PaneAction::None
        );
        assert_eq!(state.focus, Focus::AllProxy);
        assert_eq!(
            state.edit_cursor,
            state.proxy_draft.all_proxy.chars().count() - 1
        );

        assert_eq!(
            state.apply_key(KeyEvent::from(KeyCode::Right)),
            PaneAction::None
        );
        assert_eq!(state.focus, Focus::AllProxy);
        assert_eq!(
            state.edit_cursor,
            state.proxy_draft.all_proxy.chars().count()
        );

        for _ in 0..state.proxy_draft.all_proxy.chars().count() {
            state.apply_key(KeyEvent::from(KeyCode::Left));
        }
        assert_eq!(state.focus, Focus::AllProxy);
        assert_eq!(state.edit_cursor, 0);

        assert_eq!(
            state.apply_key(KeyEvent::from(KeyCode::Left)),
            PaneAction::None
        );
        assert_eq!(state.focus, Focus::Hosts);
    }

    #[test]
    fn text_input_inserts_and_deletes_at_cursor() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles.clear();
        state.selected = state.proxy_selection_index();
        state.proxy_draft.https_proxy = "ab好d".to_string();
        state.set_focus(Focus::HttpsProxy);

        state.apply_key(KeyEvent::from(KeyCode::Left));
        assert_eq!(state.edit_cursor, 3);
        assert_eq!(
            state.apply_key(KeyEvent::from(KeyCode::Char('c'))),
            PaneAction::None
        );
        assert_eq!(state.proxy_draft.https_proxy, "ab好cd");
        assert_eq!(state.edit_cursor, 4);

        assert_eq!(
            state.apply_key(KeyEvent::from(KeyCode::Backspace)),
            PaneAction::None
        );
        assert_eq!(state.proxy_draft.https_proxy, "ab好d");
        assert_eq!(state.edit_cursor, 3);
    }

    #[test]
    fn edit_enter_moves_to_next_focus_item() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles.clear();
        state.selected = 0;
        let _ = state.sync_selected_profile();

        state.set_focus(Focus::Host);
        assert_eq!(state.editing, Some(EditField::Host));
        assert_eq!(
            state.apply_key(KeyEvent::from(KeyCode::Enter)),
            PaneAction::None
        );
        assert_eq!(state.focus, Focus::Port);

        state.set_focus(Focus::Password);
        assert_eq!(state.editing, Some(EditField::SshPassword));
        assert_eq!(
            state.apply_key(KeyEvent::from(KeyCode::Enter)),
            PaneAction::None
        );
        assert_eq!(state.focus, Focus::Sudo);
        assert_eq!(state.editing, Some(EditField::SudoPassword));

        assert_eq!(
            state.apply_key(KeyEvent::from(KeyCode::Enter)),
            PaneAction::None
        );
        assert_eq!(state.focus, Focus::Remember);
    }

    #[test]
    fn password_visibility_toggles_are_not_in_default_focus_order() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles.clear();
        state.selected = 0;
        let _ = state.sync_selected_profile();
        state.set_focus(Focus::Auth);

        assert_eq!(
            state.apply_key(KeyEvent::from(KeyCode::Down)),
            PaneAction::None
        );
        assert_eq!(state.focus, Focus::Password);
        assert_eq!(
            state.apply_key(KeyEvent::from(KeyCode::Down)),
            PaneAction::None
        );
        assert_eq!(state.focus, Focus::Sudo);
        assert_eq!(
            state.apply_key(KeyEvent::from(KeyCode::Up)),
            PaneAction::None
        );
        assert_eq!(state.focus, Focus::Password);
    }

    #[test]
    fn password_field_focus_has_cursor_and_space_toggles_visibility() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles.clear();
        state.selected = 0;
        let _ = state.sync_selected_profile();
        state.ssh_password = "secret".to_string();
        state.password_mode = PasswordMode::Saved;
        state.set_focus(Focus::Password);
        let geometry = PopupGeometry::from_terminal_size((80, 24), &state);

        let (x, y) = cursor_position(geometry.details, &state).unwrap();
        let details = DetailsGeometry::from_area(geometry.details, &state);

        assert_eq!(state.editing, Some(EditField::SshPassword));
        assert_eq!(y, geometry.details.y + details.rows.password);
        assert_eq!(x, geometry.details.x + 47);
        assert_eq!(
            state.apply_key(KeyEvent::from(KeyCode::Char(' '))),
            PaneAction::None
        );
        assert!(state.show_ssh_password);
        assert_eq!(password_display(&state), "secret");
        assert_eq!(state.focus, Focus::Password);
        assert_eq!(state.editing, Some(EditField::SshPassword));
    }

    #[test]
    fn sudo_field_focus_has_cursor_and_space_toggles_visibility() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles.clear();
        state.selected = 0;
        let _ = state.sync_selected_profile();
        state.ssh_password = "secret".to_string();
        state.password_mode = PasswordMode::Enter;
        state.set_focus(Focus::Sudo);
        let geometry = PopupGeometry::from_terminal_size((80, 24), &state);

        let (x, y) = cursor_position(geometry.details, &state).unwrap();
        let details = DetailsGeometry::from_area(geometry.details, &state);

        assert_eq!(state.editing, Some(EditField::SudoPassword));
        assert_eq!(y, geometry.details.y + details.rows.sudo);
        assert_eq!(x, geometry.details.x + 47);
        assert_eq!(
            state.apply_key(KeyEvent::from(KeyCode::Char(' '))),
            PaneAction::None
        );
        assert!(state.show_sudo_password);
        assert_eq!(sudo_password_display(&state), "secret");
        assert_eq!(state.focus, Focus::Sudo);
        assert_eq!(state.editing, Some(EditField::SudoPassword));
    }

    #[test]
    fn password_row_click_focuses_without_toggling_visibility() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles.clear();
        state.selected = 0;
        let _ = state.sync_selected_profile();
        state.ssh_password = "secret".to_string();
        state.password_mode = PasswordMode::Saved;
        let geometry = PopupGeometry::from_terminal_size((80, 24), &state);
        let details = DetailsGeometry::from_area(geometry.details, &state);

        state.apply_mouse(
            crossterm::event::MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: geometry.details.x + geometry.details.width - 10,
                row: geometry.details.y + details.rows.password,
                modifiers: crossterm::event::KeyModifiers::empty(),
            },
            (80, 24),
        );

        assert!(!state.show_ssh_password);
        assert_eq!(password_display(&state), "******");
        assert_eq!(state.focus, Focus::Password);
        assert_eq!(state.editing, Some(EditField::SshPassword));
    }

    #[test]
    fn saved_password_cursor_uses_masked_display_width() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles.clear();
        state.selected = 0;
        let _ = state.sync_selected_profile();
        state.ssh_password = "secret".to_string();
        state.password_mode = PasswordMode::Saved;
        state.set_focus(Focus::Password);
        let geometry = PopupGeometry::from_terminal_size((80, 24), &state);

        let (x, y) = cursor_position(geometry.details, &state).unwrap();
        let details = DetailsGeometry::from_area(geometry.details, &state);

        assert_eq!(password_display(&state), "******");
        assert_eq!(y, geometry.details.y + details.rows.password);
        assert_eq!(x, geometry.details.x + 47);
    }

    #[test]
    fn connect_popup_password_cursor_stays_on_visible_row_for_long_password() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles.clear();
        state.selected = 0;
        let _ = state.sync_selected_profile();
        state.ssh_password = "x".repeat(120);
        state.set_focus(Focus::Password);
        let geometry = PopupGeometry::from_terminal_size((80, 24), &state);

        let (x, y) = cursor_position(geometry.details, &state).unwrap();

        let details = DetailsGeometry::from_area(geometry.details, &state);
        assert_eq!(y, geometry.details.y + details.rows.password);
        assert!(x < geometry.details.x + geometry.details.width);
    }

    #[test]
    fn connect_popup_sudo_cursor_stays_on_visible_row() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles.clear();
        state.selected = 0;
        let _ = state.sync_selected_profile();
        state.sudo_mode = SudoMode::Replace;
        state.start_edit(EditField::SudoPassword);
        let geometry = PopupGeometry::from_terminal_size((80, 24), &state);

        let (_x, y) = cursor_position(geometry.details, &state).unwrap();

        let details = DetailsGeometry::from_area(geometry.details, &state);
        assert_eq!(y, geometry.details.y + details.rows.sudo);
    }

    #[test]
    fn focused_buttons_use_plain_labels() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles.clear();
        state.selected = 0;
        let _ = state.sync_selected_profile();
        state.focus = Focus::Connect;
        assert_eq!(connect_label(&state), "Connect");

        state.profiles = vec![saved_password_profile()];
        state.selected = 0;
        let _ = state.sync_selected_profile();
        state.set_focus(Focus::Delete);
        assert_eq!(delete_label(&state), "Delete");
    }

    #[test]
    fn choice_selection_uses_different_styles_for_focused_and_inactive_selection() {
        let selected = vec![ChoiceSegment {
            label: "Password",
            selected: true,
        }];

        let focused = choice_line(selected.clone(), true);
        let inactive = choice_line(selected, false);

        assert_eq!(focused.spans[0].content.as_ref(), "Password");
        assert_eq!(inactive.spans[0].content.as_ref(), "Password");
        assert_eq!(focused.spans[0].style, active_focus_style());
        assert_eq!(inactive.spans[0].style, selected_host_style());
    }

    #[test]
    fn choice_selection_uses_plain_labels_without_focus() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles.clear();
        state.selected = 0;
        let _ = state.sync_selected_profile();
        state.focus = Focus::Hosts;
        state.auth = AuthChoice::Password;

        assert_eq!(segmented_for_test(&auth_tabs(&state)), "Password  Key");
    }

    #[test]
    fn sudo_defaults_to_ssh_password_mask_and_editing_makes_it_custom() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles.clear();
        state.selected = 0;
        let _ = state.sync_selected_profile();
        state.ssh_password = "ssh-secret".to_string();
        state.password_mode = PasswordMode::Enter;
        state.sudo_mode = SudoMode::SameAsSsh;
        state.set_focus(Focus::Sudo);

        assert_eq!(sudo_password_display(&state), "**********");
        assert_eq!(state.editing, Some(EditField::SudoPassword));
        assert_eq!(state.sudo_mode, SudoMode::Replace);
        assert_eq!(state.sudo_password, "ssh-secret");
        assert_eq!(
            state.apply_key(KeyEvent::from(KeyCode::Enter)),
            PaneAction::None
        );
        assert_eq!(state.focus, Focus::Remember);
    }

    #[test]
    fn connect_error_popup_blocks_actions_until_dismissed() {
        let mut state = ConnectRemoteHostState::load();
        state.focus = Focus::Connect;
        state.status = Status::Error("Connect failed: long diagnostic".to_string());

        assert_eq!(
            state.apply_key(KeyEvent::from(KeyCode::Enter)),
            PaneAction::None
        );
        assert!(matches!(state.status, Status::Hint(_)));

        state.status = Status::Error("Connect failed again".to_string());
        assert_eq!(
            state.apply_key(KeyEvent::from(KeyCode::Char('q'))),
            PaneAction::None
        );
        assert!(matches!(state.status, Status::Hint(_)));
    }

    #[test]
    fn connect_error_popup_ignores_connect_activation() {
        let mut state = ConnectRemoteHostState::load();
        state.focus = Focus::Connect;
        state.status = Status::Error("Connect failed".to_string());

        assert_eq!(
            state.apply_key(KeyEvent::from(KeyCode::Char('x'))),
            PaneAction::None
        );
        assert!(matches!(state.status, Status::Error(_)));
    }

    #[test]
    fn connect_popup_ignores_connect_action_while_working() {
        let mut state = ConnectRemoteHostState::load();
        state.focus = Focus::Connect;
        state.status = Status::Working("Connecting...".to_string());

        assert_eq!(
            state.apply_key(KeyEvent::from(KeyCode::Enter)),
            PaneAction::None
        );
        assert_eq!(connect_label(&state), "Connect");
    }

    #[test]
    fn connect_popup_arrow_keys_move_saved_host_selection() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles = vec![
            RemoteHostProfile {
                name: "a@127.0.0.1".to_string(),
                host: "127.0.0.1".to_string(),
                ssh_user: "a".to_string(),
                auth: RemoteHostAuthProfile::Key {
                    key_path: std::path::PathBuf::from("~/.ssh/id_rsa"),
                },
                sudo_password_secret_id: None,
                preferred_remote_port: RemotePortPreference::Auto,
                ssh_port: None,
                last_remote_port: Some(7474),
                last_endpoint: None,
                last_connected_at: None,
                use_install_proxy: true,
                ..RemoteHostProfile::default()
            },
            RemoteHostProfile {
                name: "b@127.0.0.1".to_string(),
                host: "127.0.0.1".to_string(),
                ssh_user: "b".to_string(),
                auth: RemoteHostAuthProfile::Key {
                    key_path: std::path::PathBuf::from("~/.ssh/id_rsa"),
                },
                sudo_password_secret_id: None,
                preferred_remote_port: RemotePortPreference::Auto,
                ssh_port: None,
                last_remote_port: Some(7575),
                last_endpoint: None,
                last_connected_at: None,
                use_install_proxy: true,
                ..RemoteHostProfile::default()
            },
        ];
        state.focus = Focus::Hosts;
        state.selected = state.profiles.len();
        let _ = state.sync_selected_profile();

        state.apply_key(KeyEvent::from(KeyCode::Up));
        assert_eq!(state.selected, 1);
        assert_eq!(state.ssh_user, "b");
        state.apply_key(KeyEvent::from(KeyCode::Up));
        assert_eq!(state.selected, 0);
        assert_eq!(state.ssh_user, "a");
        state.apply_key(KeyEvent::from(KeyCode::Down));
        assert_eq!(state.selected, 1);
        assert_eq!(state.ssh_user, "b");
    }

    #[test]
    fn cloud_host_kind_forces_key_auth_and_hides_password_sudo() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles.clear();
        state.selected = 0;
        let _ = state.sync_selected_profile();
        state.host = "cloud.example.com".to_string();
        state.ssh_user = "k".to_string();
        state.auth = AuthChoice::Password;
        state.sudo_mode = SudoMode::SameAsSsh;
        state.set_focus(Focus::HostKind);

        assert_eq!(state.host_kind, RemoteHostKind::Lan);
        state.adjust_choice(1);
        assert_eq!(state.host_kind, RemoteHostKind::Cloud);
        assert_eq!(state.auth, AuthChoice::Key);
        assert_eq!(state.sudo_mode, SudoMode::None);

        state.adjust_choice(1);
        assert_eq!(state.host_kind, RemoteHostKind::Lan);
    }

    #[test]
    fn validate_rejects_cloud_host_with_password_auth() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles.clear();
        state.selected = 0;
        let _ = state.sync_selected_profile();
        state.host = "cloud.example.com".to_string();
        state.ssh_user = "k".to_string();
        state.host_kind = RemoteHostKind::Cloud;
        state.auth = AuthChoice::Password;
        state.ssh_password = "secret".to_string();

        assert!(validate(&state).is_err());

        state.auth = AuthChoice::Key;
        state.key_path = "/home/k/.ssh/id_rsa".to_string();
        assert!(validate(&state).is_ok());
    }

    #[test]
    fn profile_matches_state_includes_host_kind() {
        let mut state = ConnectRemoteHostState::load();
        let profile = RemoteHostProfile {
            name: "k@127.0.0.1".to_string(),
            host: "127.0.0.1".to_string(),
            ssh_user: "k".to_string(),
            auth: RemoteHostAuthProfile::Key {
                key_path: std::path::PathBuf::from("~/.ssh/id_rsa"),
            },
            sudo_password_secret_id: None,
            preferred_remote_port: RemotePortPreference::Auto,
            ssh_port: None,
            last_remote_port: Some(7575),
            last_endpoint: None,
            last_connected_at: None,
            use_install_proxy: true,
            host_kind: RemoteHostKind::Cloud,
            ..RemoteHostProfile::default()
        };
        state.profiles = vec![profile.clone()];
        state.selected = 0;
        let _ = state.sync_selected_profile();

        assert!(profile_matches_state(&profile, &state));

        state.host_kind = RemoteHostKind::Lan;
        assert!(!profile_matches_state(&profile, &state));
    }

    #[test]
    fn profile_matches_state_includes_ssh_port() {
        let mut state = ConnectRemoteHostState::load();
        let profile = RemoteHostProfile {
            name: "root@117.157.77.4".to_string(),
            host: "117.157.77.4".to_string(),
            ssh_user: "root".to_string(),
            auth: RemoteHostAuthProfile::Password {
                password_secret_id: None,
            },
            sudo_password_secret_id: None,
            preferred_remote_port: RemotePortPreference::Auto,
            ssh_port: Some(50045),
            last_remote_port: None,
            last_endpoint: None,
            last_connected_at: None,
            use_install_proxy: true,
            host_kind: RemoteHostKind::Lan,
            ..RemoteHostProfile::default()
        };
        state.profiles = vec![profile.clone()];
        state.selected = 0;
        let _ = state.sync_selected_profile();
        state.ssh_port = "50045".to_string();

        assert!(profile_matches_state(&profile, &state));

        state.ssh_port = "22".to_string();
        assert!(!profile_matches_state(&profile, &state));
    }

    #[test]
    fn ensure_connectable_profile_saves_host_port_input_as_ssh_port() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles.clear();
        state.selected = 0;
        let _ = state.sync_selected_profile();
        state.set_focus(Focus::Host);
        for ch in "117.157.77.4:50045".chars() {
            state.apply_key(KeyEvent::from(KeyCode::Char(ch)));
        }
        state.apply_key(KeyEvent::from(KeyCode::Enter));
        state.ssh_user = "root".to_string();
        state.auth = AuthChoice::Key;
        state.key_path = "/home/k/.ssh/id_rsa".to_string();
        state.sudo_mode = SudoMode::None;

        let secret_store = test_secret_store();
        let (history_store, temp_dir) = test_history_store();

        let profile = ensure_connectable_profile(&state, &secret_store, &history_store).unwrap();

        assert_eq!(profile.host, "117.157.77.4");
        assert_eq!(profile.ssh_port, Some(50045));
        assert_eq!(profile.preferred_remote_port, RemotePortPreference::Auto);

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn host_kind_tabs_reflect_state() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles.clear();
        state.selected = 0;
        let _ = state.sync_selected_profile();

        state.host_kind = RemoteHostKind::Lan;
        assert_eq!(segmented_for_test(&host_kind_tabs(&state)), "LAN  Cloud");

        state.host_kind = RemoteHostKind::Cloud;
        let tabs = host_kind_tabs(&state);
        assert!(tabs[0].label == "LAN" && !tabs[0].selected);
        assert!(tabs[1].label == "Cloud" && tabs[1].selected);
    }

    #[test]
    fn via_tabs_reflect_state() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles.clear();
        state.selected = 0;
        let _ = state.sync_selected_profile();
        assert_eq!(state.via, RemoteNodeVia::Auto);

        state.via = RemoteNodeVia::Auto;
        assert_eq!(segmented_for_test(&via_tabs(&state)), "Auto  Direct  Relay");

        state.via = RemoteNodeVia::Relay;
        let tabs = via_tabs(&state);
        assert!(!tabs[0].selected && !tabs[1].selected && tabs[2].selected);
    }

    #[test]
    fn via_choice_cycles_with_horizontal_keys_and_space() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles.clear();
        state.selected = 0;
        let _ = state.sync_selected_profile();
        state.set_focus(Focus::Via);
        assert_eq!(state.via, RemoteNodeVia::Auto);

        state.adjust_choice(1);
        assert_eq!(state.via, RemoteNodeVia::Direct);
        state.adjust_choice(1);
        assert_eq!(state.via, RemoteNodeVia::Relay);
        state.adjust_choice(1);
        assert_eq!(state.via, RemoteNodeVia::Auto);
        state.adjust_choice(-1);
        assert_eq!(state.via, RemoteNodeVia::Relay);

        state.apply_key(KeyEvent::from(KeyCode::Char(' ')));
        assert_eq!(state.via, RemoteNodeVia::Auto);
    }

    #[test]
    fn sync_selected_profile_syncs_via_and_defaults_new_entries_to_auto() {
        // Build the profile explicitly: `load()` reads the real waitagent
        // home, which has no profiles in CI (an empty Vec would panic on
        // indexing), so tests must not depend on the user's history file.
        let mut state = ConnectRemoteHostState::load();
        let profile = RemoteHostProfile {
            name: "k@127.0.0.1".to_string(),
            host: "127.0.0.1".to_string(),
            ssh_user: "k".to_string(),
            auth: RemoteHostAuthProfile::Key {
                key_path: std::path::PathBuf::from("~/.ssh/id_rsa"),
            },
            sudo_password_secret_id: None,
            preferred_remote_port: RemotePortPreference::Auto,
            ssh_port: None,
            last_remote_port: None,
            last_endpoint: None,
            last_connected_at: None,
            use_install_proxy: true,
            tls_pin_sha256: None,
            host_kind: RemoteHostKind::Lan,
            remote_shell: None,
            via: Some("relay".to_string()),
            last_via_used: None,
        };
        state.profiles = vec![profile];
        state.selected = 0;
        let _ = state.sync_selected_profile();
        assert_eq!(state.via, RemoteNodeVia::Relay);

        // New Host row resets the choice to the auto default.
        state.selected = state.profiles.len();
        let _ = state.sync_selected_profile();
        assert_eq!(state.via, RemoteNodeVia::Auto);
    }

    #[test]
    fn via_badge_names_explicit_choice_and_auto_effective_path() {
        let mut profile = RemoteHostProfile {
            via: Some("relay".to_string()),
            ..RemoteHostProfile::default()
        };
        assert_eq!(
            via_badge(Some(&profile)).map(|span| span.content.to_string()),
            Some("via relay".to_string())
        );

        profile.via = Some("direct".to_string());
        assert_eq!(
            via_badge(Some(&profile)).map(|span| span.content.to_string()),
            Some("via direct".to_string())
        );

        profile.via = Some("auto".to_string());
        assert_eq!(
            via_badge(Some(&profile)).map(|span| span.content.to_string()),
            Some("via auto".to_string())
        );

        profile.last_via_used = Some("relay".to_string());
        assert_eq!(
            via_badge(Some(&profile)).map(|span| span.content.to_string()),
            Some("via auto → relay".to_string())
        );

        profile.last_via_used = Some("direct".to_string());
        assert_eq!(
            via_badge(Some(&profile)).map(|span| span.content.to_string()),
            Some("via auto → direct".to_string())
        );

        assert!(via_badge(None).is_none());
    }

    #[test]
    fn ensure_connectable_profile_persists_via_choice_and_preserves_memory() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles.clear();
        state.selected = 0;
        let _ = state.sync_selected_profile();
        state.host = "192.168.1.21".to_string();
        state.ssh_user = "k".to_string();
        state.ssh_password = "ssh-secret".to_string();
        state.password_mode = PasswordMode::Enter;
        state.sudo_mode = SudoMode::None;
        state.via = RemoteNodeVia::Relay;

        let secret_store = test_secret_store();
        let (history_store, temp_dir) = test_history_store();

        let profile = ensure_connectable_profile(&state, &secret_store, &history_store).unwrap();
        assert_eq!(profile.via(), RemoteNodeVia::Relay);

        // An auto profile with a remembered effective path keeps both: the
        // user's choice and the annotation.
        let mut auto = state.clone();
        auto.via = RemoteNodeVia::Auto;
        let stored = RemoteHostProfile {
            name: "k@192.168.1.21".to_string(),
            ..profile.clone()
        };
        let stored = {
            let mut s = stored;
            s.via = Some("auto".to_string());
            s.last_via_used = Some("relay".to_string());
            s
        };
        auto.profiles = vec![stored];
        auto.selected = 0;
        let _ = auto.sync_selected_profile();
        let kept = ensure_connectable_profile(&auto, &secret_store, &history_store).unwrap();
        assert_eq!(kept.via(), RemoteNodeVia::Auto);
        assert_eq!(kept.last_via_used(), Some(RemoteNodeVia::Relay));

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn profile_matches_state_includes_via() {
        // Explicit profile construction: see
        // sync_selected_profile_syncs_via_and_defaults_new_entries_to_auto
        // for why tests must not index the user's loaded history.
        let mut state = ConnectRemoteHostState::load();
        let profile = RemoteHostProfile {
            name: "k@127.0.0.1".to_string(),
            host: "127.0.0.1".to_string(),
            ssh_user: "k".to_string(),
            auth: RemoteHostAuthProfile::Key {
                key_path: std::path::PathBuf::from("~/.ssh/id_rsa"),
            },
            sudo_password_secret_id: None,
            preferred_remote_port: RemotePortPreference::Auto,
            ssh_port: None,
            last_remote_port: None,
            last_endpoint: None,
            last_connected_at: None,
            use_install_proxy: true,
            tls_pin_sha256: None,
            host_kind: RemoteHostKind::Lan,
            remote_shell: None,
            via: Some("relay".to_string()),
            last_via_used: None,
        };
        state.profiles = vec![profile.clone()];
        state.selected = 0;
        let _ = state.sync_selected_profile();

        assert!(profile_matches_state(&profile, &state));

        state.via = RemoteNodeVia::Auto;
        assert!(!profile_matches_state(&profile, &state));
    }

    fn test_secret_store() -> crate::host::ssh::remote_host_secret_store::MemoryRemoteHostSecretStore
    {
        crate::host::ssh::remote_host_secret_store::MemoryRemoteHostSecretStore::default()
    }

    fn test_history_store() -> (RemoteHostHistoryStore, std::path::PathBuf) {
        let temp_dir = std::env::temp_dir().join(format!(
            "waitagent-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let path = temp_dir.join("remote-hosts.toml");
        (RemoteHostHistoryStore::new(&path), temp_dir)
    }

    #[test]
    fn ensure_connectable_profile_saves_new_host_password_and_same_as_ssh_sudo() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles.clear();
        state.selected = 0;
        let _ = state.sync_selected_profile();
        state.host = "192.168.1.11".to_string();
        state.ssh_user = "k".to_string();
        state.ssh_password = "ssh-secret".to_string();
        state.password_mode = PasswordMode::Enter;
        state.sudo_mode = SudoMode::SameAsSsh;

        let secret_store = test_secret_store();
        let (history_store, temp_dir) = test_history_store();

        let profile = ensure_connectable_profile(&state, &secret_store, &history_store).unwrap();

        assert_eq!(profile.name, "k@192.168.1.11");
        assert_eq!(profile.host, "192.168.1.11");
        assert_eq!(profile.ssh_user, "k");
        assert_eq!(profile.host_kind, RemoteHostKind::Lan);
        assert!(
            matches!(profile.auth, RemoteHostAuthProfile::Password { .. }),
            "expected password auth"
        );

        let ssh_id = crate::host::ssh::remote_host_secret_store::RemoteHostSecretId::new(
            "waitagent.remote-host.k-192-168-1-11.ssh-password",
        )
        .unwrap();
        let stored = secret_store
            .get_secret(&ssh_id)
            .unwrap()
            .expect("ssh secret saved");
        assert_eq!(stored.expose_secret(), "ssh-secret");
        assert_eq!(profile.sudo_password_secret_id, Some(ssh_id));

        let history = history_store.load().unwrap();
        assert_eq!(history.hosts.len(), 1);
        assert_eq!(history.hosts[0].name, "k@192.168.1.11");

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn ensure_connectable_profile_saves_new_host_with_replace_sudo_password() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles.clear();
        state.selected = 0;
        let _ = state.sync_selected_profile();
        state.host = "192.168.1.12".to_string();
        state.ssh_user = "k".to_string();
        state.ssh_password = "ssh-secret".to_string();
        state.password_mode = PasswordMode::Enter;
        state.sudo_password = "sudo-secret".to_string();
        state.sudo_mode = SudoMode::Replace;

        let secret_store = test_secret_store();
        let (history_store, temp_dir) = test_history_store();

        let profile = ensure_connectable_profile(&state, &secret_store, &history_store).unwrap();

        let ssh_id = crate::host::ssh::remote_host_secret_store::RemoteHostSecretId::new(
            "waitagent.remote-host.k-192-168-1-12.ssh-password",
        )
        .unwrap();
        let sudo_id = crate::host::ssh::remote_host_secret_store::RemoteHostSecretId::new(
            "waitagent.remote-host.k-192-168-1-12.sudo-password",
        )
        .unwrap();

        assert_eq!(
            profile.auth,
            RemoteHostAuthProfile::Password {
                password_secret_id: Some(ssh_id.clone()),
            }
        );
        assert_eq!(profile.sudo_password_secret_id, Some(sudo_id.clone()));

        assert_eq!(
            secret_store
                .get_secret(&ssh_id)
                .unwrap()
                .unwrap()
                .expose_secret(),
            "ssh-secret"
        );
        assert_eq!(
            secret_store
                .get_secret(&sudo_id)
                .unwrap()
                .unwrap()
                .expose_secret(),
            "sudo-secret"
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn ensure_connectable_profile_saves_new_cloud_host_with_key_auth() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles.clear();
        state.selected = 0;
        let _ = state.sync_selected_profile();
        state.host = "cloud.example.com".to_string();
        state.ssh_user = "k".to_string();
        state.host_kind = RemoteHostKind::Cloud;
        state.auth = AuthChoice::Key;
        state.key_path = "/home/k/.ssh/cloud".to_string();
        state.sudo_mode = SudoMode::None;

        let secret_store = test_secret_store();
        let (history_store, temp_dir) = test_history_store();

        let profile = ensure_connectable_profile(&state, &secret_store, &history_store).unwrap();

        assert_eq!(profile.name, "k@cloud.example.com");
        assert_eq!(profile.host_kind, RemoteHostKind::Cloud);
        assert_eq!(
            profile.auth,
            RemoteHostAuthProfile::Key {
                key_path: std::path::PathBuf::from("/home/k/.ssh/cloud"),
            }
        );
        assert_eq!(profile.sudo_password_secret_id, None);

        let history = history_store.load().unwrap();
        assert_eq!(history.hosts.len(), 1);
        assert_eq!(history.hosts[0].host_kind, RemoteHostKind::Cloud);

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn ensure_connectable_profile_returns_existing_connectable_profile_unchanged() {
        let existing = RemoteHostProfile {
            name: "k@192.168.1.13".to_string(),
            host: "192.168.1.13".to_string(),
            ssh_user: "k".to_string(),
            auth: RemoteHostAuthProfile::Key {
                key_path: std::path::PathBuf::from("/home/k/.ssh/id_rsa"),
            },
            sudo_password_secret_id: None,
            preferred_remote_port: RemotePortPreference::Auto,
            ssh_port: None,
            last_remote_port: None,
            last_endpoint: None,
            last_connected_at: None,
            use_install_proxy: true,
            tls_pin_sha256: None,
            host_kind: RemoteHostKind::Lan,
            remote_shell: None,
            via: None,
            last_via_used: None,
        };

        let mut state = ConnectRemoteHostState::load();
        state.profiles = vec![existing.clone()];
        state.selected = 0;
        let _ = state.sync_selected_profile();
        assert!(saved_profile_can_connect_by_id(
            &state,
            state.selected_profile().unwrap()
        ));

        let secret_store = test_secret_store();
        let (history_store, temp_dir) = test_history_store();

        let profile = ensure_connectable_profile(&state, &secret_store, &history_store).unwrap();

        assert_eq!(profile, existing);
        assert!(history_store.load().unwrap().hosts.is_empty());

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn ensure_connectable_profile_preserves_connection_metadata_on_reauth() {
        let existing = RemoteHostProfile {
            name: "k@192.168.1.14".to_string(),
            host: "192.168.1.14".to_string(),
            ssh_user: "k".to_string(),
            auth: RemoteHostAuthProfile::Password {
                password_secret_id: Some(
                    crate::host::ssh::remote_host_secret_store::RemoteHostSecretId::new(
                        "waitagent.remote-host.k-192-168-1-14.ssh-password",
                    )
                    .unwrap(),
                ),
            },
            sudo_password_secret_id: None,
            preferred_remote_port: RemotePortPreference::Auto,
            ssh_port: None,
            last_remote_port: Some(7474),
            last_endpoint: Some("192.168.1.14:7474".to_string()),
            last_connected_at: None,
            use_install_proxy: true,
            tls_pin_sha256: Some("deadbeef".to_string()),
            host_kind: RemoteHostKind::Lan,
            remote_shell: None,
            via: None,
            last_via_used: None,
        };

        let mut state = ConnectRemoteHostState::load();
        state.profiles = vec![existing.clone()];
        state.selected = 0;
        let _ = state.sync_selected_profile();
        // Simulate credentials not being available in the keyring: the user must
        // re-enter the password, so saved_profile_can_connect_by_id returns false.
        state.password_mode = PasswordMode::Enter;
        state.ssh_password = "re-entered-secret".to_string();
        assert!(!saved_profile_can_connect_by_id(
            &state,
            state.selected_profile().unwrap()
        ));

        let secret_store = test_secret_store();
        let (history_store, temp_dir) = test_history_store();

        let profile = ensure_connectable_profile(&state, &secret_store, &history_store).unwrap();

        assert_eq!(profile.last_remote_port, Some(7474));
        assert_eq!(profile.last_endpoint, Some("192.168.1.14:7474".to_string()));
        assert_eq!(profile.tls_pin_sha256, Some("deadbeef".to_string()));

        let history = history_store.load().unwrap();
        assert_eq!(history.hosts.len(), 1);
        assert_eq!(history.hosts[0].last_remote_port, Some(7474));
        assert_eq!(
            history.hosts[0].last_endpoint,
            Some("192.168.1.14:7474".to_string())
        );
        assert_eq!(
            history.hosts[0].tls_pin_sha256,
            Some("deadbeef".to_string())
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    fn relay_pin() -> RelayTomlConfig {
        RelayTomlConfig {
            address: "relay.example:7475".to_string(),
            relay_fingerprint: "ab".repeat(32),
            heartbeat_interval_secs: None,
        }
    }

    fn key_event(code: KeyCode) -> crossterm::event::KeyEvent {
        crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE)
    }

    #[test]
    fn relay_selection_index_follows_the_proxy_section() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles = vec![saved_password_profile()];
        state.proxy_settings.profiles = vec![blank_proxy_profile()];
        state.proxy_settings.profiles[0].name = "corp".to_string();

        // Selection space: [0]=host, [1]=new host, [2]=proxy, [3]=new proxy,
        // [4]=relay.
        let relay_index = state.relay_selection_index();
        assert_eq!(relay_index, 4);
        state.selected = relay_index;
        assert!(state.selected_relay_config());
        assert!(!state.selected_proxy_config());

        // Arrow-down walks every slot and stops at the relay entry.
        state.selected = 0;
        state.focus = Focus::Hosts;
        for expected in 1..=relay_index {
            let _ = state.move_down();
            assert_eq!(state.selected, expected);
        }
        let _ = state.move_down();
        assert_eq!(
            state.selected, relay_index,
            "the relay entry is the last row"
        );
    }

    #[test]
    fn relay_page_tab_cycles_relay_focuses() {
        let mut state = ConnectRemoteHostState::load();
        state.relay = Some(relay_pin());
        state.selected = state.relay_selection_index();

        assert_eq!(state.default_detail_focus(), Focus::RelayAddress);
        state.set_focus(Focus::Hosts);
        assert_eq!(state.next_focus(), Focus::RelayAddress);
        state.set_focus(Focus::RelayAddress);
        assert_eq!(state.next_focus(), Focus::RelayToken);
        state.set_focus(Focus::RelayToken);
        assert_eq!(state.next_focus(), Focus::RelayJoin);
        state.set_focus(Focus::RelayJoin);
        assert_eq!(state.next_focus(), Focus::RelayRemove);
        state.set_focus(Focus::RelayRemove);
        assert_eq!(state.next_focus(), Focus::Hosts);
        state.set_focus(Focus::RelayToken);
        assert_eq!(state.prev_focus(), Focus::RelayAddress);
    }

    #[test]
    fn relay_right_arrow_only_reaches_remove_when_pinned() {
        let mut state = ConnectRemoteHostState::load();
        state.relay = Some(relay_pin());
        state.selected = state.relay_selection_index();
        state.set_focus(Focus::RelayJoin);
        let _ = state.apply_key(key_event(KeyCode::Right));
        assert_eq!(state.focus, Focus::RelayRemove);

        let mut unpinned = ConnectRemoteHostState::load();
        unpinned.selected = unpinned.relay_selection_index();
        unpinned.set_focus(Focus::RelayJoin);
        let _ = unpinned.apply_key(key_event(KeyCode::Right));
        assert_eq!(
            unpinned.focus,
            Focus::RelayJoin,
            "without a pin there is nothing to remove"
        );
    }

    #[test]
    fn relay_join_action_validates_the_draft() {
        let mut state = ConnectRemoteHostState::load();
        state.selected = state.relay_selection_index();
        state.set_focus(Focus::RelayJoin);
        let action = state.activate_focus();
        assert_eq!(action, PaneAction::None);
        assert!(matches!(&state.status, Status::Error(message) if message.contains("address")));

        state.relay_draft_address = "relay.example:7475".to_string();
        let action = state.activate_focus();
        assert_eq!(action, PaneAction::None);
        assert!(matches!(&state.status, Status::Error(message) if message.contains("token")));

        state.relay_draft_token = "invite-token".to_string();
        let action = state.activate_focus();
        assert_eq!(
            action,
            PaneAction::RelayJoin {
                address: "relay.example:7475".to_string(),
                token: "invite-token".to_string(),
                force: false,
            }
        );
    }

    #[test]
    fn relay_mismatch_prompt_defaults_to_abort() {
        let mut state = ConnectRemoteHostState::load();
        state.relay_draft_address = "relay.example:7475".to_string();
        state.relay_draft_token = "invite-token".to_string();
        state.relay_mismatch = RelayMismatchState::Prompt {
            message: "pin mismatch: ...".to_string(),
            focus: RelayMismatchFocus::Abort,
        };

        let _ = state.apply_key(key_event(KeyCode::Enter));
        assert_eq!(state.relay_mismatch, RelayMismatchState::Idle);

        state.relay_mismatch = RelayMismatchState::Prompt {
            message: "pin mismatch: ...".to_string(),
            focus: RelayMismatchFocus::SwitchAnyway,
        };
        let action = match state.apply_key(key_event(KeyCode::Enter)) {
            PaneAction::RelayJoin { force: true, .. } => true,
            other => panic!("expected the forced re-join, got {other:?}"),
        };
        assert!(action);
        assert_eq!(state.relay_mismatch, RelayMismatchState::Idle);

        state.relay_mismatch = RelayMismatchState::Prompt {
            message: "pin mismatch: ...".to_string(),
            focus: RelayMismatchFocus::SwitchAnyway,
        };
        let _ = state.apply_key(key_event(KeyCode::Esc));
        assert_eq!(state.relay_mismatch, RelayMismatchState::Idle);
    }

    #[test]
    fn relay_token_field_accepts_pasted_invite() {
        let mut state = ConnectRemoteHostState::load();
        state.selected = state.relay_selection_index();
        state.set_focus(Focus::RelayToken);
        assert_eq!(state.editing, Some(EditField::RelayToken));

        let _ = state.apply_paste("invite-token-from-clipboard\nignored-second-line");
        assert_eq!(state.relay_draft_token, "invite-token-from-clipboard");
    }

    #[test]
    fn relay_join_command_requires_the_embedded_runtime() {
        let answer = run_relay_join_command(None, "relay.example:7475", "token", false);
        assert!(
            matches!(answer, RelayNodeAnswer::Err(ref message) if message.contains("embedded console")),
            "unexpected answer: {answer:?}"
        );
        let error = run_relay_remove_command(None).expect_err("no node socket must fail");
        assert!(error.contains("embedded console"), "got: {error}");
    }

    #[test]
    fn relay_node_answer_typed_parsing() {
        let ok = serde_json::json!({"type": "Response", "payload": {"ok": true, "message": "relay joined: relay.example:7475 (fingerprint ab); link starting"}}).to_string();
        assert!(matches!(
            relay_node_answer(&ok, "pin mismatch:"),
            RelayNodeAnswer::Ok(ref message) if message.contains("relay joined")
        ));

        let mismatch = serde_json::json!({"type": "Response", "payload": {"ok": false, "message": "pin mismatch: relay at x presents y, but this node pinned z"}}).to_string();
        assert!(matches!(
            relay_node_answer(&mismatch, "pin mismatch:"),
            RelayNodeAnswer::PinMismatch(_)
        ));

        let refused = serde_json::json!({"type": "Response", "payload": {"ok": false, "message": "relay join failed: enrollment token was rejected"}}).to_string();
        assert!(matches!(
            relay_node_answer(&refused, "pin mismatch:"),
            RelayNodeAnswer::Err(ref message) if message.contains("enrollment token was rejected")
        ));

        assert!(matches!(
            relay_node_answer("OK relay removed", "pin mismatch:"),
            RelayNodeAnswer::Ok(_)
        ));
        assert!(matches!(
            relay_node_answer("ERR relay gone", "pin mismatch:"),
            RelayNodeAnswer::Err(ref message) if message == "relay gone"
        ));
        assert!(matches!(
            relay_node_answer("", "pin mismatch:"),
            RelayNodeAnswer::Err(ref message) if message.contains("empty response")
        ));
    }

    #[test]
    fn connect_popup_renders_relay_section_and_details() {
        let mut state = ConnectRemoteHostState::load();
        state.relay = Some(relay_pin());
        state.selected = state.relay_selection_index();
        state.set_focus(Focus::RelayAddress);

        let output = rendered_text(100, 34, &state);
        assert!(output.contains("Relay"), "relay header renders: {output}");
        assert!(
            output.contains("relay.example:7475"),
            "the pinned address renders in the sidebar: {output}"
        );
        assert!(
            output.contains("Join Relay"),
            "the join button renders: {output}"
        );
        assert!(
            output.contains("Remove Relay"),
            "the remove button renders for a pinned relay: {output}"
        );
        assert!(
            output.contains(&"ab".repeat(24)),
            "the pinned fingerprint renders (clipped to the card width): {output}"
        );
        assert!(
            output.contains("Invite Token"),
            "the token field renders: {output}"
        );
    }

    #[test]
    fn connect_popup_renders_no_relay_placeholder() {
        let mut state = ConnectRemoteHostState::load();
        state.relay = None;
        state.selected = state.relay_selection_index();

        let output = rendered_text(100, 34, &state);
        assert!(
            output.contains("no relay pinned"),
            "the empty state renders: {output}"
        );
        assert!(
            !output.contains("Remove Relay"),
            "without a pin there is no remove button: {output}"
        );
    }
}
