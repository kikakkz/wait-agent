//! `relay join` end-to-end tests: the node-side client enrolls against the
//! test relay through the real admin invite → enrollment listener flow and
//! pins the relay identity into relay.toml. These tests share a
//! single-thread runtime with the relay: no blocking std IO here.

use super::admin::admin_request;
use super::*;

use std::path::Path;

use crate::infra::relay_join::{join_relay, RelayJoinError};
use crate::infra::relay_toml_store::RelayTomlConfig;

fn join_paths(dir: &Path) -> (NodeCredentialPaths, PathBuf) {
    (
        NodeCredentialPaths {
            key_path: dir.join("node.key"),
            cert_path: dir.join("node.crt"),
        },
        dir.join("relay.toml"),
    )
}

async fn invite(admin_addr: &crate::platform::remote_ipc::RemoteControlAddr) -> String {
    let body = admin_request(admin_addr, r#"{"command":"invite"}"#).await;
    let response: serde_json::Value = serde_json::from_str(&body).expect("invite json");
    assert_eq!(response["ok"], true, "invite should succeed: {body}");
    response["message"]
        .as_str()
        .expect("invite message")
        .lines()
        .find_map(|line| line.strip_prefix("token: "))
        .expect("invite message carries the token")
        .to_string()
}

#[tokio::test]
async fn join_enrolls_writes_relay_toml_and_registers_on_the_main_listener() {
    let node = TestNode::generate();
    let mut server = start_test_server(&[]).await;
    let server_der = server_cert_der(&server.config);
    let relay_fingerprint = node_credentials::cert_fingerprint_from_der(&server_der)
        .expect("relay fingerprint should compute");
    let admin_addr = server.server.admin_addr().clone();
    let token = invite(&admin_addr).await;

    let dir = temp_dir("join");
    let (credentials, toml_path) = join_paths(&dir);
    node.write_pem_files(&credentials);

    let address = format!("127.0.0.1:{}", server.server.local_addr().port());
    let outcome = join_relay(&address, &token, &credentials, &toml_path, None)
        .await
        .expect("join should succeed");
    assert_eq!(outcome.relay_fingerprint, relay_fingerprint);
    assert_eq!(outcome.toml_path, toml_path);

    // relay.toml pins the address (explicit port) and the fingerprint.
    let pinned = RelayTomlConfig::load(&toml_path)
        .expect("relay.toml should load")
        .expect("relay.toml should exist");
    assert_eq!(pinned.address, address);
    assert_eq!(pinned.relay_fingerprint, relay_fingerprint);
    assert!(
        !pinned.use_proxy,
        "a direct join pins no proxy marker (issue #206)"
    );

    // The node was whitelisted by the enrollment session.
    assert!(
        server
            .config
            .authorized_nodes_dir
            .join(node.fingerprint())
            .is_file(),
        "join must whitelist the node certificate"
    );

    // And the same identity registers on the main listener afterwards.
    let mut link = register_node(&mut server, &node, &server_der).await;
    active_connections_reaches(&server.server, 1).await;
    write_frame(&mut link, &Frame::Unregister)
        .await
        .expect("unregister should write");
    server.server.shutdown().await;
}

#[tokio::test]
async fn join_with_unknown_token_maps_to_token_invalid() {
    let node = TestNode::generate();
    let server = start_test_server(&[]).await;

    let dir = temp_dir("join-bad-token");
    let (credentials, toml_path) = join_paths(&dir);
    node.write_pem_files(&credentials);

    let address = format!("127.0.0.1:{}", server.server.local_addr().port());
    let error = join_relay(&address, "bogus-token", &credentials, &toml_path, None)
        .await
        .expect_err("a bogus token must fail the join");
    assert!(
        matches!(error, RelayJoinError::TokenInvalid),
        "expected TokenInvalid, got {error:?}"
    );
    assert!(
        !toml_path.exists(),
        "a failed join must not write relay.toml"
    );
    server.server.shutdown().await;
}

#[tokio::test]
async fn join_with_a_reused_one_time_token_maps_to_token_invalid() {
    let first_node = TestNode::generate();
    let second_node = TestNode::generate();
    let server = start_test_server(&[]).await;
    let admin_addr = server.server.admin_addr().clone();
    let token = invite(&admin_addr).await;

    let first_dir = temp_dir("join-reuse-a");
    let (first_credentials, first_toml) = join_paths(&first_dir);
    first_node.write_pem_files(&first_credentials);
    let address = format!("127.0.0.1:{}", server.server.local_addr().port());
    join_relay(&address, &token, &first_credentials, &first_toml, None)
        .await
        .expect("first join should succeed");

    let second_dir = temp_dir("join-reuse-b");
    let (second_credentials, second_toml) = join_paths(&second_dir);
    second_node.write_pem_files(&second_credentials);
    let error = join_relay(&address, &token, &second_credentials, &second_toml, None)
        .await
        .expect_err("a reused one-time token must fail the join");
    assert!(
        matches!(error, RelayJoinError::TokenInvalid),
        "expected TokenInvalid, got {error:?}"
    );
    server.server.shutdown().await;
}

/// Issue #206: a proxied enrollment dials through the SOCKS5 handshake and
/// the written pin carries the proxy marker for the persistent link.
#[tokio::test]
async fn join_through_a_socks5_proxy_pins_the_proxy_marker() {
    use tokio::io::AsyncWriteExt;

    let node = TestNode::generate();
    let server = start_test_server(&[]).await;
    let server_der = server_cert_der(&server.config);
    let relay_fingerprint =
        node_credentials::cert_fingerprint_from_der(&server_der).expect("relay fingerprint");
    let admin_addr = server.server.admin_addr().clone();
    let token = invite(&admin_addr).await;

    // SOCKS5 no-auth forwarder on loopback: handshake, connect upstream,
    // then pipe both directions to the real relay enrollment listener.
    let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("proxy bind");
    let proxy_addr = proxy_listener.local_addr().expect("proxy addr");
    let forwarder = tokio::spawn(async move {
        let (mut stream, _) = proxy_listener.accept().await.expect("proxy accept");
        let mut greeting = [0_u8; 3];
        stream.read_exact(&mut greeting).await.expect("greeting");
        assert_eq!(greeting, [0x05, 0x01, 0x00], "VER5/NMETHODS1/NOAUTH");
        stream.write_all(&[0x05, 0x00]).await.expect("method reply");
        let mut head = [0_u8; 5];
        stream.read_exact(&mut head).await.expect("request head");
        assert_eq!(head[3], 0x03, "ATYP=domainname (remote resolve)");
        let len = usize::from(head[4]);
        let mut body = vec![0_u8; len + 2];
        stream.read_exact(&mut body).await.expect("request body");
        let port = u16::from_be_bytes([body[len], body[len + 1]]);
        let host = String::from_utf8_lossy(&body[..len]).to_string();
        let mut upstream = tokio::net::TcpStream::connect((host.as_str(), port))
            .await
            .expect("mock connects upstream");
        stream
            .write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
            .await
            .expect("success reply");
        tokio::io::copy_bidirectional(&mut stream, &mut upstream)
            .await
            .expect("pipe");
    });

    let dir = temp_dir("join-via-proxy");
    let (credentials, toml_path) = join_paths(&dir);
    node.write_pem_files(&credentials);

    let proxy = crate::infra::proxy_stream::SshProxy {
        scheme: crate::infra::proxy_stream::SshProxyScheme::Socks5,
        host: "127.0.0.1".to_string(),
        port: proxy_addr.port(),
    };
    let address = format!("127.0.0.1:{}", server.server.local_addr().port());
    let outcome = join_relay(&address, &token, &credentials, &toml_path, Some(proxy))
        .await
        .expect("the proxied join should succeed");
    assert_eq!(outcome.relay_fingerprint, relay_fingerprint);

    let pinned = RelayTomlConfig::load(&toml_path)
        .expect("relay.toml should load")
        .expect("relay.toml should exist");
    assert!(
        pinned.use_proxy,
        "a proxied join pins the proxy marker so the link reuses it"
    );
    assert!(
        server
            .config
            .authorized_nodes_dir
            .join(node.fingerprint())
            .is_file(),
        "the proxied join must still whitelist the node certificate"
    );
    server.server.shutdown().await;
    forwarder.abort();
}
