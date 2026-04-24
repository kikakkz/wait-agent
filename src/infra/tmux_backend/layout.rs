use super::{parse_tmux_id, validate_percent, EmbeddedTmuxBackend, TmuxError};
use crate::infra::tmux_types::{
    TmuxLayoutGateway, TmuxPaneId, TmuxPaneInfo, TmuxProgram, TmuxSplitSize, TmuxWindowHandle,
    TmuxWindowId, TmuxWorkspaceHandle,
};

impl TmuxLayoutGateway for EmbeddedTmuxBackend {
    fn current_window(
        &self,
        workspace: &TmuxWorkspaceHandle,
    ) -> Result<TmuxWindowHandle, Self::Error> {
        let args = vec![
            "display-message".to_string(),
            "-p".to_string(),
            "-t".to_string(),
            workspace.session_name.as_str().to_string(),
            "#{window_id}".to_string(),
        ];
        let output = self.run_workspace_command(workspace, &args)?;
        Ok(TmuxWindowHandle {
            workspace_id: workspace.workspace_id.clone(),
            window_id: TmuxWindowId::new(parse_tmux_id(&output.stdout, '@', "window id")?),
        })
    }

    fn current_pane(&self, workspace: &TmuxWorkspaceHandle) -> Result<TmuxPaneId, Self::Error> {
        let args = vec![
            "display-message".to_string(),
            "-p".to_string(),
            "-t".to_string(),
            workspace.session_name.as_str().to_string(),
            "#{pane_id}".to_string(),
        ];
        let output = self.run_workspace_command(workspace, &args)?;
        Ok(TmuxPaneId::new(parse_tmux_id(
            &output.stdout,
            '%',
            "pane id",
        )?))
    }

    fn list_panes(
        &self,
        workspace: &TmuxWorkspaceHandle,
        window: &TmuxWindowHandle,
    ) -> Result<Vec<TmuxPaneInfo>, Self::Error> {
        let args = vec![
            "list-panes".to_string(),
            "-t".to_string(),
            window.window_id.as_str().to_string(),
            "-F".to_string(),
            "#{pane_id}\t#{pane_title}\t#{pane_current_command}\t#{pane_current_path}\t#{pane_dead}"
                .to_string(),
        ];
        let output = self.run_workspace_command(workspace, &args)?;
        output
            .stdout
            .lines()
            .map(Self::pane_info_for_line)
            .collect::<Result<Vec<_>, _>>()
    }

    fn split_pane_right_with_program(
        &self,
        workspace: &TmuxWorkspaceHandle,
        pane: &TmuxPaneId,
        width: TmuxSplitSize,
        program: &TmuxProgram,
    ) -> Result<TmuxPaneId, Self::Error> {
        validate_split_size(&width, "right split width")?;
        let args = self.tmux_program_args(
            &[
                "split-window".to_string(),
                "-d".to_string(),
                "-P".to_string(),
                "-F".to_string(),
                "#{pane_id}".to_string(),
                "-t".to_string(),
                pane.as_str().to_string(),
                "-h".to_string(),
                "-l".to_string(),
                width.to_tmux_size(),
            ],
            program,
        );
        let output = self.run_workspace_command(workspace, &args)?;
        Ok(TmuxPaneId::new(parse_tmux_id(
            &output.stdout,
            '%',
            "pane id",
        )?))
    }

    fn split_pane_bottom_with_program(
        &self,
        workspace: &TmuxWorkspaceHandle,
        pane: &TmuxPaneId,
        height: TmuxSplitSize,
        full_width: bool,
        program: &TmuxProgram,
    ) -> Result<TmuxPaneId, Self::Error> {
        validate_split_size(&height, "bottom split height")?;
        let mut base_args = vec![
            "split-window".to_string(),
            "-d".to_string(),
            "-P".to_string(),
            "-F".to_string(),
            "#{pane_id}".to_string(),
            "-t".to_string(),
            pane.as_str().to_string(),
            "-v".to_string(),
            "-l".to_string(),
            height.to_tmux_size(),
        ];
        if full_width {
            base_args.push("-f".to_string());
        }
        let output =
            self.run_workspace_command(workspace, &self.tmux_program_args(&base_args, program))?;
        Ok(TmuxPaneId::new(parse_tmux_id(
            &output.stdout,
            '%',
            "pane id",
        )?))
    }

    fn respawn_pane(
        &self,
        workspace: &TmuxWorkspaceHandle,
        pane: &TmuxPaneId,
        program: &TmuxProgram,
    ) -> Result<(), Self::Error> {
        let args = self.tmux_program_args(
            &[
                "respawn-pane".to_string(),
                "-k".to_string(),
                "-t".to_string(),
                pane.as_str().to_string(),
            ],
            program,
        );
        self.run_workspace_command(workspace, &args)?;
        Ok(())
    }

    fn set_pane_title(
        &self,
        workspace: &TmuxWorkspaceHandle,
        pane: &TmuxPaneId,
        title: &str,
    ) -> Result<(), Self::Error> {
        let args = vec![
            "select-pane".to_string(),
            "-t".to_string(),
            pane.as_str().to_string(),
            "-T".to_string(),
            title.to_string(),
        ];
        self.run_workspace_command(workspace, &args)?;
        Ok(())
    }

    fn set_pane_width(
        &self,
        workspace: &TmuxWorkspaceHandle,
        pane: &TmuxPaneId,
        width: u16,
    ) -> Result<(), Self::Error> {
        let args = vec![
            "resize-pane".to_string(),
            "-t".to_string(),
            pane.as_str().to_string(),
            "-x".to_string(),
            width.to_string(),
        ];
        self.run_workspace_command(workspace, &args)?;
        Ok(())
    }

    fn set_pane_height(
        &self,
        workspace: &TmuxWorkspaceHandle,
        pane: &TmuxPaneId,
        height: u16,
    ) -> Result<(), Self::Error> {
        let args = vec![
            "resize-pane".to_string(),
            "-t".to_string(),
            pane.as_str().to_string(),
            "-y".to_string(),
            height.to_string(),
        ];
        self.run_workspace_command(workspace, &args)?;
        Ok(())
    }

    fn set_pane_style(
        &self,
        workspace: &TmuxWorkspaceHandle,
        pane: &TmuxPaneId,
        style: &str,
    ) -> Result<(), Self::Error> {
        let args = vec![
            "select-pane".to_string(),
            "-t".to_string(),
            pane.as_str().to_string(),
            "-P".to_string(),
            style.to_string(),
        ];
        self.run_workspace_command(workspace, &args)?;
        Ok(())
    }

    fn set_session_hook(
        &self,
        workspace: &TmuxWorkspaceHandle,
        hook_name: &str,
        command: &str,
    ) -> Result<(), Self::Error> {
        let args = vec![
            "set-hook".to_string(),
            "-t".to_string(),
            workspace.session_name.as_str().to_string(),
            hook_name.to_string(),
            command.to_string(),
        ];
        self.run_workspace_command(workspace, &args)?;
        Ok(())
    }

    fn set_pane_hook(
        &self,
        workspace: &TmuxWorkspaceHandle,
        pane: &TmuxPaneId,
        hook_name: &str,
        command: &str,
    ) -> Result<(), Self::Error> {
        let args = vec![
            "set-hook".to_string(),
            "-t".to_string(),
            pane.as_str().to_string(),
            hook_name.to_string(),
            command.to_string(),
        ];
        self.run_workspace_command(workspace, &args)?;
        Ok(())
    }

    fn set_global_hook(
        &self,
        workspace: &TmuxWorkspaceHandle,
        hook_name: &str,
        command: &str,
    ) -> Result<(), Self::Error> {
        let args = vec![
            "set-hook".to_string(),
            "-g".to_string(),
            hook_name.to_string(),
            command.to_string(),
        ];
        self.run_workspace_command(workspace, &args)?;
        Ok(())
    }

    fn set_session_option(
        &self,
        workspace: &TmuxWorkspaceHandle,
        option_name: &str,
        value: &str,
    ) -> Result<(), Self::Error> {
        let args = vec![
            "set-option".to_string(),
            "-t".to_string(),
            workspace.session_name.as_str().to_string(),
            option_name.to_string(),
            value.to_string(),
        ];
        self.run_workspace_command(workspace, &args)?;
        Ok(())
    }

    fn set_window_option(
        &self,
        workspace: &TmuxWorkspaceHandle,
        window: &TmuxWindowHandle,
        option_name: &str,
        value: &str,
    ) -> Result<(), Self::Error> {
        let args = vec![
            "set-option".to_string(),
            "-w".to_string(),
            "-t".to_string(),
            window.window_id.as_str().to_string(),
            option_name.to_string(),
            value.to_string(),
        ];
        self.run_workspace_command(workspace, &args)?;
        Ok(())
    }
}

fn validate_split_size(size: &TmuxSplitSize, label: &str) -> Result<(), TmuxError> {
    match size {
        TmuxSplitSize::Cells(value) if *value > 0 => Ok(()),
        TmuxSplitSize::Percent(value) => validate_percent(*value, label),
        TmuxSplitSize::Cells(value) => Err(TmuxError::new(format!(
            "{label} must be at least 1 cell, got {value}"
        ))),
    }
}
