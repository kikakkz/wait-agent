#![allow(dead_code)]

use crate::session::SessionAddress;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsoleId(String);

impl ConsoleId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsoleState {
    pub id: ConsoleId,
    pub focused_session: Option<SessionAddress>,
    pub peek_session: Option<SessionAddress>,
    pub input_state: InputState,
    pub switch_lock: SwitchLock,
}

impl ConsoleState {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: ConsoleId::new(id),
            focused_session: None,
            peek_session: None,
            input_state: InputState::Idle,
            switch_lock: SwitchLock::Clear,
        }
    }

    pub fn can_switch(&self) -> bool {
        matches!(self.input_state, InputState::Idle | InputState::Submitted)
    }

    pub fn focus(&mut self, session: SessionAddress) {
        self.focused_session = Some(session);
        self.peek_session = None;
    }

    pub fn start_typing(&mut self) {
        self.input_state = InputState::Typing { bytes: 0 };
    }

    pub fn set_input_len(&mut self, bytes: usize) {
        if let InputState::Typing {
            bytes: current_bytes,
        } = &mut self.input_state
        {
            *current_bytes = bytes;
        }
    }

    pub fn submit_input(&mut self) {
        self.input_state = InputState::Submitted;
    }

    pub fn clear_input(&mut self) {
        self.input_state = InputState::Idle;
    }

    pub fn enter_peek(&mut self, session: SessionAddress) {
        self.peek_session = Some(session);
    }

    pub fn exit_peek(&mut self) {
        self.peek_session = None;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputState {
    Idle,
    Typing { bytes: usize },
    Submitted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchLock {
    Clear,
    Armed,
    Blocked,
}

#[cfg(test)]
mod tests {
    use super::ConsoleState;
    use crate::session::SessionAddress;

    #[test]
    fn blocks_switching_while_typing() {
        let mut console = ConsoleState::new("console-1");
        assert!(console.can_switch());

        console.start_typing();
        console.set_input_len(3);
        assert!(!console.can_switch());

        console.submit_input();
        assert!(console.can_switch());
    }

    #[test]
    fn keeps_focus_when_peek_exits() {
        let mut console = ConsoleState::new("console-1");
        let focus = SessionAddress::new("local", "claude-1");
        let peek = SessionAddress::new("local", "codex-2");
        console.focus(focus.clone());
        console.enter_peek(peek);
        console.exit_peek();

        assert_eq!(console.focused_session, Some(focus));
        assert!(console.peek_session.is_none());
    }
}
