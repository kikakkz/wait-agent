use crate::infra::error_log::ERROR_LOG;
use crate::lifecycle::LifecycleError;
use crate::runtime::ratatui_node_runtime::{ratatui_socket_path, RatatuiSnapshot};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
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
            Ok(0) | Err(_) => RatatuiSnapshot::default(),
            Ok(_) => parse_snapshot(&line),
        };

        ERROR_LOG.log(format!(
            "[ratatui-client] snapshot session={} clients={} main={} sidebar={} footer={:?}",
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

/// Placeholder session rows for the sidebar UI skeleton.
fn placeholder_sessions() -> Vec<String> {
    vec![
        "Session Alpha".to_string(),
        "Session Beta".to_string(),
        "Session Gamma".to_string(),
        "Session Delta".to_string(),
        "Session Epsilon".to_string(),
        "Session Zeta".to_string(),
        "Session Eta".to_string(),
        "Session Theta".to_string(),
        "Session Iota".to_string(),
        "Session Kappa".to_string(),
    ]
}

fn parse_snapshot(line: &str) -> RatatuiSnapshot {
    serde_json::from_str(line.trim()).unwrap_or_default()
}

fn run_event_loop(
    mut terminal: Terminal<CrosstermBackend<io::Stdout>>,
    stream: &mut UnixStream,
    snapshot: RatatuiSnapshot,
) -> Result<(), LifecycleError> {
    let mut prefix_pressed = false;
    let mut focus = Focus::Main;
    let sessions = placeholder_sessions();
    let mut selected_index = 0usize;

    loop {
        terminal
            .draw(|frame| render(frame, &snapshot, focus, &sessions, selected_index))
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
                        KeyCode::Up if focus == Focus::Sidebar && selected_index > 0 => {
                            selected_index -= 1;
                        }
                        KeyCode::Down
                            if focus == Focus::Sidebar && selected_index + 1 < sessions.len() =>
                        {
                            selected_index += 1;
                        }
                        KeyCode::Enter if focus == Focus::Sidebar => {
                            ERROR_LOG.log(format!(
                                "[ratatui-client] placeholder activate session: {}",
                                sessions[selected_index]
                            ));
                        }
                        _ => {}
                    }
                }
            }
            Event::Resize(_, _) => {
                // Re-draw on resize is automatic because the next loop iteration
                // calls terminal.draw with the new frame.size().
            }
            _ => {}
        }
    }

    Ok(())
}

fn render(
    frame: &mut Frame,
    snapshot: &RatatuiSnapshot,
    focus: Focus,
    sessions: &[String],
    selected_index: usize,
) {
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

    let main_block = Block::default()
        .title(snapshot.main.clone())
        .borders(Borders::RIGHT)
        .border_style(border_style(focus == Focus::Main))
        .title_style(title_style(focus == Focus::Main));
    let main =
        Paragraph::new("Main pane placeholder\n\nPress q or Ctrl+B d to quit.").block(main_block);
    frame.render_widget(main, inner[0]);

    let sidebar_block = Block::default()
        .title(snapshot.sidebar.clone())
        .borders(Borders::NONE)
        .title_style(title_style(focus == Focus::Sidebar));
    let items: Vec<ListItem> = sessions
        .iter()
        .map(|name| ListItem::new(name.as_str()))
        .collect();
    let mut list_state = ListState::default();
    list_state.select(Some(selected_index));
    let list = List::new(items)
        .block(sidebar_block)
        .highlight_style(
            Style::default()
                .bg(Color::Blue)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");
    frame.render_stateful_widget(list, inner[1], &mut list_state);

    let footer_style = Style::default().bg(Color::Blue).fg(Color::White);
    let footer =
        Paragraph::new(render_footer_line(snapshot, outer[1].width as usize)).style(footer_style);
    frame.render_widget(footer, outer[1]);
}

fn render_footer_line(snapshot: &RatatuiSnapshot, area_width: usize) -> Line {
    let muted_style = Style::default().fg(Color::Gray);
    let accent_style = Style::default().fg(Color::White);

    let mut spans = Vec::new();

    // Connection status.
    if let Some(listener) = &snapshot.footer.listener_endpoint {
        spans.push(Span::styled("Listen ", accent_style));
        spans.push(Span::styled(format!("{} ", listener), muted_style));
    }
    if let Some(connect) = &snapshot.footer.connect_endpoint {
        spans.push(Span::styled("· Connect ", accent_style));
        spans.push(Span::styled(format!("{} ", connect), muted_style));
    }
    if snapshot.footer.remote_count > 0 {
        spans.push(Span::styled("· ", muted_style));
        spans.push(Span::styled(
            format!("{} remote ", snapshot.footer.remote_count),
            muted_style,
        ));
    }

    if !spans.is_empty() {
        spans.push(Span::styled("· ", muted_style));
    }

    // Shortcuts.
    spans.push(Span::styled(
        "Ctrl-N New · Ctrl-W Conn · Ctrl-S Remote · Ctrl-O Hist · Ctrl-E Logs · Ctrl-M Menu",
        muted_style,
    ));

    // Pad to full width so the background color fills the line.
    let text_width: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let fill = area_width.saturating_sub(text_width);
    if fill > 0 {
        spans.push(Span::styled(" ".repeat(fill), muted_style));
    }

    Line::from(spans)
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
