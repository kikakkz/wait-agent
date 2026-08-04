// Legacy tmux-era remote-host pane runtime kept during the ratatui migration; most items are currently unused.

use crate::cli::{prepend_global_network_args, ConnectRemoteHostPaneCommand, RemoteNetworkConfig};
use crate::host::ssh::remote_host_history_store::{
    RemoteHostAuthProfile, RemoteHostHistoryStore, RemoteHostProfile, RemotePortPreference,
};
use crate::host::ssh::remote_host_secret_store::{
    FileRemoteHostSecretStore, RemoteHostSecretStore,
};
use crate::host::ssh::remote_install_proxy_store::{
    no_proxy_for_install, RemoteInstallProxyProfile, RemoteInstallProxySettings,
    RemoteInstallProxyStore,
};
use crate::lifecycle::LifecycleError;
use crate::process::current_executable::current_waitagent_executable;
use crate::ratatui_node::node_runtime::ServerMessageJson;
use crossbeam_channel::{unbounded, Receiver as CrossbeamReceiver, Sender as CrossbeamSender};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Row, Table, Wrap};
use ratatui::{Frame, Terminal};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Clone)]
pub struct ConnectRemoteHostPaneRuntime {
    network: RemoteNetworkConfig,
    ratatui_port: Option<u16>,
    ratatui_socket_path: Option<std::path::PathBuf>,
}

impl ConnectRemoteHostPaneRuntime {
    pub fn new(network: RemoteNetworkConfig) -> Self {
        Self {
            network,
            ratatui_port: None,
            ratatui_socket_path: None,
        }
    }

    pub fn with_ratatui_port(mut self, port: u16) -> Self {
        self.ratatui_port = Some(port);
        self
    }

    pub fn with_ratatui_socket_path(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.ratatui_socket_path = Some(path.into());
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
                        Event::Key(key) => state.apply_key(key),
                        Event::Mouse(mouse) => state.apply_mouse(mouse),
                        Event::Resize(_, _) => PaneAction::Redraw,
                        _ => PaneAction::None,
                    };
                    match action {
                        PaneAction::None | PaneAction::Redraw => {}
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
                                self.ratatui_socket_path.as_deref(),
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
    remote_port_preference: String,
    last_remote_port: Option<u16>,
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
            remote_port_preference: "auto".to_string(),
            last_remote_port: None,
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
            self.remote_port_preference = "auto".to_string();
            self.last_remote_port = None;
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
        self.last_remote_port = profile.last_remote_port;
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
                if self.focus.uses_horizontal_choice() {
                    self.adjust_choice(-1);
                } else if self.focus != Focus::Hosts {
                    self.set_focus(Focus::Hosts);
                }
                PaneAction::None
            }
            KeyCode::Right => {
                if self.focus == Focus::Hosts {
                    self.set_focus(self.default_detail_focus());
                } else if self.focus.uses_horizontal_choice() {
                    self.adjust_choice(1);
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

    fn apply_delete_confirm_mouse(&mut self, mouse: crossterm::event::MouseEvent) -> PaneAction {
        let layout = DeleteConfirmGeometry::from_terminal_size(
            crossterm::terminal::size().unwrap_or((96, 24)),
        );
        if point_in_rect(mouse.column, mouse.row, layout.cancel_button) {
            self.delete_confirm = DeleteConfirmState::Idle;
            return PaneAction::None;
        }
        if point_in_rect(mouse.column, mouse.row, layout.delete_button) {
            return self.confirm_delete_action();
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

    fn apply_edit_key(&mut self, key: KeyEvent, field: EditField) -> PaneAction {
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

    fn apply_mouse(&mut self, mouse: crossterm::event::MouseEvent) -> PaneAction {
        if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            return PaneAction::None;
        }
        if !matches!(self.delete_confirm, DeleteConfirmState::Idle) {
            return self.apply_delete_confirm_mouse(mouse);
        }
        let x = mouse.column;
        let y = mouse.row;
        if matches!(self.status, Status::Error(_)) {
            let layout = ConnectErrorGeometry::from_terminal_size(
                crossterm::terminal::size().unwrap_or((96, 24)),
            );
            if point_in_rect(x, y, layout.ok_button) {
                self.dismiss_error_popup();
            }
            return PaneAction::None;
        }
        let layout = PopupGeometry::from_terminal_size(
            crossterm::terminal::size().unwrap_or((96, 24)),
            self,
        );
        if !point_in_rect(x, y, layout.dialog) {
            return PaneAction::None;
        }
        if point_in_rect(x, y, layout.host_sections.saved_hosts) {
            let inner = Block::default()
                .borders(Borders::ALL)
                .inner(layout.host_sections.saved_hosts);
            let list_start = inner.y.saturating_add(2);
            if y < list_start {
                return PaneAction::None;
            }
            let row = y.saturating_sub(list_start) as usize;
            if row <= self.profiles.len() {
                self.selected = row.min(self.profiles.len());
                self.set_focus(Focus::Hosts);
                return PaneAction::LoadSecrets(self.sync_selected_profile());
            }
            return PaneAction::None;
        }
        if point_in_rect(x, y, layout.host_sections.proxy_config) {
            let inner = Block::default()
                .borders(Borders::ALL)
                .inner(layout.host_sections.proxy_config);
            let list_start = inner.y.saturating_add(2);
            if y < list_start {
                return PaneAction::None;
            }
            let row = y.saturating_sub(list_start) as usize;
            let proxy_items = self.proxy_settings.profiles.len() + 1;
            if row < proxy_items {
                self.selected = self.proxy_selection_index().saturating_add(row);
                self.set_focus(Focus::Hosts);
                self.sync_selected_proxy();
                return PaneAction::None;
            }
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
                    return proxy_action_from_x(x, details.save);
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
            row if row == details.rows.auth && point_in_rect(x, y, details.authentication) => {
                self.set_focus(Focus::Auth)
            }
            row if row == details.rows.password && point_in_rect(x, y, details.authentication) => {
                self.set_focus(Focus::Password)
            }
            row if row == details.rows.sudo && point_in_rect(x, y, details.authentication) => {
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
            let mut next = self.prev_focus();
            if next == Focus::Hosts {
                next = self.default_detail_focus();
            }
            self.set_focus(next);
        }
        PaneAction::None
    }

    fn move_down(&mut self) -> PaneAction {
        if self.focus == Focus::Hosts {
            if self.selected < self.new_proxy_selection_index() {
                self.selected += 1;
                if self.selected_proxy_config() {
                    self.sync_selected_proxy();
                    return PaneAction::None;
                }
                return PaneAction::LoadSecrets(self.sync_selected_profile());
            }
        } else {
            let mut next = self.next_focus();
            if next == Focus::Hosts {
                next = if self.selected_proxy_config() {
                    Focus::ProxySave
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
            Focus::ProxyName
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
        if self.sudo_mode == SudoMode::None {
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
            | Focus::ProxyName
            | Focus::AllProxy
            | Focus::HttpsProxy => PaneAction::None,
            Focus::Auth => {
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

    fn connect_action(&self) -> PaneAction {
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
            Focus::Auth => {
                self.auth = self.auth.shift(step);
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
        self.focus.next(
            self.auth,
            self.has_saved_selection(),
            self.selected_proxy_config(),
        )
    }

    fn prev_focus(&self) -> Focus {
        self.focus.prev(
            self.auth,
            self.has_saved_selection(),
            self.selected_proxy_config(),
        )
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
}

impl Focus {
    fn uses_horizontal_choice(self) -> bool {
        matches!(self, Self::Auth)
    }

    fn edit_field(self, auth: AuthChoice) -> Option<EditField> {
        match self {
            Self::Host => Some(EditField::Host),
            Self::Port => Some(EditField::RemotePort),
            Self::User => Some(EditField::SshUser),
            Self::ProxyName => Some(EditField::ProxyName),
            Self::AllProxy => Some(EditField::AllProxy),
            Self::HttpsProxy => Some(EditField::HttpsProxy),
            Self::Password if auth == AuthChoice::Key => Some(EditField::KeyPath),
            Self::Password if auth == AuthChoice::Password => Some(EditField::SshPassword),
            _ => None,
        }
    }

    fn ordered(_auth: AuthChoice, has_saved_selection: bool, proxy_page: bool) -> Vec<Self> {
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
        let mut ordered = vec![
            Self::Hosts,
            Self::Host,
            Self::Port,
            Self::User,
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

    fn next(self, auth: AuthChoice, has_saved_selection: bool, proxy_page: bool) -> Self {
        let ordered = Self::ordered(auth, has_saved_selection, proxy_page);
        let index = ordered.iter().position(|field| *field == self).unwrap_or(0);
        ordered[(index + 1) % ordered.len()]
    }

    fn prev(self, auth: AuthChoice, has_saved_selection: bool, proxy_page: bool) -> Self {
        let ordered = Self::ordered(auth, has_saved_selection, proxy_page);
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
    DeleteSelectedHost { profile_name: String },
    LoadSecrets(Option<SecretLoadRequest>),
    SaveProxyConfig,
    ActivateProxyConfig,
    DeleteProxyConfig,
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
    host_sections: HostListGeometry,
}

#[derive(Debug, Clone, Copy)]
struct HostListGeometry {
    saved_hosts: Rect,
    proxy_config: Rect,
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
    auth: u16,
    password: u16,
    sudo: u16,
    remember: u16,
    install_proxy: u16,
}

#[derive(Debug, Clone, Copy)]
struct ProxyDetailsGeometry {
    proxy: Rect,
    no_proxy: Rect,
    save: Rect,
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

impl PopupGeometry {
    fn from_terminal_size((cols, rows): (u16, u16), state: &ConnectRemoteHostState) -> Self {
        let width = popup_preferred_width(state).min(cols);
        let x = cols.saturating_sub(width) / 2;

        // Keep the popup compact and centered, like the original popup,
        // instead of stretching to the full terminal height. The height is
        // fixed so it does not jump when the selected menu item changes
        // (e.g. Saved Host vs New Host). It shrinks only on very small
        // terminals to keep a visible margin.
        const POPUP_HEIGHT: u16 = 24;
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
        let right_padding = DETAIL_RIGHT_PADDING.min(
            body.width
                .saturating_sub(host_width)
                .saturating_sub(separator_width),
        );
        let details_x = body
            .x
            .saturating_add(host_width)
            .saturating_add(separator_width);
        let details_width = body
            .width
            .saturating_sub(host_width)
            .saturating_sub(separator_width)
            .saturating_sub(right_padding);
        let host_sections = HostListGeometry::from_area(
            Rect::new(body.x, body.y, host_width, body.height),
            state,
        );
        Self {
            dialog,
            hosts: Rect::new(body.x, body.y, host_width, body.height),
            details: Rect::new(details_x, body.y, details_width, body.height),
            host_sections,
        }
    }
}

impl HostListGeometry {
    fn from_area(area: Rect, state: &ConnectRemoteHostState) -> Self {
        // Inset the cards by one cell from the hosts panel so they do not touch
        // the popup border or the right separator.
        let margin = 1_u16;
        let container = Rect::new(
            area.x.saturating_add(margin),
            area.y.saturating_add(margin),
            area.width.saturating_sub(margin.saturating_mul(2)),
            area.height.saturating_sub(margin.saturating_mul(2)),
        );

        // Each card has a header row plus its content list, wrapped in a border.
        let saved_content = (state.profiles.len() + 1).max(1) as u16; // hosts + New Host
        let proxy_content = (state.proxy_settings.profiles.len() + 1).max(1) as u16; // proxies + New Proxy
        let saved_natural = saved_content.saturating_add(3); // + header + borders
        let proxy_natural = proxy_content.saturating_add(3);
        let gap = 1_u16;
        let total_natural = saved_natural.saturating_add(proxy_natural).saturating_add(gap);
        let available = container.height;

        let (saved_height, proxy_height) = if total_natural <= available {
            // Let the proxy card absorb any leftover vertical space so the
            // bottom of the sidebar does not look empty.
            let leftover = available - total_natural;
            (saved_natural, proxy_natural.saturating_add(leftover))
        } else {
            let min_each = 5_u16.min(available);
            if available <= min_each.saturating_mul(2).saturating_add(gap) {
                let half = available.saturating_sub(gap) / 2;
                (half.max(3), available - gap - half.max(3))
            } else {
                let extra = available - min_each.saturating_mul(2).saturating_sub(gap);
                let saved_extra = (saved_content as u32 * extra as u32
                    / (saved_content.saturating_add(proxy_content)) as u32)
                    as u16;
                (
                    min_each.saturating_add(saved_extra).max(3),
                    available - gap - min_each.saturating_add(saved_extra).max(3),
                )
            }
        };

        Self {
            saved_hosts: Rect::new(container.x, container.y, container.width, saved_height),
            proxy_config: Rect::new(
                container.x,
                container.y.saturating_add(saved_height).saturating_add(gap),
                container.width,
                proxy_height,
            ),
        }
    }
}

impl DetailsGeometry {
    fn from_area(area: Rect, _state: &ConnectRemoteHostState) -> Self {
        // Layout inspired by the UI mock: header bar, then Connection and
        // Authentication side-by-side inside bordered cards, then Options,
        // an info box, action buttons, and a bottom hint bar. Heights
        // account for the 1-cell borders around each card.
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // header bar
                Constraint::Min(6),    // Connection + Authentication cards
                Constraint::Length(1), // blank
                Constraint::Min(4),    // Options card
                Constraint::Length(1), // blank
                Constraint::Min(4),    // info box card
                Constraint::Length(1), // blank
                Constraint::Length(1), // buttons
                Constraint::Length(1), // hint
            ])
            .split(area);
        let top_columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(sections[1]);
        let rows = DetailsRows {
            host: sections[1].y.saturating_add(2).saturating_sub(area.y),
            port: sections[1].y.saturating_add(3).saturating_sub(area.y),
            user: sections[1].y.saturating_add(4).saturating_sub(area.y),
            auth: sections[1].y.saturating_add(2).saturating_sub(area.y),
            password: sections[1].y.saturating_add(3).saturating_sub(area.y),
            sudo: sections[1].y.saturating_add(4).saturating_sub(area.y),
            remember: sections[3].y.saturating_add(2).saturating_sub(area.y),
            install_proxy: sections[3].y.saturating_add(3).saturating_sub(area.y),
        };
        Self {
            header: sections[0],
            connection: top_columns[0],
            authentication: top_columns[1],
            options: sections[3],
            info: sections[5],
            buttons: sections[7],
            hint: sections[8],
            rows,
        }
    }
}

impl ProxyDetailsGeometry {
    fn from_area(area: Rect) -> Self {
        let bottom = area.y.saturating_add(area.height);
        let proxy_height = area.height.min(4);
        let proxy = Rect::new(area.x, area.y, area.width, proxy_height);
        let no_proxy_y = area.y.saturating_add(proxy_height);
        let no_proxy_height = bottom.saturating_sub(no_proxy_y).min(5);
        let no_proxy = Rect::new(area.x, no_proxy_y, area.width, no_proxy_height);
        let action_y = no_proxy_y
            .saturating_add(no_proxy_height)
            .saturating_add(1)
            .min(bottom);
        let action_height = u16::from(action_y < bottom);
        let save = Rect::new(area.x, action_y, area.width, action_height);
        let status_y = action_y.saturating_add(action_height).min(bottom);
        let status = Rect::new(
            area.x,
            status_y,
            area.width,
            bottom.saturating_sub(status_y),
        );
        let rows = ProxyDetailsRows {
            name: proxy.y.saturating_add(1).saturating_sub(area.y),
            all_proxy: proxy.y.saturating_add(2).saturating_sub(area.y),
            https_proxy: proxy.y.saturating_add(3).saturating_sub(area.y),
            no_proxy: no_proxy.y.saturating_sub(area.y),
            action: save.y.saturating_sub(area.y),
        };
        Self {
            proxy,
            no_proxy,
            save,
            status,
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
}

fn render_hosts(frame: &mut Frame<'_>, area: Rect, state: &ConnectRemoteHostState) {
    let geometry = HostListGeometry::from_area(area, state);
    let hosts_focused = state.focus == Focus::Hosts;
    render_host_section(
        frame,
        geometry.saved_hosts,
        hosts_focused,
        host_section_title("Saved Hosts", "▤", state.profiles.len()),
        saved_host_list_items(state),
        saved_host_list_selected(state),
    );
    render_host_section(
        frame,
        geometry.proxy_config,
        hosts_focused,
        host_section_title(
            "Proxy Configuration",
            "⛓",
            state.proxy_settings.profiles.len(),
        ),
        proxy_config_list_items(state),
        proxy_config_list_selected(state),
    );
}

fn host_section_title(title: &str, icon: &str, count: usize) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("  {icon} {title}  "),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" {count} "),
            Style::default()
                .fg(Color::White)
                .bg(Color::Rgb(50, 55, 65))
                .add_modifier(Modifier::BOLD),
        ),
    ])
}

fn saved_host_list_selected(state: &ConnectRemoteHostState) -> Option<usize> {
    if state.selected < state.proxy_selection_index() {
        Some(state.selected)
    } else {
        None
    }
}

fn proxy_config_list_selected(state: &ConnectRemoteHostState) -> Option<usize> {
    if state.selected >= state.proxy_selection_index() {
        Some(state.selected - state.proxy_selection_index())
    } else {
        None
    }
}

fn render_host_section(
    frame: &mut Frame<'_>,
    area: Rect,
    hosts_focused: bool,
    title_line: Line<'static>,
    items: Vec<ListItem<'static>>,
    selected: Option<usize>,
) {
    if area.height < 4 {
        return;
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(SECTION_BORDER))
        .style(Style::default().bg(SECTION_BG));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    // Header sits on the first inner row, leaving a blank row between it and
    // the list so the title does not feel glued to the border or content.
    let header_area = Rect::new(inner.x, inner.y, inner.width, 1);
    frame.render_widget(
        Paragraph::new(title_line).style(Style::default().bg(SECTION_BG)),
        header_area,
    );

    let list_area = Rect::new(
        inner.x,
        inner.y.saturating_add(2),
        inner.width,
        inner.height.saturating_sub(2),
    );
    let visible_height = list_area.height as usize;
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
    frame.render_stateful_widget(list, list_area, &mut list_state);
}

fn saved_host_label(profile: &RemoteHostProfile) -> String {
    format!("{}@{}", profile.ssh_user, profile.host)
}

fn saved_host_list_items(state: &ConnectRemoteHostState) -> Vec<ListItem<'static>> {
    let mut items: Vec<ListItem<'static>> = state
        .profiles
        .iter()
        .map(|profile| {
            let text = saved_host_label(profile);
            ListItem::new(Line::from(vec![
                Span::styled(" ● ", Style::default().fg(Color::Green)),
                Span::styled(text, Style::default().fg(Color::White)),
            ]))
        })
        .collect();
    items.push(action_list_item("New Host"));
    items
}

fn proxy_config_list_items(state: &ConnectRemoteHostState) -> Vec<ListItem<'static>> {
    let mut items: Vec<ListItem<'static>> = state
        .proxy_settings
        .profiles
        .iter()
        .map(|profile| {
            let active = state.proxy_settings.active.as_deref() == Some(profile.name.as_str());
            let (prefix, color) = if active {
                ("★", Color::Yellow)
            } else {
                ("●", Color::Green)
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!(" {prefix} "), Style::default().fg(color)),
                Span::styled(profile.name.clone(), Style::default().fg(Color::White)),
            ]))
        })
        .collect();
    items.push(action_list_item("New Proxy"));
    items
}

fn action_list_item(label: &str) -> ListItem<'static> {
    ListItem::new(Line::from(vec![
        Span::styled(" + ", Style::default().fg(Color::Gray).bg(ACTION_BG)),
        Span::styled(
            format!(" {label} "),
            Style::default().fg(Color::Gray).bg(ACTION_BG),
        ),
    ]))
}

const POPUP_WIDTH: u16 = 120;
const HOST_LIST_WIDTH: u16 = 29;
const DETAIL_RIGHT_PADDING: u16 = 2;
const SECTION_TITLE_INDENT: u16 = 2;
const SECTION_CONTENT_INDENT: u16 = 4;
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
const HEADER_BG: Color = Color::Rgb(32, 36, 43);
const ACTION_BG: Color = Color::Rgb(40, 44, 52);

fn render_header(frame: &mut Frame<'_>, area: Rect, state: &ConnectRemoteHostState) {
    frame.render_widget(
        Paragraph::new("").style(Style::default().bg(HEADER_BG)),
        area,
    );
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
    let mut spans = vec![
        Span::styled("● ", Style::default().fg(status_color)),
        Span::styled(
            host,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if state.selected < state.profiles.len() {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            " SSH ",
            Style::default().bg(Color::Cyan).fg(Color::Black),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            " Saved ",
            Style::default().bg(Color::Green).fg(Color::Black),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);

    let star = if state.selected >= state.profiles.len() {
        "☆"
    } else {
        "★"
    };
    let star_width = star.width() as u16;
    if area.width > star_width {
        frame.render_widget(
            Paragraph::new(star).style(Style::default().fg(Color::Yellow)),
            Rect::new(area.x + area.width - star_width, area.y, star_width, 1),
        );
    }
}

fn render_info_box(frame: &mut Frame<'_>, area: Rect, _state: &ConnectRemoteHostState) {
    if area.height == 0 {
        return;
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .style(Style::default().bg(SECTION_BG_INFO));
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

fn render_proxy_details(frame: &mut Frame<'_>, area: Rect, state: &ConnectRemoteHostState) {
    let geometry = ProxyDetailsGeometry::from_area(area);
    let rows = [
        detail_row("Name", &state.proxy_draft.name, state, Focus::ProxyName),
        detail_row(
            "all_proxy",
            &proxy_input_display(&state.proxy_draft.all_proxy),
            state,
            Focus::AllProxy,
        ),
        detail_row(
            "https_proxy",
            &proxy_input_display(&state.proxy_draft.https_proxy),
            state,
            Focus::HttpsProxy,
        ),
    ];
    render_section_title(frame, geometry.proxy, "Proxy Configuration", Color::Green);
    let table_area = Rect::new(
        geometry.proxy.x,
        geometry.proxy.y.saturating_add(1),
        geometry.proxy.width,
        geometry.proxy.height.saturating_sub(1),
    );
    render_detail_table(frame, table_area, rows);
    render_no_proxy(frame, geometry.no_proxy, state);
    render_proxy_save(frame, geometry.save, state);
    render_status(frame, geometry.status, state);
}

const PROXY_EMPTY_PLACEHOLDER: &str = "________________";

fn proxy_input_display(value: &str) -> String {
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
    let value_x = area.x.saturating_add(LABEL_WIDTH + 1);
    let value_width = area.width.saturating_sub(LABEL_WIDTH + 1);
    frame.render_widget(
        Paragraph::new(format!(
            "{:<width$}",
            "no_proxy",
            width = LABEL_WIDTH as usize
        )),
        Rect::new(area.x, area.y, LABEL_WIDTH, 1),
    );
    frame.render_widget(
        Paragraph::new(format!("auto: {}", no_proxy_for_install(&state.host, "")))
            .alignment(Alignment::Right)
            .wrap(Wrap { trim: false }),
        Rect::new(value_x, area.y, value_width, area.height),
    );
}

fn render_proxy_save(frame: &mut Frame<'_>, area: Rect, state: &ConnectRemoteHostState) {
    if area.height == 0 {
        return;
    }
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(34),
            Constraint::Percentage(33),
            Constraint::Percentage(33),
        ])
        .split(area);
    let active = state.proxy_settings.active.as_deref() == Some(state.proxy_draft.name.as_str());
    let active_label = if active { "Active" } else { "Set Active" };
    frame.render_widget(
        Paragraph::new(active_label)
            .style(action_focus_style(
                state.focus == Focus::ProxyActive,
                Focus::ProxyActive,
            ))
            .alignment(Alignment::Center),
        columns[0],
    );
    frame.render_widget(
        Paragraph::new("Save")
            .style(action_focus_style(
                state.focus == Focus::ProxySave,
                Focus::ProxySave,
            ))
            .alignment(Alignment::Center),
        columns[1],
    );
    let delete_style = if state.focus == Focus::ProxyDelete {
        delete_focus_style()
    } else {
        Style::default().fg(Color::Red)
    };
    frame.render_widget(
        Paragraph::new("Delete")
            .style(delete_style)
            .alignment(Alignment::Center),
        columns[2],
    );
}

fn proxy_action_from_x(x: u16, area: Rect) -> PaneAction {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(34),
            Constraint::Percentage(33),
            Constraint::Percentage(33),
        ])
        .split(area);
    if point_in_rect(x, area.y, columns[0]) {
        PaneAction::ActivateProxyConfig
    } else if point_in_rect(x, area.y, columns[1]) {
        PaneAction::SaveProxyConfig
    } else if point_in_rect(x, area.y, columns[2]) {
        PaneAction::DeleteProxyConfig
    } else {
        PaneAction::None
    }
}

fn render_connection(frame: &mut Frame<'_>, area: Rect, state: &ConnectRemoteHostState) {
    let block = section_block(
        "Connection",
        "◎",
        SECTION_COLOR_CONNECTION,
        SECTION_BG_CONNECTION,
    );
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
    render_detail_table(frame, table_area, rows);
}

fn render_authentication(frame: &mut Frame<'_>, area: Rect, state: &ConnectRemoteHostState) {
    let block = section_block("Authentication", "◼", SECTION_COLOR_AUTH, SECTION_BG_AUTH);
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
    if state.auth == AuthChoice::Key {
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
    rows.push(icon_password_row("▲", "Sudo", PasswordField::Sudo, state));
    render_detail_table(frame, table_area, rows);
}

fn render_options(frame: &mut Frame<'_>, area: Rect, state: &ConnectRemoteHostState) {
    let block = section_block("Options", "⚙", SECTION_COLOR_OPTIONS, SECTION_BG_OPTIONS);
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
        Paragraph::new(format!("{box_symbol} {label}")).style(style),
        area,
    );
}

const SECTION_COLOR_CONNECTION: Color = Color::Cyan;
const SECTION_COLOR_AUTH: Color = Color::Magenta;
const SECTION_COLOR_OPTIONS: Color = Color::Yellow;
const SECTION_BG_CONNECTION: Color = Color::Rgb(22, 33, 46);
const SECTION_BG_AUTH: Color = Color::Rgb(40, 24, 48);
const SECTION_BG_OPTIONS: Color = Color::Rgb(46, 40, 22);
const SECTION_BG_INFO: Color = Color::Rgb(32, 36, 43);

fn section_block(title: &str, icon: &str, color: Color, bg: Color) -> Block<'static> {
    Block::default()
        .title(section_title(title, icon, color))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(color))
        .style(Style::default().bg(bg))
}

fn section_title(title: &str, icon: &str, color: Color) -> Line<'static> {
    Line::from(format!(" {icon} {title}"))
        .style(Style::default().fg(color).add_modifier(Modifier::BOLD))
}

fn modal_title(title: &str) -> Line<'static> {
    Line::from(format!(" {title}"))
        .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD))
}

fn render_section_title(frame: &mut Frame<'_>, area: Rect, title: &str, color: Color) {
    frame.render_widget(
        Paragraph::new(section_title(title, "◈", color)),
        Rect::new(
            area.x.saturating_add(SECTION_TITLE_INDENT),
            area.y,
            area.width.saturating_sub(SECTION_TITLE_INDENT),
            1,
        ),
    );
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
    let connect_text = format!(" ▶ {}  <Enter> ", connect_label);
    let connect_width = connect_text.width() as u16;
    let delete_text = " 🗑 Delete  <D> ".to_string();
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
            Style::default()
                .bg(Color::Rgb(40, 44, 52))
                .fg(Color::Red)
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
    let connect_text = format!(" ▶ {}  <Enter> ", connect_label(state));
    let connect_width = connect_text.width() as u16;
    let delete_text = " 🗑 Delete  <D> ";
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
        Focus::Auth => "←/→: switch auth · Tab: next".to_string(),
        Focus::Remember | Focus::InstallProxy => "Space: toggle · Tab: next".to_string(),
        Focus::Connect => "Enter: connect · Tab: next".to_string(),
        Focus::Delete => "Enter: delete · Tab: next".to_string(),
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

fn action_focus_style(focused: bool, focus: Focus) -> Style {
    if focused {
        match focus {
            Focus::Delete | Focus::ProxyDelete => delete_focus_style(),
            _ => active_focus_style(),
        }
    } else {
        match focus {
            Focus::Delete | Focus::ProxyDelete => Style::default().fg(Color::Red),
            _ => Style::default().add_modifier(Modifier::BOLD),
        }
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
    detail_row(&format!("{icon} {label}"), value, state, focus)
}

fn readonly_icon_detail_row(icon: &str, label: &str, value: &str) -> Row<'static> {
    readonly_detail_row(&format!("{icon} {label}"), value)
}

fn icon_password_row(
    icon: &str,
    label: &str,
    field: PasswordField,
    state: &ConnectRemoteHostState,
) -> Row<'static> {
    password_row(&format!("{icon} {label}"), field, state)
}

fn icon_choice_row(
    icon: &str,
    label: &str,
    value: Vec<ChoiceSegment>,
    state: &ConnectRemoteHostState,
    focus: Focus,
) -> Row<'static> {
    choice_row(&format!("{icon} {label}"), value, state, focus)
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
    } else {
        state.host.clone()
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
    let rows = DetailsGeometry::from_area(details, state).rows;
    let row = match field {
        EditField::Host => rows.host,
        EditField::RemotePort => rows.port,
        EditField::SshUser => rows.user,
        EditField::KeyPath | EditField::SshPassword => rows.password,
        EditField::SudoPassword => rows.sudo,
        EditField::ProxyName | EditField::AllProxy | EditField::HttpsProxy => return None,
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

    let secret_store = FileRemoteHostSecretStore::default();
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

fn run_ratatui_connect(
    state: &ConnectRemoteHostState,
    port: u16,
    socket_path: &std::path::Path,
) -> Result<String, String> {
    let Some(profile) = state
        .selected_profile()
        .filter(|profile| saved_profile_can_connect_by_id(state, profile))
    else {
        return Err("ratatui mode only supports saved profiles with saved credentials".to_string());
    };

    let mut stream = UnixStream::connect(socket_path)
        .map_err(|error| format!("failed to connect to ratatui node on port {port}: {error}"))?;
    writeln!(stream, "CONNECT_REMOTE_HOST {}", profile.name)
        .map_err(|error| format!("failed to send connect command: {error}"))?;
    stream
        .flush()
        .map_err(|error| format!("failed to flush connect command: {error}"))?;

    let reader = stream
        .try_clone()
        .map_err(|error| format!("failed to clone node socket: {error}"))?;
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    loop {
        line.clear();
        reader
            .read_line(&mut line)
            .map_err(|error| format!("failed to read node response: {error}"))?;
        let response = line.trim();
        if response.is_empty() {
            continue;
        }
        // The node server wraps command replies in `ServerMessageJson::Response`.
        // Snapshots may also arrive on this socket while we wait, so skip them.
        if let Ok(message) = serde_json::from_str::<ServerMessageJson>(response) {
            match message {
                ServerMessageJson::Response(resp) => {
                    if resp.ok {
                        return Ok("Connected. Press Esc to close.".to_string());
                    }
                    return Err(resp.message.unwrap_or_default());
                }
                ServerMessageJson::Snapshot(_) => continue,
                ServerMessageJson::History(_) => continue,
            }
        }
        // Plain-text fallback for older/simple replies.
        if response.starts_with("OK") {
            return Ok("Connected. Press Esc to close.".to_string());
        }
        return Err(response
            .strip_prefix("ERR ")
            .unwrap_or(response)
            .to_string());
    }
}

fn run_connect(
    state: &ConnectRemoteHostState,
    command: &ConnectRemoteHostPaneCommand,
    network: &RemoteNetworkConfig,
    ratatui_port: Option<u16>,
    ratatui_socket_path: Option<&std::path::Path>,
) -> Result<String, String> {
    if let (Some(port), Some(socket_path)) = (ratatui_port, ratatui_socket_path) {
        return run_ratatui_connect(state, port, socket_path);
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
            "--auth".to_string(),
            state.auth.as_arg().to_string(),
            "--remote-port".to_string(),
            normalized_port(&state.remote_port_preference),
        ]);
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

fn profile_matches_state(profile: &RemoteHostProfile, state: &ConnectRemoteHostState) -> bool {
    profile.host == state.host
        && profile.ssh_user == state.ssh_user
        && normalized_port_matches_profile(&state.remote_port_preference, profile)
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
    FileRemoteHostSecretStore::default()
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
            last_remote_port: Some(7575),
            last_endpoint: None,
            last_connected_at: None,
            use_install_proxy: true,
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
            last_remote_port: Some(7575),
            last_endpoint: None,
            last_connected_at: None,
            use_install_proxy: true,
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
            .nth(details.save.y as usize)
            .expect("save row should render");
        let save_col = save_row.find("Save").expect("Save should render") as u16;

        assert!(save_col > details.save.x + 8);
        assert!(save_col + 4 < details.save.x + details.save.width);
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
            last_remote_port: Some(7575),
            last_endpoint: None,
            last_connected_at: None,
            use_install_proxy: true,
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
            last_remote_port: Some(7575),
            last_endpoint: None,
            last_connected_at: None,
            use_install_proxy: true,
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
            last_remote_port: Some(7575),
            last_endpoint: None,
            last_connected_at: None,
            use_install_proxy: true,
        }];
        state.selected = 0;
        let _ = state.sync_selected_profile();
        assert_eq!(state.host, "127.0.0.1");
        assert_eq!(state.ssh_user, "k");
        assert_eq!(segmented_for_test(&auth_tabs(&state)), "Password  Key");
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
                last_remote_port: Some(7575),
                last_endpoint: None,
                last_connected_at: None,
                use_install_proxy: true,
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
                last_remote_port: Some(7575),
                last_endpoint: None,
                last_connected_at: None,
                use_install_proxy: true,
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
            last_remote_port: Some(7575),
            last_endpoint: None,
            last_connected_at: None,
            use_install_proxy: true,
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
            last_remote_port: Some(7575),
            last_endpoint: None,
            last_connected_at: None,
            use_install_proxy: true,
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
            last_remote_port: Some(7575),
            last_endpoint: None,
            last_connected_at: None,
            use_install_proxy: true,
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
            last_remote_port: Some(7575),
            last_endpoint: None,
            last_connected_at: None,
            use_install_proxy: true,
        }];
        state.selected = 1;
        let _ = state.sync_selected_profile();

        let geometry = PopupGeometry::from_terminal_size((140, 26), &state);

        assert_eq!(geometry.dialog.x, 10);
        assert_eq!(geometry.dialog.width, 120);
        // Height is fixed relative to the terminal, not to the selected item.
        assert_eq!(geometry.dialog.height, 24);
        assert_eq!(geometry.hosts.y, 2);
        assert_eq!(geometry.details.y, 2);
        assert_eq!(geometry.hosts.height, 22);
        assert_eq!(geometry.hosts.width, 29);
        assert_eq!(geometry.details.width, 86);
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
            last_remote_port: Some(7575),
            last_endpoint: None,
            last_connected_at: None,
            use_install_proxy: true,
        }];
        state.selected = 0;
        let _ = state.sync_selected_profile();

        let geometry = PopupGeometry::from_terminal_size((140, 26), &state);

        assert_eq!(geometry.dialog.x, 10);
        assert_eq!(geometry.dialog.width, 120);
        assert_eq!(geometry.hosts.width, 29);
        assert_eq!(geometry.details.width, 86);
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
            last_remote_port: Some(7575),
            last_endpoint: None,
            last_connected_at: None,
            use_install_proxy: true,
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
            last_remote_port: Some(7575),
            last_endpoint: None,
            last_connected_at: None,
            use_install_proxy: true,
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

        assert_eq!(geometry.dialog.x, 10);
        assert_eq!(geometry.dialog.width, 120);
        assert_eq!(geometry.hosts.width, 29);
        assert_eq!(geometry.details.width, 86);
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
        assert_eq!(geometry.details.width, 32);
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
                last_remote_port: None,
                last_endpoint: None,
                last_connected_at: None,
                use_install_proxy: true,
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
                last_remote_port: None,
                last_endpoint: None,
                last_connected_at: None,
                use_install_proxy: true,
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
        assert_eq!(state.focus, Focus::Connect);

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

        let saved_items = saved_host_list_items(&state);
        let proxy_items = proxy_config_list_items(&state);
        assert_eq!(saved_items.len(), state.profiles.len() + 1);
        assert_eq!(proxy_items.len(), state.proxy_settings.profiles.len() + 1);
        assert_eq!(state.proxy_selection_index(), state.profiles.len() + 1);

        state.selected = state.proxy_selection_index();
        state.set_focus(Focus::Hosts);
        assert_eq!(state.default_detail_focus(), Focus::ProxyName);
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

        let items = proxy_config_list_items(&state);
        assert_eq!(items.len(), 3);
        state.selected = state.proxy_profile_selection_start();
        assert_eq!(proxy_config_list_selected(&state), Some(0));
        state.selected = state.proxy_profile_selection_start() + 1;
        assert_eq!(proxy_config_list_selected(&state), Some(1));
        state.selected = state.new_proxy_selection_index();
        assert_eq!(proxy_config_list_selected(&state), Some(2));
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
            last_remote_port: Some(7575),
            last_endpoint: None,
            last_connected_at: None,
            use_install_proxy: true,
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
        state.apply_mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: geometry.details.x + geometry.details.width - 10,
            row: geometry.details.y + details.rows.password,
            modifiers: crossterm::event::KeyModifiers::empty(),
        });

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
        assert_eq!(x, geometry.details.x + 36);
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
        assert_eq!(x, geometry.details.x + 38);
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
        assert_eq!(x, geometry.details.x + 45);

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
        assert_eq!(x, geometry.details.x + 45);

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
        assert_eq!(x, geometry.details.x + 45);
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
        assert_eq!(x, geometry.details.x + 45);
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

        state.apply_mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: geometry.details.x + geometry.details.width - 10,
            row: geometry.details.y + details.rows.password,
            modifiers: crossterm::event::KeyModifiers::empty(),
        });

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
        assert_eq!(x, geometry.details.x + 45);
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
                last_remote_port: Some(7474),
                last_endpoint: None,
                last_connected_at: None,
                use_install_proxy: true,
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
                last_remote_port: Some(7575),
                last_endpoint: None,
                last_connected_at: None,
                use_install_proxy: true,
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
}
