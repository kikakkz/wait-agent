use crate::application::remote_session_creation_service::{
    GrpcRemoteSessionCreationTransport, RemoteSessionCreationService,
};
use crate::application::target_registry_service::{TargetCatalogGateway, TargetRegistryService};
use crate::cli::RemoteNetworkConfig;
use crate::domain::session_catalog::ManagedSessionRecord;
use crate::lifecycle::LifecycleError;
use crate::runtime::remote_host::remote_host_connect_runtime::{
    RemotePortProbeFactory, SshRemotePortProbeFactory,
};
use crate::runtime::remote_host::remote_host_history_store::{
    RemoteHostHistoryStore, RemoteHostProfile, RemotePortPreference as HistoryRemotePortPreference,
};
use crate::runtime::remote_host::remote_install_proxy_store::{
    proxy_candidates, wrap_install_command_with_proxy, RemoteInstallProxyStore,
};
use crate::runtime::remote_host::remote_port_probe::{RemotePortProbe, RemotePortProbePreference};
use crate::runtime::remote_host::ssh_remote_host_bootstrapper::{
    install_reachability_preflight_command, RemoteHostBootstrapPlan, RemoteHostBootstrapper,
    SshRemoteHostBootstrapper,
};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
struct RatatuiTargetCatalogGateway {
    sessions: Arc<Mutex<Vec<ManagedSessionRecord>>>,
}

impl RatatuiTargetCatalogGateway {
    fn new(sessions: Arc<Mutex<Vec<ManagedSessionRecord>>>) -> Self {
        Self { sessions }
    }
}

impl TargetCatalogGateway for RatatuiTargetCatalogGateway {
    type Error = LifecycleError;

    fn list_targets(&self) -> Result<Vec<ManagedSessionRecord>, LifecycleError> {
        Ok(self.sessions.lock().unwrap().clone())
    }
}

/// Connect to a saved remote host profile and return the created session record.
///
/// This bypasses the tmux-backed `RemoteHostConnectRuntime` wait loop and creates
/// the remote session directly via the gRPC node ingress transport.
pub fn connect_remote_host(
    profile_name: &str,
    sessions: &Arc<Mutex<Vec<ManagedSessionRecord>>>,
    network: &RemoteNetworkConfig,
) -> Result<ManagedSessionRecord, LifecycleError> {
    let history_store = RemoteHostHistoryStore::new(RemoteHostHistoryStore::default_path());
    let history = history_store
        .load()
        .map_err(|error| LifecycleError::Protocol(error.to_string()))?;
    let profile = history
        .hosts
        .into_iter()
        .find(|host| host.name == profile_name)
        .ok_or_else(|| {
            LifecycleError::Protocol(format!("remote host profile `{profile_name}` not found"))
        })?;

    let local_connect_endpoint = network.advertised_public_endpoint_label();
    let preference = port_preference(&profile.preferred_remote_port);
    let port_probe = SshRemotePortProbeFactory.create(&profile);
    let port = port_probe
        .choose_remote_port(&preference, &local_connect_endpoint)
        .map_err(|error| LifecycleError::Protocol(error.to_string()))?;
    let authority_node_id = authority_id_for_profile_port(&profile, port.port);

    let mut plan = RemoteHostBootstrapPlan::from_profile(
        &profile,
        port.port,
        local_connect_endpoint.clone(),
        authority_node_id.clone(),
    );
    plan.install_reachability_preflight_command = Some(install_reachability_preflight_command(&[]));

    if profile.use_install_proxy {
        let proxy_config = RemoteInstallProxyStore::default()
            .load_active_config()
            .map_err(|error| LifecycleError::Protocol(error.to_string()))?;
        if proxy_config.has_proxy() {
            let proxy_env_prefixes = proxy_candidates(&proxy_config)
                .map_err(|error| LifecycleError::Protocol(error.to_string()))?
                .iter()
                .map(|candidate| candidate.env_prefix(&profile.host, &local_connect_endpoint))
                .collect::<Vec<_>>();
            plan.install_reachability_preflight_command =
                Some(install_reachability_preflight_command(&proxy_env_prefixes));
        }
        plan.install_or_update_command = wrap_install_command_with_proxy(
            &plan.install_or_update_command,
            &proxy_config,
            &profile.host,
            &local_connect_endpoint,
        )
        .map_err(|error| LifecycleError::Protocol(error.to_string()))?;
    }

    SshRemoteHostBootstrapper::default()
        .ensure_waitagent_and_start(&plan)
        .map_err(|error| LifecycleError::Protocol(error.to_string()))?;

    let gateway = RatatuiTargetCatalogGateway::new(sessions.clone());
    let catalog = TargetRegistryService::new(gateway);
    let session_creation_service = RemoteSessionCreationService::new(
        GrpcRemoteSessionCreationTransport::new(network.clone()),
        catalog,
    );

    let record = session_creation_service
        .create_session(
            crate::application::remote_session_creation_service::RemoteSessionCreationRequest {
                authority_node_id: authority_node_id.clone(),
                cwd_hint: None,
                cols: 0,
                rows: 0,
            },
        )
        .map_err(|error| LifecycleError::Protocol(error.to_string()))?;

    // Update the saved profile with the last known endpoint.
    let mut updated_profile = profile.clone();
    updated_profile.last_remote_port = Some(port.port);
    updated_profile.last_endpoint = Some(format!("{}:{}", profile.host, port.port));
    updated_profile.use_install_proxy = profile.use_install_proxy;
    let _ = history_store.upsert_profile(updated_profile);

    Ok(record)
}

fn port_preference(value: &HistoryRemotePortPreference) -> RemotePortProbePreference {
    match value {
        HistoryRemotePortPreference::Auto => RemotePortProbePreference::Auto,
        HistoryRemotePortPreference::Port(port) => RemotePortProbePreference::Port(*port),
    }
}

fn authority_id_for_profile_port(profile: &RemoteHostProfile, remote_port: u16) -> String {
    format!("{}#{}", profile.host, remote_port)
}
