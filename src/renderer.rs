#![allow(dead_code)]

use crate::console::{ConsoleState, SwitchLock};
use crate::session::{SessionAddress, SessionRecord, SessionStatus};
use crate::terminal::{ScreenSnapshot, TerminalSize};
use std::fmt;

const DEFAULT_WIDTH: u16 = 80;
const DEFAULT_HEIGHT: u16 = 24;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderFrame {
    pub mode: RenderMode,
    pub rendered_session: SessionAddress,
    pub input_owner_session: SessionAddress,
    pub top_line: String,
    pub overlay_lines: Vec<String>,
    pub viewport_lines: Vec<String>,
    pub bottom_line: String,
    pub cursor_row: u16,
    pub cursor_col: u16,
}

impl RenderFrame {
    pub fn as_text(&self) -> String {
        let mut lines =
            Vec::with_capacity(self.viewport_lines.len() + self.overlay_lines.len() + 2);
        if !self.top_line.is_empty() {
            lines.push(self.top_line.clone());
        }
        lines.extend(self.viewport_lines.iter().cloned());
        lines.extend(self.overlay_lines.iter().cloned());
        lines.push(self.bottom_line.clone());
        lines.join("\r\n")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderContext {
    pub waiting_count: usize,
    pub overlay_lines: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderMode {
    Focused,
    PeekReadOnly,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RendererState {
    last_focused_session: Option<SessionAddress>,
}

#[derive(Debug, Default)]
pub struct Renderer;

impl Renderer {
    pub fn new() -> Self {
        Self
    }

    pub fn render(
        &self,
        console: &ConsoleState,
        sessions: &[&SessionRecord],
        context: RenderContext,
    ) -> Result<RenderFrame, RenderError> {
        self.render_with_state(&mut RendererState::default(), console, sessions, context)
    }

    pub fn render_with_state(
        &self,
        state: &mut RendererState,
        console: &ConsoleState,
        sessions: &[&SessionRecord],
        context: RenderContext,
    ) -> Result<RenderFrame, RenderError> {
        let focused = console
            .focused_session
            .as_ref()
            .ok_or(RenderError::MissingFocus)?;
        if find_session(sessions, focused).is_none() {
            return Err(RenderError::MissingSession(focused.clone()));
        }

        if let Some(peeked) = console.peeked_session() {
            return self.render_peek_with_state(state, console, sessions, context, focused, peeked);
        }

        self.render_focused_with_state(state, console, sessions, context, focused)
    }

    fn render_focused_with_state(
        &self,
        state: &mut RendererState,
        console: &ConsoleState,
        sessions: &[&SessionRecord],
        context: RenderContext,
        focused: &SessionAddress,
    ) -> Result<RenderFrame, RenderError> {
        let rendered_session = find_session(sessions, focused)
            .ok_or_else(|| RenderError::MissingSession(focused.clone()))?;
        let snapshot = rendered_session
            .screen_state
            .as_ref()
            .map(|state| state.active_snapshot().clone())
            .unwrap_or_else(|| blank_snapshot(focused));
        let viewport = normalize_viewport(&snapshot, context.overlay_lines.len());
        let viewport_line_count = viewport.lines.len();
        state.last_focused_session = Some(focused.clone());

        Ok(RenderFrame {
            mode: RenderMode::Focused,
            rendered_session: focused.clone(),
            input_owner_session: focused.clone(),
            top_line: String::new(),
            overlay_lines: context.overlay_lines.clone(),
            viewport_lines: viewport.lines,
            bottom_line: render_bottom_line(
                console,
                rendered_session,
                focused,
                sessions,
                context.waiting_count,
                snapshot.size.cols as usize,
            ),
            cursor_row: snapshot
                .cursor_row
                .saturating_sub(viewport.start_row as u16)
                .min(viewport_line_count.saturating_sub(1) as u16),
            cursor_col: snapshot
                .cursor_col
                .min(snapshot.size.cols.saturating_sub(1)),
        })
    }

    fn render_peek_with_state(
        &self,
        state: &mut RendererState,
        console: &ConsoleState,
        sessions: &[&SessionRecord],
        context: RenderContext,
        focused: &SessionAddress,
        peeked: &SessionAddress,
    ) -> Result<RenderFrame, RenderError> {
        let rendered_session = find_session(sessions, peeked)
            .ok_or_else(|| RenderError::MissingSession(peeked.clone()))?;
        let snapshot = rendered_session
            .screen_state
            .as_ref()
            .map(|screen_state| screen_state.active_snapshot().clone())
            .unwrap_or_else(|| blank_snapshot(peeked));
        let viewport = normalize_viewport(&snapshot, context.overlay_lines.len());
        let viewport_line_count = viewport.lines.len();
        state.last_focused_session = Some(focused.clone());

        Ok(RenderFrame {
            mode: RenderMode::PeekReadOnly,
            rendered_session: peeked.clone(),
            input_owner_session: focused.clone(),
            top_line: String::new(),
            overlay_lines: context.overlay_lines.clone(),
            viewport_lines: viewport.lines,
            bottom_line: render_bottom_line(
                console,
                rendered_session,
                focused,
                sessions,
                context.waiting_count,
                snapshot.size.cols as usize,
            ),
            cursor_row: snapshot
                .cursor_row
                .saturating_sub(viewport.start_row as u16)
                .min(viewport_line_count.saturating_sub(1) as u16),
            cursor_col: snapshot
                .cursor_col
                .min(snapshot.size.cols.saturating_sub(1)),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderError {
    MissingFocus,
    MissingSession(SessionAddress),
}

impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingFocus => write!(f, "renderer requires a focused session"),
            Self::MissingSession(address) => write!(f, "renderer could not find session {address}"),
        }
    }
}

impl std::error::Error for RenderError {}

fn render_bottom_line(
    console: &ConsoleState,
    rendered: &SessionRecord,
    focused: &SessionAddress,
    sessions: &[&SessionRecord],
    waiting_count: usize,
    width: usize,
) -> String {
    let active_sessions = sessions
        .iter()
        .copied()
        .filter(|session| !matches!(session.status, SessionStatus::Exited))
        .collect::<Vec<_>>();
    let session_total = active_sessions.len();
    let session_index = active_sessions
        .iter()
        .position(|session| session.address() == focused)
        .map(|index| index + 1)
        .unwrap_or(1);

    let visual_mode = if console.is_peeking() {
        "peek"
    } else {
        "active"
    };
    let session_label = if console.is_peeking() {
        if let Some(working_dir) = rendered.current_working_dir.as_deref() {
            format!(
                "peek {} <- {} | {}",
                rendered.address(),
                focused,
                working_dir
            )
        } else {
            format!("peek {} <- {}", rendered.address(), focused)
        }
    } else {
        if let Some(working_dir) = rendered.current_working_dir.as_deref() {
            format!("{} | {} | {}", rendered.title, focused, working_dir)
        } else {
            format!("{} | {}", rendered.title, focused)
        }
    };
    let mut parts = vec![
        visual_mode.to_string(),
        format!("{waiting_count} waiting"),
        format!("{session_index}/{session_total}"),
    ];
    if !matches!(console.switch_lock, SwitchLock::Clear) {
        parts.push(format!("lock {}", render_lock(console.switch_lock)));
    }

    compose_bar(
        &format!("WaitAgent | {session_label}"),
        &parts.join(" | "),
        width,
    )
}

fn render_lock(lock: SwitchLock) -> &'static str {
    match lock {
        SwitchLock::Clear => "clear",
        SwitchLock::Armed => "armed",
        SwitchLock::Blocked => "blocked",
    }
}

struct ViewportProjection {
    lines: Vec<String>,
    start_row: usize,
}

fn normalize_viewport(snapshot: &ScreenSnapshot, overlay_lines: usize) -> ViewportProjection {
    let width = snapshot.size.cols as usize;
    let reserved_rows = 1 + overlay_lines;
    let available_rows = usize::max(
        1,
        (snapshot.size.rows as usize).saturating_sub(reserved_rows),
    );
    let viewport_end = snapshot
        .lines
        .iter()
        .rposition(|line| !line.trim_end().is_empty())
        .map(|index| index + 1)
        .unwrap_or_else(|| (snapshot.cursor_row as usize).saturating_add(1))
        .max((snapshot.cursor_row as usize).saturating_add(1))
        .min(snapshot.lines.len());
    let viewport_start = viewport_end.saturating_sub(available_rows);
    let lines = snapshot
        .lines
        .iter()
        .skip(viewport_start)
        .take(available_rows)
        .map(|line| trim_line(line, width))
        .collect();
    ViewportProjection {
        lines,
        start_row: viewport_start,
    }
}

fn normalize_viewport_lines(snapshot: &ScreenSnapshot, overlay_lines: usize) -> Vec<String> {
    normalize_viewport(snapshot, overlay_lines).lines
}

fn fit_width(line: &str, width: usize) -> String {
    let mut chars = line.chars().take(width).collect::<Vec<_>>();
    while chars.len() < width {
        chars.push(' ');
    }
    chars.into_iter().collect()
}

fn trim_line(line: &str, width: usize) -> String {
    fit_width(line, width).trim_end().to_string()
}

fn compose_bar(left: &str, right: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }

    let right = shorten(right, width);
    let right_len = right.chars().count();
    if right_len >= width {
        return right;
    }

    let left_max = width.saturating_sub(right_len + 1);
    let left = shorten(left, left_max);
    let left_len = left.chars().count();
    if left_len == 0 {
        return right;
    }

    let padding = width.saturating_sub(left_len + right_len);
    format!("{left}{}{right}", " ".repeat(padding))
}

fn shorten(value: &str, max_width: usize) -> String {
    value.chars().take(max_width).collect()
}

fn blank_snapshot(address: &SessionAddress) -> ScreenSnapshot {
    let size = TerminalSize {
        rows: DEFAULT_HEIGHT,
        cols: DEFAULT_WIDTH,
        pixel_width: 0,
        pixel_height: 0,
    };
    let mut lines = vec![" ".repeat(size.cols as usize); size.rows as usize];
    let label = format!("session: {address}");
    let len = usize::min(label.chars().count(), size.cols as usize);
    lines[0] = fit_width(&label, size.cols as usize);
    ScreenSnapshot {
        size,
        lines,
        scrollback: Vec::new(),
        window_title: None,
        cursor_row: 0,
        cursor_col: len as u16,
        alternate_screen: false,
    }
}

fn find_session<'a>(
    sessions: &'a [&SessionRecord],
    address: &SessionAddress,
) -> Option<&'a SessionRecord> {
    sessions
        .iter()
        .copied()
        .find(|session| session.address() == address)
}

#[cfg(test)]
mod tests {
    use super::{RenderContext, RenderMode, Renderer, RendererState};
    use crate::console::ConsoleState;
    use crate::session::SessionRegistry;
    use crate::terminal::{TerminalEngine, TerminalSize};

    fn context(waiting_count: usize) -> RenderContext {
        RenderContext {
            waiting_count,
            overlay_lines: Vec::new(),
        }
    }

    #[test]
    fn renders_focused_session_snapshot_with_status_lines() {
        let mut registry = SessionRegistry::new();
        let session = registry.create_local_session(
            "devbox-1".to_string(),
            "claude".to_string(),
            "claude".to_string(),
        );
        let address = session.address().clone();
        let mut engine = TerminalEngine::new(TerminalSize {
            rows: 2,
            cols: 96,
            pixel_width: 0,
            pixel_height: 0,
        });
        engine.feed(b"hello");
        registry.update_screen_state(&address, engine.state());

        let mut console = ConsoleState::new("console-1");
        console.focus(address.clone());

        let sessions = registry.list();
        let frame = Renderer::new()
            .render(&console, &sessions, context(2))
            .expect("render should succeed");

        assert_eq!(frame.mode, RenderMode::Focused);
        assert_eq!(frame.rendered_session, address);
        assert_eq!(frame.input_owner_session, frame.rendered_session);
        assert!(frame.top_line.is_empty());
        assert_eq!(frame.viewport_lines[0], "hello");
        assert!(frame
            .bottom_line
            .starts_with("WaitAgent | claude | devbox-1/session-1"));
        assert!(frame.bottom_line.ends_with("active | 2 waiting | 1/1"));
    }

    #[test]
    fn renders_focused_frame_snapshot_text() {
        let mut registry = SessionRegistry::new();
        let session = registry.create_local_session(
            "devbox-1".to_string(),
            "claude".to_string(),
            "claude".to_string(),
        );
        let address = session.address().clone();
        let mut engine = TerminalEngine::new(TerminalSize {
            rows: 2,
            cols: 96,
            pixel_width: 0,
            pixel_height: 0,
        });
        engine.feed(b"hello");
        registry.update_screen_state(&address, engine.state());

        let mut console = ConsoleState::new("console-1");
        console.focus(address);

        let sessions = registry.list();
        let frame = Renderer::new()
            .render(&console, &sessions, context(1))
            .expect("render should succeed");

        let rendered = frame.as_text();
        let lines = rendered.split("\r\n").collect::<Vec<_>>();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "hello");
        assert!(lines[1].starts_with("WaitAgent | claude | devbox-1/session-1"));
        assert!(lines[1].ends_with("active | 1 waiting | 1/1"));
    }

    #[test]
    fn renders_peek_chrome_without_changing_input_owner() {
        let mut registry = SessionRegistry::new();
        let focused = registry.create_local_session(
            "devbox-1".to_string(),
            "claude".to_string(),
            "claude".to_string(),
        );
        let peeked = registry.create_local_session(
            "devbox-2".to_string(),
            "codex".to_string(),
            "codex".to_string(),
        );
        let focused_address = focused.address().clone();
        let peeked_address = peeked.address().clone();
        let mut engine = TerminalEngine::new(TerminalSize {
            rows: 2,
            cols: 96,
            pixel_width: 0,
            pixel_height: 0,
        });
        engine.feed(b"peek");
        registry.update_screen_state(&peeked_address, engine.state());

        let mut console = ConsoleState::new("console-1");
        console.focus(focused_address.clone());
        console
            .enter_peek(
                &[focused_address.clone(), peeked_address.clone()],
                &peeked_address,
            )
            .expect("peek should enter");

        let sessions = registry.list();
        let frame = Renderer::new()
            .render(&console, &sessions, context(0))
            .expect("render should succeed");

        assert_eq!(frame.mode, RenderMode::PeekReadOnly);
        assert_eq!(frame.rendered_session, peeked_address);
        assert_eq!(frame.input_owner_session, focused_address.clone());
        assert!(frame.top_line.is_empty());
        assert_eq!(frame.viewport_lines[0], "peek");
        assert!(frame
            .bottom_line
            .starts_with("WaitAgent | peek devbox-2/session-2 <- devbox-1/session-1"));
        assert!(frame.bottom_line.ends_with("peek | 0 waiting | 1/2"));
        assert_eq!(console.input_owner_session(), Some(&focused_address));
    }

    #[test]
    fn renders_peek_frame_snapshot_text() {
        let mut registry = SessionRegistry::new();
        let focused = registry.create_local_session(
            "devbox-1".to_string(),
            "claude".to_string(),
            "claude".to_string(),
        );
        let peeked = registry.create_local_session(
            "devbox-2".to_string(),
            "codex".to_string(),
            "codex".to_string(),
        );
        let focused_address = focused.address().clone();
        let peeked_address = peeked.address().clone();
        let mut engine = TerminalEngine::new(TerminalSize {
            rows: 2,
            cols: 96,
            pixel_width: 0,
            pixel_height: 0,
        });
        engine.feed(b"peek");
        registry.update_screen_state(&peeked_address, engine.state());

        let mut console = ConsoleState::new("console-1");
        console.focus(focused_address.clone());
        console
            .enter_peek(
                &[focused_address.clone(), peeked_address.clone()],
                &peeked_address,
            )
            .expect("peek should enter");

        let sessions = registry.list();
        let frame = Renderer::new()
            .render(&console, &sessions, context(0))
            .expect("render should succeed");

        let rendered = frame.as_text();
        let lines = rendered.split("\r\n").collect::<Vec<_>>();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "peek");
        assert!(lines[1].starts_with("WaitAgent | peek devbox-2/session-2 <- devbox-1/session-1"));
        assert!(lines[1].ends_with("peek | 0 waiting | 1/2"));
    }

    #[test]
    fn renders_lock_state_when_auto_switch_is_not_clear() {
        let mut registry = SessionRegistry::new();
        let session = registry.create_local_session(
            "devbox-1".to_string(),
            "claude".to_string(),
            "claude".to_string(),
        );
        let address = session.address().clone();

        let mut console = ConsoleState::new("console-1");
        console.focus(address.clone());
        console.arm_switch_lock();

        let sessions = registry.list();
        let frame = Renderer::new()
            .render(&console, &sessions, context(1))
            .expect("render should succeed");

        assert!(frame
            .bottom_line
            .starts_with("WaitAgent | claude | devbox-1/session-1"));
        assert!(frame
            .bottom_line
            .ends_with("active | 1 waiting | 1/1 | lock armed"));
    }

    #[test]
    fn focus_change_uses_target_snapshot_without_extra_notice_noise() {
        let mut registry = SessionRegistry::new();
        let first = registry.create_local_session(
            "devbox-1".to_string(),
            "claude".to_string(),
            "claude".to_string(),
        );
        let second = registry.create_local_session(
            "devbox-1".to_string(),
            "codex".to_string(),
            "codex".to_string(),
        );
        let first_address = first.address().clone();
        let second_address = second.address().clone();

        let mut first_engine = TerminalEngine::new(TerminalSize {
            rows: 2,
            cols: 96,
            pixel_width: 0,
            pixel_height: 0,
        });
        first_engine.feed(b"first");
        registry.update_screen_state(&first_address, first_engine.state());

        let mut second_engine = TerminalEngine::new(TerminalSize {
            rows: 2,
            cols: 96,
            pixel_width: 0,
            pixel_height: 0,
        });
        second_engine.feed(b"second");
        registry.update_screen_state(&second_address, second_engine.state());

        let mut console = ConsoleState::new("console-1");
        console.focus(first_address);
        let sessions = registry.list();
        let renderer = Renderer::new();
        let mut state = RendererState::default();

        let first_frame = renderer
            .render_with_state(&mut state, &console, &sessions, context(0))
            .expect("first render should succeed");
        assert!(first_frame
            .bottom_line
            .ends_with("active | 0 waiting | 1/2"));

        console.focus(second_address.clone());
        let second_frame = renderer
            .render_with_state(&mut state, &console, &sessions, context(0))
            .expect("second render should succeed");
        assert_eq!(second_frame.mode, RenderMode::Focused);
        assert_eq!(second_frame.rendered_session, second_address);
        assert_eq!(second_frame.viewport_lines[0], "second");
        assert!(second_frame
            .bottom_line
            .ends_with("active | 0 waiting | 2/2"));

        let third_frame = renderer
            .render_with_state(&mut state, &console, &sessions, context(0))
            .expect("steady render should succeed");
        assert!(third_frame
            .bottom_line
            .ends_with("active | 0 waiting | 2/2"));
    }

    #[test]
    fn renders_overlay_lines_after_viewport() {
        let mut registry = SessionRegistry::new();
        let session = registry.create_local_session(
            "devbox-1".to_string(),
            "bash".to_string(),
            "bash".to_string(),
        );
        let address = session.address().clone();
        let mut engine = TerminalEngine::new(TerminalSize {
            rows: 4,
            cols: 96,
            pixel_width: 0,
            pixel_height: 0,
        });
        engine.feed(b"hello");
        registry.update_screen_state(&address, engine.state());

        let mut console = ConsoleState::new("console-1");
        console.focus(address);

        let sessions = registry.list();
        let frame = Renderer::new()
            .render(
                &console,
                &sessions,
                RenderContext {
                    waiting_count: 0,
                    overlay_lines: vec![":/new".to_string()],
                },
            )
            .expect("render should succeed");

        assert_eq!(frame.overlay_lines, vec![":/new"]);
        assert_eq!(frame.viewport_lines.len(), 2);
        assert_eq!(frame.viewport_lines[0], "hello");
        let rendered = frame.as_text();
        let lines = rendered.split("\r\n").collect::<Vec<_>>();
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0], "hello");
        assert_eq!(lines[1], "");
        assert_eq!(lines[2], ":/new");
        assert!(lines[3].starts_with("WaitAgent | bash | devbox-1/session-1"));
        assert!(lines[3].ends_with("active | 0 waiting | 1/1"));
    }

    #[test]
    fn viewport_tracks_latest_visible_rows_when_bottom_space_is_reserved() {
        let mut registry = SessionRegistry::new();
        let session = registry.create_local_session(
            "devbox-1".to_string(),
            "bash".to_string(),
            "bash".to_string(),
        );
        let address = session.address().clone();
        let mut engine = TerminalEngine::new(TerminalSize {
            rows: 5,
            cols: 96,
            pixel_width: 0,
            pixel_height: 0,
        });
        engine.feed(b"one\r\ntwo\r\nthree");
        registry.update_screen_state(&address, engine.state());

        let mut console = ConsoleState::new("console-1");
        console.focus(address);

        let sessions = registry.list();
        let frame = Renderer::new()
            .render(
                &console,
                &sessions,
                RenderContext {
                    waiting_count: 0,
                    overlay_lines: vec![":/sessions".to_string()],
                },
            )
            .expect("render should succeed");

        assert_eq!(frame.viewport_lines, vec!["one", "two", "three"]);
    }
}
