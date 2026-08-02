use crate::infra::error_log::ERROR_LOG;
use crate::lifecycle::LifecycleError;
use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::tty::{self, Options, Shell};
use std::borrow::Cow;
use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use super::runtime::SharedState;
use super::state_event::StateEvent;

/// A local session backed by a real PTY + `alacritty_terminal` emulator.
pub struct RatatuiLocalSession {
    pub term: Arc<FairMutex<Term<EventProxy>>>,
    event_loop_sender: Arc<Mutex<Option<EventLoopSender>>>,
}

impl RatatuiLocalSession {
    /// Spawn a new shell in a PTY and start the alacritty terminal event loop.
    pub fn spawn(
        session_id: impl Into<String>,
        _command_name: impl Into<String>,
        cols: u16,
        rows: u16,
        shared: Arc<SharedState>,
    ) -> Result<Arc<Self>, LifecycleError> {
        let session_id = session_id.into();

        tty::setup_env();

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
        let options = Options {
            shell: Some(Shell::new(shell, Vec::new())),
            working_directory: std::env::current_dir().ok(),
            drain_on_exit: true,
            env: HashMap::new(),
            #[cfg(target_os = "windows")]
            escape_args: false,
        };

        let window_size = WindowSize {
            num_lines: rows,
            num_cols: cols,
            cell_width: 1,
            cell_height: 1,
        };
        let pty = tty::new(&options, window_size, 0)
            .map_err(|error| LifecycleError::Io("failed to create local PTY".to_string(), error))?;

        let dimensions = TermSize {
            cols: cols as usize,
            rows: rows as usize,
        };

        let sender_slot: Arc<Mutex<Option<EventLoopSender>>> = Arc::new(Mutex::new(None));
        let proxy = EventProxy {
            shared: shared.clone(),
            target_id: session_id.clone(),
            sender: sender_slot.clone(),
        };

        let term = Arc::new(FairMutex::new(Term::new(
            Config::default(),
            &dimensions,
            proxy.clone(),
        )));

        let master_fd = pty.file().as_raw_fd();
        let child_pid = pty.child().id() as i32;

        let event_loop =
            EventLoop::new(term.clone(), proxy, pty, true, false).map_err(|error| {
                LifecycleError::Io("failed to create terminal event loop".to_string(), error)
            })?;

        let sender = event_loop.channel();
        *sender_slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(sender.clone());

        let _join_handle = event_loop.spawn();

        spawn_task_state_monitor(session_id.clone(), master_fd, child_pid, shared.clone());

        ERROR_LOG.log(format!(
            "[ratatui-local-session] spawned session={} cols={} rows={}",
            session_id, cols, rows
        ));

        Ok(Arc::new(Self {
            term,
            event_loop_sender: sender_slot,
        }))
    }

    /// Send bytes to the PTY as if typed by the user.
    pub fn feed_input(&self, bytes: impl Into<Vec<u8>>) {
        if let Some(sender) = self
            .event_loop_sender
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            let _ = sender.send(Msg::Input(Cow::Owned(bytes.into())));
        }
    }

    /// Resize the PTY and terminal emulator.
    pub fn resize(&self, cols: u16, rows: u16) {
        ERROR_LOG.log(format!(
            "[ratatui-local-session] resize PTY cols={cols} rows={rows}"
        ));
        if let Some(sender) = self
            .event_loop_sender
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            let _ = sender.send(Msg::Resize(WindowSize {
                num_lines: rows,
                num_cols: cols,
                cell_width: 1,
                cell_height: 1,
            }));
        }
        // The PTY resize is handled asynchronously by the alacritty event loop.
        // Resize the terminal emulator grid synchronously so snapshot line widths
        // match the PTY width; otherwise output wraps at the initial grid size.
        {
            let mut term = self.term.lock();
            term.resize(TermSize {
                cols: cols as usize,
                rows: rows as usize,
            });
        }
    }

    /// Snapshot the visible screen as plain/styled text lines and the cursor position.
    pub fn snapshot(&self) -> (Vec<String>, Vec<String>, Option<(u16, u16)>) {
        let term = self.term.lock();
        let grid = term.grid();
        let screen_lines = grid.screen_lines();
        let columns = grid.columns();
        let display_offset = grid.display_offset() as i32;

        let mut lines = Vec::with_capacity(screen_lines);
        let mut styled_lines = Vec::with_capacity(screen_lines);
        for row in 0..screen_lines {
            let line = Line(row as i32 - display_offset);
            let (text, styled) = render_grid_line(&grid, line, columns);
            lines.push(text);
            styled_lines.push(styled);
        }

        let cursor = if term
            .mode()
            .contains(alacritty_terminal::term::TermMode::SHOW_CURSOR)
        {
            let point = grid.cursor.point;
            let col = point.column.0 as u16;
            let row = (point.line.0 + display_offset) as u16;
            if row < screen_lines as u16 && col < columns as u16 {
                Some((col, row))
            } else {
                None
            }
        } else {
            None
        };

        (lines, styled_lines, cursor)
    }

    /// Return the full scrollback history plus the visible screen as plain/styled lines.
    pub fn history_snapshot(&self) -> (Vec<String>, Vec<String>) {
        let term = self.term.lock();
        let grid = term.grid();
        let screen_lines = grid.screen_lines();
        let columns = grid.columns();
        let display_offset = grid.display_offset() as i32;
        let history_len = grid.total_lines().saturating_sub(screen_lines);

        let mut lines = Vec::with_capacity(history_len + screen_lines);
        let mut styled_lines = Vec::with_capacity(history_len + screen_lines);
        for offset in (1..=history_len).rev() {
            let line = Line(-(offset as i32));
            let (text, styled) = render_grid_line(&grid, line, columns);
            lines.push(text);
            styled_lines.push(styled);
        }
        for row in 0..screen_lines {
            let line = Line(row as i32 - display_offset);
            let (text, styled) = render_grid_line(&grid, line, columns);
            lines.push(text);
            styled_lines.push(styled);
        }
        (lines, styled_lines)
    }
}

fn alacritty_style_to_text_style(
    cell: &alacritty_terminal::term::cell::Cell,
) -> crate::terminal::TextStyle {
    use crate::terminal::ColorValue;
    use alacritty_terminal::term::cell::Flags;
    use alacritty_terminal::vte::ansi::{Color, NamedColor};

    let mut style = crate::terminal::TextStyle {
        bold: cell.flags.contains(Flags::BOLD),
        italic: cell.flags.contains(Flags::ITALIC),
        underline: cell.flags.contains(Flags::UNDERLINE),
        inverse: cell.flags.contains(Flags::INVERSE),
        dim: cell.flags.contains(Flags::DIM),
        strikethrough: cell.flags.contains(Flags::STRIKEOUT),
        ..Default::default()
    };

    let color_to_value = |color: &Color| -> Option<ColorValue> {
        match color {
            Color::Named(NamedColor::Black) => Some(ColorValue::Indexed(0)),
            Color::Named(NamedColor::Red) => Some(ColorValue::Indexed(1)),
            Color::Named(NamedColor::Green) => Some(ColorValue::Indexed(2)),
            Color::Named(NamedColor::Yellow) => Some(ColorValue::Indexed(3)),
            Color::Named(NamedColor::Blue) => Some(ColorValue::Indexed(4)),
            Color::Named(NamedColor::Magenta) => Some(ColorValue::Indexed(5)),
            Color::Named(NamedColor::Cyan) => Some(ColorValue::Indexed(6)),
            Color::Named(NamedColor::White) => Some(ColorValue::Indexed(7)),
            Color::Named(NamedColor::BrightBlack) => Some(ColorValue::Indexed(8)),
            Color::Named(NamedColor::BrightRed) => Some(ColorValue::Indexed(9)),
            Color::Named(NamedColor::BrightGreen) => Some(ColorValue::Indexed(10)),
            Color::Named(NamedColor::BrightYellow) => Some(ColorValue::Indexed(11)),
            Color::Named(NamedColor::BrightBlue) => Some(ColorValue::Indexed(12)),
            Color::Named(NamedColor::BrightMagenta) => Some(ColorValue::Indexed(13)),
            Color::Named(NamedColor::BrightCyan) => Some(ColorValue::Indexed(14)),
            Color::Named(NamedColor::BrightWhite) => Some(ColorValue::Indexed(15)),
            Color::Named(
                NamedColor::Foreground
                | NamedColor::Background
                | NamedColor::Cursor
                | NamedColor::BrightForeground
                | NamedColor::DimForeground,
            ) => None,
            Color::Named(NamedColor::DimBlack) => Some(ColorValue::Indexed(0)),
            Color::Named(NamedColor::DimRed) => Some(ColorValue::Indexed(1)),
            Color::Named(NamedColor::DimGreen) => Some(ColorValue::Indexed(2)),
            Color::Named(NamedColor::DimYellow) => Some(ColorValue::Indexed(3)),
            Color::Named(NamedColor::DimBlue) => Some(ColorValue::Indexed(4)),
            Color::Named(NamedColor::DimMagenta) => Some(ColorValue::Indexed(5)),
            Color::Named(NamedColor::DimCyan) => Some(ColorValue::Indexed(6)),
            Color::Named(NamedColor::DimWhite) => Some(ColorValue::Indexed(7)),
            Color::Indexed(index) => Some(ColorValue::Indexed(*index)),
            Color::Spec(rgb) => Some(ColorValue::Rgb(rgb.r, rgb.g, rgb.b)),
        }
    };

    style.foreground = color_to_value(&cell.fg);
    style.background = color_to_value(&cell.bg);
    style
}

/// Render a single grid row into plain and ANSI-styled text.
fn render_grid_line(
    grid: &alacritty_terminal::grid::Grid<alacritty_terminal::term::cell::Cell>,
    line: Line,
    columns: usize,
) -> (String, String) {
    let mut text = String::with_capacity(columns);
    let mut styled = String::with_capacity(columns * 8);
    let mut active_style = crate::terminal::TextStyle::default();

    for col in 0..columns {
        let cell = &grid[line][Column(col)];
        text.push(cell.c);
        let style = alacritty_style_to_text_style(cell);
        if style != active_style {
            styled.push_str(&style.to_ansi());
            active_style = style;
        }
        styled.push(cell.c);
    }

    if active_style != crate::terminal::TextStyle::default() {
        styled.push_str("\x1b[0m");
    }

    (text, styled)
}

/// Bridge terminal emulator events back into the WaitAgent server.
///
/// Phase 1 routes lifecycle events to `StateEventLoop` instead of mutating
/// `SharedState` directly.  Wakeup output events still trigger a snapshot
/// broadcast directly because they are read-only with respect to session state.
#[derive(Clone)]
pub struct EventProxy {
    shared: Arc<SharedState>,
    target_id: String,
    sender: Arc<Mutex<Option<EventLoopSender>>>,
}

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        match event {
            Event::Wakeup => {
                let _ = self
                    .shared
                    .state_sender()
                    .send(StateEvent::LocalSessionOutput {
                        target_id: self.target_id.clone(),
                    });
            }
            Event::ChildExit(status) => {
                let _ = self
                    .shared
                    .state_sender()
                    .send(StateEvent::LocalSessionChildExit {
                        target_id: self.target_id.clone(),
                        exit_code: status,
                    });
            }
            Event::Exit => {
                let _ = self
                    .shared
                    .state_sender()
                    .send(StateEvent::LocalSessionChildExit {
                        target_id: self.target_id.clone(),
                        exit_code: -1,
                    });
            }
            Event::PtyWrite(text) => {
                if let Some(sender) = self
                    .sender
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                {
                    let _ = sender.send(Msg::Input(text.into_bytes().into()));
                }
            }
            Event::Title(title) => {
                let _ = self
                    .shared
                    .state_sender()
                    .send(StateEvent::LocalSessionTitleChanged {
                        target_id: self.target_id.clone(),
                        title,
                    });
            }
            _ => {}
        }
    }
}

/// Minimal `Dimensions` implementation for creating a `Term`.
struct TermSize {
    cols: usize,
    rows: usize,
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

/// Spawn a background thread that watches the PTY foreground process group
/// to infer whether the shell is at a prompt (Input) or a command is running
/// (Running).  The result is sent to `StateEventLoop` as a task-state change.
fn spawn_task_state_monitor(
    target_id: String,
    master_fd: std::os::unix::io::RawFd,
    child_pid: i32,
    shared: Arc<SharedState>,
) {
    thread::spawn(move || {
        let running = Arc::new(AtomicBool::new(true));
        let mut last_state = None;

        while running.load(Ordering::Relaxed) {
            // SAFETY: `tcgetpgrp` and `getpgid` are async-signal-safe POSIX
            // calls operating on the PTY master fd owned by the alacritty
            // event loop.  The fd remains valid until the child exits.
            let fg_pgid = unsafe { libc::tcgetpgrp(master_fd) };
            if fg_pgid < 0 {
                break;
            }
            let shell_pgid = unsafe { libc::getpgid(child_pid) };
            if shell_pgid < 0 {
                break;
            }

            let task_state = if fg_pgid == shell_pgid {
                crate::domain::session_catalog::ManagedSessionTaskState::Input
            } else {
                crate::domain::session_catalog::ManagedSessionTaskState::Running
            };

            if last_state != Some(task_state) {
                last_state = Some(task_state);
                let _ = shared
                    .state_sender()
                    .send(StateEvent::SessionTaskStateChanged {
                        target_id: target_id.clone(),
                        task_state,
                    });
            }

            thread::sleep(Duration::from_millis(200));
        }

        ERROR_LOG.log(format!(
            "[ratatui-local-session] task-state monitor exiting for {target_id}"
        ));
    });
}
