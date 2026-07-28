use crate::application::remote_session_creation_service::{
    GrpcRemoteSessionCreationTransport, RemoteSessionCreationCatalog, RemoteSessionCreationError,
    RemoteSessionCreationRequest, RemoteSessionCreationService,
};
use crate::application::target_registry_service::{TargetCatalogGateway, TargetRegistryService};
use crate::cli::{default_remote_node_port, RemoteNetworkConfig};
use crate::domain::session_catalog::{ManagedSessionRecord, SessionAvailability};
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
use std::thread;
use std::time::{Duration, Instant};

const ENDPOINT_POLL_INTERVAL: Duration = Duration::from_millis(100);
const ENDPOINT_WAIT_TIMEOUT: Duration = Duration::from_secs(30);

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

    // If we already have a live remote session for this host, reuse its
    // authority instead of starting a second daemon on a new port.
    let (authority_node_id, remote_port, should_bootstrap) =
        match find_online_remote_target_for_profile(sessions, &profile) {
            Some(existing) => {
                let authority = existing.address.authority_id().to_string();
                let port = authority
                    .rsplit_once('#')
                    .and_then(|(_, port)| port.parse().ok())
                    .unwrap_or_else(|| {
                        profile
                            .last_remote_port
                            .unwrap_or_else(default_remote_node_port)
                    });
                (authority, port, false)
            }
            None => {
                let preference = port_preference(&profile.preferred_remote_port);
                let port_probe = SshRemotePortProbeFactory.create(&profile);
                let port = port_probe
                    .choose_remote_port(&preference, &local_connect_endpoint)
                    .map_err(|error| LifecycleError::Protocol(error.to_string()))?;
                let authority_node_id = authority_id_for_profile_port(&profile, port.port);
                (authority_node_id, port.port, true)
            }
        };

    if should_bootstrap {
        let mut plan = RemoteHostBootstrapPlan::from_profile(
            &profile,
            remote_port,
            local_connect_endpoint.clone(),
            authority_node_id.clone(),
        );
        plan.install_reachability_preflight_command =
            Some(install_reachability_preflight_command(&[]));

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
    }

    let gateway = RatatuiTargetCatalogGateway::new(sessions.clone());
    let catalog = TargetRegistryService::new(gateway);
    let session_creation_service = RemoteSessionCreationService::new(
        GrpcRemoteSessionCreationTransport::new(network.clone()),
        catalog,
    );

    let record = wait_for_authority_and_create_session(
        &session_creation_service,
        RemoteSessionCreationRequest {
            authority_node_id: authority_node_id.clone(),
            cwd_hint: None,
            cols: 0,
            rows: 0,
        },
        &authority_node_id,
    )
    .map_err(|error| LifecycleError::Protocol(error.to_string()))?;

    // Update the saved profile with the last known endpoint.
    let mut updated_profile = profile.clone();
    updated_profile.last_remote_port = Some(remote_port);
    updated_profile.last_endpoint = Some(format!("{}:{}", profile.host, remote_port));
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

fn find_online_remote_target_for_profile(
    sessions: &Arc<Mutex<Vec<ManagedSessionRecord>>>,
    profile: &RemoteHostProfile,
) -> Option<ManagedSessionRecord> {
    let guard = sessions.lock().unwrap();
    guard
        .iter()
        .find(|target| {
            target.availability == SessionAvailability::Online
                && target
                    .address
                    .authority_id()
                    .starts_with(&format!("{}#", profile.host))
        })
        .cloned()
}

fn wait_for_authority_and_create_session<C>(
    service: &RemoteSessionCreationService<GrpcRemoteSessionCreationTransport, C>,
    request: RemoteSessionCreationRequest,
    authority_node_id: &str,
) -> Result<ManagedSessionRecord, RemoteSessionCreationError>
where
    C: RemoteSessionCreationCatalog,
    C::Error: ToString,
{
    let deadline = Instant::now() + ENDPOINT_WAIT_TIMEOUT;
    loop {
        match service.create_session(request.clone()) {
            Ok(record) => return Ok(record),
            Err(RemoteSessionCreationError::Rejected {
                code: "create_session_failed",
                message,
            }) if message.contains("is not connected") => {
                if Instant::now() >= deadline {
                    return Err(RemoteSessionCreationError::Rejected {
                        code: "create_session_failed",
                        message: format!(
                            "timed out after {}s waiting for remote authority `{}` to connect back; check `/tmp/waitagent-*.log` on the remote host",
                            ENDPOINT_WAIT_TIMEOUT.as_secs(),
                            authority_node_id
                        ),
                    });
                }
                thread::sleep(ENDPOINT_POLL_INTERVAL);
            }
            Err(other) => return Err(other),
        }
    }
}
