use crate::domain::local_runtime::LocalRuntimeEvent;
use crate::event::{EventBus, EventEnvelope, SubscriberId};
use std::sync::mpsc::Receiver;

pub trait LocalRuntimeEventPublisher {
    fn publish(&mut self, event: LocalRuntimeEvent) -> usize;
}

pub trait LocalRuntimeEventSubscriber {
    fn subscribe(&mut self) -> (SubscriberId, Receiver<EventEnvelope<LocalRuntimeEvent>>);
    fn unsubscribe(&mut self, subscriber_id: SubscriberId) -> bool;
}

#[derive(Debug, Default)]
pub struct LocalRuntimeEventBus {
    inner: EventBus<LocalRuntimeEvent>,
}

impl LocalRuntimeEventBus {
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    pub fn subscriber_count(&self) -> usize {
        self.inner.subscriber_count()
    }
}

impl LocalRuntimeEventPublisher for LocalRuntimeEventBus {
    fn publish(&mut self, event: LocalRuntimeEvent) -> usize {
        self.inner.publish(event)
    }
}

impl LocalRuntimeEventSubscriber for LocalRuntimeEventBus {
    fn subscribe(&mut self) -> (SubscriberId, Receiver<EventEnvelope<LocalRuntimeEvent>>) {
        self.inner.subscribe()
    }

    fn unsubscribe(&mut self, subscriber_id: SubscriberId) -> bool {
        self.inner.unsubscribe(subscriber_id)
    }
}

#[cfg(test)]
mod tests {
    use super::{LocalRuntimeEventBus, LocalRuntimeEventPublisher, LocalRuntimeEventSubscriber};
    use crate::domain::local_runtime::{ChromeEvent, ChromeSurface, LocalRuntimeEvent};

    #[test]
    fn local_runtime_event_bus_publishes_to_subscribers() {
        let mut bus = LocalRuntimeEventBus::new();
        let (_subscriber_id, rx) = bus.subscribe();

        assert_eq!(
            bus.publish(LocalRuntimeEvent::Chrome(ChromeEvent::SurfaceResized {
                surface: ChromeSurface::FooterPane,
                width: 80,
                height: 1,
            })),
            1
        );

        let envelope = rx.recv().expect("local runtime event should arrive");
        match envelope.payload {
            LocalRuntimeEvent::Chrome(ChromeEvent::SurfaceResized {
                surface,
                width,
                height,
            }) => {
                assert_eq!(surface, ChromeSurface::FooterPane);
                assert_eq!(width, 80);
                assert_eq!(height, 1);
            }
            other => panic!("unexpected event payload: {other:?}"),
        }
    }

    #[test]
    fn unsubscribe_stops_future_local_runtime_delivery() {
        let mut bus = LocalRuntimeEventBus::new();
        let (subscriber_id, rx) = bus.subscribe();

        assert!(bus.unsubscribe(subscriber_id));
        assert_eq!(bus.subscriber_count(), 0);
        assert_eq!(
            bus.publish(LocalRuntimeEvent::Chrome(ChromeEvent::FullscreenChanged {
                is_fullscreen: true,
            })),
            0
        );
        assert!(rx.try_recv().is_err());
    }
}
