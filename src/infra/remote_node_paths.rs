//! Unix-domain-socket paths for remote-node ingress. The only consumer is the
//! Unix-only owner-control-socket binding in `ratatui_node::runtime`, so the
//! module is empty on other platforms.

#![cfg(unix)]

use crate::cli::RemoteNetworkConfig;
use std::path::PathBuf;

pub fn remote_node_ingress_owner_socket_path(network: &RemoteNetworkConfig) -> PathBuf {
    std::env::temp_dir().join(format!(
        "waitagent-remote-node-ingress-{}.sock",
        sanitize_socket_component(&network.listener_addr().to_string())
    ))
}

fn sanitize_socket_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' => ch,
            _ => '_',
        })
        .collect()
}
