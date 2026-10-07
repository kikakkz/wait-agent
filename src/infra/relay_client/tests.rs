use super::*;
use rustls::client::danger::ServerCertVerifier;

#[test]
fn retry_policy_defaults_to_half_second_initial_and_five_second_cap() {
    let policy = RelayRetryPolicy::default();
    assert_eq!(policy.initial_delay, Duration::from_millis(500));
    assert_eq!(policy.max_delay, Duration::from_secs(5));
}

#[test]
fn from_relay_toml_defaults_heartbeat_and_retry() {
    let config = RelayClientConfig::from_relay_toml(
        RelayTomlConfig {
            address: "relay.example:7475".to_string(),
            relay_fingerprint: "ab".to_string(),
            heartbeat_interval_secs: None,
        },
        NodeCredentialPaths {
            key_path: std::path::PathBuf::from("node.key"),
            cert_path: std::path::PathBuf::from("node.crt"),
        },
    );
    assert_eq!(config.heartbeat_interval, DEFAULT_RELAY_HEARTBEAT_INTERVAL);
    assert_eq!(config.retry, RelayRetryPolicy::default());
}

#[test]
fn from_relay_toml_applies_the_configured_heartbeat() {
    let config = RelayClientConfig::from_relay_toml(
        RelayTomlConfig {
            address: "relay.example:7475".to_string(),
            relay_fingerprint: "ab".to_string(),
            heartbeat_interval_secs: Some(30),
        },
        NodeCredentialPaths {
            key_path: std::path::PathBuf::from("node.key"),
            cert_path: std::path::PathBuf::from("node.crt"),
        },
    );
    assert_eq!(config.heartbeat_interval, Duration::from_secs(30));
}

#[test]
fn address_without_port_defaults_to_the_relay_port() {
    let (host, port) = parse_relay_address("relay.example").expect("parse");
    assert_eq!(host, "relay.example");
    assert_eq!(port, DEFAULT_RELAY_LISTEN_PORT);
}

#[test]
fn address_with_explicit_port_wins() {
    let (host, port) = parse_relay_address("relay.example:9999").expect("parse");
    assert_eq!(host, "relay.example");
    assert_eq!(port, 9999);
}

#[test]
fn address_rejects_garbage() {
    assert!(matches!(
        parse_relay_address(""),
        Err(RelayClientConnectError::Address(..))
    ));
    assert!(matches!(
        parse_relay_address(":9999"),
        Err(RelayClientConnectError::Address(..))
    ));
    assert!(matches!(
        parse_relay_address("host:notaport"),
        Err(RelayClientConnectError::Address(..))
    ));
}

#[test]
fn control_frame_handler_folds_error_into_teardown_reason() {
    let (event_tx, mut event_rx) = mpsc::channel(4);
    let outcome = handle_control_frame(
        &event_tx,
        Frame::Error {
            stream_id: 0,
            code: 0x000a,
            message: "relay heartbeat timed out; the link was evicted".to_string(),
        },
    );
    assert!(
        matches!(outcome, ControlOutcome::Break(ref reason) if reason == "relay error 0x000a: relay heartbeat timed out; the link was evicted"),
        "an error frame must tear the link down with the folded reason: {outcome:?}"
    );
    assert!(
        matches!(
            event_rx.try_recv(),
            Ok(RelayClientEvent::RelayError { code: 0x000a, .. })
        ),
        "the structured relay error must be emitted before the teardown"
    );
}

#[test]
fn control_frame_handler_forwards_presence_and_keeps_supervising() {
    let (event_tx, mut event_rx) = mpsc::channel(4);
    let outcome = handle_control_frame(
        &event_tx,
        Frame::Presence {
            node_id: "peer".to_string(),
            online: false,
        },
    );
    assert!(
        matches!(outcome, ControlOutcome::Continue),
        "presence keeps the link up: {outcome:?}"
    );
    assert!(matches!(
        event_rx.try_recv(),
        Ok(RelayClientEvent::Presence { node_id, online: false }) if node_id == "peer"
    ));
}

#[test]
fn control_frame_handler_ignores_unexpected_frames() {
    let (event_tx, mut event_rx) = mpsc::channel(4);
    let outcome = handle_control_frame(&event_tx, Frame::Heartbeat);
    assert!(matches!(outcome, ControlOutcome::Continue));
    assert!(
        event_rx.try_recv().is_err(),
        "an unexpected control frame emits no event"
    );
}

#[tokio::test]
async fn death_reason_surfaces_control_frames_queued_before_link_death() {
    // The eviction race (issue #114): the mux reader queues the relay's
    // `Error` frame and dies on the close that follows it, both before
    // the supervisor observes the death. The drain must emit the queued
    // frames and fold the reason instead of reporting a bare teardown.
    let (client_io, server_io) = tokio::io::duplex(1024);
    let (control_tx, mut control_rx) = mpsc::channel(16);
    let conn = MuxConnection::spawn_relay_link(client_io, control_tx.clone());
    drop(server_io);
    tokio::time::timeout(Duration::from_secs(2), async {
        while !conn.is_closed() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("connection should die when the peer half closes");

    control_tx
        .try_send(Frame::Presence {
            node_id: "peer".to_string(),
            online: true,
        })
        .expect("queue presence");
    control_tx
        .try_send(Frame::Error {
            stream_id: 0,
            code: 0x000a,
            message: "evicted".to_string(),
        })
        .expect("queue eviction error");

    let (event_tx, mut event_rx) = mpsc::channel(16);
    let link_state = Arc::new(ClientLinkState {
        opener_slot: Mutex::new(None),
        inbound_tx: mpsc::channel(4).0,
        watch_interests: Mutex::new(HashSet::new()),
        admin_seq: AtomicU64::new(0),
        pending_admin: Mutex::new(HashMap::new()),
    });
    let reason = pending_control_or_dead_reason(&mut control_rx, &conn, &event_tx, &link_state);
    assert_eq!(reason, "relay error 0x000a: evicted");
    assert!(matches!(
        event_rx.try_recv(),
        Ok(RelayClientEvent::Presence { node_id, online: true }) if node_id == "peer"
    ));
    assert!(matches!(
        event_rx.try_recv(),
        Ok(RelayClientEvent::RelayError { code: 0x000a, .. })
    ));
}

#[test]
fn pinned_verifier_accepts_matching_fingerprint_case_insensitively() {
    let cert = rcgen::Certificate::from_params(rcgen::CertificateParams::new(vec![
        "waitagent".to_string()
    ]))
    .expect("cert should generate");
    let der = cert.serialize_der().expect("cert should serialize");
    let fingerprint = node_credentials::cert_fingerprint_from_der(&der).expect("fingerprint");
    let uppercased = fingerprint.to_uppercase();
    let verifier = PinnedServerCertVerifier {
        expected_fingerprint: uppercased,
    };
    let now = rustls::pki_types::UnixTime::now();
    let result = verifier.verify_server_cert(
        &rustls::pki_types::CertificateDer::from(der.clone()),
        &[],
        &rustls::pki_types::ServerName::try_from("waitagent").expect("server name"),
        &[],
        now,
    );
    assert!(result.is_ok(), "pin match must verify: {result:?}");
}

#[test]
fn pinned_verifier_rejects_mismatched_fingerprint() {
    let cert = rcgen::Certificate::from_params(rcgen::CertificateParams::new(vec![
        "waitagent".to_string()
    ]))
    .expect("cert should generate");
    let der = cert.serialize_der().expect("cert should serialize");
    let verifier = PinnedServerCertVerifier {
        expected_fingerprint: "deadbeef".to_string(),
    };
    let now = rustls::pki_types::UnixTime::now();
    let result = verifier.verify_server_cert(
        &rustls::pki_types::CertificateDer::from(der),
        &[],
        &rustls::pki_types::ServerName::try_from("waitagent").expect("server name"),
        &[],
        now,
    );
    assert!(result.is_err(), "pin mismatch must fail");
}

#[test]
fn egress_ip_label_resolves_for_literal_relay_hosts() {
    // UDP connect sends no packets but lets the kernel pick the egress
    // route, so a literal relay address yields this node's local IP.
    assert_eq!(egress_ip_label("127.0.0.1"), Some("127.0.0.1".to_string()));
    assert_eq!(egress_ip_label("::1"), Some("::1".to_string()));
}

#[test]
fn egress_ip_label_skips_hostname_relays_without_dns() {
    // Resolving the relay's hostname could block on DNS inside the link
    // loop; hostname relays simply contribute no IP label.
    assert_eq!(egress_ip_label("relay.example"), None);
    assert_eq!(egress_ip_label(""), None);
}

#[test]
fn push_label_keeps_first_occurrence_and_skips_blanks() {
    let mut labels = Vec::new();
    push_label(&mut labels, "nas".to_string());
    push_label(&mut labels, "nas".to_string());
    push_label(&mut labels, "  ".to_string());
    push_label(&mut labels, "10.0.1.5".to_string());
    assert_eq!(labels, vec!["nas".to_string(), "10.0.1.5".to_string()]);
}

#[test]
fn node_labels_combine_hostname_and_literal_egress_ip() {
    let labels = node_labels("127.0.0.1");
    assert!(
        labels.iter().any(|label| label == "127.0.0.1"),
        "literal relay host contributes the egress IP: {labels:?}"
    );
    // A hostname relay adds no IP label and never the relay's own name.
    let labels = node_labels("relay.example");
    assert!(
        !labels.iter().any(|label| label == "relay.example"),
        "the relay's own name is not a label for this node: {labels:?}"
    );
}
