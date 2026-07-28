use crate::application::layout_service::{FOOTER_PANE_TITLE, SIDEBAR_PANE_TITLE};
use crate::application::target_registry_service::{
    DefaultTargetCatalogGateway, TargetRegistryService,
};
use crate::cli::ToggleFullscreenCommand;
use crate::domain::session_catalog::{ManagedSessionRecord, SessionTransport};
use crate::domain::workspace::WorkspaceInstanceId;
use crate::infra::tmux::{
    EmbeddedTmuxBackend, TmuxError, TmuxGateway, TmuxLayoutGateway, TmuxPaneId, TmuxSessionName,
    TmuxSocketName, TmuxWorkspaceHandle,
};
use crate::lifecycle::LifecycleError;
use crate::runtime::remote_main_slot::remote_main_slot_pane_runtime::history_pane_option_name;
use crate::runtime::workspace::main_slot_runtime::WAITAGENT_ACTIVE_TARGET_OPTION;
use crate::runtime::workspace_layout_runtime::WorkspaceLayoutRuntime;
use std::io;

const WAITAGENT_MAIN_PANE_OPTION: &str = "@waitagent_main_pane_id";

pub struct NativePaneFullscreenRuntime {
    backend: EmbeddedTmuxBackend,
    target_registry: TargetRegistryService<DefaultTargetCatalogGateway>,
    _layout_runtime: WorkspaceLayoutRuntime,
}

impl NativePaneFullscreenRuntime {
    pub fn new(
        backend: EmbeddedTmuxBackend,
        target_registry: TargetRegistryService<DefaultTargetCatalogGateway>,
        layout_runtime: WorkspaceLayoutRuntime,
    ) -> Self {
        Self {
            backend,
            target_registry,
            _layout_runtime: layout_runtime,
        }
    }

    pub fn run_toggle(&self, command: ToggleFullscreenCommand) -> Result<(), LifecycleError> {
        let session = self.resolve_session(&command.socket_name, &command.session_name)?;
        if !session.is_workspace_chrome() {
            return Err(LifecycleError::Protocol(format!(
                "history mode is only available for workspace sessions, got `{}`",
                session.address.qualified_target()
            )));
        }

        let workspace = workspace_handle(&command.socket_name, &command.session_name);
        let main_pane = self.workspace_main_pane(&workspace)?;

        // Remote sessions keep their scrollback in a dedicated history pane so
        // engine-mode frame rendering does not pollute the history.
        if let Some(history_pane) = self.remote_history_pane(&workspace)? {
            if self
                .backend
                .pane_in_mode_on_socket(workspace.socket_name.as_str(), history_pane.as_str())
                .map_err(history_error)?
            {
                self.backend
                    .cancel_pane_mode_on_socket(
                        workspace.socket_name.as_str(),
                        history_pane.as_str(),
                    )
                    .map_err(history_error)?;
                self.backend
                    .select_pane(&workspace, &main_pane)
                    .map_err(history_error)?;
                return Ok(());
            }

            self.backend
                .select_pane(&workspace, &history_pane)
                .map_err(history_error)?;
            self.backend
                .enter_copy_mode(&workspace, &history_pane)
                .map_err(history_error)?;
            return Ok(());
        }

        if self
            .backend
            .pane_in_mode_on_socket(workspace.socket_name.as_str(), main_pane.as_str())
            .map_err(history_error)?
        {
            self.backend
                .cancel_pane_mode_on_socket(workspace.socket_name.as_str(), main_pane.as_str())
                .map_err(history_error)?;
            self.backend
                .select_pane(&workspace, &main_pane)
                .map_err(history_error)?;
            return Ok(());
        }

        self.backend
            .select_pane(&workspace, &main_pane)
            .map_err(history_error)?;
        self.backend
            .enter_copy_mode(&workspace, &main_pane)
            .map_err(history_error)
    }

    fn remote_history_pane(
        &self,
        workspace: &TmuxWorkspaceHandle,
    ) -> Result<Option<TmuxPaneId>, LifecycleError> {
        let Some(active_target) = self
            .backend
            .show_session_option(workspace, WAITAGENT_ACTIVE_TARGET_OPTION)
            .map_err(history_error)?
            .filter(|target| !target.is_empty())
        else {
            return Ok(None);
        };
        let is_remote = self
            .target_registry
            .find_target(&active_target)
            .ok()
            .flatten()
            .map(|session| *session.address.transport() == SessionTransport::RemotePeer)
            .unwrap_or(false);
        if !is_remote {
            return Ok(None);
        }
        let option = history_pane_option_name(&active_target);
        Ok(self
            .backend
            .show_session_option(workspace, &option)
            .map_err(history_error)?
            .filter(|pane| !pane.is_empty())
            .map(TmuxPaneId::new))
    }

    fn resolve_session(
        &self,
        socket_name: &str,
        session_name: &str,
    ) -> Result<ManagedSessionRecord, LifecycleError> {
        self.target_registry
            .resolve_target_on_authority_session(socket_name, session_name)
            .map_err(history_error)?
            .ok_or_else(|| {
                LifecycleError::Protocol(format!(
                    "tmux session `{socket_name}:{session_name}` could not be resolved"
                ))
            })
    }

    fn workspace_main_pane(
        &self,
        workspace: &TmuxWorkspaceHandle,
    ) -> Result<TmuxPaneId, LifecycleError> {
        if let Some(pane) = self
            .backend
            .show_session_option(workspace, WAITAGENT_MAIN_PANE_OPTION)
            .map_err(history_error)?
        {
            return Ok(TmuxPaneId::new(pane));
        }
        self.infer_main_pane(workspace).ok_or_else(|| {
            LifecycleError::Protocol(format!(
                "workspace `{}` has no main pane",
                workspace.session_name.as_str()
            ))
        })
    }

    fn infer_main_pane(&self, workspace: &TmuxWorkspaceHandle) -> Option<TmuxPaneId> {
        let window = self.backend.current_window(workspace).ok()?;
        let panes = self.backend.list_panes(workspace, &window).ok()?;
        panes
            .iter()
            .find(|pane| pane.title != SIDEBAR_PANE_TITLE && pane.title != FOOTER_PANE_TITLE)
            .or_else(|| panes.first())
            .map(|pane| pane.pane_id.clone())
    }
}

fn workspace_handle(socket_name: &str, session_name: &str) -> TmuxWorkspaceHandle {
    TmuxWorkspaceHandle {
        workspace_id: WorkspaceInstanceId::new(session_name),
        socket_name: TmuxSocketName::new(socket_name),
        session_name: TmuxSessionName::new(session_name),
    }
}

fn history_error(error: TmuxError) -> LifecycleError {
    LifecycleError::Io(
        "tmux main-pane history command failed".to_string(),
        io::Error::new(io::ErrorKind::Other, error.to_string()),
    )
}
