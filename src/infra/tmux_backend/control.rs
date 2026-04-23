use super::EmbeddedTmuxBackend;
use crate::infra::tmux_types::{TmuxControlGateway, TmuxPaneId, TmuxWorkspaceHandle};

impl TmuxControlGateway for EmbeddedTmuxBackend {
    fn bind_key_without_prefix(
        &self,
        workspace: &TmuxWorkspaceHandle,
        key: &str,
        command_and_args: &[String],
    ) -> Result<(), Self::Error> {
        let mut args = vec!["bind-key".to_string(), "-n".to_string(), key.to_string()];
        args.extend(command_and_args.iter().cloned());
        self.run_workspace_command(workspace, &args)?;
        Ok(())
    }

    fn bind_main_pane_zoom_toggle(
        &self,
        workspace: &TmuxWorkspaceHandle,
        key: &str,
        pane: &TmuxPaneId,
    ) -> Result<(), Self::Error> {
        let args = vec![
            "bind-key".to_string(),
            "-n".to_string(),
            key.to_string(),
            "select-pane".to_string(),
            "-t".to_string(),
            pane.as_str().to_string(),
            "\\;".to_string(),
            "resize-pane".to_string(),
            "-t".to_string(),
            pane.as_str().to_string(),
            "-Z".to_string(),
        ];
        self.run_workspace_command(workspace, &args)?;
        Ok(())
    }
}
