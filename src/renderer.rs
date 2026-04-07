#![allow(dead_code)]

use crate::console::{ConsoleState, SwitchLock};
use crate::session::{SessionAddress, SessionRecord};
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
    pub viewport_lines: Vec<String>,
    pub bottom_line: String,
}

impl RenderFrame {
    pub fn as_text(&self) -> String {
        let mut lines = Vec::with_capacity(self.viewport_lines.len() + 2);
        lines.push(self.top_line.clone());
        lines.extend(self.viewport_lines.iter().cloned());
        lines.push(self.bottom_line.clone());
        lines.join("\n")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderContext {
    pub waiting_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderMode {
    Focused,
    PeekReadOnly,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RendererState {
    last_focused_session: Option<SessionAddress>,
    last_rendered_session: Option<SessionAddress>,
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
        let restore_notice = render_restore_notice(state, focused);
        state.last_focused_session = Some(focused.clone());
        state.last_rendered_session = Some(focused.clone());

        Ok(RenderFrame {
            mode: RenderMode::Focused,
            rendered_session: focused.clone(),
            input_owner_session: focused.clone(),
            top_line: render_top_line(console, focused, focused, context.waiting_count),
            viewport_lines: normalize_viewport_lines(&snapshot),
            bottom_line: render_bottom_line(console, focused, restore_notice.as_deref()),
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
        state.last_focused_session = Some(focused.clone());
        state.last_rendered_session = Some(peeked.clone());

        Ok(RenderFrame {
            mode: RenderMode::PeekReadOnly,
            rendered_session: peeked.clone(),
            input_owner_session: focused.clone(),
            top_line: render_top_line(console, peeked, focused, context.waiting_count),
            viewport_lines: normalize_viewport_lines(&snapshot),
            bottom_line: render_bottom_line(console, focused, None),
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

fn render_top_line(
    console: &ConsoleState,
    rendered: &SessionAddress,
    focused: &SessionAddress,
    waiting_count: usize,
) -> String {
    if console.is_peeking() {
        return format!("[peek] {rendered} | return: {focused}");
    }

    let mut parts = vec![format!("[{focused}] active")];
    if waiting_count > 0 {
        parts.push(format!("{waiting_count} waiting"));
    }

    if !matches!(console.switch_lock, SwitchLock::Clear) {
        parts.push(format!("lock: {}", render_lock(console.switch_lock)));
    }

    parts.join(" | ")
}

fn render_bottom_line(
    console: &ConsoleState,
    focused: &SessionAddress,
    restore_notice: Option<&str>,
) -> String {
    let mode = if console.is_peeking() {
        "peek"
    } else {
        "normal"
    };
    let mut parts = vec![
        format!("focus: {focused}"),
        format!("node: {}", focused.node_id()),
        format!("mode: {mode}"),
    ];
    if let Some(restore_notice) = restore_notice {
        parts.push(restore_notice.to_string());
    }
    parts.join(" | ")
}

fn render_lock(lock: SwitchLock) -> &'static str {
    match lock {
        SwitchLock::Clear => "clear",
        SwitchLock::Armed => "armed",
        SwitchLock::Blocked => "blocked",
    }
}

fn normalize_viewport_lines(snapshot: &ScreenSnapshot) -> Vec<String> {
    let width = snapshot.size.cols as usize;
    snapshot
        .lines
        .iter()
        .map(|line| fit_width(line, width))
        .collect()
}

fn fit_width(line: &str, width: usize) -> String {
    let mut chars = line.chars().take(width).collect::<Vec<_>>();
    while chars.len() < width {
        chars.push(' ');
    }
    chars.into_iter().collect()
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
        cursor_row: 0,
        cursor_col: len as u16,
        alternate_screen: false,
    }
}

fn render_restore_notice(state: &RendererState, focused: &SessionAddress) -> Option<String> {
    if state.last_focused_session.as_ref() == Some(focused) {
        None
    } else if state.last_focused_session.is_some() {
        Some(format!("restored: {focused}"))
    } else {
        None
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
            cols: 8,
            pixel_width: 0,
            pixel_height: 0,
        });
        engine.feed(b"hello");
        registry.update_screen_state(&address, engine.state());

        let mut console = ConsoleState::new("console-1");
        console.focus(address.clone());

        let sessions = registry.list();
        let frame = Renderer::new()
            .render(&console, &sessions, RenderContext { waiting_count: 2 })
            .expect("render should succeed");

        assert_eq!(frame.mode, RenderMode::Focused);
        assert_eq!(frame.rendered_session, address);
        assert_eq!(frame.input_owner_session, frame.rendered_session);
        assert_eq!(frame.top_line, "[devbox-1/session-1] active | 2 waiting");
        assert_eq!(frame.viewport_lines[0], "hello   ");
        assert_eq!(
            frame.bottom_line,
            "focus: devbox-1/session-1 | node: devbox-1 | mode: normal"
        );
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
            cols: 8,
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
            .render(&console, &sessions, RenderContext { waiting_count: 0 })
            .expect("render should succeed");

        assert_eq!(frame.mode, RenderMode::PeekReadOnly);
        assert_eq!(frame.rendered_session, peeked_address);
        assert_eq!(frame.input_owner_session, focused_address.clone());
        assert_eq!(
            frame.top_line,
            "[peek] devbox-2/session-2 | return: devbox-1/session-1"
        );
        assert_eq!(frame.viewport_lines[0], "peek    ");
        assert_eq!(
            frame.bottom_line,
            "focus: devbox-1/session-1 | node: devbox-1 | mode: peek"
        );
        assert_eq!(console.input_owner_session(), Some(&focused_address));
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
            .render(&console, &sessions, RenderContext { waiting_count: 1 })
            .expect("render should succeed");

        assert_eq!(
            frame.top_line,
            "[devbox-1/session-1] active | 1 waiting | lock: armed"
        );
    }

    #[test]
    fn emits_restore_notice_when_focus_changes_and_uses_target_snapshot() {
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
            cols: 8,
            pixel_width: 0,
            pixel_height: 0,
        });
        first_engine.feed(b"first");
        registry.update_screen_state(&first_address, first_engine.state());

        let mut second_engine = TerminalEngine::new(TerminalSize {
            rows: 2,
            cols: 8,
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
            .render_with_state(
                &mut state,
                &console,
                &sessions,
                RenderContext { waiting_count: 0 },
            )
            .expect("first render should succeed");
        assert_eq!(
            first_frame.bottom_line,
            "focus: devbox-1/session-1 | node: devbox-1 | mode: normal"
        );

        console.focus(second_address.clone());
        let second_frame = renderer
            .render_with_state(
                &mut state,
                &console,
                &sessions,
                RenderContext { waiting_count: 0 },
            )
            .expect("second render should succeed");
        assert_eq!(second_frame.mode, RenderMode::Focused);
        assert_eq!(second_frame.rendered_session, second_address);
        assert_eq!(second_frame.viewport_lines[0], "second  ");
        assert_eq!(
            second_frame.bottom_line,
            "focus: devbox-1/session-2 | node: devbox-1 | mode: normal | restored: devbox-1/session-2"
        );

        let third_frame = renderer
            .render_with_state(
                &mut state,
                &console,
                &sessions,
                RenderContext { waiting_count: 0 },
            )
            .expect("steady render should succeed");
        assert_eq!(
            third_frame.bottom_line,
            "focus: devbox-1/session-2 | node: devbox-1 | mode: normal"
        );
    }
}
