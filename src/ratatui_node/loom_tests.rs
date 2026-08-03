#[cfg(feature = "loom_test")]
mod model_tests {
    use loom::sync::mpsc;
    use loom::thread;

    /// Models the core concurrency contract of the ratatui node server: session
    /// events and client lifecycle events from multiple producers are serialized
    /// by a single mpsc channel before they reach the single-writer
    /// `StateEventLoop`. The model verifies that messages from each sender are
    /// delivered in order and that no messages are lost.
    #[test]
    fn state_event_channel_delivers_in_order() {
        let (tx, rx) = mpsc::channel::<usize>();
        let tx2 = tx.clone();

        let h1 = thread::spawn(move || {
            tx.send(1).unwrap();
            tx.send(3).unwrap();
        });
        let h2 = thread::spawn(move || {
            tx2.send(2).unwrap();
            tx2.send(4).unwrap();
        });

        h1.join().unwrap();
        h2.join().unwrap();
        drop(tx);

        let mut received = Vec::new();
        while let Ok(v) = rx.recv() {
            received.push(v);
        }
        // Each sender's messages arrive in order; interleaving between senders is allowed.
        let sender_a: Vec<_> = received.iter().copied().filter(|v| v % 2 == 1).collect();
        let sender_b: Vec<_> = received.iter().copied().filter(|v| v % 2 == 0).collect();
        assert_eq!(sender_a, vec![1, 3]);
        assert_eq!(sender_b, vec![2, 4]);
    }

    /// `SharedState` is built on `std::sync::Mutex`, so a full loom model of the
    /// state itself would require making every registry generic over the sync
    /// backend. Instead, this test models the message-passing contract: a
    /// session-insert event and a client-attach event can be produced
    /// concurrently, but both are delivered through the same channel and
    /// therefore serialized for the single writer.
    #[test]
    fn concurrent_session_insert_and_client_attach() {
        let (tx, rx) = mpsc::channel::<&'static str>();
        let tx2 = tx.clone();

        let h1 = thread::spawn(move || {
            tx.send("insert_session").unwrap();
        });
        let h2 = thread::spawn(move || {
            tx2.send("attach_client").unwrap();
        });

        h1.join().unwrap();
        h2.join().unwrap();
        drop(tx);

        let mut events = Vec::new();
        while let Ok(e) = rx.recv() {
            events.push(e);
        }
        assert!(events.contains(&"insert_session"));
        assert!(events.contains(&"attach_client"));
    }
}
