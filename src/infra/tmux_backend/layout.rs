use super::{parse_tmux_id, validate_percent, EmbeddedTmuxBackend, TmuxError};
use crate::infra::tmux_types::{
    TmuxLayoutGateway, TmuxPaneId, TmuxPaneInfo, TmuxProgram, TmuxSplitSize, TmuxWindowHandle,
    TmuxWindowId, TmuxWorkspaceHandle,
};

impl EmbeddedTmuxBackend {
    pub(crate) fn break_pane_to_window(
        &self,
        workspace: &TmuxWorkspaceHandle,
        pane: &TmuxPaneId,
        window_name: Option<&str>,
    ) -> Result<(TmuxWindowHandle, TmuxPaneId), TmuxError> {
        let output = self.run_workspace_command(workspace, &break_pane_args(pane, window_name))?;
        parse_break_pane_result(workspace, &output.stdout)
    }

    pub(crate) fn join_pane(
        &self,
        workspace: &TmuxWorkspaceHandle,
        source: &TmuxPaneId,
        destination: &TmuxPaneId,
        full_size: bool,
    ) -> Result<(), TmuxError> {
        self.run_workspace_command(workspace, &join_pane_args(source, destination, full_size))?;
        Ok(())
    }

    pub(crate) fn swap_panes(
        &self,
        workspace: &TmuxWorkspaceHandle,
        source: &TmuxPaneId,
        destination: &TmuxPaneId,
    ) -> Result<(), TmuxError> {
        self.run_workspace_command(workspace, &swap_panes_args(source, destination))?;
        Ok(())
    }

    pub(crate) fn kill_pane(
        &self,
        workspace: &TmuxWorkspaceHandle,
        pane: &TmuxPaneId,
    ) -> Result<(), TmuxError> {
        self.run_workspace_command(workspace, &kill_pane_args(pane))?;
        Ok(())
    }

    pub(crate) fn pipe_pane_output(
        &self,
        workspace: &TmuxWorkspaceHandle,
        pane: &TmuxPaneId,
        shell_command: &str,
    ) -> Result<(), TmuxError> {
        self.run_workspace_command(workspace, &pipe_pane_output_args(pane, shell_command))?;
        Ok(())
    }

    pub(crate) fn clear_pane_pipe(
        &self,
        workspace: &TmuxWorkspaceHandle,
        pane: &TmuxPaneId,
    ) -> Result<(), TmuxError> {
        self.run_workspace_command(workspace, &clear_pane_pipe_args(pane))?;
        Ok(())
    }
}

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
            "#{pane_id}\t#{pane_pid}\t#{pane_title}\t#{pane_current_command}\t#{pane_current_path}\t#{pane_dead}"
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

    fn set_pane_option(
        &self,
        workspace: &TmuxWorkspaceHandle,
        pane: &TmuxPaneId,
        option_name: &str,
        value: &str,
    ) -> Result<(), Self::Error> {
        let args = vec![
            "set-option".to_string(),
            "-p".to_string(),
            "-t".to_string(),
            pane.as_str().to_string(),
            option_name.to_string(),
            value.to_string(),
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
        let args = set_pane_hook_args(pane, hook_name, command);
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

fn break_pane_args(pane: &TmuxPaneId, window_name: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "break-pane".to_string(),
        "-d".to_string(),
        "-P".to_string(),
        "-F".to_string(),
        "#{window_id}\t#{pane_id}".to_string(),
        "-s".to_string(),
        pane.as_str().to_string(),
    ];
    if let Some(window_name) = window_name {
        args.push("-n".to_string());
        args.push(window_name.to_string());
    }
    args
}

fn join_pane_args(source: &TmuxPaneId, destination: &TmuxPaneId, full_size: bool) -> Vec<String> {
    let mut args = vec![
        "join-pane".to_string(),
        "-d".to_string(),
        "-s".to_string(),
        source.as_str().to_string(),
        "-t".to_string(),
        destination.as_str().to_string(),
    ];
    if full_size {
        args.push("-f".to_string());
    }
    args
}

fn swap_panes_args(source: &TmuxPaneId, destination: &TmuxPaneId) -> Vec<String> {
    vec![
        "swap-pane".to_string(),
        "-d".to_string(),
        "-s".to_string(),
        source.as_str().to_string(),
        "-t".to_string(),
        destination.as_str().to_string(),
    ]
}

fn kill_pane_args(pane: &TmuxPaneId) -> Vec<String> {
    vec![
        "kill-pane".to_string(),
        "-t".to_string(),
        pane.as_str().to_string(),
    ]
}

fn pipe_pane_output_args(pane: &TmuxPaneId, shell_command: &str) -> Vec<String> {
    vec![
        "pipe-pane".to_string(),
        "-O".to_string(),
        "-t".to_string(),
        pane.as_str().to_string(),
        shell_command.to_string(),
    ]
}

fn clear_pane_pipe_args(pane: &TmuxPaneId) -> Vec<String> {
    vec![
        "pipe-pane".to_string(),
        "-t".to_string(),
        pane.as_str().to_string(),
    ]
}

fn set_pane_hook_args(pane: &TmuxPaneId, hook_name: &str, command: &str) -> Vec<String> {
    vec![
        "set-hook".to_string(),
        "-p".to_string(),
        "-t".to_string(),
        pane.as_str().to_string(),
        hook_name.to_string(),
        command.to_string(),
    ]
}

fn parse_break_pane_result(
    workspace: &TmuxWorkspaceHandle,
    output: &str,
) -> Result<(TmuxWindowHandle, TmuxPaneId), TmuxError> {
    let mut parts = output.trim().split('\t');
    let Some(window_id) = parts.next() else {
        return Err(TmuxError::new("tmux break-pane did not return a window id"));
    };
    let Some(pane_id) = parts.next() else {
        return Err(TmuxError::new("tmux break-pane did not return a pane id"));
    };

    Ok((
        TmuxWindowHandle {
            workspace_id: workspace.workspace_id.clone(),
            window_id: TmuxWindowId::new(parse_tmux_id(window_id, '@', "window id")?),
        },
        TmuxPaneId::new(parse_tmux_id(pane_id, '%', "pane id")?),
    ))
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

#[cfg(test)]
mod tests {
    use super::{
        break_pane_args, clear_pane_pipe_args, join_pane_args, kill_pane_args,
        parse_break_pane_result, pipe_pane_output_args, set_pane_hook_args, swap_panes_args,
    };
    use crate::domain::workspace::WorkspaceInstanceId;
    use crate::infra::tmux::{TmuxPaneId, TmuxSessionName, TmuxSocketName, TmuxWorkspaceHandle};

    #[test]
    fn break_pane_args_request_detached_window_and_parseable_output() {
        let args = break_pane_args(&TmuxPaneId::new("%7"), Some("waitagent-target"));

        assert_eq!(
            args,
            vec![
                "break-pane",
                "-d",
                "-P",
                "-F",
                "#{window_id}\t#{pane_id}",
                "-s",
                "%7",
                "-n",
                "waitagent-target",
            ]
        );
    }

    #[test]
    fn join_swap_and_kill_args_use_native_tmux_primitives() {
        assert_eq!(
            join_pane_args(&TmuxPaneId::new("%2"), &TmuxPaneId::new("%9"), true),
            vec!["join-pane", "-d", "-s", "%2", "-t", "%9", "-f"]
        );
        assert_eq!(
            swap_panes_args(&TmuxPaneId::new("%2"), &TmuxPaneId::new("%9")),
            vec!["swap-pane", "-d", "-s", "%2", "-t", "%9"]
        );
        assert_eq!(
            kill_pane_args(&TmuxPaneId::new("%2")),
            vec!["kill-pane", "-t", "%2"]
        );
        assert_eq!(
            pipe_pane_output_args(&TmuxPaneId::new("%2"), "waitagent __chrome-refresh-stream"),
            vec![
                "pipe-pane",
                "-O",
                "-t",
                "%2",
                "waitagent __chrome-refresh-stream",
            ]
        );
        assert_eq!(
            clear_pane_pipe_args(&TmuxPaneId::new("%2")),
            vec!["pipe-pane", "-t", "%2"]
        );
        assert_eq!(
            set_pane_hook_args(&TmuxPaneId::new("%2"), "pane-died", "run-shell true"),
            vec!["set-hook", "-p", "-t", "%2", "pane-died", "run-shell true",]
        );
    }

    #[test]
    fn parse_break_pane_result_returns_window_and_pane_handles() {
        let workspace = TmuxWorkspaceHandle {
            workspace_id: WorkspaceInstanceId::new("sess-1"),
            socket_name: TmuxSocketName::new("wa-1"),
            session_name: TmuxSessionName::new("sess-1"),
        };

        let (window, pane) =
            parse_break_pane_result(&workspace, "@4\t%11").expect("break-pane output should parse");

        assert_eq!(window.window_id.as_str(), "@4");
        assert_eq!(pane.as_str(), "%11");
    }
}
