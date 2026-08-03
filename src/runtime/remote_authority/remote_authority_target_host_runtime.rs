// Legacy tmux-era authority target-host runtime kept during the ratatui migration; most items are currently unused.
#![allow(dead_code)]

use crate::cli::RemoteNetworkConfig;
use crate::lifecycle::LifecycleError;
use crate::runtime::remote_target_publication_runtime::{
    signal_publication_sender_live_session_registered,
    signal_publication_sender_live_session_unregistered, RemoteTargetPublicationRuntime,
};
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

/// Gateway used by the remote-node session sync runtime to register/unregister
/// live authority-host sessions with the local publication runtime.
pub trait RemoteAuthorityPublicationGateway: Send + Sync + Clone + 'static {
    fn ensure_live_session_registered(
        &self,
        socket_name: &str,
        target_session_name: &str,
        authority_id: &str,
        target_id: &str,
        transport_socket_path: &str,
        authority_socket_path: &Path,
    ) -> Result<(), LifecycleError>;

    fn ensure_live_session_unregistered(
        &self,
        socket_name: &str,
        target_session_name: &str,
    ) -> Result<(), LifecycleError>;

    fn signal_source_session_closed(
        &self,
        socket_name: &str,
        target_session_name: &str,
    ) -> Result<(), LifecycleError>;

    fn signal_local_runtime_changed(&self, socket_name: &str) -> Result<(), LifecycleError>;
}

impl<B> RemoteAuthorityPublicationGateway for RemoteTargetPublicationRuntime<B>
where
    B: crate::runtime::remote_publication::remote_target_publication_backend::RemoteTargetPublicationBackend,
{
    fn ensure_live_session_registered(
        &self,
        socket_name: &str,
        target_session_name: &str,
        authority_id: &str,
        target_id: &str,
        transport_socket_path: &str,
        authority_socket_path: &Path,
    ) -> Result<(), LifecycleError> {
        self.ensure_publication_sender_running(socket_name)?;
        signal_publication_sender_live_session_registered(
            socket_name,
            target_session_name,
            authority_id,
            target_id,
            transport_socket_path,
        )?;
        wait_for_ready_socket(authority_socket_path)?;
        Ok(())
    }

    fn ensure_live_session_unregistered(
        &self,
        socket_name: &str,
        target_session_name: &str,
    ) -> Result<(), LifecycleError> {
        signal_publication_sender_live_session_unregistered(socket_name, target_session_name)
    }

    fn signal_source_session_closed(
        &self,
        socket_name: &str,
        target_session_name: &str,
    ) -> Result<(), LifecycleError> {
        self.signal_source_session_closed(socket_name, target_session_name)
    }

    fn signal_local_runtime_changed(&self, socket_name: &str) -> Result<(), LifecycleError> {
        self.signal_local_runtime_changed(socket_name)
    }
}

pub(crate) fn wait_for_ready_socket(socket_path: &Path) -> Result<(), LifecycleError> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if socket_path.exists() {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    Err(LifecycleError::Protocol(format!(
        "authority socket did not become ready at {}",
        socket_path.display()
    )))
}

fn stable_socket_hash(parts: &[&str]) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    for part in parts {
        part.hash(&mut hasher);
    }
    format!("{:x}", hasher.finish())
}

pub fn authority_output_ingest_socket_path(
    transport_socket_path: &str,
    target_id: &str,
) -> PathBuf {
    let hash = stable_socket_hash(&[transport_socket_path, target_id]);
    std::env::temp_dir().join(format!("waitagent-authority-output-{hash}.sock"))
}

pub fn authority_input_socket_path(transport_socket_path: &str, target_id: &str) -> PathBuf {
    let hash = stable_socket_hash(&[transport_socket_path, target_id]);
    std::env::temp_dir().join(format!("waitagent-authority-input-{hash}.sock"))
}

pub fn authority_event_socket_path(transport_socket_path: &str, target_id: &str) -> PathBuf {
    let hash = stable_socket_hash(&[transport_socket_path, target_id]);
    std::env::temp_dir().join(format!("waitagent-authority-event-{hash}.sock"))
}

/// Path for the per-session diagnostic file written when the target host exits.
pub fn authority_diag_path(transport_socket_path: &str, target_id: &str) -> PathBuf {
    let hash = stable_socket_hash(&[transport_socket_path, target_id]);
    std::env::temp_dir().join(format!("waitagent-diag-{hash}.diag"))
}

fn send_pane_died_event(event_socket_path: &str, pane_id: &str) {
    if let Ok(mut stream) = UnixStream::connect(event_socket_path) {
        let _ = stream.write_all(pane_id.as_bytes());
    }
}

#[allow(dead_code)]
fn remote_authority_target_host_args(
    _network: &RemoteNetworkConfig,
    _socket_name: &str,
    _session_name: &str,
    _target_id: &str,
) -> Vec<String> {
    // Preserved for ratatui remote path; tmux dependency to be removed in a later phase.
    Vec::new()
}

#[cfg(test)]
mod remote_authority_target_host_runtime_test {
    use super::*;

    #[test]
    fn authority_socket_paths_are_stable() {
        let path1 = authority_event_socket_path("/tmp/wa-test.sock", "target-1");
        let path2 = authority_event_socket_path("/tmp/wa-test.sock", "target-1");
        assert_eq!(path1, path2);
    }
}
