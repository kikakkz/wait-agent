#![allow(dead_code)]

use crate::application::remote_session_creation_service::{
    RemoteSessionCreationRequest, RemoteSessionCreationService,
};
use crate::application::target_registry_service::{TargetCatalogGateway, TargetRegistryService};
use crate::cli::ConnectRemoteHostCommand;
use crate::domain::session_catalog::{ManagedSessionRecord, SessionAvailability};
use crate::lifecycle::LifecycleError;
use crate::runtime::remote_host::remote_host_history_store::{
    RemoteHostAuthProfile, RemoteHostHistoryStore, RemoteHostProfile,
    RemotePortPreference as HistoryRemotePortPreference,
};
use crate::runtime::remote_host::remote_host_secret_store::{
    FileRemoteHostSecretStore, RemoteHostSecretId, RemoteHostSecretStore, RemoteHostSecretValue,
};
use crate::runtime::remote_host::remote_install_proxy_store::{
    proxy_candidates, wrap_install_command_with_proxy, RemoteInstallProxyStore,
};
use crate::runtime::remote_host::remote_port_probe::{
    RemotePortProbe, RemotePortProbePreference, SshRemotePortProbe,
};
use crate::runtime::remote_host::ssh_remote_host_bootstrapper::{
    install_reachability_preflight_command, RemoteHostBootstrapPlan, RemoteHostBootstrapper,
};
use std::io::{self, Read};

const DEFAULT_ENDPOINT_POLL_INTERVAL: Duration = Duration::from_millis(100);
// First-time installs can take a while to download the release asset and
// start the remote daemon, so give the endpoint more time to publish.
const DEFAULT_ENDPOINT_WAIT_TIMEOUT: Duration = Duration::from_secs(60);
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

pub trait RemotePortProbeFactory {
    type Probe;

    fn create(&self, profile: &RemoteHostProfile) -> Self::Probe;
}

#[derive(Debug, Clone, Default)]
pub struct SshRemotePortProbeFactory;

impl RemotePortProbeFactory for SshRemotePortProbeFactory {
    type Probe = SshRemotePortProbe;

    fn create(&self, profile: &RemoteHostProfile) -> Self::Probe {
        SshRemotePortProbe::new(profile.clone())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteHostConnectRequest {
    pub profile_name: Option<String>,
    pub direct_profile: Option<RemoteHostProfile>,
    pub save_profile_name: Option<String>,
    pub replace_profile_name: Option<String>,
    pub local_connect_endpoint: String,
    pub cwd_hint: Option<PathBuf>,
    pub use_install_proxy: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteHostConnectOutcome {
    pub authority_node_id: String,
    pub created_target: ManagedSessionRecord,
    pub reused_existing_endpoint: bool,
}

pub struct RemoteHostConnectRuntime<H, P, B, T, C> {
    history_store: H,
    port_probe_factory: P,
    bootstrapper: B,
    target_registry: TargetRegistryService<C>,
    session_creation_service: RemoteSessionCreationService<T, TargetRegistryService<C>>,
}

impl<H, P, B, T, C> RemoteHostConnectRuntime<H, P, B, T, C> {
    pub fn new(
        history_store: H,
        port_probe_factory: P,
        bootstrapper: B,
        target_registry: TargetRegistryService<C>,
        session_creation_service: RemoteSessionCreationService<T, TargetRegistryService<C>>,
    ) -> Self {
        Self {
            history_store,
            port_probe_factory,
            bootstrapper,
            target_registry,
            session_creation_service,
        }
    }
}

impl<P, B, T, C> RemoteHostConnectRuntime<RemoteHostHistoryStore, P, B, T, C>
where
    P: RemotePortProbeFactory,
    P::Probe: RemotePortProbe,
    <P::Probe as RemotePortProbe>::Error: ToString,
    B: RemoteHostBootstrapper,
    B::Error: ToString,
    C: TargetCatalogGateway + Clone,
    C::Error: ToString,
    T: crate::application::remote_session_creation_service::RemoteSessionCreationTransport,
    T::Error: ToString,
{
    pub fn connect(
        &self,
        request: RemoteHostConnectRequest,
    ) -> Result<RemoteHostConnectOutcome, LifecycleError> {
        let mut profile = self.resolve_profile(&request)?;
        let should_save_profile =
            request.profile_name.is_some() || request.save_profile_name.is_some();
        if let Some(name) = request.save_profile_name.as_ref() {
            profile.name = name.clone();
        }

        let preference = port_preference(&profile.preferred_remote_port);
        let port_probe = self.port_probe_factory.create(&profile);
        let port = port_probe
            .choose_remote_port(&preference, &request.local_connect_endpoint)
            .map_err(|error| LifecycleError::Protocol(error.to_string()))?;
        let authority_node_id = authority_id_for_profile_port(&profile, port.port);
        let existing_endpoint = self.find_connected_endpoint(&profile)?;
        let mut plan = RemoteHostBootstrapPlan::from_profile(
            &profile,
            port.port,
            request.local_connect_endpoint.clone(),
            authority_node_id.clone(),
        );
        plan.install_reachability_preflight_command =
            Some(install_reachability_preflight_command(&[]));
        if request.use_install_proxy {
            let proxy_config = RemoteInstallProxyStore::default()
                .load_active_config()
                .map_err(|error| LifecycleError::Protocol(error.to_string()))?;
            if proxy_config.has_proxy() {
                let proxy_env_prefixes = proxy_candidates(&proxy_config)
                    .map_err(|error| LifecycleError::Protocol(error.to_string()))?
                    .iter()
                    .map(|candidate| {
                        candidate.env_prefix(&profile.host, &request.local_connect_endpoint)
                    })
                    .collect::<Vec<_>>();
                plan.install_reachability_preflight_command =
                    Some(install_reachability_preflight_command(&proxy_env_prefixes));
            } else {
                plan.install_reachability_preflight_command =
                    Some(install_reachability_preflight_command(&[]));
            }
            plan.install_or_update_command = wrap_install_command_with_proxy(
                &plan.install_or_update_command,
                &proxy_config,
                &profile.host,
                &request.local_connect_endpoint,
            )
            .map_err(|error| LifecycleError::Protocol(error.to_string()))?;
        }
        self.bootstrapper
            .ensure_waitagent_and_start(&plan)
            .map_err(|error| LifecycleError::Protocol(error.to_string()))?;

        if let Some(endpoint) = existing_endpoint {
            let created = self.create_remote_session(&endpoint, request.cwd_hint.clone())?;
            if should_save_profile {
                profile.last_remote_port = Some(port.port);
                profile.last_endpoint = Some(format!("{}:{}", profile.host, port.port));
                profile.use_install_proxy = request.use_install_proxy;
                self.save_connected_profile(&request, profile)?;
            }
            return Ok(RemoteHostConnectOutcome {
                authority_node_id: endpoint,
                created_target: created,
                reused_existing_endpoint: true,
            });
        }

        let default_target = self.wait_for_first_online_target(
            &authority_node_id,
            DEFAULT_ENDPOINT_POLL_INTERVAL,
            DEFAULT_ENDPOINT_WAIT_TIMEOUT,
        )?;
        profile.last_remote_port = Some(port.port);
        profile.last_endpoint = Some(format!("{}:{}", profile.host, port.port));
        profile.use_install_proxy = request.use_install_proxy;
        if should_save_profile {
            self.save_connected_profile(&request, profile)?;
        }

        Ok(RemoteHostConnectOutcome {
            authority_node_id,
            created_target: default_target,
            reused_existing_endpoint: port.reused_existing_waitagent,
        })
    }

    fn resolve_profile(
        &self,
        request: &RemoteHostConnectRequest,
    ) -> Result<RemoteHostProfile, LifecycleError> {
        if let Some(profile) = &request.direct_profile {
            return Ok(profile.clone());
        }
        let Some(profile_name) = request.profile_name.as_deref() else {
            return Err(LifecycleError::Protocol(
                "remote host profile or direct host arguments are required".to_string(),
            ));
        };
        self.history_store
            .load()
            .map_err(|error| LifecycleError::Protocol(error.to_string()))?
            .hosts
            .into_iter()
            .find(|profile| profile.name == profile_name)
            .ok_or_else(|| {
                LifecycleError::Protocol(format!(
                    "remote host profile `{profile_name}` was not found"
                ))
            })
    }

    fn find_connected_endpoint(
        &self,
        profile: &RemoteHostProfile,
    ) -> Result<Option<String>, LifecycleError> {
        let targets = self
            .target_registry
            .list_targets()
            .map_err(|error| LifecycleError::Protocol(error.to_string()))?;
        Ok(targets
            .into_iter()
            .find(|target| is_online_remote_target_for_profile(target, profile))
            .map(|target| target.address.authority_id().to_string()))
    }

    fn wait_for_first_online_target(
        &self,
        expected: &str,
        poll_interval: Duration,
        timeout: Duration,
    ) -> Result<ManagedSessionRecord, LifecycleError> {
        let expected = expected.to_string();
        let deadline = Instant::now() + timeout;
        loop {
            let targets = self
                .target_registry
                .list_targets_on_authority(&expected)
                .map_err(|error| LifecycleError::Protocol(error.to_string()))?;
            if let Some(target) = targets
                .into_iter()
                .find(|target| target.availability == SessionAvailability::Online)
            {
                return Ok(target);
            }
            if Instant::now() >= deadline {
                return Err(LifecycleError::Protocol(format!(
                    "timed out after {}s waiting for remote WaitAgent `{expected}` to publish a target; check `/tmp/waitagent-*.log` on the remote host",
                    timeout.as_secs()
                )));
            }
            thread::sleep(poll_interval);
        }
    }

    fn create_remote_session(
        &self,
        authority_node_id: &str,
        cwd_hint: Option<PathBuf>,
    ) -> Result<ManagedSessionRecord, LifecycleError> {
        self.session_creation_service
            .create_session(RemoteSessionCreationRequest {
                authority_node_id: authority_node_id.to_string(),
                cwd_hint,
                cols: 0,
                rows: 0,
            })
            .map_err(|error| LifecycleError::Protocol(error.to_string()))
    }

    fn save_connected_profile(
        &self,
        request: &RemoteHostConnectRequest,
        profile: RemoteHostProfile,
    ) -> Result<(), LifecycleError> {
        if let Some(replace_name) = request.replace_profile_name.as_deref() {
            if replace_name != profile.name {
                self.history_store
                    .remove_profile(replace_name)
                    .map_err(|error| LifecycleError::Protocol(error.to_string()))?;
            }
        }
        self.history_store
            .upsert_profile(profile)
            .map_err(|error| LifecycleError::Protocol(error.to_string()))
    }
}

pub fn request_from_command(
    command: &ConnectRemoteHostCommand,
    local_connect_endpoint: String,
    cwd_hint: Option<PathBuf>,
) -> Result<RemoteHostConnectRequest, LifecycleError> {
    let direct_profile = match command.host.as_deref() {
        Some(host) => Some(profile_from_direct_args(command, host)?),
        None => None,
    };
    Ok(RemoteHostConnectRequest {
        profile_name: command.profile.clone(),
        direct_profile,
        save_profile_name: command.save_profile.clone(),
        replace_profile_name: command.replace_profile.clone(),
        local_connect_endpoint,
        cwd_hint,
        use_install_proxy: command.use_install_proxy.unwrap_or(true),
    })
}

fn profile_from_direct_args(
    command: &ConnectRemoteHostCommand,
    host: &str,
) -> Result<RemoteHostProfile, LifecycleError> {
    let ssh_user = command.ssh_user.clone().ok_or_else(|| {
        LifecycleError::Protocol("--ssh-user is required with --host".to_string())
    })?;
    let profile_name = command
        .save_profile
        .clone()
        .unwrap_or_else(|| default_profile_name(host, &ssh_user));
    let mut stdin_passwords = None;
    if command.ssh_password_stdin || command.sudo_password_stdin {
        stdin_passwords = Some(read_passwords_from_stdin()?);
    }
    let secret_store = FileRemoteHostSecretStore::default();
    let auth = match command.auth.as_deref().unwrap_or("password") {
        "password" => {
            let secret_id = if command.ssh_password_stdin {
                let password = stdin_passwords
                    .as_ref()
                    .map(|passwords| passwords.ssh_password.as_str())
                    .unwrap_or_default();
                if password.is_empty() {
                    return Err(LifecycleError::Protocol(
                        "SSH password is required with --ssh-password-stdin".to_string(),
                    ));
                }
                let id = generated_secret_id(&profile_name, "ssh-password")?;
                secret_store
                    .put_secret(&id, RemoteHostSecretValue::new(password))
                    .map_err(|error| LifecycleError::Protocol(error.to_string()))?;
                Some(id)
            } else {
                optional_secret_id(command.ssh_password_secret_id.clone())?
            };
            RemoteHostAuthProfile::Password {
                password_secret_id: secret_id,
            }
        }
        "key" => RemoteHostAuthProfile::Key {
            key_path: PathBuf::from(command.key_path.clone().ok_or_else(|| {
                LifecycleError::Protocol("--key-path is required with --auth key".to_string())
            })?),
        },
        other => {
            return Err(LifecycleError::Protocol(format!(
                "unsupported remote host auth `{other}`"
            )));
        }
    };
    let sudo_password_secret_id = if command.sudo_password_stdin {
        let password = stdin_passwords
            .as_ref()
            .map(|passwords| passwords.sudo_password.as_str())
            .unwrap_or_default();
        if password.is_empty() {
            None
        } else {
            let id = generated_secret_id(&profile_name, "sudo-password")?;
            secret_store
                .put_secret(&id, RemoteHostSecretValue::new(password))
                .map_err(|error| LifecycleError::Protocol(error.to_string()))?;
            Some(id)
        }
    } else {
        optional_secret_id(command.sudo_password_secret_id.clone())?
    };
    Ok(RemoteHostProfile {
        name: profile_name,
        host: host.to_string(),
        ssh_user,
        auth,
        sudo_password_secret_id,
        preferred_remote_port: parse_remote_port(command.remote_port.as_deref())?,
        last_remote_port: None,
        last_endpoint: None,
        last_connected_at: None,
        use_install_proxy: command.use_install_proxy.unwrap_or(true),
    })
}

#[derive(Debug, Clone, Default)]
struct StdinPasswords {
    ssh_password: String,
    sudo_password: String,
}

fn read_passwords_from_stdin() -> Result<StdinPasswords, LifecycleError> {
    let mut text = String::new();
    io::stdin().read_to_string(&mut text).map_err(|error| {
        LifecycleError::Io("failed to read remote host passwords".to_string(), error)
    })?;
    let mut lines = text.lines();
    Ok(StdinPasswords {
        ssh_password: lines.next().unwrap_or_default().to_string(),
        sudo_password: lines.next().unwrap_or_default().to_string(),
    })
}

fn default_profile_name(host: &str, ssh_user: &str) -> String {
    format!("{}@{}", ssh_user, host)
}

fn generated_secret_id(
    profile_name: &str,
    purpose: &str,
) -> Result<RemoteHostSecretId, LifecycleError> {
    RemoteHostSecretId::new(format!(
        "waitagent.remote-host.{}.{}",
        secret_id_segment(profile_name),
        purpose
    ))
    .map_err(|error| LifecycleError::Protocol(error.to_string()))
}

fn secret_id_segment(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push('-');
        }
    }
    let collapsed = out
        .split('-')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if collapsed.is_empty() {
        "remote".to_string()
    } else {
        collapsed
    }
}

fn optional_secret_id(value: Option<String>) -> Result<Option<RemoteHostSecretId>, LifecycleError> {
    value
        .map(RemoteHostSecretId::new)
        .transpose()
        .map_err(|error| LifecycleError::Protocol(error.to_string()))
}

fn parse_remote_port(value: Option<&str>) -> Result<HistoryRemotePortPreference, LifecycleError> {
    match value.unwrap_or("auto") {
        "auto" => Ok(HistoryRemotePortPreference::Auto),
        raw => raw
            .parse::<u16>()
            .map(HistoryRemotePortPreference::Port)
            .map_err(|_| LifecycleError::Protocol(format!("invalid remote port `{raw}`"))),
    }
}

fn authority_id_for_profile_port(profile: &RemoteHostProfile, remote_port: u16) -> String {
    format!("{}#{}", profile.host, remote_port)
}

fn port_preference(value: &HistoryRemotePortPreference) -> RemotePortProbePreference {
    match value {
        HistoryRemotePortPreference::Auto => RemotePortProbePreference::Auto,
        HistoryRemotePortPreference::Port(port) => RemotePortProbePreference::Port(*port),
    }
}

fn is_online_remote_target_for_profile(
    target: &ManagedSessionRecord,
    profile: &RemoteHostProfile,
) -> bool {
    target.availability == SessionAvailability::Online
        && target
            .address
            .authority_id()
            .starts_with(&format!("{}#", profile.host))
}

#[cfg(test)]
mod direct_arg_tests {
    use super::*;

    #[test]
    fn generated_profile_names_follow_user_at_host() {
        assert_eq!(default_profile_name("10.1.29.130", "kk"), "kk@10.1.29.130");
        assert_eq!(secret_id_segment("kk@10.1.29.130"), "kk-10-1-29-130");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::remote_session_creation_service::{
        CreateSessionReply, RemoteSessionCreationTransport,
    };
    use crate::domain::session_catalog::{ManagedSessionAddress, ManagedSessionTaskState};
    use crate::domain::workspace::{WorkspaceInstanceId, WorkspaceSessionRole};
    use crate::infra::remote_protocol::{
        CreateSessionAcceptedPayload, CreateSessionRequestPayload,
    };
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::time::Duration;

    #[derive(Clone)]
    struct FakeGateway {
        targets: Rc<RefCell<Vec<ManagedSessionRecord>>>,
    }

    impl TargetCatalogGateway for FakeGateway {
        type Error = String;

        fn list_targets(&self) -> Result<Vec<ManagedSessionRecord>, Self::Error> {
            Ok(self.targets.borrow().clone())
        }
    }

    #[derive(Clone)]
    struct DelayedGateway {
        calls: Rc<RefCell<usize>>,
        authority_id: String,
        session_id: String,
    }

    impl TargetCatalogGateway for DelayedGateway {
        type Error = String;

        fn list_targets(&self) -> Result<Vec<ManagedSessionRecord>, Self::Error> {
            let mut calls = self.calls.borrow_mut();
            *calls += 1;
            if *calls == 1 {
                Ok(Vec::new())
            } else {
                Ok(vec![remote_target(&self.authority_id, &self.session_id)])
            }
        }
    }

    #[derive(Clone)]
    struct FakeProbe {
        calls: Rc<RefCell<usize>>,
        port: u16,
    }

    impl RemotePortProbeFactory for FakeProbe {
        type Probe = FakeProbe;

        fn create(&self, _profile: &RemoteHostProfile) -> Self::Probe {
            self.clone()
        }
    }

    impl RemotePortProbe for FakeProbe {
        type Error = String;

        fn choose_remote_port(
            &self,
            _preference: &RemotePortProbePreference,
            _local_connect_endpoint: &str,
        ) -> Result<
            crate::runtime::remote_host::remote_port_probe::RemotePortProbeResult,
            Self::Error,
        > {
            *self.calls.borrow_mut() += 1;
            Ok(
                crate::runtime::remote_host::remote_port_probe::RemotePortProbeResult {
                    port: self.port,
                    reused_existing_waitagent: false,
                },
            )
        }
    }

    #[derive(Clone)]
    struct FakeBootstrapper {
        plans: Rc<RefCell<Vec<RemoteHostBootstrapPlan>>>,
        catalog_targets: Option<Rc<RefCell<Vec<ManagedSessionRecord>>>>,
    }

    impl RemoteHostBootstrapper for FakeBootstrapper {
        type Error = String;

        fn ensure_waitagent_and_start(
            &self,
            plan: &RemoteHostBootstrapPlan,
        ) -> Result<(), Self::Error> {
            self.plans.borrow_mut().push(plan.clone());
            if let Some(targets) = &self.catalog_targets {
                targets.borrow_mut().push(remote_target(
                    &format!("{}#{}", plan.host, plan.start_plan.remote_port),
                    "seed",
                ));
            }
            Ok(())
        }
    }

    #[derive(Clone)]
    struct FailingBootstrapper;

    impl RemoteHostBootstrapper for FailingBootstrapper {
        type Error = String;

        fn ensure_waitagent_and_start(
            &self,
            _plan: &RemoteHostBootstrapPlan,
        ) -> Result<(), Self::Error> {
            Err("bootstrap failed".to_string())
        }
    }

    #[derive(Clone)]
    struct FakeCreateTransport {
        requests: Rc<RefCell<Vec<CreateSessionRequestPayload>>>,
        catalog_targets: Rc<RefCell<Vec<ManagedSessionRecord>>>,
    }

    impl RemoteSessionCreationTransport for FakeCreateTransport {
        type Error = String;

        fn create_session(
            &self,
            request: CreateSessionRequestPayload,
            _accept_timeout: Duration,
        ) -> Result<CreateSessionReply, Self::Error> {
            self.requests.borrow_mut().push(request.clone());
            self.catalog_targets
                .borrow_mut()
                .push(remote_target(&request.authority_node_id, "created-1"));
            Ok(CreateSessionReply::Accepted(CreateSessionAcceptedPayload {
                request_id: request.request_id,
                session_id: "created-1".to_string(),
                target_id: format!("remote-peer:{}:created-1", request.authority_node_id),
            }))
        }
    }

    #[test]
    fn remote_host_connect_prefights_before_reusing_existing_online_endpoint() {
        let path = unique_path("remote-host-connect-reuse.toml");
        let history = RemoteHostHistoryStore::new(&path);
        history.upsert_profile(profile()).unwrap();
        let probe_calls = Rc::new(RefCell::new(0));
        let bootstrap_plans = Rc::new(RefCell::new(Vec::new()));
        let create_requests = Rc::new(RefCell::new(Vec::new()));
        let catalog_targets = Rc::new(RefCell::new(vec![remote_target(
            "10.1.29.130#7474",
            "seed",
        )]));
        let gateway = FakeGateway {
            targets: catalog_targets.clone(),
        };
        let registry = TargetRegistryService::new(gateway.clone());
        let runtime = RemoteHostConnectRuntime::new(
            history,
            FakeProbe {
                calls: probe_calls.clone(),
                port: 7476,
            },
            FakeBootstrapper {
                plans: bootstrap_plans.clone(),
                catalog_targets: None,
            },
            registry.clone(),
            RemoteSessionCreationService::new(
                FakeCreateTransport {
                    requests: create_requests.clone(),
                    catalog_targets: catalog_targets.clone(),
                },
                registry,
            ),
        );

        let outcome = runtime
            .connect(RemoteHostConnectRequest {
                profile_name: Some("130".to_string()),
                direct_profile: None,
                save_profile_name: None,
                replace_profile_name: None,
                local_connect_endpoint: "10.1.26.84:7474".to_string(),
                cwd_hint: None,
                use_install_proxy: true,
            })
            .unwrap();

        assert!(outcome.reused_existing_endpoint);
        assert_eq!(outcome.authority_node_id, "10.1.29.130#7474");
        assert_eq!(*probe_calls.borrow(), 1);
        assert_eq!(bootstrap_plans.borrow().len(), 1);
        assert_eq!(
            create_requests.borrow()[0].authority_node_id,
            "10.1.29.130#7474"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn remote_host_connect_bootstraps_when_no_endpoint_exists() {
        let path = unique_path("remote-host-connect-bootstrap.toml");
        let history = RemoteHostHistoryStore::new(&path);
        history.upsert_profile(profile()).unwrap();
        let bootstrap_plans = Rc::new(RefCell::new(Vec::new()));
        let create_requests = Rc::new(RefCell::new(Vec::new()));
        let catalog_targets = Rc::new(RefCell::new(Vec::new()));
        let gateway = FakeGateway {
            targets: catalog_targets.clone(),
        };
        let registry = TargetRegistryService::new(gateway.clone());
        let runtime = RemoteHostConnectRuntime::new(
            history,
            FakeProbe {
                calls: Rc::new(RefCell::new(0)),
                port: 7476,
            },
            FakeBootstrapper {
                plans: bootstrap_plans.clone(),
                catalog_targets: Some(catalog_targets.clone()),
            },
            registry.clone(),
            RemoteSessionCreationService::new(
                FakeCreateTransport {
                    requests: create_requests.clone(),
                    catalog_targets: catalog_targets.clone(),
                },
                registry,
            ),
        );

        let outcome = runtime
            .connect(RemoteHostConnectRequest {
                profile_name: Some("130".to_string()),
                direct_profile: None,
                save_profile_name: None,
                replace_profile_name: None,
                local_connect_endpoint: "10.1.26.84:7474".to_string(),
                cwd_hint: Some(PathBuf::from("/opt/data/workspace/app-insight")),
                use_install_proxy: true,
            })
            .unwrap();

        assert!(!outcome.reused_existing_endpoint);
        assert_eq!(outcome.authority_node_id, "10.1.29.130#7476");
        assert_eq!(bootstrap_plans.borrow().len(), 1);
        assert!(bootstrap_plans.borrow()[0]
            .start_plan
            .command
            .contains("--connect '10.1.26.84:7474'"));
        assert!(!bootstrap_plans.borrow()[0]
            .start_plan
            .command
            .contains("all_proxy"));
        assert!(bootstrap_plans.borrow()[0]
            .start_plan
            .command
            .contains("--node-id '10.1.29.130#7476'"));
        assert!(
            create_requests.borrow().is_empty(),
            "first Ctrl-W bootstrap must activate the default published session without creating an extra session"
        );
        assert_eq!(
            outcome.created_target.address.authority_id(),
            "10.1.29.130#7476"
        );
        assert_eq!(outcome.created_target.address.session_id(), "seed");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn remote_host_connect_does_not_save_direct_profile_when_bootstrap_fails() {
        let path = unique_path("remote-host-connect-failed-direct.toml");
        let history = RemoteHostHistoryStore::new(&path);
        let catalog_targets = Rc::new(RefCell::new(Vec::new()));
        let gateway = FakeGateway {
            targets: catalog_targets.clone(),
        };
        let registry = TargetRegistryService::new(gateway.clone());
        let runtime = RemoteHostConnectRuntime::new(
            history.clone(),
            FakeProbe {
                calls: Rc::new(RefCell::new(0)),
                port: 7476,
            },
            FailingBootstrapper,
            registry.clone(),
            RemoteSessionCreationService::new(
                FakeCreateTransport {
                    requests: Rc::new(RefCell::new(Vec::new())),
                    catalog_targets,
                },
                registry,
            ),
        );

        let result = runtime.connect(RemoteHostConnectRequest {
            profile_name: None,
            direct_profile: Some(profile()),
            save_profile_name: Some("130".to_string()),
            replace_profile_name: None,
            local_connect_endpoint: "10.1.26.84:7474".to_string(),
            cwd_hint: None,
            use_install_proxy: true,
        });

        assert!(result.is_err());
        assert!(history.load().unwrap().hosts.is_empty());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn remote_host_connect_replaces_old_profile_name_after_success() {
        let path = unique_path("remote-host-connect-replace-profile.toml");
        let history = RemoteHostHistoryStore::new(&path);
        let mut original = profile();
        original.name = "kk@10.1.29.130".to_string();
        original.host = "10.1.29.130".to_string();
        original.ssh_user = "kk".to_string();
        history.upsert_profile(original).unwrap();
        let catalog_targets = Rc::new(RefCell::new(Vec::new()));
        let gateway = FakeGateway {
            targets: catalog_targets.clone(),
        };
        let registry = TargetRegistryService::new(gateway.clone());
        let runtime = RemoteHostConnectRuntime::new(
            history.clone(),
            FakeProbe {
                calls: Rc::new(RefCell::new(0)),
                port: 7476,
            },
            FakeBootstrapper {
                plans: Rc::new(RefCell::new(Vec::new())),
                catalog_targets: Some(catalog_targets.clone()),
            },
            registry.clone(),
            RemoteSessionCreationService::new(
                FakeCreateTransport {
                    requests: Rc::new(RefCell::new(Vec::new())),
                    catalog_targets,
                },
                registry,
            ),
        );
        let mut edited = profile();
        edited.name = "kk@10.1.29.140".to_string();
        edited.host = "10.1.29.140".to_string();
        edited.ssh_user = "kk".to_string();

        runtime
            .connect(RemoteHostConnectRequest {
                profile_name: None,
                direct_profile: Some(edited),
                save_profile_name: Some("kk@10.1.29.140".to_string()),
                replace_profile_name: Some("kk@10.1.29.130".to_string()),
                local_connect_endpoint: "10.1.26.84:7474".to_string(),
                cwd_hint: None,
                use_install_proxy: true,
            })
            .unwrap();

        let hosts = history.load().unwrap().hosts;
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].name, "kk@10.1.29.140");
        assert_eq!(hosts[0].host, "10.1.29.140");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn remote_host_connect_does_not_save_unsaved_direct_profile_after_success() {
        let path = unique_path("remote-host-connect-unsaved-direct.toml");
        let history = RemoteHostHistoryStore::new(&path);
        let catalog_targets = Rc::new(RefCell::new(Vec::new()));
        let gateway = FakeGateway {
            targets: catalog_targets.clone(),
        };
        let registry = TargetRegistryService::new(gateway.clone());
        let runtime = RemoteHostConnectRuntime::new(
            history.clone(),
            FakeProbe {
                calls: Rc::new(RefCell::new(0)),
                port: 7476,
            },
            FakeBootstrapper {
                plans: Rc::new(RefCell::new(Vec::new())),
                catalog_targets: Some(catalog_targets.clone()),
            },
            registry.clone(),
            RemoteSessionCreationService::new(
                FakeCreateTransport {
                    requests: Rc::new(RefCell::new(Vec::new())),
                    catalog_targets,
                },
                registry,
            ),
        );

        runtime
            .connect(RemoteHostConnectRequest {
                profile_name: None,
                direct_profile: Some(profile()),
                save_profile_name: None,
                replace_profile_name: None,
                local_connect_endpoint: "10.1.26.84:7474".to_string(),
                cwd_hint: None,
                use_install_proxy: true,
            })
            .unwrap();

        assert!(history.load().unwrap().hosts.is_empty());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn wait_for_first_online_target_waits_for_signal_without_timeout_failure() {
        let calls = Rc::new(RefCell::new(0));
        let gateway = DelayedGateway {
            calls: calls.clone(),
            authority_id: "10.1.29.130#7476".to_string(),
            session_id: "seed".to_string(),
        };
        let registry = TargetRegistryService::new(gateway.clone());
        let runtime = RemoteHostConnectRuntime::new(
            RemoteHostHistoryStore::new(unique_path("remote-host-connect-delayed-signal.toml")),
            FakeProbe {
                calls: Rc::new(RefCell::new(0)),
                port: 7476,
            },
            FakeBootstrapper {
                plans: Rc::new(RefCell::new(Vec::new())),
                catalog_targets: None,
            },
            registry.clone(),
            RemoteSessionCreationService::new(
                FakeCreateTransport {
                    requests: Rc::new(RefCell::new(Vec::new())),
                    catalog_targets: Rc::new(RefCell::new(Vec::new())),
                },
                registry,
            ),
        );

        let target = runtime
            .wait_for_first_online_target(
                "10.1.29.130#7476",
                Duration::from_millis(0),
                Duration::from_secs(1),
            )
            .unwrap();

        assert_eq!(target.address.authority_id(), "10.1.29.130#7476");
        assert_eq!(target.address.session_id(), "seed");
        assert_eq!(*calls.borrow(), 2);
    }

    #[test]
    fn wait_for_first_online_target_times_out_when_remote_never_publishes() {
        let registry = TargetRegistryService::new(FakeGateway {
            targets: Rc::new(RefCell::new(Vec::new())),
        });
        let runtime = RemoteHostConnectRuntime::new(
            RemoteHostHistoryStore::new(unique_path("remote-host-connect-timeout.toml")),
            FakeProbe {
                calls: Rc::new(RefCell::new(0)),
                port: 7476,
            },
            FakeBootstrapper {
                plans: Rc::new(RefCell::new(Vec::new())),
                catalog_targets: None,
            },
            registry.clone(),
            RemoteSessionCreationService::new(
                FakeCreateTransport {
                    requests: Rc::new(RefCell::new(Vec::new())),
                    catalog_targets: Rc::new(RefCell::new(Vec::new())),
                },
                registry,
            ),
        );

        let error = runtime
            .wait_for_first_online_target(
                "10.1.29.130#7476",
                Duration::from_millis(0),
                Duration::from_millis(1),
            )
            .unwrap_err();

        assert!(error.to_string().contains("timed out"));
        assert!(error.to_string().contains("10.1.29.130#7476"));
    }

    #[test]
    fn remote_host_connect_respects_disabled_install_proxy() {
        let catalog_targets = Rc::new(RefCell::new(Vec::new()));
        let bootstrap_plans = Rc::new(RefCell::new(Vec::new()));
        let gateway = FakeGateway {
            targets: catalog_targets.clone(),
        };
        let registry = TargetRegistryService::new(gateway.clone());
        let runtime = RemoteHostConnectRuntime::new(
            RemoteHostHistoryStore::new(unique_path("remote-host-connect-no-proxy.toml")),
            FakeProbe {
                calls: Rc::new(RefCell::new(0)),
                port: 7476,
            },
            FakeBootstrapper {
                plans: bootstrap_plans.clone(),
                catalog_targets: Some(catalog_targets.clone()),
            },
            registry.clone(),
            RemoteSessionCreationService::new(
                FakeCreateTransport {
                    requests: Rc::new(RefCell::new(Vec::new())),
                    catalog_targets,
                },
                registry,
            ),
        );

        runtime
            .connect(RemoteHostConnectRequest {
                profile_name: None,
                direct_profile: Some(profile()),
                save_profile_name: None,
                replace_profile_name: None,
                local_connect_endpoint: "10.1.26.84:7474".to_string(),
                cwd_hint: None,
                use_install_proxy: false,
            })
            .unwrap();

        let plans = bootstrap_plans.borrow();
        assert_eq!(plans.len(), 1);
        assert!(!plans[0].install_or_update_command.contains("all_proxy"));
        assert!(!plans[0].start_plan.command.contains("all_proxy"));
    }

    fn profile() -> RemoteHostProfile {
        RemoteHostProfile {
            name: "130".to_string(),
            host: "10.1.29.130".to_string(),
            ssh_user: "kk".to_string(),
            auth: RemoteHostAuthProfile::Password {
                password_secret_id: None,
            },
            sudo_password_secret_id: None,
            preferred_remote_port: HistoryRemotePortPreference::Auto,
            last_remote_port: None,
            last_endpoint: None,
            last_connected_at: None,
            use_install_proxy: true,
        }
    }

    fn remote_target(authority_id: &str, session_id: &str) -> ManagedSessionRecord {
        ManagedSessionRecord {
            address: ManagedSessionAddress::remote_peer(authority_id, session_id),
            selector: Some(format!("{authority_id}:{session_id}")),
            availability: SessionAvailability::Online,
            workspace_dir: Some(PathBuf::from("/tmp/demo")),
            workspace_key: Some(WorkspaceInstanceId::new(session_id).as_str().to_string()),
            session_role: Some(WorkspaceSessionRole::TargetHost),
            opened_by: Vec::new(),
            attached_clients: 1,
            window_count: 1,
            command_name: Some("bash".to_string()),
            display_command_name: None,
            current_path: Some(PathBuf::from("/tmp/demo")),
            task_state: ManagedSessionTaskState::Input,
        }
    }

    fn unique_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("waitagent-{name}-{}", std::process::id()))
    }
}
