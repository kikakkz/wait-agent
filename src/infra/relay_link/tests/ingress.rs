//! Inbound relay → node ingress wiring (issue #129): a peer dialing with
//! `via = "relay"` must surface in the ingress listener's event stream
//! exactly like a direct TCP dial. The listener consumes the relay client's
//! `accept_inbound` queue and pushes every routed stream through the same
//! server-side pipeline (tonic server TLS, ClientHello, operator auth); the
//! dialer side uses the production pinned connector, so the test proves the
//! full receive path end to end. Blocking handle calls and std-channel event
//! waits stay off the single-threaded test runtime (the relay server runs on
//! it).

use std::net::SocketAddr;
use std::sync::mpsc;
use std::time::Duration;

use tokio::sync::mpsc as tokio_mpsc;
use tokio::time::timeout;
use tokio_stream::wrappers::ReceiverStream;
use tonic::Request;

use super::pair::{open_stream_blocking, spawn_connected_pair};
use super::*;
use crate::infra::remote_grpc_proto::v1::node_session_envelope::Body;
use crate::infra::remote_grpc_proto::v1::node_session_service_client::NodeSessionServiceClient;
use crate::infra::remote_grpc_proto::v1::{
    ClientHello, Heartbeat, NodeSessionEnvelope, ProtocolVersion,
};
use crate::infra::remote_grpc_transport::{
    test_connect_channel, test_relay_dialer, GrpcRemoteNodeTransport, RemoteNodeTransport,
    RemoteNodeTransportEvent,
};

fn client_hello_envelope(node_id: &str) -> NodeSessionEnvelope {
    NodeSessionEnvelope {
        message_id: "client-hello-1".to_string(),
        sent_at: None,
        session_instance_id: "client-session-1".to_string(),
        correlation_id: None,
        route: None,
        body: Some(Body::ClientHello(ClientHello {
            node_id: node_id.to_string(),
            node_instance_id: "instance-a".to_string(),
            min_supported_version: Some(ProtocolVersion { major: 1, minor: 0 }),
            max_supported_version: Some(ProtocolVersion { major: 1, minor: 0 }),
            capabilities: None,
            resume: None,
            auth_scheme: "none".to_string(),
            challenge_response: vec![],
        })),
    }
}

fn heartbeat_envelope(node_id: &str) -> NodeSessionEnvelope {
    NodeSessionEnvelope {
        message_id: "heartbeat-1".to_string(),
        sent_at: None,
        session_instance_id: "client-session-1".to_string(),
        correlation_id: None,
        route: None,
        body: Some(Body::Heartbeat(Heartbeat {
            runtime_id: node_id.to_string(),
        })),
    }
}

fn unused_local_addr() -> SocketAddr {
    let listener =
        std::net::TcpListener::bind("127.0.0.1:0").expect("ephemeral listener should bind");
    let addr = listener
        .local_addr()
        .expect("ephemeral listener should report local addr");
    drop(listener);
    addr
}

async fn next_listener_event(
    events: &mut tokio_mpsc::UnboundedReceiver<RemoteNodeTransportEvent>,
    context: &str,
) -> RemoteNodeTransportEvent {
    timeout(NO_DEADLOCK, events.recv())
        .await
        .unwrap_or_else(|_| panic!("{context}: event should arrive within the deadline"))
        .unwrap_or_else(|| panic!("{context}: listener event stream should stay open"))
}

/// Bridges the listener's std event channel onto the async runtime so event
/// waits never block the single-threaded test runtime (the relay server runs
/// on it).
fn forward_listener_events(
    event_rx: mpsc::Receiver<RemoteNodeTransportEvent>,
) -> tokio_mpsc::UnboundedReceiver<RemoteNodeTransportEvent> {
    let (listener_event_tx, listener_events) = tokio_mpsc::unbounded_channel();
    std::thread::spawn(move || {
        while let Ok(event) = event_rx.recv() {
            if listener_event_tx.send(event).is_err() {
                break;
            }
        }
    });
    listener_events
}

/// Spins until `handle`'s strong count reaches `expected`, so teardown can
/// prove the listener's relay accept worker released its clone (the count
/// includes `NodePair`'s own clone).
async fn wait_strong_count(
    handle: &std::sync::Arc<crate::infra::relay_client::RelayClientHandle>,
    expected: usize,
) {
    let deadline = std::time::Instant::now() + NO_DEADLOCK;
    while std::sync::Arc::strong_count(handle) != expected {
        assert!(
            std::time::Instant::now() < deadline,
            "relay handle strong count should reach {expected}, got {}",
            std::sync::Arc::strong_count(handle)
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn relay_inbound_stream_reaches_ingress_listener_event_stream() {
    let pair = spawn_connected_pair().await;

    // The listener presents B's node credential certificate: the routed pin
    // (B's relay fingerprint) is the SPKI hash of this certificate, which is
    // also what the dialer's TLS pin check verifies. The authorized-operators
    // dir stays empty, mirroring the direct-path inbound tests.
    let auth_dir = temp_dir("relay-ingress-auth");
    let bind_addr = unused_local_addr();
    let transport = GrpcRemoteNodeTransport::with_tls(
        pair.credentials_b.cert_path.clone(),
        pair.credentials_b.key_path.clone(),
    )
    .with_authorized_operators_dir(auth_dir)
    .with_relay_client(Some(pair.handle_b.clone()));
    let (event_tx, event_rx) = mpsc::channel();
    let listener_guard = transport
        .listen_inbound(bind_addr, event_tx)
        .expect("ingress listener should start");
    let mut listener_events = forward_listener_events(event_rx);

    // Dial in through the relay with the production pinned connector. The
    // URI host is ignored on the relay path: the pin routes the stream to B
    // and pins B's presented certificate.
    let channel = timeout(
        NO_DEADLOCK,
        test_connect_channel(
            "http://unused:443",
            &pair.fp_b,
            test_relay_dialer(pair.handle_a.clone()),
        ),
    )
    .await
    .expect("relay dial + inner TLS should complete within the deadline")
    .expect("relay dial + inner TLS handshake should succeed");
    let mut client = NodeSessionServiceClient::new(channel);
    let (hello_tx, hello_rx) = tokio_mpsc::channel(8);
    hello_tx
        .send(client_hello_envelope("peer-a"))
        .await
        .expect("client hello should send");
    let response = client
        .open_node_session(Request::new(ReceiverStream::new(hello_rx)))
        .await
        .expect("node session should open over the relay-routed stream");
    let mut inbound = response.into_inner();
    let server_hello = inbound
        .message()
        .await
        .expect("server hello should decode")
        .expect("server hello should be present");
    assert!(
        matches!(server_hello.body, Some(Body::ServerHello(_))),
        "the session must start with a server hello"
    );

    // The ingress event stream must show SessionOpened exactly like a TCP
    // accept — this is the receive path the issue wires up.
    let opened = next_listener_event(&mut listener_events, "session opened").await;
    let session = match opened {
        RemoteNodeTransportEvent::SessionOpened { session } => session,
        other => panic!("unexpected transport event: {other:?}"),
    };
    assert_eq!(session.node_id(), "peer-a");
    assert!(!session.session_instance_id().is_empty());

    // Envelope path, dialer → listener.
    hello_tx
        .send(heartbeat_envelope("peer-a"))
        .await
        .expect("heartbeat should send");
    let received = next_listener_event(&mut listener_events, "envelope received").await;
    match received {
        RemoteNodeTransportEvent::EnvelopeReceived {
            node_id, envelope, ..
        } => {
            assert_eq!(node_id, "peer-a");
            assert!(
                matches!(envelope.body, Some(Body::Heartbeat(_))),
                "heartbeat envelope expected"
            );
        }
        other => panic!("unexpected transport event: {other:?}"),
    }

    // Envelope path, listener → dialer.
    session
        .send(heartbeat_envelope("server"))
        .expect("outbound heartbeat should queue");
    let outbound = inbound
        .message()
        .await
        .expect("outbound heartbeat should decode")
        .expect("outbound heartbeat should be present");
    assert!(
        matches!(outbound.body, Some(Body::Heartbeat(_))),
        "outbound heartbeat envelope expected"
    );

    // Teardown: the listener's relay accept worker holds a handle clone for
    // its lifetime. Dropping the guard sets its stop flag; a final nudge
    // stream wakes the blocked accept so the exit (and Arc release) is
    // deterministic, then NodePair::finish's sole-owner check applies.
    drop(inbound);
    drop(client);
    drop(listener_guard);
    drop(transport);
    let _nudge = open_stream_blocking(&pair.handle_a, &pair.fp_b);
    wait_strong_count(&pair.handle_b, 1).await;
    wait_strong_count(&pair.handle_a, 1).await;
    pair.finish().await;
}

#[tokio::test]
async fn relay_inbound_dial_presents_credential_identity_without_explicit_tls() {
    let pair = spawn_connected_pair().await;

    // The e2e topology starts nodes without --node-cert-path/--node-key-path,
    // so the ingress listener has no explicit TLS identity. A relay-enrolled
    // node is dialed in by pin (tls_pin_sha256 == the relay identity
    // fingerprint == the SPKI hash of the node credential certificate), so
    // the listener must fall back to the credential identity; a plaintext
    // listener fails the pinned dial instantly (rustls InvalidContentType
    // against the h2 preface). No with_tls here on purpose.
    let auth_dir = temp_dir("relay-ingress-credential-auth");
    let bind_addr = unused_local_addr();
    let transport = GrpcRemoteNodeTransport::new()
        .with_authorized_operators_dir(auth_dir)
        .with_credential_tls_identity(pair.credentials_b.clone())
        .with_relay_client(Some(pair.handle_b.clone()));
    let (event_tx, event_rx) = mpsc::channel();
    let listener_guard = transport
        .listen_inbound(bind_addr, event_tx)
        .expect("ingress listener should start");
    let mut listener_events = forward_listener_events(event_rx);

    let channel = timeout(
        NO_DEADLOCK,
        test_connect_channel(
            "http://unused:443",
            &pair.fp_b,
            test_relay_dialer(pair.handle_a.clone()),
        ),
    )
    .await
    .expect("relay dial + inner TLS should complete within the deadline")
    .expect("pinned TLS over the relay stream should verify the credential identity");
    let mut client = NodeSessionServiceClient::new(channel);
    let (hello_tx, hello_rx) = tokio_mpsc::channel(8);
    hello_tx
        .send(client_hello_envelope("peer-a"))
        .await
        .expect("client hello should send");
    let response = client
        .open_node_session(Request::new(ReceiverStream::new(hello_rx)))
        .await
        .expect("node session should open over the credential-TLS listener");
    let mut inbound = response.into_inner();
    let server_hello = inbound
        .message()
        .await
        .expect("server hello should decode")
        .expect("server hello should be present");
    assert!(
        matches!(server_hello.body, Some(Body::ServerHello(_))),
        "the session must start with a server hello"
    );

    let opened = next_listener_event(&mut listener_events, "session opened").await;
    match opened {
        RemoteNodeTransportEvent::SessionOpened { session } => {
            assert_eq!(session.node_id(), "peer-a");
        }
        other => panic!("unexpected transport event: {other:?}"),
    }

    // Teardown: same protocol as the explicit-TLS test.
    drop(inbound);
    drop(client);
    drop(listener_guard);
    drop(transport);
    let _nudge = open_stream_blocking(&pair.handle_a, &pair.fp_b);
    wait_strong_count(&pair.handle_b, 1).await;
    wait_strong_count(&pair.handle_a, 1).await;
    pair.finish().await;
}
