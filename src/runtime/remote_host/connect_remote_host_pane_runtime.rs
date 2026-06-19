use crate::cli::{prepend_global_network_args, ConnectRemoteHostPaneCommand, RemoteNetworkConfig};
use crate::lifecycle::LifecycleError;
use crate::runtime::current_executable::current_waitagent_executable;
use crate::runtime::remote_host::remote_host_history_store::{
    RemoteHostAuthProfile, RemoteHostHistoryStore, RemoteHostProfile, RemotePortPreference,
};
use crate::runtime::remote_host::remote_host_secret_store::{
    FileRemoteHostSecretStore, RemoteHostSecretStore,
};
use crate::ui::chrome::display_width;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Row, Table};
use ratatui::{Frame, Terminal};
use std::io::{self, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct ConnectRemoteHostPaneRuntime {
    network: RemoteNetworkConfig,
}

impl ConnectRemoteHostPaneRuntime {
    pub fn new(network: RemoteNetworkConfig) -> Self {
        Self { network }
    }

    pub fn run(&self, command: ConnectRemoteHostPaneCommand) -> Result<(), LifecycleError> {
        enable_raw_mode().map_err(write_error)?;
        crossterm::execute!(io::stdout(), crossterm::event::EnableMouseCapture)
            .map_err(write_error)?;
        let backend = CrosstermBackend::new(io::stdout());
        let mut terminal = Terminal::new(backend).map_err(write_error)?;
        terminal.clear().map_err(write_error)?;

        let (mut state, initial_secret_request) =
            ConnectRemoteHostState::load_with_initial_secret_request();
        let result =
            self.run_event_loop(&mut terminal, &mut state, command, initial_secret_request);

        crossterm::execute!(io::stdout(), crossterm::event::DisableMouseCapture)
            .map_err(write_error)?;
        disable_raw_mode().map_err(write_error)?;
        terminal.show_cursor().map_err(write_error)?;
        result
    }

    fn run_event_loop(
        &self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
        state: &mut ConnectRemoteHostState,
        command: ConnectRemoteHostPaneCommand,
        initial_secret_request: Option<SecretLoadRequest>,
    ) -> Result<(), LifecycleError> {
        let (secret_tx, secret_rx) = mpsc::channel();
        if let Some(request) = initial_secret_request {
            spawn_secret_loader(request, secret_tx.clone());
        }
        terminal
            .draw(|frame| render(frame, state))
            .map_err(write_error)?;
        loop {
            let mut needs_draw = self.apply_secret_results(state, &secret_rx);
            if !needs_draw && !event::poll(Duration::from_millis(25)).map_err(write_error)? {
                continue;
            }
            let action = if event::poll(Duration::from_millis(0)).map_err(write_error)? {
                match event::read().map_err(write_error)? {
                    Event::Key(key) => {
                        needs_draw = true;
                        state.apply_key(key)
                    }
                    Event::Mouse(mouse) => {
                        needs_draw = true;
                        state.apply_mouse(mouse)
                    }
                    Event::Resize(_, _) => {
                        needs_draw = true;
                        PaneAction::Redraw
                    }
                    _ => PaneAction::None,
                }
            } else {
                PaneAction::None
            };
            match action {
                PaneAction::None | PaneAction::Redraw => {}
                PaneAction::Close => return Ok(()),
                PaneAction::LoadSecrets(request) => {
                    if let Some(request) = request {
                        spawn_secret_loader(request, secret_tx.clone());
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
                        .draw(|frame| render(frame, state))
                        .map_err(write_error)?;
                    match run_connect(state, &command, &self.network) {
                        Ok(_) => return Ok(()),
                        Err(message) => state.status = Status::Error(message),
                    }
                }
            }
            if needs_draw {
                terminal
                    .draw(|frame| render(frame, state))
                    .map_err(write_error)?;
            }
        }
    }

    fn apply_secret_results(
        &self,
        state: &mut ConnectRemoteHostState,
        secret_rx: &Receiver<SecretLoadResult>,
    ) -> bool {
        let mut changed = false;
        while let Ok(result) = secret_rx.try_recv() {
            state.apply_secret_result(result);
            changed = true;
        }
        changed
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ConnectRemoteHostState {
    profiles: Vec<RemoteHostProfile>,
    selected: usize,
    focus: Focus,
    host: String,
    ssh_user: String,
    remote_port: String,
    auth: AuthChoice,
    key_path: String,
    ssh_password: String,
    sudo_password: String,
    password_mode: PasswordMode,
    sudo_mode: SudoMode,
    show_ssh_password: bool,
    show_sudo_password: bool,
    remember: bool,
    editing: Option<EditField>,
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
            remote_port: "auto".to_string(),
            auth: AuthChoice::Password,
            key_path: String::new(),
            ssh_password: String::new(),
            sudo_password: String::new(),
            password_mode: PasswordMode::Enter,
            sudo_mode: SudoMode::SameAsSsh,
            show_ssh_password: false,
            show_sudo_password: false,
            remember: true,
            editing: None,
            status: Status::Hint("Select a saved host or fill a new host.".to_string()),
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
            self.remote_port = "auto".to_string();
            self.auth = AuthChoice::Password;
            self.key_path.clear();
            self.ssh_password.clear();
            self.sudo_password.clear();
            self.password_mode = PasswordMode::Enter;
            self.sudo_mode = SudoMode::SameAsSsh;
            self.show_ssh_password = false;
            self.show_sudo_password = false;
            self.remember = true;
            self.status = Status::Hint("Select a saved host or fill a new host.".to_string());
            return None;
        }
        let Some(profile) = self.profiles.get(self.selected).cloned() else {
            return None;
        };
        self.host = profile.host.clone();
        self.ssh_user = profile.ssh_user.clone();
        self.remote_port = match profile.last_remote_port {
            Some(port) => port.to_string(),
            None => match profile.preferred_remote_port {
                RemotePortPreference::Auto => "auto".to_string(),
                RemotePortPreference::Port(port) => port.to_string(),
            },
        };
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
        if request.has_work() {
            self.status = Status::Loading("Loading saved credentials...".to_string());
            self.secret_load = SecretLoadState::Loading {
                id: request.id,
                selected: request.selected,
            };
            Some(request)
        } else {
            self.status = Status::Hint("Select a saved host or fill a new host.".to_string());
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
            self.status = Status::Hint("Select a saved host or fill a new host.".to_string());
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
                self.set_focus(self.focus.next(self.auth, self.has_saved_selection()));
                PaneAction::None
            }
            KeyCode::BackTab => {
                self.set_focus(self.focus.prev(self.auth, self.has_saved_selection()));
                PaneAction::None
            }
            KeyCode::Up => self.move_up(),
            KeyCode::Down => self.move_down(),
            KeyCode::Left => {
                if self.focus == Focus::PasswordToggle {
                    self.set_focus(Focus::Password);
                } else if self.focus == Focus::SudoToggle {
                    self.set_focus(Focus::Sudo);
                } else if self.focus.uses_horizontal_choice() {
                    self.adjust_choice(-1);
                } else if self.focus != Focus::Hosts {
                    self.set_focus(Focus::Hosts);
                }
                PaneAction::None
            }
            KeyCode::Right => {
                if self.focus == Focus::Hosts {
                    self.set_focus(self.default_detail_focus());
                } else if self.focus == Focus::Password {
                    self.set_focus(Focus::PasswordToggle);
                } else if self.focus == Focus::Sudo {
                    self.set_focus(Focus::SudoToggle);
                } else {
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
                }
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
            KeyCode::Esc | KeyCode::Left => self.set_focus(Focus::Hosts),
            KeyCode::Right if field == EditField::SshPassword => {
                self.set_focus(Focus::PasswordToggle)
            }
            KeyCode::Tab => self.set_focus(self.focus.next(self.auth, self.has_saved_selection())),
            KeyCode::BackTab => {
                self.set_focus(self.focus.prev(self.auth, self.has_saved_selection()))
            }
            KeyCode::Up => return self.move_up(),
            KeyCode::Down => return self.move_down(),
            KeyCode::Enter => {
                self.set_focus(self.focus.next(self.auth, self.has_saved_selection()))
            }
            KeyCode::Backspace => {
                edit_buffer(self, field).pop();
            }
            KeyCode::Char(ch) if !ch.is_control() => edit_buffer(self, field).push(ch),
            _ => {}
        }
        PaneAction::None
    }

    fn apply_sudo_password_edit_key(&mut self, key: KeyEvent) -> PaneAction {
        match key.code {
            KeyCode::Esc => self.set_focus(Focus::Sudo),
            KeyCode::Tab => self.set_focus(self.focus.next(self.auth, self.has_saved_selection())),
            KeyCode::BackTab => {
                self.set_focus(self.focus.prev(self.auth, self.has_saved_selection()))
            }
            KeyCode::Up => return self.move_up(),
            KeyCode::Down => return self.move_down(),
            KeyCode::Left => self.set_focus(Focus::Hosts),
            KeyCode::Right => self.set_focus(Focus::SudoToggle),
            KeyCode::Enter => {
                self.set_focus(self.focus.next(self.auth, self.has_saved_selection()))
            }
            KeyCode::Backspace => {
                self.sudo_password.pop();
            }
            KeyCode::Char(ch) if !ch.is_control() => self.sudo_password.push(ch),
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
        let layout = PopupGeometry::from_terminal_size(
            crossterm::terminal::size().unwrap_or((96, 24)),
            self,
        );
        if !point_in_rect(x, y, layout.dialog) {
            return PaneAction::None;
        }
        if point_in_rect(x, y, layout.hosts) {
            let row = y.saturating_sub(layout.hosts.y + 1) as usize;
            if row <= self.profiles.len() {
                self.selected = row;
                self.set_focus(Focus::Hosts);
                return PaneAction::LoadSecrets(self.sync_selected_profile());
            }
            return PaneAction::None;
        }
        if !point_in_rect(x, y, layout.details) {
            return PaneAction::None;
        }
        let row = y.saturating_sub(layout.details.y);
        let details = DetailsGeometry::from_area(layout.details, self);
        match row {
            row if row == details.rows.host => self.set_focus(Focus::Host),
            row if row == details.rows.port => self.set_focus(Focus::Port),
            row if row == details.rows.user => self.set_focus(Focus::User),
            row if row == details.rows.auth => self.set_focus(Focus::Auth),
            row if row == details.rows.password => {
                if password_visibility_button_hit(x, layout.details.x, self, PasswordField::Ssh) {
                    self.set_focus(Focus::Password);
                    self.toggle_password_visibility();
                } else {
                    self.set_focus(Focus::Password);
                }
            }
            row if row == details.rows.sudo => {
                if password_visibility_button_hit(x, layout.details.x, self, PasswordField::Sudo) {
                    self.set_focus(Focus::Sudo);
                    self.toggle_password_visibility();
                } else {
                    self.set_focus(Focus::Sudo);
                }
            }
            row if row == details.rows.remember => {
                self.set_focus(Focus::Remember);
                self.delete_confirm = DeleteConfirmState::Idle;
                self.remember = !self.remember;
            }
            row if Some(row) == details.rows.delete => {
                self.set_focus(Focus::Delete);
                return self.delete_action();
            }
            row if row == details.rows.connect => {
                self.set_focus(Focus::Connect);
                return self.connect_action();
            }
            _ => {}
        }
        PaneAction::None
    }

    fn move_up(&mut self) -> PaneAction {
        if self.focus == Focus::Hosts {
            if self.selected > 0 {
                self.selected -= 1;
                return PaneAction::LoadSecrets(self.sync_selected_profile());
            }
        } else {
            let mut next = self.focus.prev(self.auth, self.has_saved_selection());
            if next == Focus::Hosts {
                next = Focus::Host;
            }
            self.set_focus(next);
        }
        PaneAction::None
    }

    fn move_down(&mut self) -> PaneAction {
        if self.focus == Focus::Hosts {
            if self.selected < self.profiles.len() {
                self.selected += 1;
                return PaneAction::LoadSecrets(self.sync_selected_profile());
            }
        } else {
            let mut next = self.focus.next(self.auth, self.has_saved_selection());
            if next == Focus::Hosts {
                next = Focus::Connect;
            }
            self.set_focus(next);
        }
        PaneAction::None
    }

    fn default_detail_focus(&self) -> Focus {
        if self.selected >= self.profiles.len() {
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
            Focus::Password | Focus::PasswordToggle if self.auth == AuthChoice::Password => {
                self.show_ssh_password = !self.show_ssh_password;
            }
            Focus::Sudo | Focus::SudoToggle if self.sudo_mode != SudoMode::None => {
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
            Focus::Host | Focus::Port | Focus::User => PaneAction::None,
            Focus::Auth => {
                self.adjust_choice(1);
                PaneAction::None
            }
            Focus::Password => PaneAction::None,
            Focus::PasswordToggle => {
                self.toggle_password_visibility_for(Focus::PasswordToggle);
                PaneAction::None
            }
            Focus::Sudo => {
                self.start_sudo_password_edit();
                PaneAction::None
            }
            Focus::SudoToggle => {
                self.toggle_password_visibility_for(Focus::SudoToggle);
                PaneAction::None
            }
            Focus::Remember => {
                self.delete_confirm = DeleteConfirmState::Idle;
                self.remember = !self.remember;
                PaneAction::None
            }
            Focus::Delete => self.delete_action(),
            Focus::Connect => self.connect_action(),
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

    fn credentials_loading(&self) -> bool {
        matches!(self.secret_load, SecretLoadState::Loading { .. })
            || self.password_mode == PasswordMode::Loading
            || self.sudo_mode == SudoMode::Loading
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
    PasswordToggle,
    Sudo,
    SudoToggle,
    Remember,
    Delete,
    Connect,
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
            Self::Password if auth == AuthChoice::Key => Some(EditField::KeyPath),
            Self::Password if auth == AuthChoice::Password => Some(EditField::SshPassword),
            _ => None,
        }
    }

    fn ordered(_auth: AuthChoice, has_saved_selection: bool) -> Vec<Self> {
        let mut ordered = vec![
            Self::Hosts,
            Self::Host,
            Self::Port,
            Self::User,
            Self::Auth,
            Self::Password,
            Self::Sudo,
            Self::Remember,
        ];
        ordered.push(Self::Connect);
        if has_saved_selection {
            ordered.push(Self::Delete);
        }
        ordered
    }

    fn next(self, auth: AuthChoice, has_saved_selection: bool) -> Self {
        let ordered = Self::ordered(auth, has_saved_selection);
        let index = ordered.iter().position(|field| *field == self).unwrap_or(0);
        ordered[(index + 1) % ordered.len()]
    }

    fn prev(self, auth: AuthChoice, has_saved_selection: bool) -> Self {
        let ordered = Self::ordered(auth, has_saved_selection);
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SecretLoadRequest {
    id: u64,
    selected: usize,
    ssh_secret_id:
        Option<crate::runtime::remote_host::remote_host_secret_store::RemoteHostSecretId>,
    sudo_secret_id:
        Option<crate::runtime::remote_host::remote_host_secret_store::RemoteHostSecretId>,
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
}

#[derive(Debug, Clone, Copy)]
struct DetailsGeometry {
    connection: Rect,
    authentication: Rect,
    options: Rect,
    status: Rect,
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
    connect: u16,
    delete: Option<u16>,
}

#[derive(Debug, Clone, Copy)]
struct DeleteConfirmGeometry {
    dialog: Rect,
    cancel_button: Rect,
    delete_button: Rect,
}

impl DeleteConfirmGeometry {
    fn from_terminal_size((cols, rows): (u16, u16)) -> Self {
        let width = cols.min(56).max(36);
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

impl PopupGeometry {
    fn from_terminal_size((cols, rows): (u16, u16), state: &ConnectRemoteHostState) -> Self {
        let dialog = Rect::new(0, 0, cols, rows);
        let body = Rect::new(
            dialog.x,
            dialog.y.saturating_add(2),
            dialog.width,
            dialog.height.saturating_sub(2),
        );
        let host_width = host_list_width(state, body.width);
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(host_width),
                Constraint::Length(1),
                Constraint::Min(DETAIL_MIN_WIDTH),
            ])
            .split(body);
        Self {
            dialog,
            hosts: columns[0],
            details: columns[2],
        }
    }
}

impl DetailsGeometry {
    fn from_area(area: Rect, state: &ConnectRemoteHostState) -> Self {
        let options_rows = if state.has_saved_selection() { 4 } else { 3 };
        let options_height = 1 + options_rows;
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(4),
                Constraint::Length(4),
                Constraint::Length(options_height),
                Constraint::Min(0),
            ])
            .split(area);
        let rows = DetailsRows {
            host: sections[0].y.saturating_add(1).saturating_sub(area.y),
            port: sections[0].y.saturating_add(2).saturating_sub(area.y),
            user: sections[0].y.saturating_add(3).saturating_sub(area.y),
            auth: sections[1].y.saturating_add(1).saturating_sub(area.y),
            password: sections[1].y.saturating_add(2).saturating_sub(area.y),
            sudo: sections[1].y.saturating_add(3).saturating_sub(area.y),
            remember: sections[2].y.saturating_add(1).saturating_sub(area.y),
            connect: sections[2].y.saturating_add(3).saturating_sub(area.y),
            delete: state
                .has_saved_selection()
                .then(|| sections[2].y.saturating_add(4).saturating_sub(area.y)),
        };
        Self {
            connection: sections[0],
            authentication: sections[1],
            options: sections[2],
            status: sections[3],
            rows,
        }
    }
}

fn render(frame: &mut Frame<'_>, state: &ConnectRemoteHostState) {
    let geometry =
        PopupGeometry::from_terminal_size((frame.size().width, frame.size().height), state);
    frame.render_widget(Clear, geometry.dialog);
    frame.render_widget(
        Paragraph::new(
            Line::from("Connect Remote Host")
                .style(Style::default().add_modifier(Modifier::BOLD))
                .alignment(Alignment::Center),
        ),
        Rect::new(
            geometry.dialog.x,
            geometry.dialog.y,
            geometry.dialog.width,
            1,
        ),
    );

    render_hosts(frame, geometry.hosts, state);
    frame.render_widget(
        Paragraph::new("│").style(Style::default().fg(Color::DarkGray)),
        Rect::new(
            geometry.hosts.x + geometry.hosts.width,
            geometry.hosts.y,
            1,
            geometry.hosts.height,
        ),
    );
    render_details(frame, geometry.details, state);
    render_cursor(frame, geometry.details, state);
    render_delete_confirm(frame, state);
}

fn render_hosts(frame: &mut Frame<'_>, area: Rect, state: &ConnectRemoteHostState) {
    let items = host_list_labels(state)
        .into_iter()
        .map(ListItem::new)
        .collect::<Vec<_>>();
    let list = List::new(items)
        .block(
            Block::default()
                .title(section_title("Saved Hosts"))
                .borders(Borders::RIGHT),
        )
        .highlight_style(if state.focus == Focus::Hosts {
            active_focus_style()
        } else {
            selected_host_style()
        })
        .highlight_symbol(if state.focus == Focus::Hosts {
            "> "
        } else {
            "  "
        });
    let mut list_state = ratatui::widgets::ListState::default();
    list_state.select(Some(state.selected));
    frame.render_stateful_widget(list, area, &mut list_state);
}

fn saved_host_label(profile: &RemoteHostProfile) -> String {
    format!("{}@{}", profile.ssh_user, profile.host)
}

fn host_list_labels(state: &ConnectRemoteHostState) -> Vec<String> {
    let mut labels = state
        .profiles
        .iter()
        .map(saved_host_label)
        .collect::<Vec<_>>();
    labels.push("+ New Host".to_string());
    labels
}

const HOST_LIST_MIN_WIDTH: u16 = 20;
const HOST_LIST_MAX_WIDTH: u16 = 34;
const DETAIL_MIN_WIDTH: u16 = 42;
const SEPARATOR_WIDTH: u16 = 1;
const LIST_PADDING: u16 = 6;

fn host_list_width(state: &ConnectRemoteHostState, body_width: u16) -> u16 {
    let content_width = host_list_labels(state)
        .iter()
        .map(|label| display_width(label) as u16)
        .chain(std::iter::once(display_width("Saved Hosts") as u16))
        .max()
        .unwrap_or(0)
        .saturating_add(LIST_PADDING);
    let preferred = content_width.clamp(HOST_LIST_MIN_WIDTH, HOST_LIST_MAX_WIDTH);
    let available = body_width.saturating_sub(DETAIL_MIN_WIDTH + SEPARATOR_WIDTH);
    preferred
        .min(available)
        .max(HOST_LIST_MIN_WIDTH.min(available))
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
    let geometry = DetailsGeometry::from_area(area, state);
    render_connection(frame, geometry.connection, state);
    render_authentication(frame, geometry.authentication, state);
    render_options(frame, geometry.options, state);
    render_status(frame, geometry.status, state);
}

fn render_connection(frame: &mut Frame<'_>, area: Rect, state: &ConnectRemoteHostState) {
    let rows = [
        detail_row("Host", &state.host, state, Focus::Host),
        detail_row("Port", &state.remote_port, state, Focus::Port),
        detail_row("SSH User", &state.ssh_user, state, Focus::User),
    ];
    render_detail_table(frame, area, "Connection", rows);
}

fn render_authentication(frame: &mut Frame<'_>, area: Rect, state: &ConnectRemoteHostState) {
    let mut rows = vec![choice_row("Auth", auth_tabs(state), state, Focus::Auth)];
    if state.auth == AuthChoice::Key {
        rows.push(detail_row(
            "Key",
            &password_display(state),
            state,
            Focus::Password,
        ));
    } else {
        rows.push(password_row("Password", PasswordField::Ssh, state));
    }
    rows.push(password_row("Sudo", PasswordField::Sudo, state));
    render_detail_table(frame, area, "Authentication", rows);
}

fn render_options(frame: &mut Frame<'_>, area: Rect, state: &ConnectRemoteHostState) {
    let mut rows = vec![detail_row(
        "Save",
        if state.remember {
            "[x] Remember host"
        } else {
            "[ ] Do not save"
        },
        state,
        Focus::Remember,
    )];
    rows.push(Row::new(vec![String::new(), String::new()]));
    rows.push(connect_row(connect_label(state), state));
    if state.has_saved_selection() {
        rows.push(action_row(delete_label(state), state, Focus::Delete));
    }
    render_detail_table(frame, area, "Options", rows);
}

fn section_title(title: &str) -> Line<'static> {
    Line::from(title.to_string()).style(Style::default().add_modifier(Modifier::BOLD))
}

fn render_detail_table<I>(frame: &mut Frame<'_>, area: Rect, title: &str, rows: I)
where
    I: IntoIterator<Item = Row<'static>>,
{
    let table = Table::new(rows, [Constraint::Length(12), Constraint::Min(20)])
        .block(
            Block::default()
                .title(section_title(title))
                .borders(Borders::TOP),
        )
        .column_spacing(1);
    frame.render_widget(table, area);
}

fn detail_row(
    label: &str,
    value: &str,
    state: &ConnectRemoteHostState,
    focus: Focus,
) -> Row<'static> {
    let prefix = " ";
    let style = detail_focus_style(state, focus);
    Row::new(vec![label.to_string(), format!("{prefix}{value}")]).style(style)
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
        Line::from(label.to_string()),
        password_control_line(field, state),
    ])
}

fn password_control_line(field: PasswordField, state: &ConnectRemoteHostState) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    let display = password_control_display(field, state);
    let value_style = if state.focus == display.field_focus {
        active_focus_style()
    } else {
        Style::default()
    };
    spans.push(Span::styled(display.value, value_style));
    if display.toggle_available {
        spans.push(Span::raw("  "));
        let toggle_style = if state.focus == display.toggle_focus {
            active_focus_style()
        } else {
            Style::default()
        };
        spans.push(Span::styled(
            password_visibility_label(display.show_plaintext),
            toggle_style,
        ));
    }
    Line::from(spans)
}

struct PasswordControlDisplay {
    value: String,
    show_plaintext: bool,
    field_focus: Focus,
    toggle_focus: Focus,
    toggle_available: bool,
}

fn password_control_display(
    field: PasswordField,
    state: &ConnectRemoteHostState,
) -> PasswordControlDisplay {
    match field {
        PasswordField::Ssh if state.auth == AuthChoice::Password => PasswordControlDisplay {
            value: password_field_display(
                &state.ssh_password,
                state.password_mode == PasswordMode::Loading,
                state.show_ssh_password,
                "",
            ),
            show_plaintext: state.show_ssh_password,
            field_focus: Focus::Password,
            toggle_focus: Focus::PasswordToggle,
            toggle_available: state.password_mode != PasswordMode::Loading,
        },
        PasswordField::Sudo if state.sudo_mode != SudoMode::None => PasswordControlDisplay {
            value: password_field_display(
                sudo_password_value(state),
                state.sudo_mode == SudoMode::Loading,
                state.show_sudo_password,
                "",
            ),
            show_plaintext: state.show_sudo_password,
            field_focus: Focus::Sudo,
            toggle_focus: Focus::SudoToggle,
            toggle_available: state.sudo_mode != SudoMode::Loading,
        },
        PasswordField::Sudo => PasswordControlDisplay {
            value: "No sudo password".to_string(),
            show_plaintext: false,
            field_focus: Focus::Sudo,
            toggle_focus: Focus::SudoToggle,
            toggle_available: false,
        },
        PasswordField::Ssh => PasswordControlDisplay {
            value: password_display(state),
            show_plaintext: false,
            field_focus: Focus::Password,
            toggle_focus: Focus::PasswordToggle,
            toggle_available: false,
        },
    }
}

fn action_row(
    value: impl Into<String>,
    state: &ConnectRemoteHostState,
    focus: Focus,
) -> Row<'static> {
    let focused = state.focus == focus;
    let style = action_focus_style(focused, focus);
    Row::new(vec![String::new(), format!(" {}", value.into())]).style(style)
}

fn action_focus_style(focused: bool, focus: Focus) -> Style {
    if focused {
        match focus {
            Focus::Delete => delete_focus_style(),
            _ => active_focus_style(),
        }
    } else {
        match focus {
            Focus::Delete => Style::default().fg(Color::Red),
            _ => Style::default().add_modifier(Modifier::BOLD),
        }
    }
}

fn connect_row(value: impl Into<String>, state: &ConnectRemoteHostState) -> Row<'static> {
    action_row(value, state, Focus::Connect)
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
        Line::from(label.to_string()).style(label_style),
        choice_line(value, focused),
    ])
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
    let mut spans = vec![Span::raw(" ")];
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

fn password_display(state: &ConnectRemoteHostState) -> String {
    match state.auth {
        AuthChoice::Password => password_field_with_toggle_display(
            &state.ssh_password,
            state.password_mode == PasswordMode::Loading,
            state.show_ssh_password,
            "",
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

#[cfg(test)]
fn sudo_password_display(state: &ConnectRemoteHostState) -> String {
    if state.sudo_mode == SudoMode::None {
        return "No sudo password".to_string();
    }
    password_field_with_toggle_display(
        sudo_password_value(state),
        state.sudo_mode == SudoMode::Loading,
        state.show_sudo_password,
        "",
    )
}

fn password_field_with_toggle_display(
    value: &str,
    loading: bool,
    show_plaintext: bool,
    empty_label: &str,
) -> String {
    let value = password_field_display(value, loading, show_plaintext, empty_label);
    if loading {
        value
    } else {
        format!("{}  {}", value, password_visibility_label(show_plaintext))
    }
}

fn password_field_display(
    value: &str,
    loading: bool,
    show_plaintext: bool,
    empty_label: &str,
) -> String {
    if loading {
        "Loading saved...".to_string()
    } else if value.is_empty() {
        empty_label.to_string()
    } else if show_plaintext {
        value.to_string()
    } else {
        password_mask(value)
    }
}

fn password_mask(value: &str) -> String {
    "*".repeat(value.chars().count().max(6))
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

fn password_visibility_label(show_plaintext: bool) -> &'static str {
    if show_plaintext {
        "Hide"
    } else {
        "Show"
    }
}

fn password_visibility_button_hit(
    x: u16,
    details_x: u16,
    state: &ConnectRemoteHostState,
    field: PasswordField,
) -> bool {
    let (value, loading, show_plaintext, empty_label) = match field {
        PasswordField::Ssh if state.auth == AuthChoice::Password => (
            state.ssh_password.as_str(),
            state.password_mode == PasswordMode::Loading,
            state.show_ssh_password,
            "",
        ),
        PasswordField::Sudo if state.sudo_mode != SudoMode::None => (
            sudo_password_value(state),
            state.sudo_mode == SudoMode::Loading,
            state.show_sudo_password,
            "",
        ),
        _ => return false,
    };
    if loading {
        return false;
    }
    let field_start = details_x.saturating_add(14);
    let value_width = password_field_display(value, loading, show_plaintext, empty_label)
        .chars()
        .count() as u16;
    let button_start = field_start.saturating_add(value_width).saturating_add(2);
    let button_end =
        button_start.saturating_add(password_visibility_label(show_plaintext).len() as u16);
    x >= button_start && x < button_end
}

fn delete_label(_state: &ConnectRemoteHostState) -> String {
    "Delete".to_string()
}

fn connect_label(state: &ConnectRemoteHostState) -> String {
    let label = if matches!(state.status, Status::Working(_)) {
        "Connecting..."
    } else if state.credentials_loading() {
        "Loading..."
    } else {
        "Connect"
    };
    label.to_string()
}

fn render_status(frame: &mut Frame<'_>, area: Rect, state: &ConnectRemoteHostState) {
    let color = match &state.status {
        Status::Hint(_) | Status::Loading(_) | Status::Working(_) => Color::DarkGray,
        Status::Error(_) => Color::Red,
    };
    let message = status_message(state);
    frame.render_widget(
        Paragraph::new(message)
            .style(Style::default().fg(color))
            .alignment(Alignment::Left),
        area,
    );
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
    let geometry =
        DeleteConfirmGeometry::from_terminal_size((frame.size().width, frame.size().height));
    frame.render_widget(Clear, geometry.dialog);
    let block = Block::default()
        .title(section_title("Delete saved host"))
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
    let rows = DetailsGeometry::from_area(details, state).rows;
    let (row, value) = match field {
        EditField::Host => (rows.host, state.host.as_str().to_string()),
        EditField::RemotePort => (rows.port, state.remote_port.as_str().to_string()),
        EditField::SshUser => (rows.user, state.ssh_user.as_str().to_string()),
        EditField::KeyPath => (rows.password, state.key_path.as_str().to_string()),
        EditField::SshPassword => (
            rows.password,
            password_field_display(
                &state.ssh_password,
                state.password_mode == PasswordMode::Loading,
                state.show_ssh_password,
                "",
            ),
        ),
        EditField::SudoPassword => (
            rows.sudo,
            password_field_display(
                sudo_password_value(state),
                state.sudo_mode == SudoMode::Loading,
                state.show_sudo_password,
                "",
            ),
        ),
    };
    let value_width = value.chars().count() as u16;
    let desired_x = details.x.saturating_add(14).saturating_add(value_width);
    let max_x = details.x.saturating_add(details.width.saturating_sub(1));
    Some((desired_x.min(max_x), details.y.saturating_add(row)))
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

fn edit_buffer(state: &mut ConnectRemoteHostState, field: EditField) -> &mut String {
    match field {
        EditField::Host => &mut state.host,
        EditField::RemotePort => &mut state.remote_port,
        EditField::SshUser => &mut state.ssh_user,
        EditField::KeyPath => &mut state.key_path,
        EditField::SshPassword => &mut state.ssh_password,
        EditField::SudoPassword => &mut state.sudo_password,
    }
}

fn edit_focus(field: EditField) -> Focus {
    match field {
        EditField::Host => Focus::Host,
        EditField::RemotePort => Focus::Port,
        EditField::SshUser => Focus::User,
        EditField::KeyPath | EditField::SshPassword => Focus::Password,
        EditField::SudoPassword => Focus::Sudo,
    }
}

fn spawn_secret_loader(request: SecretLoadRequest, tx: Sender<SecretLoadResult>) {
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

fn run_connect(
    state: &ConnectRemoteHostState,
    command: &ConnectRemoteHostPaneCommand,
    network: &RemoteNetworkConfig,
) -> Result<String, String> {
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
    if let Some(profile) = state
        .selected_profile()
        .filter(|_| saved_profile_can_connect_by_id(state))
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
            normalized_port(&state.remote_port),
        ]);
        if state.remember {
            args.push("--save-profile".to_string());
            args.push(format!("{}@{}", state.ssh_user, state.host));
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
        if let Some(stdin) = child.stdin.as_mut() {
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

fn saved_profile_can_connect_by_id(state: &ConnectRemoteHostState) -> bool {
    state.password_mode == PasswordMode::Saved
        && matches!(state.sudo_mode, SudoMode::Saved | SudoMode::None)
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
    id: &crate::runtime::remote_host::remote_host_secret_store::RemoteHostSecretId,
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
        }];
        state.selected = 0;
        let _ = state.sync_selected_profile();
        assert_eq!(state.host, "127.0.0.1");
        assert_eq!(state.ssh_user, "k");
        assert_eq!(segmented_for_test(&auth_tabs(&state)), "Password  Key");
    }

    #[test]
    fn connect_popup_initial_secret_load_request_only_targets_selected_profile() {
        let ssh_id =
            crate::runtime::remote_host::remote_host_secret_store::RemoteHostSecretId::new(
                "waitagent.remote-host.first.ssh-password",
            )
            .unwrap();
        let sudo_id =
            crate::runtime::remote_host::remote_host_secret_store::RemoteHostSecretId::new(
                "waitagent.remote-host.first.sudo-password",
            )
            .unwrap();
        let second_id =
            crate::runtime::remote_host::remote_host_secret_store::RemoteHostSecretId::new(
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
        let ssh_id =
            crate::runtime::remote_host::remote_host_secret_store::RemoteHostSecretId::new(
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
        }];
        state.selected = 0;

        let initial_request = state.sync_selected_profile();

        assert!(initial_request.is_some());
        assert!(state.credentials_loading());
        assert_eq!(connect_label(&state), "Loading...");
    }

    #[test]
    fn connect_popup_loads_saved_passwords_through_event_loop_result() {
        let ssh_id =
            crate::runtime::remote_host::remote_host_secret_store::RemoteHostSecretId::new(
                "waitagent.remote-host.k-127-0-0-1.ssh-password",
            )
            .unwrap();
        let sudo_id =
            crate::runtime::remote_host::remote_host_secret_store::RemoteHostSecretId::new(
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
        assert_eq!(password_display(&state), "**********  Show");
        assert_eq!(sudo_password_display(&state), "***********  Show");
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
        };

        assert_eq!(saved_host_label(&profile), "k@127.0.0.1");
    }

    #[test]
    fn popup_geometry_uses_content_sized_dialog_for_short_profiles() {
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
        }];
        state.selected = 1;
        let _ = state.sync_selected_profile();

        let geometry = PopupGeometry::from_terminal_size((66, 17), &state);

        assert_eq!(geometry.dialog.width, 66);
        assert_eq!(geometry.dialog.height, 17);
        assert_eq!(geometry.hosts.y, 2);
        assert_eq!(geometry.details.y, 2);
        assert_eq!(geometry.hosts.height, 15);
        assert_eq!(geometry.hosts.width, 20);
        assert!(geometry.details.width >= DETAIL_MIN_WIDTH);
    }

    #[test]
    fn host_list_width_uses_compact_width_for_short_saved_hosts() {
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
        }];

        assert_eq!(host_list_width(&state, 98), 20);
    }

    #[test]
    fn host_list_width_caps_long_saved_hosts() {
        let mut state = ConnectRemoteHostState::load();
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
        }];

        assert_eq!(host_list_width(&state, 98), 34);
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
        assert_eq!(state.focus, Focus::Remember);
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
        let popup = PopupGeometry::from_terminal_size((66, 17), &state);
        let details = DetailsGeometry::from_area(popup.details, &state);

        assert_eq!(popup.details.y, 2);
        assert_eq!(popup.details.height, 15);
        assert_eq!(details.rows.delete, Some(12));
        assert!(
            popup.details.y + details.rows.delete.unwrap() < popup.details.y + popup.details.height
        );

        let output = rendered_text(66, 17, &state);
        assert!(output.contains("Connect Remote Host"));
        assert!(output.contains("Remember host"));
        assert!(output.contains("Connect"));
        assert!(output.contains("Delete"));
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
            column: geometry.details.x + 20,
            row: geometry.details.y + details.rows.password,
            modifiers: crossterm::event::KeyModifiers::empty(),
        });

        assert_eq!(state.focus, Focus::Password);
        assert_eq!(state.editing, Some(EditField::SshPassword));
    }

    #[test]
    fn password_rows_style_only_the_focused_subcontrol() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles.clear();
        state.selected = 0;
        let _ = state.sync_selected_profile();
        state.ssh_password = "secret".to_string();

        state.set_focus(Focus::Password);
        assert_password_control_styles(
            password_control_line(PasswordField::Ssh, &state),
            active_focus_style(),
            Style::default(),
        );

        state.set_focus(Focus::PasswordToggle);
        assert_password_control_styles(
            password_control_line(PasswordField::Ssh, &state),
            Style::default(),
            active_focus_style(),
        );

        state.set_focus(Focus::Sudo);
        assert_password_control_styles(
            password_control_line(PasswordField::Sudo, &state),
            active_focus_style(),
            Style::default(),
        );

        state.set_focus(Focus::SudoToggle);
        assert_password_control_styles(
            password_control_line(PasswordField::Sudo, &state),
            Style::default(),
            active_focus_style(),
        );
    }

    fn assert_password_control_styles(
        line: Line<'static>,
        input_style: Style,
        toggle_style: Style,
    ) {
        assert_eq!(line.spans[0].style, Style::default());
        assert_eq!(line.spans[1].content.as_ref(), "******");
        assert_eq!(line.spans[1].style, input_style);
        assert_eq!(line.spans[2].style, Style::default());
        assert_eq!(line.spans[3].content.as_ref(), "Show");
        assert_eq!(line.spans[3].style, toggle_style);
    }

    #[test]
    fn password_and_sudo_empty_states_leave_input_display_empty() {
        let mut state = ConnectRemoteHostState::load();
        state.profiles.clear();
        state.selected = 0;
        let _ = state.sync_selected_profile();
        state.set_focus(Focus::Password);

        let password_line = password_control_line(PasswordField::Ssh, &state);
        assert_eq!(password_line.spans[1].content.as_ref(), "");
        assert_eq!(password_line.spans[3].content.as_ref(), "Show");
        assert_eq!(password_display(&state), "  Show");

        state.set_focus(Focus::Sudo);
        let sudo_line = password_control_line(PasswordField::Sudo, &state);
        assert_eq!(sudo_line.spans[1].content.as_ref(), "");
        assert_eq!(sudo_line.spans[3].content.as_ref(), "Show");
        assert_eq!(sudo_password_display(&state), "  Show");
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
        assert_eq!(x, geometry.details.x + 14);
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
    fn password_field_focus_has_cursor_and_right_enter_toggles_visibility() {
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
        assert_eq!(x, geometry.details.x + 14 + 6);
        assert_eq!(
            state.apply_key(KeyEvent::from(KeyCode::Right)),
            PaneAction::None
        );
        assert_eq!(state.focus, Focus::PasswordToggle);
        assert_eq!(
            state.apply_key(KeyEvent::from(KeyCode::Enter)),
            PaneAction::None
        );
        assert!(state.show_ssh_password);
        assert_eq!(password_display(&state), "secret  Hide");
        assert_eq!(
            state.apply_key(KeyEvent::from(KeyCode::Left)),
            PaneAction::None
        );
        assert_eq!(state.focus, Focus::Password);
        assert_eq!(state.editing, Some(EditField::SshPassword));
    }

    #[test]
    fn sudo_field_focus_has_cursor_and_right_enter_toggles_visibility() {
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
        assert_eq!(x, geometry.details.x + 14 + 6);
        assert_eq!(
            state.apply_key(KeyEvent::from(KeyCode::Right)),
            PaneAction::None
        );
        assert_eq!(state.focus, Focus::SudoToggle);
        assert_eq!(
            state.apply_key(KeyEvent::from(KeyCode::Enter)),
            PaneAction::None
        );
        assert!(state.show_sudo_password);
        assert_eq!(sudo_password_display(&state), "secret  Hide");
        assert_eq!(
            state.apply_key(KeyEvent::from(KeyCode::Left)),
            PaneAction::None
        );
        assert_eq!(state.focus, Focus::Sudo);
        assert_eq!(state.editing, Some(EditField::SudoPassword));
    }

    #[test]
    fn password_visibility_button_toggles_show_hide_state() {
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
            column: geometry.details.x + 14 + 6 + 2,
            row: geometry.details.y + details.rows.password,
            modifiers: crossterm::event::KeyModifiers::empty(),
        });

        assert!(state.show_ssh_password);
        assert_eq!(password_display(&state), "secret  Hide");
        assert_eq!(state.focus, Focus::Password);
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

        assert_eq!(password_display(&state), "******  Show");
        assert_eq!(y, geometry.details.y + details.rows.password);
        assert_eq!(x, geometry.details.x + 14 + 6);
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

        assert_eq!(focused.spans[1].content.as_ref(), "Password");
        assert_eq!(inactive.spans[1].content.as_ref(), "Password");
        assert_eq!(focused.spans[1].style, active_focus_style());
        assert_eq!(inactive.spans[1].style, selected_host_style());
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

        assert_eq!(sudo_password_display(&state), "**********  Show");
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
    fn connect_popup_ignores_connect_action_while_working() {
        let mut state = ConnectRemoteHostState::load();
        state.focus = Focus::Connect;
        state.status = Status::Working("Connecting...".to_string());

        assert_eq!(
            state.apply_key(KeyEvent::from(KeyCode::Enter)),
            PaneAction::None
        );
        assert_eq!(connect_label(&state), "Connecting...");
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
