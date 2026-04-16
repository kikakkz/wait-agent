#![allow(dead_code)]

use crate::console::{ConsoleState, SwitchLock};
use crate::session::{SessionAddress, SessionRecord, SessionStatus};

const DEFAULT_OUTPUT_QUIET_MS: u128 = 800;
const DEFAULT_INPUT_GRACE_MS: u128 = 1_200;
const DEFAULT_STARTUP_GRACE_MS: u128 = 2_000;
const DEFAULT_COMPLETION_OUTPUT_BYTES: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaitingHeuristic {
    pub output_quiet_ms: u128,
    pub input_grace_ms: u128,
    pub startup_grace_ms: u128,
}

impl Default for WaitingHeuristic {
    fn default() -> Self {
        Self {
            output_quiet_ms: DEFAULT_OUTPUT_QUIET_MS,
            input_grace_ms: DEFAULT_INPUT_GRACE_MS,
            startup_grace_ms: DEFAULT_STARTUP_GRACE_MS,
        }
    }
}

impl WaitingHeuristic {
    pub fn classify(&self, session: &SessionRecord, now_unix_ms: u128) -> SessionStatus {
        if matches!(session.status, SessionStatus::Exited) {
            return SessionStatus::Exited;
        }

        let age_ms = now_unix_ms.saturating_sub(session.created_at_unix_ms);
        if age_ms < self.startup_grace_ms
            && session.last_output_at_unix_ms.is_none()
            && session.last_input_at_unix_ms.is_none()
        {
            return SessionStatus::Running;
        }

        if let Some(last_output_at) = session.last_output_at_unix_ms {
            if now_unix_ms.saturating_sub(last_output_at) < self.output_quiet_ms {
                return SessionStatus::Running;
            }

            let input_recent = session
                .last_input_at_unix_ms
                .map(|last_input_at| {
                    now_unix_ms.saturating_sub(last_input_at) < self.input_grace_ms
                })
                .unwrap_or(false);

            if !input_recent {
                return SessionStatus::WaitingInput;
            }
        }

        if let Some(last_input_at) = session.last_input_at_unix_ms {
            if now_unix_ms.saturating_sub(last_input_at) < self.input_grace_ms {
                return SessionStatus::Running;
            }
        }

        SessionStatus::Idle
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitingEntry {
    pub address: SessionAddress,
    pub since_unix_ms: u128,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WaitingQueue {
    entries: Vec<WaitingEntry>,
}

impl WaitingQueue {
    pub fn entries(&self) -> &[WaitingEntry] {
        &self.entries
    }

    pub fn addresses(&self) -> Vec<SessionAddress> {
        self.entries
            .iter()
            .map(|entry| entry.address.clone())
            .collect()
    }

    fn sync(&mut self, evaluations: &[SessionEvaluation], now_unix_ms: u128) {
        let mut next_entries = Vec::new();

        for entry in &self.entries {
            if evaluations.iter().any(|evaluation| {
                evaluation.address == entry.address
                    && matches!(evaluation.status, SessionStatus::WaitingInput)
            }) {
                next_entries.push(entry.clone());
            }
        }

        for evaluation in evaluations {
            if !matches!(evaluation.status, SessionStatus::WaitingInput) {
                continue;
            }

            if next_entries
                .iter()
                .any(|entry| entry.address == evaluation.address)
            {
                continue;
            }

            next_entries.push(WaitingEntry {
                address: evaluation.address.clone(),
                since_unix_ms: now_unix_ms,
            });
        }

        self.entries = next_entries;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEvaluation {
    pub address: SessionAddress,
    pub status: SessionStatus,
}

#[derive(Debug, Default)]
pub struct SchedulerState {
    heuristic: WaitingHeuristic,
    waiting_queue: WaitingQueue,
    phase: SchedulerPhase,
}

impl SchedulerState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_heuristic(heuristic: WaitingHeuristic) -> Self {
        Self {
            heuristic,
            waiting_queue: WaitingQueue::default(),
            phase: SchedulerPhase::Idle,
        }
    }

    pub fn heuristic(&self) -> WaitingHeuristic {
        self.heuristic
    }

    pub fn waiting_queue(&self) -> &WaitingQueue {
        &self.waiting_queue
    }

    pub fn phase(&self) -> &SchedulerPhase {
        &self.phase
    }

    pub fn evaluate_sessions<'a, I>(
        &mut self,
        sessions: I,
        now_unix_ms: u128,
    ) -> Vec<SessionEvaluation>
    where
        I: IntoIterator<Item = &'a SessionRecord>,
    {
        let evaluations = sessions
            .into_iter()
            .map(|session| SessionEvaluation {
                address: session.address().clone(),
                status: self.heuristic.classify(session, now_unix_ms),
            })
            .collect::<Vec<_>>();

        self.waiting_queue.sync(&evaluations, now_unix_ms);
        evaluations
    }

    pub fn on_input_submitted(&mut self, console: &mut ConsoleState, now_unix_ms: u128) {
        self.on_input_submitted_with_bytes(console, now_unix_ms, 0);
    }

    pub fn on_input_submitted_with_bytes(
        &mut self,
        console: &mut ConsoleState,
        now_unix_ms: u128,
        submitted_input_bytes: usize,
    ) {
        console.submit_input();
        console.arm_switch_lock();
        self.phase = SchedulerPhase::ObservingContinuation {
            session: console.focused_session.clone(),
            entered_at_unix_ms: now_unix_ms,
            saw_output: false,
            output_bytes_after_enter: 0,
            submitted_input_bytes,
        };
    }

    pub fn on_session_output(
        &mut self,
        session: &SessionAddress,
        output_at_unix_ms: u128,
        output_bytes: usize,
    ) {
        if let SchedulerPhase::ObservingContinuation {
            session: current_session,
            entered_at_unix_ms,
            saw_output,
            output_bytes_after_enter,
            ..
        } = &mut self.phase
        {
            if current_session.as_ref() == Some(session) && output_at_unix_ms >= *entered_at_unix_ms
            {
                *saw_output = true;
                *output_bytes_after_enter += output_bytes;
            }
        }
    }

    pub fn on_manual_switch(&mut self, console: &mut ConsoleState) {
        console.clear_switch_lock();
        self.phase = SchedulerPhase::Idle;
    }

    pub fn decide_auto_switch<'a, I>(
        &mut self,
        console: &mut ConsoleState,
        sessions: I,
        now_unix_ms: u128,
    ) -> SchedulingDecision
    where
        I: IntoIterator<Item = &'a SessionRecord>,
    {
        let sessions = sessions.into_iter().collect::<Vec<_>>();
        let evaluations = self.evaluate_sessions(sessions.iter().copied(), now_unix_ms);
        let focused_session = console.focused_session.clone();

        if matches!(self.phase, SchedulerPhase::LockedAfterAutoSwitch)
            || matches!(console.switch_lock, SwitchLock::Blocked)
        {
            return SchedulingDecision::no_action(evaluations);
        }

        if let SchedulerPhase::ObservingContinuation {
            session,
            entered_at_unix_ms,
            saw_output,
            output_bytes_after_enter,
            submitted_input_bytes,
        } = &self.phase
        {
            let focused_matches = session.as_ref() == focused_session.as_ref();
            let current_status = current_session_status(&evaluations, focused_session.as_ref());
            let meaningful_output_after_enter =
                output_bytes_after_enter.saturating_sub(submitted_input_bytes.saturating_add(2));
            let current_continues = focused_matches
                && *saw_output
                && meaningful_output_after_enter >= DEFAULT_COMPLETION_OUTPUT_BYTES
                && current_status == Some(SessionStatus::Running);

            if current_continues {
                return SchedulingDecision::stay(evaluations);
            }

            let current_round_completed = focused_matches
                && *saw_output
                && meaningful_output_after_enter >= DEFAULT_COMPLETION_OUTPUT_BYTES
                && current_status == Some(SessionStatus::WaitingInput);

            if current_round_completed {
                console.clear_switch_lock();
                self.phase = SchedulerPhase::Idle;
                return SchedulingDecision::stay(evaluations);
            }

            if now_unix_ms.saturating_sub(*entered_at_unix_ms) < self.heuristic.input_grace_ms {
                return SchedulingDecision::no_action(evaluations);
            }

            let recent_output_after_enter = sessions
                .iter()
                .find(|record| Some(record.address()) == focused_session.as_ref())
                .and_then(|record| record.last_output_at_unix_ms)
                .map(|last_output_at| last_output_at >= *entered_at_unix_ms)
                .unwrap_or(false);

            if recent_output_after_enter
                && meaningful_output_after_enter >= DEFAULT_COMPLETION_OUTPUT_BYTES
                && current_session_status(&evaluations, focused_session.as_ref())
                    == Some(SessionStatus::Running)
            {
                return SchedulingDecision::stay(evaluations);
            }

            self.phase = SchedulerPhase::ArmedAfterEnter;
        }

        if !matches!(self.phase, SchedulerPhase::ArmedAfterEnter)
            || !matches!(console.switch_lock, SwitchLock::Armed)
        {
            return SchedulingDecision::no_action(evaluations);
        }

        let next_target = self
            .waiting_queue
            .entries()
            .iter()
            .find(|entry| Some(&entry.address) != focused_session.as_ref())
            .map(|entry| entry.address.clone());

        if let Some(target) = next_target {
            console.focus(target.clone());
            console.block_switch_lock();
            self.phase = SchedulerPhase::LockedAfterAutoSwitch;
            return SchedulingDecision::switch(target, evaluations);
        }

        SchedulingDecision::no_action(evaluations)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchedulerPhase {
    Idle,
    ObservingContinuation {
        session: Option<SessionAddress>,
        entered_at_unix_ms: u128,
        saw_output: bool,
        output_bytes_after_enter: usize,
        submitted_input_bytes: usize,
    },
    ArmedAfterEnter,
    LockedAfterAutoSwitch,
}

impl Default for SchedulerPhase {
    fn default() -> Self {
        Self::Idle
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchedulingAction {
    None,
    StayOnCurrent,
    SwitchTo(SessionAddress),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchedulingDecision {
    pub action: SchedulingAction,
    pub evaluations: Vec<SessionEvaluation>,
}

impl SchedulingDecision {
    fn no_action(evaluations: Vec<SessionEvaluation>) -> Self {
        Self {
            action: SchedulingAction::None,
            evaluations,
        }
    }

    fn stay(evaluations: Vec<SessionEvaluation>) -> Self {
        Self {
            action: SchedulingAction::StayOnCurrent,
            evaluations,
        }
    }

    fn switch(target: SessionAddress, evaluations: Vec<SessionEvaluation>) -> Self {
        Self {
            action: SchedulingAction::SwitchTo(target),
            evaluations,
        }
    }
}

fn current_session_status(
    evaluations: &[SessionEvaluation],
    focused_session: Option<&SessionAddress>,
) -> Option<SessionStatus> {
    let focused_session = focused_session?;
    evaluations
        .iter()
        .find(|evaluation| &evaluation.address == focused_session)
        .map(|evaluation| evaluation.status.clone())
}

#[cfg(test)]
mod tests {
    use super::{SchedulerPhase, SchedulerState, SchedulingAction, WaitingHeuristic};
    use crate::console::{ConsoleState, SwitchLock};
    use crate::session::{SessionRegistry, SessionStatus};

    #[test]
    fn classifies_recent_output_as_running() {
        let mut registry = SessionRegistry::new();
        let session = registry.create_local_session(
            "local".to_string(),
            "agent-a".to_string(),
            "agent-a".to_string(),
        );
        let address = session.address().clone();
        let base = session.created_at_unix_ms;
        registry.mark_running_at(&address, Some(42), None);
        registry.mark_output_at(&address, base + 100);

        let mut scheduler = SchedulerState::new();
        let evaluations = scheduler.evaluate_sessions(registry.list(), base + 300);

        assert_eq!(evaluations[0].status, SessionStatus::Running);
        assert!(scheduler.waiting_queue().entries().is_empty());
    }

    #[test]
    fn classifies_quiet_output_as_waiting_input() {
        let mut registry = SessionRegistry::new();
        let session = registry.create_local_session(
            "local".to_string(),
            "agent-b".to_string(),
            "agent-b".to_string(),
        );
        let address = session.address().clone();
        let base = session.created_at_unix_ms;
        registry.mark_running_at(&address, Some(42), None);
        registry.mark_output_at(&address, base + 100);

        let mut scheduler = SchedulerState::new();
        let evaluations = scheduler.evaluate_sessions(registry.list(), base + 2_000);

        assert_eq!(evaluations[0].status, SessionStatus::WaitingInput);
        assert_eq!(scheduler.waiting_queue().addresses(), vec![address]);
    }

    #[test]
    fn preserves_fifo_waiting_order() {
        let mut registry = SessionRegistry::new();
        let first = registry.create_local_session(
            "local".to_string(),
            "agent-a".to_string(),
            "agent-a".to_string(),
        );
        let second = registry.create_local_session(
            "local".to_string(),
            "agent-b".to_string(),
            "agent-b".to_string(),
        );
        let first_address = first.address().clone();
        let second_address = second.address().clone();
        let base = first.created_at_unix_ms.max(second.created_at_unix_ms);
        registry.mark_running_at(&first_address, Some(1), None);
        registry.mark_running_at(&second_address, Some(2), None);
        registry.mark_output_at(&first_address, base + 100);
        registry.mark_output_at(&second_address, base + 300);

        let mut scheduler = SchedulerState::new();
        scheduler.evaluate_sessions(registry.list(), base + 1_200);
        scheduler.evaluate_sessions(registry.list(), base + 2_000);

        assert_eq!(
            scheduler.waiting_queue().addresses(),
            vec![first_address, second_address]
        );
    }

    #[test]
    fn removes_session_from_queue_when_it_becomes_active_again() {
        let mut registry = SessionRegistry::new();
        let session = registry.create_local_session(
            "local".to_string(),
            "agent-c".to_string(),
            "agent-c".to_string(),
        );
        let address = session.address().clone();
        let base = session.created_at_unix_ms;
        registry.mark_running_at(&address, Some(7), None);
        registry.mark_output_at(&address, base + 100);

        let mut scheduler = SchedulerState::new();
        scheduler.evaluate_sessions(registry.list(), base + 2_000);
        assert_eq!(scheduler.waiting_queue().addresses(), vec![address.clone()]);

        registry.mark_input_at(&address, base + 2_100);
        let evaluations = scheduler.evaluate_sessions(registry.list(), base + 2_200);

        assert_eq!(evaluations[0].status, SessionStatus::Running);
        assert!(scheduler.waiting_queue().entries().is_empty());
    }

    #[test]
    fn keeps_new_silent_sessions_in_startup_grace_running_state() {
        let mut registry = SessionRegistry::new();
        let session = registry.create_local_session(
            "local".to_string(),
            "agent-d".to_string(),
            "agent-d".to_string(),
        );
        let address = session.address().clone();
        let base = session.created_at_unix_ms;
        registry.mark_running_at(&address, Some(9), None);

        let mut scheduler = SchedulerState::with_heuristic(WaitingHeuristic {
            startup_grace_ms: 5_000,
            ..WaitingHeuristic::default()
        });
        let evaluations = scheduler.evaluate_sessions(registry.list(), base + 1_000);

        assert_eq!(evaluations[0].status, SessionStatus::Running);
        assert!(scheduler.waiting_queue().entries().is_empty());
    }

    #[test]
    fn auto_switches_once_after_enter_when_waiter_exists() {
        let mut registry = SessionRegistry::new();
        let current = registry.create_local_session(
            "local".to_string(),
            "current".to_string(),
            "current".to_string(),
        );
        let waiter = registry.create_local_session(
            "local".to_string(),
            "waiter".to_string(),
            "waiter".to_string(),
        );
        let current_address = current.address().clone();
        let waiter_address = waiter.address().clone();
        let base = current.created_at_unix_ms.max(waiter.created_at_unix_ms);
        registry.mark_running_at(&current_address, Some(1), None);
        registry.mark_running_at(&waiter_address, Some(2), None);
        registry.mark_output_at(&waiter_address, base + 100);

        let mut console = ConsoleState::new("console-1");
        console.focus(current_address.clone());
        let mut scheduler = SchedulerState::new();
        scheduler.on_input_submitted(&mut console, base + 500);

        let decision = scheduler.decide_auto_switch(&mut console, registry.list(), base + 2_000);
        assert_eq!(
            decision.action,
            SchedulingAction::SwitchTo(waiter_address.clone())
        );
        assert_eq!(console.focused_session, Some(waiter_address));
        assert_eq!(console.switch_lock, SwitchLock::Blocked);
        assert_eq!(scheduler.phase(), &SchedulerPhase::LockedAfterAutoSwitch);

        let second = scheduler.decide_auto_switch(&mut console, registry.list(), base + 3_000);
        assert_eq!(second.action, SchedulingAction::None);
    }

    #[test]
    fn does_not_switch_before_continuation_observation_window_expires() {
        let mut registry = SessionRegistry::new();
        let current = registry.create_local_session(
            "local".to_string(),
            "current".to_string(),
            "current".to_string(),
        );
        let waiter = registry.create_local_session(
            "local".to_string(),
            "waiter".to_string(),
            "waiter".to_string(),
        );
        let current_address = current.address().clone();
        let waiter_address = waiter.address().clone();
        let base = current.created_at_unix_ms.max(waiter.created_at_unix_ms);
        registry.mark_running_at(&current_address, Some(1), None);
        registry.mark_running_at(&waiter_address, Some(2), None);
        registry.mark_output_at(&waiter_address, base + 100);

        let mut console = ConsoleState::new("console-1");
        console.focus(current_address.clone());
        let mut scheduler = SchedulerState::new();
        scheduler.on_input_submitted(&mut console, base + 500);

        let early = scheduler.decide_auto_switch(&mut console, registry.list(), base + 900);
        assert_eq!(early.action, SchedulingAction::None);
        assert_eq!(console.focused_session, Some(current_address));
        assert_eq!(console.switch_lock, SwitchLock::Armed);

        let later = scheduler.decide_auto_switch(&mut console, registry.list(), base + 2_000);
        assert_eq!(later.action, SchedulingAction::SwitchTo(waiter_address));
    }

    #[test]
    fn continuation_output_keeps_focus_when_current_round_returns_to_prompt() {
        let mut registry = SessionRegistry::new();
        let current = registry.create_local_session(
            "local".to_string(),
            "current".to_string(),
            "current".to_string(),
        );
        let waiter = registry.create_local_session(
            "local".to_string(),
            "waiter".to_string(),
            "waiter".to_string(),
        );
        let current_address = current.address().clone();
        let waiter_address = waiter.address().clone();
        let base = current.created_at_unix_ms.max(waiter.created_at_unix_ms);
        registry.mark_running_at(&current_address, Some(1), None);
        registry.mark_running_at(&waiter_address, Some(2), None);
        registry.mark_output_at(&waiter_address, base + 100);

        let mut console = ConsoleState::new("console-1");
        console.focus(current_address.clone());
        let mut scheduler = SchedulerState::new();
        scheduler.on_input_submitted(&mut console, base + 500);
        registry.mark_output_at(&current_address, base + 700);
        scheduler.on_session_output(&current_address, base + 700, 32);

        let first = scheduler.decide_auto_switch(&mut console, registry.list(), base + 900);
        assert_eq!(first.action, SchedulingAction::StayOnCurrent);
        assert_eq!(console.focused_session, Some(current_address.clone()));
        assert_eq!(console.switch_lock, SwitchLock::Armed);

        let second = scheduler.decide_auto_switch(&mut console, registry.list(), base + 2_000);
        assert_eq!(second.action, SchedulingAction::StayOnCurrent);
        assert_eq!(console.focused_session, Some(current_address));
        assert_eq!(console.switch_lock, SwitchLock::Clear);
        assert_eq!(scheduler.phase(), &SchedulerPhase::Idle);
    }

    #[test]
    fn switches_to_waiter_when_focused_session_had_prior_prompt_but_no_post_enter_output() {
        let mut registry = SessionRegistry::new();
        let current = registry.create_local_session(
            "local".to_string(),
            "current".to_string(),
            "current".to_string(),
        );
        let waiter = registry.create_local_session(
            "local".to_string(),
            "waiter".to_string(),
            "waiter".to_string(),
        );
        let current_address = current.address().clone();
        let waiter_address = waiter.address().clone();
        let base = current.created_at_unix_ms.max(waiter.created_at_unix_ms);

        registry.mark_running_at(&current_address, Some(1), None);
        registry.mark_running_at(&waiter_address, Some(2), None);

        // Both sessions had already rendered a shell prompt and settled into waiting state.
        registry.mark_output_at(&current_address, base + 100);
        registry.mark_output_at(&waiter_address, base + 200);

        let mut console = ConsoleState::new("console-1");
        console.focus(current_address.clone());
        let mut scheduler = SchedulerState::new();

        scheduler.evaluate_sessions(registry.list(), base + 2_000);
        assert_eq!(
            scheduler.waiting_queue().addresses(),
            vec![current_address.clone(), waiter_address.clone()]
        );

        // The focused session receives a blocking command. It has input, but no meaningful output after Enter yet.
        registry.mark_input_at(&current_address, base + 2_100);
        scheduler.on_input_submitted_with_bytes(&mut console, base + 2_100, 7);

        let decision = scheduler.decide_auto_switch(&mut console, registry.list(), base + 3_500);

        assert_eq!(
            decision.action,
            SchedulingAction::SwitchTo(waiter_address.clone())
        );
        assert_eq!(console.focused_session, Some(waiter_address));
        assert_eq!(console.switch_lock, SwitchLock::Blocked);
    }

    #[test]
    fn switches_to_waiter_when_post_enter_echo_is_followed_by_silence() {
        let mut registry = SessionRegistry::new();
        let current = registry.create_local_session(
            "local".to_string(),
            "current".to_string(),
            "current".to_string(),
        );
        let waiter = registry.create_local_session(
            "local".to_string(),
            "waiter".to_string(),
            "waiter".to_string(),
        );
        let current_address = current.address().clone();
        let waiter_address = waiter.address().clone();
        let base = current.created_at_unix_ms.max(waiter.created_at_unix_ms);

        registry.mark_running_at(&current_address, Some(1), None);
        registry.mark_running_at(&waiter_address, Some(2), None);
        registry.mark_output_at(&current_address, base + 100);
        registry.mark_output_at(&waiter_address, base + 200);

        let mut console = ConsoleState::new("console-1");
        console.focus(current_address.clone());
        let mut scheduler = SchedulerState::new();

        scheduler.evaluate_sessions(registry.list(), base + 2_000);
        registry.mark_input_at(&current_address, base + 2_100);
        scheduler.on_input_submitted(&mut console, base + 2_100);

        // A tiny immediate post-Enter echo happened, but the command then went silent.
        registry.mark_output_at(&current_address, base + 2_120);
        scheduler.on_session_output(&current_address, base + 2_120, 2);

        let decision = scheduler.decide_auto_switch(&mut console, registry.list(), base + 3_500);

        assert_eq!(
            decision.action,
            SchedulingAction::SwitchTo(waiter_address.clone())
        );
        assert_eq!(console.focused_session, Some(waiter_address));
        assert_eq!(console.switch_lock, SwitchLock::Blocked);
    }

    #[test]
    fn tiny_continuation_noise_does_not_block_auto_switch() {
        let mut registry = SessionRegistry::new();
        let current = registry.create_local_session(
            "local".to_string(),
            "current".to_string(),
            "current".to_string(),
        );
        let waiter = registry.create_local_session(
            "local".to_string(),
            "waiter".to_string(),
            "waiter".to_string(),
        );
        let current_address = current.address().clone();
        let waiter_address = waiter.address().clone();
        let base = current.created_at_unix_ms.max(waiter.created_at_unix_ms);

        registry.mark_running_at(&current_address, Some(1), None);
        registry.mark_running_at(&waiter_address, Some(2), None);
        registry.mark_output_at(&current_address, base + 100);
        registry.mark_output_at(&waiter_address, base + 200);

        let mut console = ConsoleState::new("console-1");
        console.focus(current_address.clone());
        let mut scheduler = SchedulerState::new();

        scheduler.evaluate_sessions(registry.list(), base + 2_000);
        registry.mark_input_at(&current_address, base + 2_100);
        scheduler.on_input_submitted(&mut console, base + 2_100);

        // Small post-Enter PTY chatter keeps the session classified as running,
        // but should not count as real continuation after subtracting echoed input.
        registry.mark_output_at(&current_address, base + 2_900);
        scheduler.on_session_output(&current_address, base + 2_900, 9);

        let decision = scheduler.decide_auto_switch(&mut console, registry.list(), base + 3_500);

        assert_eq!(
            decision.action,
            SchedulingAction::SwitchTo(waiter_address.clone())
        );
        assert_eq!(console.focused_session, Some(waiter_address));
        assert_eq!(console.switch_lock, SwitchLock::Blocked);
    }

    #[test]
    fn new_input_submission_clears_previous_auto_switch_lock() {
        let mut registry = SessionRegistry::new();
        let current = registry.create_local_session(
            "local".to_string(),
            "current".to_string(),
            "current".to_string(),
        );
        let waiter = registry.create_local_session(
            "local".to_string(),
            "waiter".to_string(),
            "waiter".to_string(),
        );
        let current_address = current.address().clone();
        let waiter_address = waiter.address().clone();
        let base = current.created_at_unix_ms.max(waiter.created_at_unix_ms);
        registry.mark_running_at(&current_address, Some(1), None);
        registry.mark_running_at(&waiter_address, Some(2), None);
        registry.mark_output_at(&waiter_address, base + 100);

        let mut console = ConsoleState::new("console-1");
        console.focus(current_address.clone());
        let mut scheduler = SchedulerState::new();
        scheduler.on_input_submitted(&mut console, base + 500);
        scheduler.decide_auto_switch(&mut console, registry.list(), base + 2_000);
        assert_eq!(console.switch_lock, SwitchLock::Blocked);

        scheduler.on_input_submitted(&mut console, base + 3_000);
        assert_eq!(console.switch_lock, SwitchLock::Armed);
        assert!(matches!(
            scheduler.phase(),
            SchedulerPhase::ObservingContinuation { .. }
        ));
    }

    #[test]
    fn manual_switch_resets_scheduler_state() {
        let mut console = ConsoleState::new("console-1");
        let focused = crate::session::SessionAddress::new("local", "session-1");
        console.focus(focused);
        let mut scheduler = SchedulerState::new();

        scheduler.on_input_submitted(&mut console, 1_000);
        scheduler.on_manual_switch(&mut console);

        assert_eq!(console.switch_lock, SwitchLock::Clear);
        assert_eq!(scheduler.phase(), &SchedulerPhase::Idle);
    }

    #[test]
    fn does_not_auto_switch_when_only_focused_session_is_waiting() {
        let mut registry = SessionRegistry::new();
        let current = registry.create_local_session(
            "local".to_string(),
            "current".to_string(),
            "current".to_string(),
        );
        let current_address = current.address().clone();
        let base = current.created_at_unix_ms;
        registry.mark_running_at(&current_address, Some(1), None);
        registry.mark_output_at(&current_address, base + 100);

        let mut console = ConsoleState::new("console-1");
        console.focus(current_address.clone());
        let mut scheduler = SchedulerState::new();
        scheduler.on_input_submitted(&mut console, base + 500);

        let decision = scheduler.decide_auto_switch(&mut console, registry.list(), base + 2_000);

        assert_eq!(decision.action, SchedulingAction::None);
        assert_eq!(console.focused_session, Some(current_address.clone()));
        assert_eq!(scheduler.waiting_queue().addresses(), vec![current_address]);
        assert_eq!(console.switch_lock, SwitchLock::Armed);
    }
}
