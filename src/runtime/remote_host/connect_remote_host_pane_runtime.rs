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
use crossterm::event::{self, Event, KeyCode, KeyEvent, MouseButton, MouseEventKind};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
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

        let mut state = ConnectRemoteHostState::load();
        let result = self.run_event_loop(&mut terminal, &mut state, command);

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
    ) -> Result<(), LifecycleError> {
        let (secret_tx, secret_rx) = mpsc::channel();
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
    remember: bool,
    editing: Option<EditField>,
    status: Status,
    secret_load: SecretLoadState,
    next_secret_request_id: u64,
}

impl ConnectRemoteHostState {
    fn load() -> Self {
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
            remember: true,
            editing: None,
            status: Status::Hint("Select a saved host or fill a new host.".to_string()),
            secret_load: SecretLoadState::Idle,
            next_secret_request_id: 1,
        };
        let _ = state.sync_selected_profile();
        state
    }

    fn sync_selected_profile(&mut self) -> Option<SecretLoadRequest> {
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
                self.set_focus(self.focus.next(self.auth));
                PaneAction::None
            }
            KeyCode::BackTab => {
                self.set_focus(self.focus.prev(self.auth));
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
                } else {
                    self.adjust_choice(1);
                }
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

    fn apply_edit_key(&mut self, key: KeyEvent, field: EditField) -> PaneAction {
        match key.code {
            KeyCode::Esc | KeyCode::Left => self.set_focus(Focus::Hosts),
            KeyCode::Tab => self.set_focus(self.focus.next(self.auth)),
            KeyCode::BackTab => self.set_focus(self.focus.prev(self.auth)),
            KeyCode::Up => return self.move_up(),
            KeyCode::Down => return self.move_down(),
            KeyCode::Enter => {}
            KeyCode::Backspace => {
                edit_buffer(self, field).pop();
            }
            KeyCode::Char(ch) if !ch.is_control() => edit_buffer(self, field).push(ch),
            _ => {}
        }
        PaneAction::None
    }

    fn apply_mouse(&mut self, mouse: crossterm::event::MouseEvent) -> PaneAction {
        if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
            return PaneAction::None;
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
        match row {
            1 => self.set_focus(Focus::Host),
            2 => self.set_focus(Focus::Port),
            3 => self.set_focus(Focus::User),
            6 => self.set_focus(Focus::Auth),
            7 => self.set_focus(Focus::Password),
            row if row == sudo_row_for(self.auth) as u16 => self.set_focus(Focus::Sudo),
            row if row == remember_row_for(self.auth) as u16 => {
                self.set_focus(Focus::Remember);
                self.remember = !self.remember;
            }
            row if row == connect_row_for(self.auth) as u16 => {
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
            let mut next = self.focus.prev(self.auth);
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
            let mut next = self.focus.next(self.auth);
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
        self.focus = focus;
        self.editing = focus.edit_field(self.auth);
        if focus == Focus::Password
            && self.auth == AuthChoice::Password
            && self.ssh_password.is_empty()
        {
            self.password_mode = PasswordMode::Enter;
        }
        if focus == Focus::Sudo && self.sudo_mode == SudoMode::Replace {
            self.editing = Some(EditField::SudoPassword);
        }
    }

    fn start_edit(&mut self, field: EditField) {
        self.focus = edit_focus(field);
        self.editing = Some(field);
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
            Focus::Sudo => {
                self.adjust_choice(1);
                PaneAction::None
            }
            Focus::Remember => {
                self.remember = !self.remember;
                PaneAction::None
            }
            Focus::Connect => self.connect_action(),
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
            Focus::Sudo => {
                self.sudo_mode = self
                    .sudo_mode
                    .shift(step, self.auth, self.saved_sudo_password());
                self.set_focus(Focus::Sudo);
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

    fn saved_ssh_password(&self) -> bool {
        matches!(
            self.selected_profile().map(|profile| &profile.auth),
            Some(RemoteHostAuthProfile::Password {
                password_secret_id: Some(_),
            })
        )
    }

    fn saved_sudo_password(&self) -> bool {
        self.selected_profile()
            .and_then(|profile| profile.sudo_password_secret_id.as_ref())
            .is_some()
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
    Connect,
}

impl Focus {
    fn uses_horizontal_choice(self) -> bool {
        matches!(self, Self::Auth | Self::Sudo)
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

    fn ordered(_auth: AuthChoice) -> Vec<Self> {
        vec![
            Self::Hosts,
            Self::Host,
            Self::Port,
            Self::User,
            Self::Auth,
            Self::Password,
            Self::Sudo,
            Self::Remember,
            Self::Connect,
        ]
    }

    fn next(self, auth: AuthChoice) -> Self {
        let ordered = Self::ordered(auth);
        let index = ordered.iter().position(|field| *field == self).unwrap_or(0);
        ordered[(index + 1) % ordered.len()]
    }

    fn prev(self, auth: AuthChoice) -> Self {
        let ordered = Self::ordered(auth);
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

impl SudoMode {
    fn shift(self, step: i32, auth: AuthChoice, saved: bool) -> Self {
        let mut values = Vec::new();
        if auth == AuthChoice::Password {
            values.push(Self::SameAsSsh);
        }
        if saved {
            values.push(Self::Saved);
        }
        values.extend([Self::Replace, Self::None]);
        shift_value(&values, self, step)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Status {
    Hint(String),
    Loading(String),
    Working(String),
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PaneAction {
    None,
    Redraw,
    Close,
    Connect,
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
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5),
            Constraint::Length(5),
            Constraint::Length(6),
            Constraint::Min(0),
        ])
        .split(area);
    render_connection(frame, sections[0], state);
    render_authentication(frame, sections[1], state);
    render_options(frame, sections[2], state);
    render_status(frame, sections[3], state);
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
    let mut rows = vec![focused_row("Auth", auth_tabs(state), state, Focus::Auth)];
    rows.push(detail_row(
        if state.auth == AuthChoice::Key {
            "Key"
        } else {
            "Password"
        },
        &password_display(state),
        state,
        Focus::Password,
    ));
    rows.push(focused_row("Sudo", sudo_tabs(state), state, Focus::Sudo));
    render_detail_table(frame, area, "Authentication", rows);
}

fn render_options(frame: &mut Frame<'_>, area: Rect, state: &ConnectRemoteHostState) {
    let rows = [
        detail_row(
            "Save",
            if state.remember {
                "[x] Remember host"
            } else {
                "[ ] Do not save"
            },
            state,
            Focus::Remember,
        ),
        Row::new(vec![String::new(), String::new()]),
        connect_row(connect_label(state), state),
    ];
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
    let style = if state.focus == focus {
        active_focus_style()
    } else {
        Style::default()
    };
    Row::new(vec![label.to_string(), format!("{prefix}{value}")]).style(style)
}

fn connect_row(value: impl Into<String>, state: &ConnectRemoteHostState) -> Row<'static> {
    let style = if state.focus == Focus::Connect {
        active_focus_style()
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    };
    Row::new(vec![String::new(), format!(" {}", value.into())]).style(style)
}

fn focused_row(
    label: &str,
    value: impl Into<String>,
    state: &ConnectRemoteHostState,
    focus: Focus,
) -> Row<'static> {
    let focused = state.focus == focus;
    let style = if focused {
        active_focus_style()
    } else {
        Style::default()
    };
    let prefix = " ";
    Row::new(vec![label.to_string(), format!("{prefix}{}", value.into())]).style(style)
}

fn active_focus_style() -> Style {
    Style::default()
        .bg(Color::Blue)
        .fg(Color::White)
        .add_modifier(Modifier::BOLD)
}

fn selected_host_style() -> Style {
    Style::default().bg(Color::Gray).fg(Color::Black)
}

fn auth_tabs(state: &ConnectRemoteHostState) -> String {
    segmented(&[
        ("Password", state.auth == AuthChoice::Password),
        ("Key", state.auth == AuthChoice::Key),
    ])
}

fn sudo_tabs(state: &ConnectRemoteHostState) -> String {
    let mut segments = Vec::new();
    if state.auth == AuthChoice::Password {
        segments.push(("Same", state.sudo_mode == SudoMode::SameAsSsh));
    }
    if state.sudo_mode == SudoMode::Loading {
        segments.push(("Loading", true));
    } else if state.saved_sudo_password() {
        segments.push(("Saved", state.sudo_mode == SudoMode::Saved));
    }
    segments.push(("Replace", state.sudo_mode == SudoMode::Replace));
    segments.push(("None", state.sudo_mode == SudoMode::None));
    segmented(&segments)
}

fn segmented(segments: &[(&str, bool)]) -> String {
    segments
        .iter()
        .map(|(label, selected)| {
            if *selected {
                format!("[{label}]")
            } else {
                label.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("  ")
}

fn password_display(state: &ConnectRemoteHostState) -> String {
    match state.auth {
        AuthChoice::Password => match state.password_mode {
            PasswordMode::Loading => "Loading saved...".to_string(),
            PasswordMode::Saved => "Saved encrypted".to_string(),
            PasswordMode::Enter if state.ssh_password.is_empty() => "Enter password".to_string(),
            PasswordMode::Enter => "*".repeat(state.ssh_password.chars().count().max(6)),
        },
        AuthChoice::Key => {
            if state.key_path.is_empty() {
                "Key file path".to_string()
            } else {
                state.key_path.clone()
            }
        }
    }
}

fn connect_label(state: &ConnectRemoteHostState) -> String {
    let label = if matches!(state.status, Status::Working(_)) {
        "Connecting..."
    } else if state.credentials_loading() {
        "Loading..."
    } else {
        "Connect"
    };
    if state.focus == Focus::Connect {
        format!("[ {label} ]")
    } else {
        label.to_string()
    }
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

fn render_cursor(frame: &mut Frame<'_>, details: Rect, state: &ConnectRemoteHostState) {
    let Some(field) = state.editing else {
        return;
    };
    let (row, value) = match field {
        EditField::Host => (1, state.host.as_str()),
        EditField::RemotePort => (2, state.remote_port.as_str()),
        EditField::SshUser => (3, state.ssh_user.as_str()),
        EditField::KeyPath => (7, state.key_path.as_str()),
        EditField::SshPassword => (7, state.ssh_password.as_str()),
        EditField::SudoPassword => (
            sudo_row_for(state.auth) as u16 + 1,
            state.sudo_password.as_str(),
        ),
    };
    let x = details.x + 14 + value.chars().count() as u16;
    let y = details.y + row;
    frame.set_cursor(x, y);
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

fn sudo_row_for(_auth: AuthChoice) -> usize {
    8
}

fn remember_row_for(_auth: AuthChoice) -> usize {
    10
}

fn connect_row_for(_auth: AuthChoice) -> usize {
    11
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
    if let Some(profile) = state.selected_profile().filter(|_| {
        state.password_mode == PasswordMode::Saved && state.sudo_mode != SudoMode::Replace
    }) {
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
                SudoMode::SameAsSsh => {
                    if !state.ssh_password.is_empty() {
                        state.ssh_password.clone()
                    } else {
                        saved_ssh_password_value(state)?
                    }
                }
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

fn saved_ssh_password_value(state: &ConnectRemoteHostState) -> Result<String, String> {
    let id =
        saved_ssh_secret_id(state).ok_or_else(|| "Saved SSH password is missing.".to_string())?;
    load_secret_value_from_str(&id)
}

fn load_secret_value_from_str(id: &str) -> Result<String, String> {
    let id = crate::runtime::remote_host::remote_host_secret_store::RemoteHostSecretId::new(id)
        .map_err(|error| error.to_string())?;
    load_secret_value(&id)
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
        assert_eq!(auth_tabs(&state), "[Password]  Key");
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
        assert_eq!(password_display(&state), "Saved encrypted");
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

        let geometry = PopupGeometry::from_terminal_size((66, 16), &state);

        assert_eq!(geometry.dialog.width, 66);
        assert_eq!(geometry.dialog.height, 16);
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

        state.apply_mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: geometry.details.x + 20,
            row: geometry.details.y + 7,
            modifiers: crossterm::event::KeyModifiers::empty(),
        });

        assert_eq!(state.focus, Focus::Password);
        assert_eq!(state.editing, Some(EditField::SshPassword));
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
        assert_eq!(connect_label(&state), "[ Connecting... ]");
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
