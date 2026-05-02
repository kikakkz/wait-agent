use crate::domain::session_catalog::ManagedSessionRecord;
use crate::domain::session_catalog::SessionTransport;
use crate::infra::tmux::TmuxSessionGateway;
use crate::infra::tmux::{EmbeddedTmuxBackend, TmuxError};
use crate::runtime::remote_runtime_owner_runtime::RemoteRuntimeOwnerRuntime;
use std::collections::HashMap;
use std::path::Path;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultTargetCatalogGateway {
    local_tmux: EmbeddedTmuxBackend,
    remote_runtime_owner: RemoteRuntimeOwnerRuntime,
    current_socket_name: Option<String>,
}

impl DefaultTargetCatalogGateway {
    pub fn from_build_env() -> Result<Self, TmuxError> {
        Self::from_build_env_with_current_socket_name(current_tmux_socket_name_from_env())
    }

    pub fn from_build_env_with_socket_name(
        socket_name: impl Into<String>,
    ) -> Result<Self, TmuxError> {
        Self::from_build_env_with_current_socket_name(Some(socket_name.into()))
    }

    fn from_build_env_with_current_socket_name(
        current_socket_name: Option<String>,
    ) -> Result<Self, TmuxError> {
        Ok(Self {
            local_tmux: EmbeddedTmuxBackend::from_build_env()?,
            remote_runtime_owner: RemoteRuntimeOwnerRuntime::from_build_env().map_err(|error| {
                TmuxError::new(format!(
                    "failed to initialize remote runtime owner gateway: {error}"
                ))
            })?,
            current_socket_name,
        })
    }
}

impl TargetCatalogGateway for DefaultTargetCatalogGateway {
    type Error = TmuxError;

    fn list_targets(&self) -> Result<Vec<ManagedSessionRecord>, Self::Error> {
        let remote_sessions = match self.current_socket_name.as_deref() {
            Some(socket_name) => self
                .remote_runtime_owner
                .try_snapshot(socket_name)
                .map_err(|error| {
                    TmuxError::new(format!(
                        "failed to read remote runtime owner snapshot for socket `{socket_name}`: {error}"
                    ))
                })?
                .sessions,
            None => Vec::new(),
        };
        Ok(merge_targets_by_identity([
            self.local_tmux.list_sessions()?,
            remote_sessions,
        ]))
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
        let targets = self.list_targets()?;
        Ok(project_visible_targets(
            &targets,
            authority_id,
            workspace_session_id,
            active_target,
        ))
    }
}

pub fn project_visible_targets(
    targets: &[ManagedSessionRecord],
    authority_id: &str,
    workspace_session_id: &str,
    active_target: Option<&str>,
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
            (target.address.transport() == &SessionTransport::LocalTmux
                && target.address.authority_id() == authority_id
                && target.is_target_host())
                || target.address.transport() == &SessionTransport::RemotePeer
        })
        .cloned()
        .collect::<Vec<_>>();

    if visible_targets.is_empty() {
        return workspace_runtime.into_iter().collect();
    }

    if let Some(active_target) = active_target {
        if let Some(workspace_runtime) = workspace_runtime.as_ref() {
            if let Some(active_session) = visible_targets.iter_mut().find(|target| {
                target.address.transport() == &SessionTransport::LocalTmux
                    && target.address.authority_id() == authority_id
                    && target.address.qualified_target() == active_target
            }) {
                active_session.command_name = workspace_runtime.command_name.clone();
                active_session.current_path = workspace_runtime.current_path.clone();
                active_session.task_state = workspace_runtime.task_state;
            }
        }
    }

    sort_targets_for_display(&mut visible_targets);
    visible_targets
}

pub fn is_activation_target(target: &ManagedSessionRecord) -> bool {
    (target.address.transport() == &SessionTransport::LocalTmux && target.is_target_host())
        || target.address.transport() == &SessionTransport::RemotePeer
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
        is_activation_target, project_visible_targets, TargetCatalogGateway, TargetRegistryService,
    };
    use crate::domain::session_catalog::{
        ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState, SessionAvailability,
    };
    use crate::domain::workspace::WorkspaceSessionRole;
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
    fn visible_targets_overlay_workspace_runtime_onto_active_target() {
        let targets = project_visible_targets(
            &[
                session(
                    "wa-1",
                    "workspace",
                    "codex",
                    WorkspaceSessionRole::WorkspaceChrome,
                ),
                session("wa-1", "target-1", "bash", WorkspaceSessionRole::TargetHost),
                session("wa-1", "target-2", "bash", WorkspaceSessionRole::TargetHost),
            ],
            "wa-1",
            "workspace",
            Some("wa-1:target-2"),
        );

        assert_eq!(targets.len(), 2);
        assert_eq!(targets[1].address.session_id(), "target-2");
        assert_eq!(targets[1].command_name.as_deref(), Some("codex"));
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
            current_path: Some(PathBuf::from("/tmp/demo")),
            task_state: ManagedSessionTaskState::Input,
        }
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
            current_path: None,
            task_state: ManagedSessionTaskState::Running,
        }
    }
}
