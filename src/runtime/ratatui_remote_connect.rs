use crate::application::remote_session_creation_service::{
    GrpcRemoteSessionCreationTransport, RemoteSessionCreationCatalog, RemoteSessionCreationError,
    RemoteSessionCreationRequest, RemoteSessionCreationService,
};
use crate::application::target_registry_service::{TargetCatalogGateway, TargetRegistryService};
use crate::cli::{default_remote_node_port, RemoteNetworkConfig};
use crate::domain::session_catalog::{ManagedSessionRecord, SessionAvailability};
use crate::infra::error_log::ERROR_LOG;
use crate::lifecycle::LifecycleError;
use crate::runtime::remote_host::remote_host_connect_runtime::{
    RemotePortProbeFactory, SshRemotePortProbeFactory,
};
use crate::runtime::remote_host::remote_host_history_store::{
    RemoteHostHistoryStore, RemoteHostProfile, RemotePortPreference as HistoryRemotePortPreference,
};
use crate::runtime::remote_host::remote_port_probe::{RemotePortProbe, RemotePortProbePreference};
use crate::runtime::remote_host::ssh_remote_host_bootstrapper::{
    RemoteHostBootstrapPlan, RemoteHostBootstrapper, SshRemoteHostBootstrapper,
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

    // Determine the port we intend to use for this connection.  The preference
    // order is: explicit preferred port, last known port, default port.  We
    // only reuse an existing remote session when it is on the exact same
    // `host#port`; matching by host prefix alone would reuse a stale session on
    // a different port and prevent the user from reaching a new server.
    let intended_port = profile
        .preferred_remote_port
        .explicit_port()
        .or(profile.last_remote_port)
        .unwrap_or_else(default_remote_node_port);
    let intended_authority = authority_id_for_profile_port(&profile, intended_port);
    let existing_target = find_online_remote_target_for_authority(sessions, &intended_authority);

    let (authority_node_id, remote_port) = match &existing_target {
        Some(existing) => {
            let authority = existing.address.authority_id().to_string();
            let port = authority
                .rsplit_once('#')
                .and_then(|(_, port)| port.parse().ok())
                .unwrap_or(intended_port);
            (authority, port)
        }
        None => {
            // If the profile already names a port (preferred or last-known), use
            // that port directly.  Only probe for a free port when the user has
            // not pinned a port, which keeps reconnections stable and respects
            // the "one port == one server" model.
            let chosen_port = if profile.preferred_remote_port.explicit_port().is_some()
                || profile.last_remote_port.is_some()
            {
                intended_port
            } else {
                let preference = port_preference(&profile.preferred_remote_port);
                let port_probe = SshRemotePortProbeFactory.create(&profile);
                port_probe
                    .choose_remote_port(&preference, &local_connect_endpoint)
                    .map_err(|error| LifecycleError::Protocol(error.to_string()))?
                    .port
            };
            (
                authority_id_for_profile_port(&profile, chosen_port),
                chosen_port,
            )
        }
    };

    if existing_target.is_none() {
        let plan = RemoteHostBootstrapPlan::from_profile(
            &profile,
            remote_port,
            local_connect_endpoint.clone(),
            authority_node_id.clone(),
        )
        .with_local_binary_deploy();

        let bootstrapper = SshRemoteHostBootstrapper::default();
        let daemon_already_running = bootstrapper
            .remote_waitagent_daemon_is_running(&plan)
            .map_err(|error| LifecycleError::Protocol(error.to_string()))?;

        if daemon_already_running {
            ERROR_LOG.log(format!(
                "[connect-remote-host] daemon already running on {}:{remote_port}; waiting for reconnect",
                profile.host
            ));
        } else {
            bootstrapper
                .ensure_waitagent_and_start(&plan)
                .map_err(|error| LifecycleError::Protocol(error.to_string()))?;
        }
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

fn find_online_remote_target_for_authority(
    sessions: &Arc<Mutex<Vec<ManagedSessionRecord>>>,
    authority_id: &str,
) -> Option<ManagedSessionRecord> {
    let guard = sessions.lock().unwrap();
    guard
        .iter()
        .find(|target| {
            target.availability == SessionAvailability::Online
                && target.address.authority_id() == authority_id
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
