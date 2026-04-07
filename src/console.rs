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
    pub last_interactive_session: Option<SessionAddress>,
    pub peek_session: Option<SessionAddress>,
    pub input_state: InputState,
    pub switch_lock: SwitchLock,
}

impl ConsoleState {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: ConsoleId::new(id),
            focused_session: None,
            last_interactive_session: None,
            peek_session: None,
            input_state: InputState::Idle,
            switch_lock: SwitchLock::Clear,
        }
    }

    pub fn can_switch(&self) -> bool {
        matches!(self.input_state, InputState::Idle | InputState::Submitted)
    }

    pub fn focus(&mut self, session: SessionAddress) {
        self.apply_focus(session, false);
    }

    pub fn select_initial_focus(&mut self, sessions: &[SessionAddress]) -> Option<SessionAddress> {
        if let Some(current) = self
            .focused_session
            .as_ref()
            .filter(|current| sessions.contains(current))
        {
            return Some(current.clone());
        }

        let target = if sessions.len() == 1 {
            Some(sessions[0].clone())
        } else if let Some(last_interactive) = self
            .last_interactive_session
            .as_ref()
            .filter(|session| sessions.contains(session))
        {
            Some(last_interactive.clone())
        } else {
            sessions.first().cloned()
        }?;

        self.apply_focus(target.clone(), false);
        Some(target)
    }

    pub fn focus_next(&mut self, sessions: &[SessionAddress]) -> Option<SessionAddress> {
        self.switch_in_order(sessions, 1)
    }

    pub fn focus_previous(&mut self, sessions: &[SessionAddress]) -> Option<SessionAddress> {
        self.switch_in_order(sessions, -1)
    }

    pub fn focus_index(
        &mut self,
        sessions: &[SessionAddress],
        index: usize,
    ) -> Option<SessionAddress> {
        if !self.can_switch() {
            return None;
        }

        let target = sessions.get(index.checked_sub(1)?).cloned()?;
        self.apply_focus(target.clone(), true);
        Some(target)
    }

    pub fn focus_address(
        &mut self,
        sessions: &[SessionAddress],
        address: &SessionAddress,
    ) -> Option<SessionAddress> {
        if !self.can_switch() {
            return None;
        }

        let target = sessions.iter().find(|session| *session == address)?.clone();
        self.apply_focus(target.clone(), true);
        Some(target)
    }

    pub fn handle_focus_loss(&mut self, sessions: &[SessionAddress]) -> Option<SessionAddress> {
        let current = self.focused_session.as_ref()?;
        if sessions.contains(current) {
            return Some(current.clone());
        }

        self.focused_session = None;
        self.peek_session = None;
        self.select_initial_focus(sessions)
    }

    fn apply_focus(&mut self, session: SessionAddress, clear_lock: bool) {
        self.focused_session = Some(session);
        self.peek_session = None;
        if clear_lock {
            self.switch_lock = SwitchLock::Clear;
        }
    }

    fn switch_in_order(
        &mut self,
        sessions: &[SessionAddress],
        delta: isize,
    ) -> Option<SessionAddress> {
        if !self.can_switch() || sessions.is_empty() {
            return None;
        }

        let next_index = match self
            .focused_session
            .as_ref()
            .and_then(|current| sessions.iter().position(|session| session == current))
        {
            Some(current_index) => wrap_index(current_index, sessions.len(), delta),
            None => 0,
        };

        let target = sessions.get(next_index)?.clone();
        self.apply_focus(target.clone(), true);
        Some(target)
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
        self.last_interactive_session = self.focused_session.clone();
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

    pub fn arm_switch_lock(&mut self) {
        self.switch_lock = SwitchLock::Armed;
    }

    pub fn block_switch_lock(&mut self) {
        self.switch_lock = SwitchLock::Blocked;
    }

    pub fn clear_switch_lock(&mut self) {
        self.switch_lock = SwitchLock::Clear;
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

fn wrap_index(current: usize, len: usize, delta: isize) -> usize {
    let len = len as isize;
    ((current as isize + delta).rem_euclid(len)) as usize
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

    #[test]
    fn selects_last_interactive_session_when_attaching() {
        let mut console = ConsoleState::new("console-1");
        let first = SessionAddress::new("local", "session-1");
        let second = SessionAddress::new("local", "session-2");
        console.focus(second.clone());
        console.submit_input();
        console.focused_session = None;

        let selected = console.select_initial_focus(&[first, second.clone()]);

        assert_eq!(selected, Some(second.clone()));
        assert_eq!(console.focused_session, Some(second));
    }

    #[test]
    fn cycles_focus_forward_and_backward() {
        let mut console = ConsoleState::new("console-1");
        let first = SessionAddress::new("local", "session-1");
        let second = SessionAddress::new("local", "session-2");
        let third = SessionAddress::new("local", "session-3");
        let sessions = vec![first.clone(), second.clone(), third.clone()];

        assert_eq!(console.focus_next(&sessions), Some(first.clone()));
        assert_eq!(console.focus_next(&sessions), Some(second.clone()));
        assert_eq!(console.focus_previous(&sessions), Some(first));
        assert_eq!(console.focus_previous(&sessions), Some(third));
    }

    #[test]
    fn focus_loss_selects_next_available_session() {
        let mut console = ConsoleState::new("console-1");
        let first = SessionAddress::new("local", "session-1");
        let second = SessionAddress::new("local", "session-2");
        console.focus(second);

        let selected = console.handle_focus_loss(&[first.clone()]);

        assert_eq!(selected, Some(first.clone()));
        assert_eq!(console.focused_session, Some(first));
        assert!(console.peek_session.is_none());
    }

    #[test]
    fn blocks_direct_focus_switches_while_typing() {
        let mut console = ConsoleState::new("console-1");
        let first = SessionAddress::new("local", "session-1");
        let second = SessionAddress::new("local", "session-2");
        let sessions = vec![first, second.clone()];

        console.start_typing();
        console.set_input_len(4);

        assert_eq!(console.focus_index(&sessions, 2), None);
        assert_eq!(console.focus_address(&sessions, &second), None);
        assert!(console.focused_session.is_none());
    }

    #[test]
    fn switch_lock_helpers_update_state() {
        let mut console = ConsoleState::new("console-1");

        console.arm_switch_lock();
        assert_eq!(console.switch_lock, super::SwitchLock::Armed);

        console.block_switch_lock();
        assert_eq!(console.switch_lock, super::SwitchLock::Blocked);

        console.clear_switch_lock();
        assert_eq!(console.switch_lock, super::SwitchLock::Clear);
    }
}
