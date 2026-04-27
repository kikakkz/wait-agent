use crate::application::layout_service::{FOOTER_PANE_TITLE, SIDEBAR_PANE_TITLE};
use crate::application::session_service::SessionService;
use crate::cli::ToggleFullscreenCommand;
use crate::domain::session_catalog::ManagedSessionRecord;
use crate::domain::workspace::WorkspaceInstanceId;
use crate::infra::tmux::{
    EmbeddedTmuxBackend, TmuxError, TmuxGateway, TmuxLayoutGateway, TmuxPaneId, TmuxSessionName,
    TmuxSocketName, TmuxWorkspaceHandle,
};
use crate::lifecycle::LifecycleError;
use crate::runtime::workspace_layout_runtime::WorkspaceLayoutRuntime;
use std::io;

const WAITAGENT_MAIN_PANE_OPTION: &str = "@waitagent_main_pane_id";
const WAITAGENT_FULLSCREEN_OWNS_COPY_MODE_OPTION: &str = "@waitagent_fullscreen_owns_copy_mode";
const WAITAGENT_FULLSCREEN_STATUS_FORMAT0_PRESENT_OPTION: &str =
    "@waitagent_fullscreen_status_format0_present";
const WAITAGENT_FULLSCREEN_STATUS_FORMAT0_VALUE_OPTION: &str =
    "@waitagent_fullscreen_status_format0_value";
const WAITAGENT_FULLSCREEN_STATUS_FORMAT_HAS_OTHER_OPTION: &str =
    "@waitagent_fullscreen_status_format_has_other";
const TMUX_STATUS_FORMAT_OPTION: &str = "status-format[0]";
const TMUX_STATUS_FORMAT_ARRAY_OPTION: &str = "status-format";
const TMUX_FULLSCREEN_STATUS_FORMAT: &str = "#{E:@waitagent_fullscreen_footer_line}";

pub struct NativePaneFullscreenRuntime {
    backend: EmbeddedTmuxBackend,
    session_service: SessionService<EmbeddedTmuxBackend>,
    layout_runtime: WorkspaceLayoutRuntime,
}

impl NativePaneFullscreenRuntime {
    pub fn new(
        backend: EmbeddedTmuxBackend,
        session_service: SessionService<EmbeddedTmuxBackend>,
        layout_runtime: WorkspaceLayoutRuntime,
    ) -> Self {
        Self {
            backend,
            session_service,
            layout_runtime,
        }
    }

    pub fn run_toggle(&self, command: ToggleFullscreenCommand) -> Result<(), LifecycleError> {
        let session = self.resolve_session(&command.socket_name, &command.session_name)?;
        if !session.is_workspace_chrome() {
            return Err(LifecycleError::Protocol(format!(
                "fullscreen is only available for workspace sessions, got `{}`",
                session.address.qualified_target()
            )));
        }

        let workspace = workspace_handle(&command.socket_name, &command.session_name);
        let workspace_dir = session.workspace_dir.clone().ok_or_else(|| {
            LifecycleError::Protocol(format!(
                "workspace `{}` has no workspace directory metadata",
                workspace.session_name.as_str()
            ))
        })?;
        let main_pane = self.workspace_main_pane(&workspace)?;
        let was_zoomed = self
            .backend
            .window_zoomed_on_socket(workspace.socket_name.as_str(), main_pane.as_str())
            .map_err(fullscreen_error)?;

        if was_zoomed {
            self.exit_fullscreen_history(&workspace, &main_pane)?;
        } else {
            self.prepare_fullscreen_history_entry(&workspace, &main_pane)?;
        }

        self.backend
            .run_socket_command(
                &workspace.socket_name,
                &[
                    "resize-pane".to_string(),
                    "-t".to_string(),
                    main_pane.as_str().to_string(),
                    "-Z".to_string(),
                ],
            )
            .map_err(fullscreen_error)?;

        let zoomed = self
            .backend
            .window_zoomed_on_socket(workspace.socket_name.as_str(), main_pane.as_str())
            .map_err(fullscreen_error)?;
        if zoomed {
            if !was_zoomed {
                self.backend
                    .enter_copy_mode(&workspace, &main_pane)
                    .map_err(fullscreen_error)?;
            }
            self.apply_fullscreen_status_line(&workspace)?;
            self.layout_runtime
                .refresh_workspace_chrome(&workspace, &workspace_dir)?;
        } else {
            self.clear_fullscreen_status_line(&workspace)?;
            self.layout_runtime
                .refresh_workspace_chrome(&workspace, &workspace_dir)?;
        }

        Ok(())
    }

    fn apply_fullscreen_status_line(
        &self,
        workspace: &TmuxWorkspaceHandle,
    ) -> Result<(), LifecycleError> {
        self.backend
            .set_session_option(
                workspace,
                TMUX_STATUS_FORMAT_OPTION,
                TMUX_FULLSCREEN_STATUS_FORMAT,
            )
            .map_err(fullscreen_error)
    }

    fn prepare_fullscreen_history_entry(
        &self,
        workspace: &TmuxWorkspaceHandle,
        main_pane: &TmuxPaneId,
    ) -> Result<(), LifecycleError> {
        self.save_fullscreen_restore_state(workspace, main_pane)?;
        self.backend
            .select_pane(workspace, main_pane)
            .map_err(fullscreen_error)
    }

    fn exit_fullscreen_history(
        &self,
        workspace: &TmuxWorkspaceHandle,
        main_pane: &TmuxPaneId,
    ) -> Result<(), LifecycleError> {
        if self.fullscreen_owns_copy_mode(workspace)?
            && self
                .backend
                .pane_in_mode_on_socket(workspace.socket_name.as_str(), main_pane.as_str())
                .map_err(fullscreen_error)?
        {
            self.backend
                .cancel_pane_mode_on_socket(workspace.socket_name.as_str(), main_pane.as_str())
                .map_err(fullscreen_error)?;
        }
        self.backend
            .select_pane(workspace, main_pane)
            .map_err(fullscreen_error)
    }

    fn clear_fullscreen_status_line(
        &self,
        workspace: &TmuxWorkspaceHandle,
    ) -> Result<(), LifecycleError> {
        match self.saved_status_restore_plan(workspace)? {
            StatusFormatRestorePlan::RestoreValue(value) => self
                .backend
                .set_session_option(workspace, TMUX_STATUS_FORMAT_OPTION, &value)
                .map_err(fullscreen_error)?,
            StatusFormatRestorePlan::UnsetIndex0 => self
                .backend
                .unset_session_option(workspace, TMUX_STATUS_FORMAT_OPTION)
                .map_err(fullscreen_error)?,
            StatusFormatRestorePlan::UnsetArray => self
                .backend
                .unset_session_option(workspace, TMUX_STATUS_FORMAT_ARRAY_OPTION)
                .map_err(fullscreen_error)?,
        }
        self.clear_fullscreen_restore_state(workspace)
    }

    fn save_fullscreen_restore_state(
        &self,
        workspace: &TmuxWorkspaceHandle,
        main_pane: &TmuxPaneId,
    ) -> Result<(), LifecycleError> {
        let pane_was_in_mode = self
            .backend
            .pane_in_mode_on_socket(workspace.socket_name.as_str(), main_pane.as_str())
            .map_err(fullscreen_error)?;
        self.backend
            .set_session_option(
                workspace,
                WAITAGENT_FULLSCREEN_OWNS_COPY_MODE_OPTION,
                bool_option_value(!pane_was_in_mode),
            )
            .map_err(fullscreen_error)?;

        let prior_status_format0 = self
            .backend
            .show_session_option(workspace, TMUX_STATUS_FORMAT_OPTION)
            .map_err(fullscreen_error)?;
        self.backend
            .set_session_option(
                workspace,
                WAITAGENT_FULLSCREEN_STATUS_FORMAT0_PRESENT_OPTION,
                bool_option_value(prior_status_format0.is_some()),
            )
            .map_err(fullscreen_error)?;
        if let Some(value) = prior_status_format0 {
            self.backend
                .set_session_option(
                    workspace,
                    WAITAGENT_FULLSCREEN_STATUS_FORMAT0_VALUE_OPTION,
                    &value,
                )
                .map_err(fullscreen_error)?;
        } else {
            let _ = self
                .backend
                .unset_session_option(workspace, WAITAGENT_FULLSCREEN_STATUS_FORMAT0_VALUE_OPTION);
        }

        let local_status_entries = self
            .backend
            .show_session_local_option_names(workspace, TMUX_STATUS_FORMAT_ARRAY_OPTION)
            .map_err(fullscreen_error)?;
        let has_other_local_entries = local_status_entries
            .iter()
            .any(|name| name != TMUX_STATUS_FORMAT_OPTION);
        self.backend
            .set_session_option(
                workspace,
                WAITAGENT_FULLSCREEN_STATUS_FORMAT_HAS_OTHER_OPTION,
                bool_option_value(has_other_local_entries),
            )
            .map_err(fullscreen_error)
    }

    fn fullscreen_owns_copy_mode(
        &self,
        workspace: &TmuxWorkspaceHandle,
    ) -> Result<bool, LifecycleError> {
        self.saved_bool_option(workspace, WAITAGENT_FULLSCREEN_OWNS_COPY_MODE_OPTION)
    }

    fn saved_status_restore_plan(
        &self,
        workspace: &TmuxWorkspaceHandle,
    ) -> Result<StatusFormatRestorePlan, LifecycleError> {
        let had_status_format0 = self.saved_bool_option(
            workspace,
            WAITAGENT_FULLSCREEN_STATUS_FORMAT0_PRESENT_OPTION,
        )?;
        if had_status_format0 {
            let value = self
                .backend
                .show_session_option(workspace, WAITAGENT_FULLSCREEN_STATUS_FORMAT0_VALUE_OPTION)
                .map_err(fullscreen_error)?
                .ok_or_else(|| {
                    LifecycleError::Protocol(
                        "fullscreen restore state is missing saved status-format[0]".to_string(),
                    )
                })?;
            return Ok(status_format_restore_plan(
                had_status_format0,
                Some(value),
                false,
            ));
        }

        let has_other_entries = self.saved_bool_option(
            workspace,
            WAITAGENT_FULLSCREEN_STATUS_FORMAT_HAS_OTHER_OPTION,
        )?;
        Ok(status_format_restore_plan(
            had_status_format0,
            None,
            has_other_entries,
        ))
    }

    fn saved_bool_option(
        &self,
        workspace: &TmuxWorkspaceHandle,
        option_name: &str,
    ) -> Result<bool, LifecycleError> {
        Ok(self
            .backend
            .show_session_option(workspace, option_name)
            .map_err(fullscreen_error)?
            .as_deref()
            == Some("1"))
    }

    fn clear_fullscreen_restore_state(
        &self,
        workspace: &TmuxWorkspaceHandle,
    ) -> Result<(), LifecycleError> {
        for option_name in [
            WAITAGENT_FULLSCREEN_OWNS_COPY_MODE_OPTION,
            WAITAGENT_FULLSCREEN_STATUS_FORMAT0_PRESENT_OPTION,
            WAITAGENT_FULLSCREEN_STATUS_FORMAT0_VALUE_OPTION,
            WAITAGENT_FULLSCREEN_STATUS_FORMAT_HAS_OTHER_OPTION,
        ] {
            self.backend
                .unset_session_option(workspace, option_name)
                .map_err(fullscreen_error)?;
        }
        Ok(())
    }

    fn resolve_session(
        &self,
        socket_name: &str,
        session_name: &str,
    ) -> Result<ManagedSessionRecord, LifecycleError> {
        self.session_service
            .list_sessions_on_socket(&TmuxSocketName::new(socket_name))
            .map_err(fullscreen_error)?
            .into_iter()
            .find(|session| session.address.session_id() == session_name)
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
            .map_err(fullscreen_error)?
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

fn fullscreen_error(error: TmuxError) -> LifecycleError {
    LifecycleError::Io(
        "native tmux fullscreen command failed".to_string(),
        io::Error::new(io::ErrorKind::Other, error.to_string()),
    )
}

fn bool_option_value(value: bool) -> &'static str {
    if value {
        "1"
    } else {
        "0"
    }
}

fn status_format_restore_plan(
    had_status_format0: bool,
    saved_status_format0: Option<String>,
    has_other_local_entries: bool,
) -> StatusFormatRestorePlan {
    if had_status_format0 {
        return StatusFormatRestorePlan::RestoreValue(
            saved_status_format0.expect("status-format[0] value must exist when marked present"),
        );
    }

    if has_other_local_entries {
        StatusFormatRestorePlan::UnsetIndex0
    } else {
        StatusFormatRestorePlan::UnsetArray
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum StatusFormatRestorePlan {
    RestoreValue(String),
    UnsetIndex0,
    UnsetArray,
}

#[cfg(test)]
mod tests {
    use super::{status_format_restore_plan, StatusFormatRestorePlan};

    #[test]
    fn restore_plan_uses_saved_value_when_status_format0_existed() {
        assert_eq!(
            status_format_restore_plan(true, Some("custom".to_string()), false),
            StatusFormatRestorePlan::RestoreValue("custom".to_string())
        );
    }

    #[test]
    fn restore_plan_unsets_index0_when_other_local_entries_exist() {
        assert_eq!(
            status_format_restore_plan(false, None, true),
            StatusFormatRestorePlan::UnsetIndex0
        );
    }

    #[test]
    fn restore_plan_unsets_array_when_no_local_entries_existed_before_fullscreen() {
        assert_eq!(
            status_format_restore_plan(false, None, false),
            StatusFormatRestorePlan::UnsetArray
        );
    }
}
