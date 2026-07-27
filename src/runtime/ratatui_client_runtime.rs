use crate::infra::error_log::ERROR_LOG;
use crate::lifecycle::LifecycleError;
use crate::runtime::ratatui_node_runtime::ratatui_socket_path;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{Frame, Terminal};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

/// Ratatui TUI client: connects to a session's node server and renders the workspace chrome.
pub struct RatatuiClientRuntime {
    session_name: String,
}

impl RatatuiClientRuntime {
    pub fn from_session(session_name: String) -> Result<Self, LifecycleError> {
        Ok(Self { session_name })
    }

    pub fn run(&self) -> Result<(), LifecycleError> {
        let socket_path = ratatui_socket_path(&self.session_name);
        ERROR_LOG.log(format!(
            "[ratatui-client] connecting to socket={} session={}",
            socket_path.display(),
            self.session_name
        ));

        let mut stream = UnixStream::connect(&socket_path).map_err(|error| {
            LifecycleError::Io(
                format!(
                    "failed to connect to ratatui node socket {}",
                    socket_path.display()
                ),
                error,
            )
        })?;

        let reader = stream.try_clone().map_err(|error| {
            LifecycleError::Io("failed to clone ratatui client stream".to_string(), error)
        })?;
        let mut reader = BufReader::new(reader);

        // Read the initial snapshot from the server.
        let mut line = String::new();
        let snapshot = match reader.read_line(&mut line) {
            Ok(0) | Err(_) => Snapshot::default(),
            Ok(_) => Snapshot::parse(&line),
        };

        ERROR_LOG.log(format!(
            "[ratatui-client] snapshot session={} clients={} main={} sidebar={} footer={}",
            snapshot.session_name,
            snapshot.client_count,
            snapshot.main,
            snapshot.sidebar,
            snapshot.footer
        ));

        let original_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = restore_terminal();
            original_hook(info);
        }));

        let terminal = init_terminal().map_err(|error| {
            LifecycleError::Io("failed to initialize ratatui terminal".to_string(), error)
        })?;
        let result = run_event_loop(terminal, &mut stream, snapshot);
        let _ = restore_terminal();

        result
    }
}

fn init_terminal() -> io::Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend)
}

fn restore_terminal() -> io::Result<()> {
    disable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, LeaveAlternateScreen)?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Main,
    Sidebar,
}

impl Focus {
    fn label(self) -> &'static str {
        match self {
            Focus::Main => "MAIN",
            Focus::Sidebar => "SIDEBAR",
        }
    }
}

#[derive(Debug, Clone)]
struct Snapshot {
    session_name: String,
    client_count: usize,
    main: String,
    sidebar: String,
    footer: String,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            session_name: "1".to_string(),
            client_count: 1,
            main: "Main pane placeholder".to_string(),
            sidebar: "Sidebar placeholder".to_string(),
            footer:
                "Ctrl-N New · Ctrl-W Conn · Ctrl-S Remote · Ctrl-O Hist · Ctrl-E Logs · Ctrl-M Menu"
                    .to_string(),
        }
    }
}

impl Snapshot {
    fn parse(line: &str) -> Self {
        let parts: Vec<&str> = line.trim().splitn(6, '|').collect();
        if parts.len() == 6 && parts[0] == "SNAPSHOT" {
            Self {
                session_name: parts[1].to_string(),
                client_count: parts[2].parse().unwrap_or(1),
                main: parts[3].to_string(),
                sidebar: parts[4].to_string(),
                footer: parts[5].to_string(),
            }
        } else {
            Self::default()
        }
    }
}

fn run_event_loop(
    mut terminal: Terminal<CrosstermBackend<io::Stdout>>,
    stream: &mut UnixStream,
    snapshot: Snapshot,
) -> Result<(), LifecycleError> {
    let mut prefix_pressed = false;
    let mut focus = Focus::Main;

    loop {
        terminal
            .draw(|frame| render(frame, &snapshot, focus))
            .map_err(|error| {
                LifecycleError::Io("failed to draw ratatui frame".to_string(), error)
            })?;

        match event::read().map_err(|error| {
            LifecycleError::Io("failed to read crossterm event".to_string(), error)
        })? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if prefix_pressed {
                    prefix_pressed = false;
                    match key.code {
                        KeyCode::Char('d') | KeyCode::Char('D') => {
                            let _ = writeln!(stream, "DETACH");
                            let _ = stream.flush();
                            break;
                        }
                        _ => {}
                    }
                } else {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Char('Q') => {
                            let _ = writeln!(stream, "DETACH");
                            let _ = stream.flush();
                            break;
                        }
                        KeyCode::Char('b') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            prefix_pressed = true;
                        }
                        KeyCode::Right if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            focus = Focus::Sidebar;
                        }
                        KeyCode::Left if focus == Focus::Sidebar => {
                            focus = Focus::Main;
                        }
                        _ => {}
                    }
                }
            }
            Event::Resize(_, _) => {
                // Phase 0: re-draw on resize is automatic because the next loop
                // iteration calls terminal.draw with the new frame.size().
            }
            _ => {}
        }
    }

    Ok(())
}

fn render(frame: &mut Frame, snapshot: &Snapshot, focus: Focus) {
    let area = frame.size();

    // Outer vertical layout: chrome above, footer below.
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area);

    // Inner horizontal layout: main pane left, sidebar right.
    let inner = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(32)])
        .split(outer[0]);

    let main_title = format!(
        "{} {}",
        focus_indicator(focus == Focus::Main),
        snapshot.main
    );
    let main_block = Block::default()
        .title(main_title)
        .borders(Borders::RIGHT)
        .border_style(border_style(focus == Focus::Main));
    let main =
        Paragraph::new("Main pane placeholder\n\nPress q or Ctrl+B d to quit.").block(main_block);
    frame.render_widget(main, inner[0]);

    let sidebar_title = format!(
        "{} {}",
        focus_indicator(focus == Focus::Sidebar),
        snapshot.sidebar
    );
    let sidebar_block = Block::default()
        .title(sidebar_title)
        .borders(Borders::NONE)
        .title_style(title_style(focus == Focus::Sidebar));
    let sidebar = Paragraph::new("Sidebar placeholder").block(sidebar_block);
    frame.render_widget(sidebar, inner[1]);

    let footer_text = format!(
        "[{}] {} · {} {}",
        snapshot.session_name,
        focus.label(),
        snapshot.footer,
        render_fill(
            outer[1].width as usize,
            snapshot.footer.chars().count()
                + snapshot.session_name.chars().count()
                + focus.label().chars().count()
                + 8
        )
    );
    let footer = Paragraph::new(Line::from(vec![Span::styled(
        footer_text,
        Style::default().bg(Color::Blue).fg(Color::White),
    )]));
    frame.render_widget(footer, outer[1]);
}

fn focus_indicator(active: bool) -> &'static str {
    if active {
        "▸"
    } else {
        " "
    }
}

fn title_style(active: bool) -> Style {
    if active {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    }
}

fn border_style(active: bool) -> Style {
    if active {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

fn render_fill(area_width: usize, text_width: usize) -> String {
    let fill = area_width.saturating_sub(text_width);
    " ".repeat(fill)
}
