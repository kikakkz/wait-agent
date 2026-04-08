#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventGroup {
    Console,
    Session,
    Pty,
    Scheduler,
    Transport,
}

pub trait EventBusMessage {
    fn event_group(&self) -> EventGroup;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventEnvelope<T> {
    pub sequence: u64,
    pub emitted_at_unix_ms: u128,
    pub group: EventGroup,
    pub payload: T,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SubscriberId(u64);

impl SubscriberId {
    pub fn new(value: u64) -> Self {
        Self(value)
    }
}

#[derive(Debug)]
pub struct EventBus<T> {
    next_sequence: u64,
    next_subscriber_id: u64,
    subscribers: HashMap<SubscriberId, Sender<EventEnvelope<T>>>,
}

impl<T> Default for EventBus<T> {
    fn default() -> Self {
        Self {
            next_sequence: 0,
            next_subscriber_id: 0,
            subscribers: HashMap::new(),
        }
    }
}

impl<T> EventBus<T>
where
    T: EventBusMessage + Clone + Send + 'static,
{
    pub fn new() -> Self {
        Self::default()
    }

    pub fn subscribe(&mut self) -> (SubscriberId, Receiver<EventEnvelope<T>>) {
        self.next_subscriber_id += 1;
        let subscriber_id = SubscriberId::new(self.next_subscriber_id);
        let (tx, rx) = mpsc::channel();
        self.subscribers.insert(subscriber_id, tx);
        (subscriber_id, rx)
    }

    pub fn unsubscribe(&mut self, subscriber_id: SubscriberId) -> bool {
        self.subscribers.remove(&subscriber_id).is_some()
    }

    pub fn publish(&mut self, payload: T) -> usize {
        self.next_sequence += 1;
        let envelope = EventEnvelope {
            sequence: self.next_sequence,
            emitted_at_unix_ms: now_unix_ms(),
            group: payload.event_group(),
            payload,
        };

        let mut delivered = 0;
        self.subscribers.retain(|_, subscriber| {
            if subscriber.send(envelope.clone()).is_ok() {
                delivered += 1;
                true
            } else {
                false
            }
        });

        delivered
    }

    pub fn subscriber_count(&self) -> usize {
        self.subscribers.len()
    }
}

fn now_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(test)]
mod tests {
    use super::{EventBus, EventBusMessage, EventGroup};

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum TestEvent {
        FocusChanged,
        SessionStarted,
        StdoutChunk,
    }

    impl EventBusMessage for TestEvent {
        fn event_group(&self) -> EventGroup {
            match self {
                Self::FocusChanged => EventGroup::Console,
                Self::SessionStarted => EventGroup::Session,
                Self::StdoutChunk => EventGroup::Pty,
            }
        }
    }

    #[test]
    fn publishes_ordered_envelopes_to_multiple_subscribers() {
        let mut bus = EventBus::new();
        let (_first_id, first_rx) = bus.subscribe();
        let (_second_id, second_rx) = bus.subscribe();

        assert_eq!(bus.publish(TestEvent::SessionStarted), 2);
        assert_eq!(bus.publish(TestEvent::StdoutChunk), 2);

        let first_one = first_rx.recv().expect("first event should arrive");
        let first_two = first_rx.recv().expect("second event should arrive");
        let second_one = second_rx.recv().expect("first event should arrive");
        let second_two = second_rx.recv().expect("second event should arrive");

        assert_eq!(first_one.sequence, 1);
        assert_eq!(first_one.group, EventGroup::Session);
        assert_eq!(first_two.sequence, 2);
        assert_eq!(first_two.group, EventGroup::Pty);
        assert_eq!(second_one.sequence, 1);
        assert_eq!(second_two.sequence, 2);
    }

    #[test]
    fn unsubscribe_removes_subscriber_from_future_deliveries() {
        let mut bus = EventBus::new();
        let (first_id, first_rx) = bus.subscribe();
        let (_second_id, second_rx) = bus.subscribe();

        assert!(bus.unsubscribe(first_id));
        assert_eq!(bus.publish(TestEvent::FocusChanged), 1);
        assert!(first_rx.try_recv().is_err());

        let envelope = second_rx
            .recv()
            .expect("remaining subscriber should receive");
        assert_eq!(envelope.group, EventGroup::Console);
        assert_eq!(bus.subscriber_count(), 1);
    }
}
