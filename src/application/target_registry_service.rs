use crate::cli::RemoteNetworkConfig;
use crate::domain::session_catalog::ManagedSessionRecord;
use crate::domain::session_catalog::SessionAvailability;
use crate::domain::session_catalog::SessionTransport;
use crate::infra::error_log::ERROR_LOG;
use crate::infra::tmux::TmuxSessionGateway;
use crate::infra::tmux::{EmbeddedTmuxBackend, TmuxError, TmuxSocketName};
use crate::runtime::network_state_runtime::recover_network_config_for_socket;
use crate::runtime::remote_runtime_owner_runtime::{
    RemoteRuntimeOwnerRuntime, RemoteRuntimeOwnerSnapshot,
};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const REMOTE_OWNER_SNAPSHOT_CACHE_TTL: Duration = Duration::from_millis(100);

/// In-memory cache for local tmux session snapshots.
///
/// This replaces `SessionCatalogSnapshotStore` and avoids writing session state
/// to disk. The store is meant to be shared by cloning it across the runtimes
/// that need a consistent view of the same local tmux socket.
#[derive(Debug, Clone, Default)]
pub struct SessionCatalogMemoryStore {
    snapshots: Arc<Mutex<HashMap<String, Option<Vec<ManagedSessionRecord>>>>>,
}

impl SessionCatalogMemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn load(&self, socket_name: &str) -> Option<Vec<ManagedSessionRecord>> {
        self.snapshots
            .lock()
            .expect("session catalog memory store mutex should not be poisoned")
            .get(socket_name)
            .cloned()
            .flatten()
    }

    pub fn store(&self, socket_name: &str, sessions: &[ManagedSessionRecord]) {
        self.snapshots
            .lock()
            .expect("session catalog memory store mutex should not be poisoned")
            .insert(socket_name.to_string(), Some(sessions.to_vec()));
    }

    pub fn remove(&self, socket_name: &str) {
        self.snapshots
            .lock()
            .expect("session catalog memory store mutex should not be poisoned")
            .remove(socket_name);
    }

    pub fn remove_target(&self, socket_name: &str, session_name: &str) {
        let mut guard = self
            .snapshots
            .lock()
            .expect("session catalog memory store mutex should not be poisoned");
        if let Some(Some(sessions)) = guard.get_mut(socket_name) {
            sessions.retain(|session| session.address.session_id() != session_name);
        }
    }
}

pub trait TargetCatalogGateway {
    type Error;

    fn list_targets(&self) -> Result<Vec<ManagedSessionRecord>, Self::Error>;
}

impl<G> TargetCatalogGateway for G
where
    G: TmuxSessionGateway,
{
    type Error = G::Error;

    fn list_targets(&self) -> Result<Vec<ManagedSessionRecord>, Self::Error> {
        self.list_sessions()
    }
}

#[derive(Debug, Clone)]
pub struct DefaultTargetCatalogGateway {
    local_tmux: EmbeddedTmuxBackend,
    remote_runtime_owner: RemoteRuntimeOwnerRuntime,
    current_socket_name: Option<String>,
    remote_snapshot_cache: Arc<Mutex<Option<CachedRemoteRuntimeOwnerSnapshot>>>,
    local_session_store: SessionCatalogMemoryStore,
}

#[derive(Debug, Clone)]
struct CachedRemoteRuntimeOwnerSnapshot {
    captured_at: Instant,
    snapshot: RemoteRuntimeOwnerSnapshot,
}

impl DefaultTargetCatalogGateway {
    #[cfg(test)]
    pub fn from_build_env() -> Result<Self, TmuxError> {
        Self::from_build_env_with_current_socket_name_and_network(
            current_tmux_socket_name_from_env(),
            None,
            SessionCatalogMemoryStore::new(),
        )
    }

    pub fn from_build_env_with_network(network: RemoteNetworkConfig) -> Result<Self, TmuxError> {
        Self::from_build_env_with_current_socket_name_and_network(
            current_tmux_socket_name_from_env(),
            Some(network),
            SessionCatalogMemoryStore::new(),
        )
    }

    pub fn from_build_env_with_network_and_store(
        network: RemoteNetworkConfig,
        local_session_store: SessionCatalogMemoryStore,
    ) -> Result<Self, TmuxError> {
        Self::from_build_env_with_current_socket_name_and_network(
            current_tmux_socket_name_from_env(),
            Some(network),
            local_session_store,
        )
    }

    pub fn from_build_env_with_socket_name(
        socket_name: impl Into<String>,
    ) -> Result<Self, TmuxError> {
        Self::from_build_env_with_current_socket_name_and_network(
            Some(socket_name.into()),
            None,
            SessionCatalogMemoryStore::new(),
        )
    }

    fn from_build_env_with_current_socket_name_and_network(
        current_socket_name: Option<String>,
        network: Option<RemoteNetworkConfig>,
        local_session_store: SessionCatalogMemoryStore,
    ) -> Result<Self, TmuxError> {
        let local_tmux = EmbeddedTmuxBackend::from_build_env()?;
        let network = network
            .or_else(|| {
                current_socket_name.as_deref().and_then(|socket_name| {
                    recover_network_config_for_socket(&local_tmux, socket_name)
                })
            })
            .unwrap_or_default();

        Ok(Self {
            local_tmux,
            remote_runtime_owner: RemoteRuntimeOwnerRuntime::from_build_env_with_network(network)
                .map_err(|error| {
                TmuxError::new(format!(
                    "failed to initialize remote runtime owner gateway: {error}"
                ))
            })?,
            current_socket_name,
            remote_snapshot_cache: Arc::new(Mutex::new(None)),
            local_session_store,
        })
    }

    pub fn with_fresh_local_tmux(self) -> Self {
        if let Some(socket_name) = self.current_socket_name.as_deref() {
            self.local_session_store.remove(socket_name);
        }
        self
    }

    pub fn clear_local_session_store(&self, socket_name: &str) {
        self.local_session_store.remove(socket_name);
    }

    pub fn remove_local_target(&self, socket_name: &str, session_name: &str) {
        self.local_session_store
            .remove_target(socket_name, session_name);
    }

    pub fn list_local_targets_on_authority(
        &self,
        authority_id: &str,
    ) -> Result<Vec<ManagedSessionRecord>, TmuxError> {
        Ok(self
            .local_targets_on_current_socket()?
            .into_iter()
            .filter(|target| target.address.authority_id() == authority_id)
            .collect())
    }

    pub fn resolve_local_target_on_authority_session(
        &self,
        authority_id: &str,
        transport_session_id: &str,
    ) -> Result<Option<ManagedSessionRecord>, TmuxError> {
        Ok(self
            .list_local_targets_on_authority(authority_id)?
            .into_iter()
            .find(|target| target.address.session_id() == transport_session_id))
    }

    pub fn list_local_workspace_chrome_targets_on_authority(
        &self,
        authority_id: &str,
    ) -> Result<Vec<ManagedSessionRecord>, TmuxError> {
        Ok(self
            .list_local_targets_on_authority(authority_id)?
            .into_iter()
            .filter(ManagedSessionRecord::is_workspace_chrome)
            .collect())
    }

    fn local_targets_on_current_socket(&self) -> Result<Vec<ManagedSessionRecord>, TmuxError> {
        if let Some(socket_name) = self.current_socket_name.as_deref() {
            self.local_targets_on_socket(socket_name)
        } else {
            self.local_tmux.list_sessions()
        }
    }

    fn local_targets_on_socket(
        &self,
        socket_name: &str,
    ) -> Result<Vec<ManagedSessionRecord>, TmuxError> {
        if let Some(cached) = self.local_session_store.load(socket_name) {
            let merged = self.merge_live_local_content_panes(socket_name, cached)?;
            ERROR_LOG.log(format!(
                "[diag-newhost] list_targets local_memory_cache current_socket={:?} sessions={}",
                self.current_socket_name,
                merged.len()
            ));
            return Ok(merged);
        }

        let session_backed = self
            .local_tmux
            .list_sessions_on_socket(&TmuxSocketName::new(socket_name))?;
        let pane_backed = self
            .local_tmux
            .list_local_target_content_pane_sessions(&TmuxSocketName::new(socket_name))?;
        let sessions = merge_local_targets_by_identity(session_backed, pane_backed);
        self.local_session_store.store(socket_name, &sessions);
        Ok(sessions)
    }

    fn merge_live_local_content_panes(
        &self,
        socket_name: &str,
        sessions: Vec<ManagedSessionRecord>,
    ) -> Result<Vec<ManagedSessionRecord>, TmuxError> {
        let pane_backed = self
            .local_tmux
            .list_local_target_content_pane_sessions(&TmuxSocketName::new(socket_name))?;
        let pane_backed_ids: HashSet<String> = pane_backed
            .iter()
            .map(|target| target.address.id().as_str().to_string())
            .collect();
        let mut merged = merge_local_targets_by_identity(sessions.clone(), pane_backed);
        // The cache may hold target-host sessions whose content panes have
        // already exited. Dropping them here ensures the sidebar does not keep
        // showing stale sessions when the explicit TargetExited event is lost
        // or delayed.
        merged.retain(|target| {
            !target.is_target_host() || pane_backed_ids.contains(target.address.id().as_str())
        });
        if merged != sessions {
            self.local_session_store.store(socket_name, &merged);
        }
        Ok(merged)
    }

    fn remote_snapshot(
        &self,
    ) -> Result<RemoteRuntimeOwnerSnapshot, crate::lifecycle::LifecycleError> {
        if let Some(snapshot) = self.cached_remote_snapshot() {
            ERROR_LOG.log(format!(
                "[diag-newhost] list_targets remote_snapshot_cache_hit current_socket={:?} sessions={}",
                self.current_socket_name,
                snapshot.sessions.len()
            ));
            return Ok(snapshot);
        }
        let snapshot = self.remote_runtime_owner.try_snapshot()?;
        let mut guard = self
            .remote_snapshot_cache
            .lock()
            .expect("remote owner snapshot cache mutex should not be poisoned");
        *guard = Some(CachedRemoteRuntimeOwnerSnapshot {
            captured_at: Instant::now(),
            snapshot: snapshot.clone(),
        });
        Ok(snapshot)
    }

    fn cached_remote_snapshot(&self) -> Option<RemoteRuntimeOwnerSnapshot> {
        let guard = self
            .remote_snapshot_cache
            .lock()
            .expect("remote owner snapshot cache mutex should not be poisoned");
        let cached = guard.as_ref()?;
        (cached.captured_at.elapsed() <= REMOTE_OWNER_SNAPSHOT_CACHE_TTL)
            .then(|| cached.snapshot.clone())
    }
}

impl TargetCatalogGateway for DefaultTargetCatalogGateway {
    type Error = TmuxError;

    fn list_targets(&self) -> Result<Vec<ManagedSessionRecord>, Self::Error> {
        let t_list = std::time::Instant::now();
        let remote_sessions = self
            .remote_snapshot()
            .map(|snapshot| {
                ERROR_LOG.log(format!(
                    "[diag-native] list_targets: remote_snapshot_sessions={}",
                    snapshot.sessions.len()
                ));
                ERROR_LOG.log(format!(
                    "[diag-newhost] list_targets remote_snapshot current_socket={:?} sessions={} elapsed={:?}",
                    self.current_socket_name,
                    snapshot.sessions.len(),
                    t_list.elapsed()
                ));
                snapshot.sessions
            })
            .unwrap_or_else(|error| {
                ERROR_LOG.log(format!(
                    "[diag] list_targets: remote snapshot failed: {error}"
                ));
                ERROR_LOG.log(format!(
                    "[diag-newhost] list_targets remote_snapshot_failed current_socket={:?} elapsed={:?}",
                    self.current_socket_name,
                    t_list.elapsed()
                ));
                Vec::new()
            });
        let t_local = std::time::Instant::now();
        let local_sessions = self.local_targets_on_current_socket()?;
        ERROR_LOG.log(format!(
            "[diag-newhost] list_targets local_tmux current_socket={:?} sessions={} elapsed={:?} total={:?}",
            self.current_socket_name,
            local_sessions.len(),
            t_local.elapsed(),
            t_list.elapsed()
        ));
        let merged = merge_targets_by_identity([local_sessions, remote_sessions]);
        ERROR_LOG.log(format!(
            "[diag-newhost] list_targets merged current_socket={:?} sessions={} total={:?}",
            self.current_socket_name,
            merged.len(),
            t_list.elapsed()
        ));
        Ok(merged)
    }
}

fn merge_targets_by_identity(groups: [Vec<ManagedSessionRecord>; 2]) -> Vec<ManagedSessionRecord> {
    let mut merged = Vec::new();
    let mut positions = HashMap::<String, usize>::new();
    for targets in groups {
        for target in targets {
            let target_id = target.address.id().as_str().to_string();
            if let Some(index) = positions.get(&target_id).copied() {
                merged[index] = target;
            } else {
                positions.insert(target_id, merged.len());
                merged.push(target);
            }
        }
    }
    merged
}

pub(crate) fn merge_local_targets_by_identity(
    session_backed: Vec<ManagedSessionRecord>,
    pane_backed: Vec<ManagedSessionRecord>,
) -> Vec<ManagedSessionRecord> {
    let mut merged = Vec::new();
    let mut positions = HashMap::<String, usize>::new();
    for target in session_backed {
        let target_id = target.address.id().as_str().to_string();
        positions.insert(target_id, merged.len());
        merged.push(target);
    }
    for target in pane_backed {
        let target_id = target.address.id().as_str().to_string();
        if let Some(index) = positions.get(&target_id).copied() {
            if !merged[index].is_target_host() {
                continue;
            }
            merged[index].command_name = target.command_name;
            merged[index].display_command_name = target.display_command_name;
            merged[index].current_path = target.current_path;
            merged[index].task_state = target.task_state;
            merged[index].availability = target.availability;
            merged[index].selector = target.selector;
        } else {
            positions.insert(target_id, merged.len());
            merged.push(target);
        }
    }
    merged
}

pub struct TargetRegistryService<G> {
    gateway: G,
}

impl<G> Clone for TargetRegistryService<G>
where
    G: Clone,
{
    fn clone(&self) -> Self {
        Self {
            gateway: self.gateway.clone(),
        }
    }
}

impl<G> TargetRegistryService<G>
where
    G: TargetCatalogGateway,
{
    pub fn new(gateway: G) -> Self {
        Self { gateway }
    }

    pub fn list_targets(&self) -> Result<Vec<ManagedSessionRecord>, G::Error> {
        self.gateway.list_targets()
    }

    pub fn list_targets_on_authority(
        &self,
        authority_id: &str,
    ) -> Result<Vec<ManagedSessionRecord>, G::Error> {
        Ok(self
            .list_targets()?
            .into_iter()
            .filter(|target| target.address.authority_id() == authority_id)
            .collect())
    }

    pub fn list_workspace_chrome_targets(&self) -> Result<Vec<ManagedSessionRecord>, G::Error> {
        Ok(self
            .list_targets()?
            .into_iter()
            .filter(ManagedSessionRecord::is_workspace_chrome)
            .collect())
    }

    #[allow(dead_code)]
    pub fn list_workspace_chrome_targets_on_authority(
        &self,
        authority_id: &str,
    ) -> Result<Vec<ManagedSessionRecord>, G::Error> {
        Ok(self
            .list_targets_on_authority(authority_id)?
            .into_iter()
            .filter(ManagedSessionRecord::is_workspace_chrome)
            .collect())
    }

    pub fn list_activation_targets(&self) -> Result<Vec<ManagedSessionRecord>, G::Error> {
        let mut targets = self
            .list_targets()?
            .into_iter()
            .filter(is_activation_target)
            .collect::<Vec<_>>();
        sort_targets_for_display(&mut targets);
        Ok(targets)
    }

    pub fn find_target(&self, value: &str) -> Result<Option<ManagedSessionRecord>, G::Error> {
        Ok(self
            .list_targets()?
            .into_iter()
            .find(|target| target.matches_target(value)))
    }

    pub fn find_activation_target(
        &self,
        value: &str,
    ) -> Result<Option<ManagedSessionRecord>, G::Error> {
        Ok(self
            .list_activation_targets()?
            .into_iter()
            .find(|target| target.matches_target(value)))
    }

    pub fn find_target_on_authority(
        &self,
        authority_id: &str,
        value: &str,
    ) -> Result<Option<ManagedSessionRecord>, G::Error> {
        Ok(self
            .list_targets_on_authority(authority_id)?
            .into_iter()
            .find(|target| target.matches_target(value)))
    }

    pub fn resolve_target_on_authority_session(
        &self,
        authority_id: &str,
        transport_session_id: &str,
    ) -> Result<Option<ManagedSessionRecord>, G::Error> {
        Ok(self
            .list_targets_on_authority(authority_id)?
            .into_iter()
            .find(|target| target.address.session_id() == transport_session_id))
    }

    pub fn visible_targets_in_workspace(
        &self,
        authority_id: &str,
        workspace_session_id: &str,
        active_target: Option<&str>,
    ) -> Result<Vec<ManagedSessionRecord>, G::Error> {
        let t_visible = std::time::Instant::now();
        let targets = self.list_targets()?;
        let visible =
            project_visible_targets(&targets, authority_id, workspace_session_id, active_target);
        ERROR_LOG.log(format!(
            "[diag-newhost] visible_targets authority={} workspace={} active={:?} targets={} visible={} elapsed={:?}",
            authority_id,
            workspace_session_id,
            active_target,
            targets.len(),
            visible.len(),
            t_visible.elapsed()
        ));
        Ok(visible)
    }
}

impl TargetRegistryService<DefaultTargetCatalogGateway> {
    pub fn resolve_local_target_on_authority_session(
        &self,
        authority_id: &str,
        transport_session_id: &str,
    ) -> Result<Option<ManagedSessionRecord>, TmuxError> {
        self.gateway
            .resolve_local_target_on_authority_session(authority_id, transport_session_id)
    }

    pub fn list_local_workspace_chrome_targets_on_authority(
        &self,
        authority_id: &str,
    ) -> Result<Vec<ManagedSessionRecord>, TmuxError> {
        self.gateway
            .list_local_workspace_chrome_targets_on_authority(authority_id)
    }

    pub fn clear_local_session_store(&self, socket_name: &str) {
        self.gateway.clear_local_session_store(socket_name);
    }

    pub fn remove_local_target(&self, socket_name: &str, session_name: &str) {
        self.gateway.remove_local_target(socket_name, session_name);
    }
}

#[cfg(test)]
impl DefaultTargetCatalogGateway {
    fn remote_runtime_owner_network(&self) -> crate::cli::RemoteNetworkConfig {
        self.remote_runtime_owner.network_config_for_tests()
    }
}

pub fn project_visible_targets(
    targets: &[ManagedSessionRecord],
    authority_id: &str,
    workspace_session_id: &str,
    _active_target: Option<&str>,
) -> Vec<ManagedSessionRecord> {
    let workspace_runtime = targets
        .iter()
        .find(|target| {
            target.address.transport() == &SessionTransport::LocalTmux
                && target.address.authority_id() == authority_id
                && target.address.session_id() == workspace_session_id
        })
        .cloned();
    let mut visible_targets = targets
        .iter()
        .filter(|target| {
            target.availability != SessionAvailability::Exited
                && ((target.address.transport() == &SessionTransport::LocalTmux
                    && target.address.authority_id() == authority_id
                    && target.is_target_host())
                    || target.address.transport() == &SessionTransport::RemotePeer)
        })
        .cloned()
        .collect::<Vec<_>>();

    if visible_targets.is_empty() {
        return workspace_runtime.into_iter().collect();
    }

    sort_targets_for_display(&mut visible_targets);
    visible_targets
}

pub fn is_activation_target(target: &ManagedSessionRecord) -> bool {
    target.availability != SessionAvailability::Exited
        && ((target.address.transport() == &SessionTransport::LocalTmux && target.is_target_host())
            || target.address.transport() == &SessionTransport::RemotePeer)
}

fn sort_targets_for_display(targets: &mut [ManagedSessionRecord]) {
    targets.sort_by(|left, right| {
        transport_sort_key(left)
            .cmp(&transport_sort_key(right))
            .then_with(|| {
                left.address
                    .authority_id()
                    .cmp(right.address.authority_id())
            })
            .then_with(|| left.address.session_id().cmp(right.address.session_id()))
            .then_with(|| {
                left.command_name
                    .as_deref()
                    .unwrap_or("bash")
                    .cmp(right.command_name.as_deref().unwrap_or("bash"))
            })
    });
}

fn transport_sort_key(target: &ManagedSessionRecord) -> u8 {
    match target.address.transport() {
        SessionTransport::LocalTmux => 0,
        SessionTransport::RemotePeer => 1,
    }
}

fn current_tmux_socket_name_from_env() -> Option<String> {
    let tmux = std::env::var("TMUX").ok()?;
    let socket_path = tmux.split(',').next()?.trim();
    if socket_path.is_empty() {
        return None;
    }
    Path::new(socket_path)
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::{
        is_activation_target, project_visible_targets, DefaultTargetCatalogGateway,
        SessionCatalogMemoryStore, TargetCatalogGateway, TargetRegistryService,
    };
    use crate::domain::session_catalog::{
        ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState, SessionAvailability,
    };
    use crate::domain::workspace::WorkspaceSessionRole;
    use crate::infra::tmux::{TmuxGateway, TmuxSessionGateway};
    use std::path::PathBuf;

    #[derive(Clone)]
    struct FakeGateway {
        targets: Vec<ManagedSessionRecord>,
    }

    impl TargetCatalogGateway for FakeGateway {
        type Error = &'static str;

        fn list_targets(&self) -> Result<Vec<ManagedSessionRecord>, Self::Error> {
            Ok(self.targets.clone())
        }
    }

    #[test]
    fn registry_finds_targets_by_compatibility_selector() {
        let registry = TargetRegistryService::new(FakeGateway {
            targets: vec![session(
                "wa-1",
                "waitagent-1",
                "codex",
                WorkspaceSessionRole::TargetHost,
            )],
        });

        let target = registry
            .find_target("wa-1:waitagent-1")
            .expect("lookup should succeed")
            .expect("target should exist");

        assert_eq!(target.address.authority_id(), "wa-1");
    }

    #[test]
    fn registry_scopes_targets_by_authority() {
        let registry = TargetRegistryService::new(FakeGateway {
            targets: vec![
                session(
                    "wa-1",
                    "workspace",
                    "bash",
                    WorkspaceSessionRole::WorkspaceChrome,
                ),
                session(
                    "wa-2",
                    "workspace",
                    "bash",
                    WorkspaceSessionRole::WorkspaceChrome,
                ),
            ],
        });

        let targets = registry
            .list_targets_on_authority("wa-1")
            .expect("authority-scoped listing should succeed");

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].address.authority_id(), "wa-1");
    }

    #[test]
    fn registry_scopes_workspace_chrome_targets_by_authority() {
        let registry = TargetRegistryService::new(FakeGateway {
            targets: vec![
                session(
                    "wa-1",
                    "workspace-1",
                    "bash",
                    WorkspaceSessionRole::WorkspaceChrome,
                ),
                session("wa-1", "target-1", "bash", WorkspaceSessionRole::TargetHost),
                session(
                    "wa-2",
                    "workspace-2",
                    "bash",
                    WorkspaceSessionRole::WorkspaceChrome,
                ),
            ],
        });

        let targets = registry
            .list_workspace_chrome_targets_on_authority("wa-1")
            .expect("authority-scoped workspace chrome listing should succeed");

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].address.authority_id(), "wa-1");
        assert_eq!(targets[0].address.session_id(), "workspace-1");
    }

    #[test]
    fn registry_lists_only_activation_targets() {
        let registry = TargetRegistryService::new(FakeGateway {
            targets: vec![
                session(
                    "wa-1",
                    "workspace",
                    "bash",
                    WorkspaceSessionRole::WorkspaceChrome,
                ),
                session(
                    "wa-1",
                    "target-1",
                    "codex",
                    WorkspaceSessionRole::TargetHost,
                ),
                remote_session("peer-a", "shell-1", "bash"),
            ],
        });

        let targets = registry
            .list_activation_targets()
            .expect("activation targets should list successfully");

        assert_eq!(targets.len(), 2);
        assert!(targets.iter().all(is_activation_target));
        assert_eq!(targets[0].address.qualified_target(), "wa-1:target-1");
        assert_eq!(targets[1].address.qualified_target(), "peer-a:shell-1");
    }

    #[test]
    fn registry_finds_only_activation_targets() {
        let registry = TargetRegistryService::new(FakeGateway {
            targets: vec![
                session(
                    "wa-1",
                    "workspace",
                    "bash",
                    WorkspaceSessionRole::WorkspaceChrome,
                ),
                remote_session("peer-a", "shell-1", "codex"),
            ],
        });

        assert!(registry
            .find_activation_target("wa-1:workspace")
            .expect("activation lookup should succeed")
            .is_none());

        let target = registry
            .find_activation_target("peer-a:shell-1")
            .expect("activation lookup should succeed")
            .expect("remote activation target should exist");

        assert_eq!(target.address.qualified_target(), "peer-a:shell-1");
    }

    #[test]
    fn visible_targets_keep_target_hosts_visible_until_one_is_active() {
        let targets = project_visible_targets(
            &[
                session(
                    "wa-1",
                    "workspace",
                    "codex",
                    WorkspaceSessionRole::WorkspaceChrome,
                ),
                session("wa-1", "target-1", "bash", WorkspaceSessionRole::TargetHost),
            ],
            "wa-1",
            "workspace",
            None,
        );

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].address.session_id(), "target-1");
    }

    #[test]
    fn visible_targets_keep_active_target_runtime_owned_by_target_session() {
        let targets = project_visible_targets(
            &[
                ManagedSessionRecord {
                    task_state: ManagedSessionTaskState::Running,
                    ..session(
                        "wa-1",
                        "workspace",
                        "codex",
                        WorkspaceSessionRole::WorkspaceChrome,
                    )
                },
                ManagedSessionRecord {
                    task_state: ManagedSessionTaskState::Confirm,
                    ..session("wa-1", "target-1", "bash", WorkspaceSessionRole::TargetHost)
                },
                ManagedSessionRecord {
                    task_state: ManagedSessionTaskState::Input,
                    ..session(
                        "wa-1",
                        "target-2",
                        "codex",
                        WorkspaceSessionRole::TargetHost,
                    )
                },
            ],
            "wa-1",
            "workspace",
            Some("wa-1:target-2"),
        );

        assert_eq!(targets.len(), 2);
        assert_eq!(targets[1].address.session_id(), "target-2");
        assert_eq!(targets[1].command_name.as_deref(), Some("codex"));
        assert_eq!(targets[1].task_state, ManagedSessionTaskState::Input);
    }

    #[test]
    fn visible_targets_preserve_confirm_state_when_workspace_is_running() {
        let targets = project_visible_targets(
            &[
                ManagedSessionRecord {
                    task_state: ManagedSessionTaskState::Running,
                    ..session(
                        "wa-1",
                        "workspace",
                        "codex",
                        WorkspaceSessionRole::WorkspaceChrome,
                    )
                },
                ManagedSessionRecord {
                    task_state: ManagedSessionTaskState::Confirm,
                    ..session(
                        "wa-1",
                        "target-1",
                        "codex",
                        WorkspaceSessionRole::TargetHost,
                    )
                },
                session("wa-1", "target-2", "bash", WorkspaceSessionRole::TargetHost),
            ],
            "wa-1",
            "workspace",
            Some("wa-1:target-1"),
        );

        let active = targets
            .iter()
            .find(|target| target.address.session_id() == "target-1")
            .expect("active target should remain visible");
        assert_eq!(active.command_name.as_deref(), Some("codex"));
        assert_eq!(active.task_state, ManagedSessionTaskState::Confirm);
    }

    #[test]
    fn visible_targets_preserve_input_state_when_workspace_is_running() {
        let targets = project_visible_targets(
            &[
                ManagedSessionRecord {
                    task_state: ManagedSessionTaskState::Running,
                    ..session(
                        "wa-1",
                        "workspace",
                        "codex",
                        WorkspaceSessionRole::WorkspaceChrome,
                    )
                },
                ManagedSessionRecord {
                    task_state: ManagedSessionTaskState::Input,
                    ..session(
                        "wa-1",
                        "target-1",
                        "codex",
                        WorkspaceSessionRole::TargetHost,
                    )
                },
            ],
            "wa-1",
            "workspace",
            Some("wa-1:target-1"),
        );

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].command_name.as_deref(), Some("codex"));
        assert_eq!(targets[0].task_state, ManagedSessionTaskState::Input);
    }

    #[test]
    fn visible_targets_use_workspace_runtime_only_when_no_target_hosts_exist() {
        let targets = project_visible_targets(
            &[session(
                "wa-1",
                "workspace",
                "codex",
                WorkspaceSessionRole::WorkspaceChrome,
            )],
            "wa-1",
            "workspace",
            None,
        );

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].address.session_id(), "workspace");
        assert_eq!(targets[0].command_name.as_deref(), Some("codex"));
    }

    #[test]
    fn visible_targets_include_remote_peers_alongside_local_target_hosts() {
        let targets = project_visible_targets(
            &[
                session(
                    "wa-1",
                    "workspace",
                    "codex",
                    WorkspaceSessionRole::WorkspaceChrome,
                ),
                session("wa-1", "target-1", "bash", WorkspaceSessionRole::TargetHost),
                remote_session("peer-a", "shell-1", "codex"),
            ],
            "wa-1",
            "workspace",
            None,
        );

        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].address.session_id(), "target-1");
        assert_eq!(targets[1].address.authority_id(), "peer-a");
    }

    #[test]
    fn visible_targets_do_not_overlay_workspace_runtime_onto_remote_target() {
        let targets = project_visible_targets(
            &[
                session(
                    "wa-1",
                    "workspace",
                    "local-main",
                    WorkspaceSessionRole::WorkspaceChrome,
                ),
                session("wa-1", "target-1", "bash", WorkspaceSessionRole::TargetHost),
                remote_session("peer-a", "shell-1", "remote-codex"),
            ],
            "wa-1",
            "workspace",
            Some("peer-a:shell-1"),
        );

        assert_eq!(targets[1].command_name.as_deref(), Some("remote-codex"));
    }

    #[test]
    fn activation_targets_sort_remote_sessions_by_node_then_session() {
        let registry = TargetRegistryService::new(FakeGateway {
            targets: vec![
                remote_session("peer-b", "shell-2", "bash"),
                remote_session("peer-a", "shell-3", "bash"),
                remote_session("peer-a", "shell-1", "codex"),
            ],
        });

        let targets = registry
            .list_activation_targets()
            .expect("activation targets should list successfully");

        assert_eq!(targets[0].address.qualified_target(), "peer-a:shell-1");
        assert_eq!(targets[1].address.qualified_target(), "peer-a:shell-3");
        assert_eq!(targets[2].address.qualified_target(), "peer-b:shell-2");
    }

    fn session(
        authority_id: &str,
        session_id: &str,
        command: &str,
        role: WorkspaceSessionRole,
    ) -> ManagedSessionRecord {
        ManagedSessionRecord {
            address: ManagedSessionAddress::local_tmux(authority_id, session_id),
            selector: Some(format!("{authority_id}:{session_id}")),
            availability: SessionAvailability::Online,
            workspace_dir: Some(PathBuf::from("/tmp/demo")),
            workspace_key: None,
            session_role: Some(role),
            opened_by: Vec::new(),
            attached_clients: 1,
            window_count: 1,
            command_name: Some(command.to_string()),
            display_command_name: None,
            current_path: Some(PathBuf::from("/tmp/demo")),
            task_state: ManagedSessionTaskState::Input,
        }
    }

    #[test]
    fn session_catalog_memory_store_removes_target_for_socket() {
        let store = SessionCatalogMemoryStore::new();
        let socket_name = "wa-1";
        let sessions = vec![
            session(
                "wa-1",
                "workspace",
                "bash",
                WorkspaceSessionRole::WorkspaceChrome,
            ),
            session("wa-1", "target-1", "bash", WorkspaceSessionRole::TargetHost),
            session("wa-1", "target-2", "bash", WorkspaceSessionRole::TargetHost),
        ];
        store.store(socket_name, &sessions);

        store.remove_target(socket_name, "target-1");

        let remaining = store
            .load(socket_name)
            .expect("socket should still have sessions");
        assert_eq!(remaining.len(), 2);
        assert!(
            remaining
                .iter()
                .all(|s| s.address.session_id() != "target-1"),
            "removed target should no longer be in cache"
        );
        assert!(
            remaining
                .iter()
                .any(|s| s.address.session_id() == "workspace"),
            "workspace session should remain"
        );
        assert!(
            remaining
                .iter()
                .any(|s| s.address.session_id() == "target-2"),
            "other target should remain"
        );
    }

    #[test]
    fn session_catalog_memory_store_remove_target_is_noop_for_unknown_socket() {
        let store = SessionCatalogMemoryStore::new();
        store.remove_target("wa-unknown", "target-1");
        assert!(store.load("wa-unknown").is_none());
    }

    #[test]
    fn catalog_gateway_uses_socket_scoped_network_config_for_remote_snapshot() {
        let _guard = crate::test_support::integration_test_lock();
        let backend = crate::infra::tmux::EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("time should be monotonic")
            .as_nanos();
        let socket_name = format!("wa-test-target-catalog-network-{nonce:x}");
        let network = crate::cli::RemoteNetworkConfig {
            port: 17575,
            connect: Some("127.0.0.1:7575".to_string()),
            node_id: None,
            public_endpoint: None,
        };

        backend
            .ensure_workspace(&crate::domain::workspace::WorkspaceInstanceConfig {
                workspace_dir: std::env::temp_dir(),
                workspace_key: format!("target-catalog-network-{nonce:x}"),
                socket_name: socket_name.clone(),
                session_name: format!("waitagent-test-target-catalog-network-{nonce:x}"),
                session_role: crate::domain::workspace::WorkspaceSessionRole::WorkspaceChrome,
                initial_rows: None,
                initial_cols: None,
                initial_program: None,
            })
            .expect("workspace should be created");
        crate::runtime::network_state_runtime::persist_socket_network_config(
            &backend,
            &socket_name,
            &network,
        )
        .expect("network config should persist on socket");

        let gateway =
            DefaultTargetCatalogGateway::from_build_env_with_socket_name(socket_name.clone())
                .expect("target catalog should build");

        assert_eq!(gateway.remote_runtime_owner_network(), network);
        let _ = backend.kill_server(&crate::infra::tmux::TmuxSocketName::new(socket_name));
    }
    fn remote_session(authority_id: &str, session_id: &str, command: &str) -> ManagedSessionRecord {
        ManagedSessionRecord {
            address: ManagedSessionAddress::remote_peer(authority_id, session_id),
            selector: Some(format!("wa-{authority_id}:{session_id}")),
            availability: SessionAvailability::Online,
            workspace_dir: None,
            workspace_key: None,
            session_role: Some(WorkspaceSessionRole::TargetHost),
            opened_by: Vec::new(),
            attached_clients: 0,
            window_count: 1,
            command_name: Some(command.to_string()),
            display_command_name: None,
            current_path: None,
            task_state: ManagedSessionTaskState::Running,
        }
    }
}
