use crate::application::claude_hooks_config_service::ClaudeHooksConfigService;
use crate::application::codex_hooks_config_service::CodexHooksConfigService;
use crate::application::kimi_hooks_config_service::KimiHooksConfigService;
use crate::application::remote_session_creation_service::{
    GrpcRemoteSessionCreationTransport, RemoteSessionCreationService,
};
use crate::application::target_registry_service::TargetRegistryService;
use crate::cli::{Command, RatatuiClientCommand, RatatuiNodeServerCommand, RemoteNetworkConfig};
use crate::error::AppError;
use crate::infra::settings_store::SettingsStore;
use crate::ports::hooks_config::HooksConfigPort;
use crate::ports::session_creation::SessionCreationPort;
use crate::ports::target_registry::TargetRegistryPort;
use crate::process::agent_signal_sender_bundle::extract_agent_signal_sender;
use crate::ratatui_node::client_runtime::RatatuiClientRuntime;
use crate::ratatui_node::node_runtime::RatatuiNodeRuntime;
use crate::ratatui_node::runtime::RatatuiTargetCatalogGateway;
use crate::ratatui_node::workspace_runtime::RatatuiWorkspaceRuntime;
use crate::remote::node::remote_runtime_owner_runtime::RemoteRuntimeOwnerRuntime;
use crate::ui::banner::print_banner;
use std::sync::Arc;

// This dispatcher is the single command-side ownership boundary for the
// accepted local default route. `workspace` and `attach` must continue to flow
// into `WorkspaceCommandRuntime`, while hidden chrome panes stay on the
// dedicated event-driven pane runtimes.
pub struct CommandDispatcher {
    network: RemoteNetworkConfig,
}

impl CommandDispatcher {
    pub fn from_build_env_with_network_and_command(
        mut network: RemoteNetworkConfig,
        _command: &Command,
    ) -> Result<Self, AppError> {
        if network.public_endpoint.is_none() {
            let store = SettingsStore::default_path();
            if let Ok(Some(endpoint)) = SettingsStore::new(store).saved_public_endpoint() {
                network.public_endpoint = Some(endpoint);
            }
        }
        Ok(Self { network })
    }

    pub fn dispatch(&self, command: Command) -> Result<(), AppError> {
        match command {
            Command::Workspace => self
                .ratatui_workspace()?
                .run_workspace_entry()
                .map_err(AppError::from),
            Command::Attach(command) => self
                .ratatui_workspace()?
                .attach(command.target)
                .map_err(AppError::from),
            Command::List => self.ratatui_workspace()?.list().map_err(AppError::from),
            Command::Cleanup => self.ratatui_workspace()?.cleanup().map_err(AppError::from),
            Command::Detach(command) => self
                .ratatui_workspace()?
                .detach(command.target)
                .map_err(AppError::from),
            Command::Stop(command) => self
                .ratatui_workspace()?
                .stop(command.target)
                .map_err(AppError::from),
            Command::RatatuiListSessions(command) => self
                .ratatui_workspace()?
                .list_sessions(command.target)
                .map_err(AppError::from),
            Command::RatatuiNodeServer(command) => self
                .ratatui_node_server(command)
                .and_then(|runtime| runtime.run().map_err(AppError::from)),
            Command::RatatuiClient(command) => self
                .ratatui_client(command)
                .and_then(|runtime| runtime.run().map_err(AppError::from)),
            Command::Help(help) => {
                print_banner();
                println!("{help}");
                Ok(())
            }
            Command::Version => {
                let full =
                    option_env!("WAITAGENT_VERSION_FULL").unwrap_or(env!("CARGO_PKG_VERSION"));
                println!("waitagent {full}");
                Ok(())
            }
            Command::ShowErrorLog => {
                let entries = crate::infra::error_log::ERROR_LOG.entries();
                if entries.is_empty() {
                    println!("(no error log entries)");
                } else {
                    for (ts, msg) in &entries {
                        let secs = ts / 1000;
                        let millis = ts % 1000;
                        println!("[{}.{:03}] {}", secs, millis, msg);
                    }
                }
                Ok(())
            }
        }
    }

    fn ratatui_workspace(&self) -> Result<RatatuiWorkspaceRuntime, AppError> {
        RatatuiWorkspaceRuntime::from_build_env_with_network(self.network.clone())
            .map_err(AppError::from)
    }

    fn ratatui_node_server(
        &self,
        _command: RatatuiNodeServerCommand,
    ) -> Result<RatatuiNodeRuntime, AppError> {
        let network = self.network.clone();
        let remote_owner = RemoteRuntimeOwnerRuntime::from_build_env_with_network(network.clone())?;

        let target_gateway = RatatuiTargetCatalogGateway::new(remote_owner.clone());
        let target_registry = TargetRegistryService::new(target_gateway);

        let session_transport = GrpcRemoteSessionCreationTransport::new(network.clone());
        let session_creation =
            RemoteSessionCreationService::new(session_transport, target_registry.clone());
        let session_creation_port: Arc<dyn SessionCreationPort> = Arc::new(session_creation);
        let target_registry_port: Arc<dyn TargetRegistryPort> = Arc::new(target_registry);

        let sender_path = extract_agent_signal_sender()?;
        let hooks_config_ports: Vec<Box<dyn HooksConfigPort>> = vec![
            Box::new(ClaudeHooksConfigService::from_env(sender_path.clone())),
            Box::new(CodexHooksConfigService::from_env(sender_path.clone())),
            Box::new(KimiHooksConfigService::from_env(sender_path)),
        ];

        RatatuiNodeRuntime::from_network(
            network,
            remote_owner,
            session_creation_port,
            target_registry_port,
            hooks_config_ports,
            SettingsStore::new(SettingsStore::default_path()),
        )
        .map_err(AppError::from)
    }

    fn ratatui_client(
        &self,
        _command: RatatuiClientCommand,
    ) -> Result<RatatuiClientRuntime, AppError> {
        RatatuiClientRuntime::from_port(
            self.network.port,
            self.network.clone(),
            SettingsStore::new(SettingsStore::default_path()),
        )
        .map_err(AppError::from)
    }
}
