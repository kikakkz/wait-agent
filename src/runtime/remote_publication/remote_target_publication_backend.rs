// Legacy tmux-era publication backend abstraction kept during the ratatui migration; most items are currently unused.
#![allow(dead_code)]

use crate::cli::RemoteNetworkConfig;
use crate::domain::session_catalog::ManagedSessionRecord;
use crate::lifecycle::LifecycleError;
use std::path::Path;

/// Backend abstraction for `RemoteTargetPublicationRuntime`.
///
/// The publication runtime is responsible for the remote-target protocol:
/// accepting discovered remote sessions, signalling exits, refreshing local
/// views, and managing the bindings that publish local target hosts. This
/// trait hides the difference between the traditional tmux-backed deployment
/// (where every workspace is a tmux server and sidecars are spawned as
/// separate processes) and the single-process ratatui server model (where
/// sessions live in an in-memory catalog and snapshots are broadcast to TUI
/// clients).
pub trait RemoteTargetPublicationBackend: Clone + Send + Sync + 'static {
    /// Return the names of live local workspaces/sockets that the publication
    /// runtime should refresh when the remote catalog changes.
    fn live_workspace_socket_names(
        &self,
        network: &RemoteNetworkConfig,
    ) -> Result<Vec<String>, LifecycleError>;

    /// Return whether the given workspace/socket is currently live.
    fn socket_is_live(&self, socket_name: &str) -> bool;

    /// List all sessions belonging to a workspace/socket.
    fn list_sessions_on_socket(
        &self,
        socket_name: &str,
    ) -> Result<Vec<ManagedSessionRecord>, LifecycleError>;

    /// Find the publication binding for a specific target session, if any.
    fn find_publication_binding(
        &self,
        socket_name: &str,
        target_session_name: &str,
    ) -> Result<Option<RemoteTargetPublicationBinding>, LifecycleError>;

    /// List all publication bindings on a workspace/socket.
    fn list_publication_bindings(
        &self,
        socket_name: &str,
    ) -> Result<Vec<RemoteTargetPublicationBinding>, LifecycleError>;

    /// Publish a local target session as available to remote viewers.
    fn bind_publication(
        &self,
        socket_name: &str,
        target_session_name: &str,
        authority_id: &str,
        transport_session_id: &str,
        selector: Option<&str>,
    ) -> Result<(), LifecycleError>;

    /// Remove a publication binding.
    fn unbind_publication(
        &self,
        socket_name: &str,
        target_session_name: &str,
    ) -> Result<(), LifecycleError>;

    /// Return whether the session has a live content pane.
    fn live_content_pane_for_session(
        &self,
        socket_name: &str,
        session_name: &str,
    ) -> Result<bool, LifecycleError>;

    /// Install the publication hooks on a workspace/socket.
    fn ensure_publication_hooks(
        &self,
        socket_name: &str,
        network: &RemoteNetworkConfig,
    ) -> Result<(), LifecycleError>;

    /// Signal that a remote target has exited.
    fn signal_remote_target_exited(
        &self,
        socket_name: &str,
        session_name: &str,
        target: &str,
        executable: &Path,
    ) -> Result<(), LifecycleError>;

    /// Signal workspace chrome sessions that a remote target exited.
    fn signal_remote_target_exited_to_workspace(
        &self,
        socket_name: &str,
        target: &str,
        executable: &Path,
    ) -> Result<usize, LifecycleError>;

    /// Refresh the chrome UI for a workspace/socket.
    fn refresh_workspace_socket(
        &self,
        socket_name: &str,
        executable: &Path,
    ) -> Result<(), LifecycleError>;

    /// Ensure the publication server sidecar is running.
    fn ensure_publication_server_running(
        &self,
        socket_name: &str,
        network: &RemoteNetworkConfig,
        executable: &Path,
    ) -> Result<(), LifecycleError>;

    /// Ensure the publication agent sidecar is running.
    fn ensure_publication_agent_running(
        &self,
        socket_name: &str,
        network: &RemoteNetworkConfig,
        executable: &Path,
    ) -> Result<(), LifecycleError>;

    /// Ensure the publication sender sidecar is running.
    fn ensure_publication_sender_running(
        &self,
        socket_name: &str,
        network: &RemoteNetworkConfig,
        executable: &Path,
    ) -> Result<(), LifecycleError>;

    /// Ensure the publication owner sidecar is running.
    fn ensure_publication_owner_running(
        &self,
        socket_name: &str,
        target_session_name: &str,
        network: &RemoteNetworkConfig,
        executable: &Path,
    ) -> Result<(), LifecycleError>;

    /// Signal that a remote node is offline.
    fn signal_remote_node_offline(&self, node_id: &str) -> Result<(), LifecycleError>;

    /// Optional hook called after a discovered remote session has been upserted
    /// into the remote runtime owner. The ratatui backend uses this to keep the
    /// in-memory SharedState catalog in sync with published remote metadata such
    /// as current working directory and task state.
    fn on_remote_session_upserted(
        &self,
        _node_id: &str,
        _session: &ManagedSessionRecord,
    ) -> Result<(), LifecycleError> {
        Ok(())
    }
}

/// Binding that publishes a local target host session to remote viewers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteTargetPublicationBinding {
    pub socket_name: String,
    pub target_session_name: String,
    pub authority_id: String,
    pub transport_session_id: String,
    pub selector: Option<String>,
}
