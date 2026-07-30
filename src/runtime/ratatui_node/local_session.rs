use crate::infra::error_log::ERROR_LOG;
use crate::lifecycle::LifecycleError;
use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::tty::{self, Options, Shell};
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::runtime::SharedState;
use super::state_event::StateEvent;

/// A local session backed by a real PTY + `alacritty_terminal` emulator.
pub struct RatatuiLocalSession {
    pub session_id: String,
    pub command_name: String,
    pub term: Arc<FairMutex<Term<EventProxy>>>,
    event_loop_sender: Arc<Mutex<Option<EventLoopSender>>>,
}

impl RatatuiLocalSession {
    /// Spawn a new shell in a PTY and start the alacritty terminal event loop.
    pub fn spawn(
        session_id: impl Into<String>,
        command_name: impl Into<String>,
        cols: u16,
        rows: u16,
        shared: Arc<SharedState>,
    ) -> Result<Arc<Self>, LifecycleError> {
        let session_id = session_id.into();
        let command_name = command_name.into();

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
            session_id: session_id.clone(),
            sender: sender_slot.clone(),
        };

        let term = Arc::new(FairMutex::new(Term::new(
            Config::default(),
            &dimensions,
            proxy.clone(),
        )));

        let event_loop =
            EventLoop::new(term.clone(), proxy, pty, true, false).map_err(|error| {
                LifecycleError::Io("failed to create terminal event loop".to_string(), error)
            })?;

        let sender = event_loop.channel();
        *sender_slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(sender.clone());

        let _join_handle = event_loop.spawn();

        ERROR_LOG.log(format!(
            "[ratatui-local-session] spawned session={} cols={} rows={}",
            session_id, cols, rows
        ));

        Ok(Arc::new(Self {
            session_id,
            command_name,
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
    }

    /// Snapshot the visible screen as plain text lines and the cursor position.
    pub fn snapshot(&self) -> (Vec<String>, Option<(u16, u16)>) {
        let term = self.term.lock();
        let grid = term.grid();
        let screen_lines = grid.screen_lines();
        let columns = grid.columns();
        let display_offset = grid.display_offset() as i32;

        let mut lines = Vec::with_capacity(screen_lines);
        for row in 0..screen_lines {
            let line = Line(row as i32 - display_offset);
            let grid_row = &grid[line];
            let mut text = String::with_capacity(columns);
            for col in 0..columns {
                let cell = &grid_row[Column(col)];
                if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                    continue;
                }
                text.push(cell.c);
            }
            lines.push(text);
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

        (lines, cursor)
    }
}

/// Bridge terminal emulator events back into the WaitAgent server.
///
/// Phase 1 routes lifecycle events to `StateEventLoop` instead of mutating
/// `SharedState` directly.  Wakeup output events still trigger a snapshot
/// broadcast directly because they are read-only with respect to session state.
#[derive(Clone)]
pub struct EventProxy {
    shared: Arc<SharedState>,
    session_id: String,
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
                        session_id: self.session_id.clone(),
                    });
            }
            Event::ChildExit(status) => {
                let _ = self
                    .shared
                    .state_sender()
                    .send(StateEvent::LocalSessionChildExit {
                        session_id: self.session_id.clone(),
                        exit_code: status,
                    });
            }
            Event::Exit => {
                let _ = self
                    .shared
                    .state_sender()
                    .send(StateEvent::LocalSessionChildExit {
                        session_id: self.session_id.clone(),
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
                        session_id: self.session_id.clone(),
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
