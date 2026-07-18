use super::{
    parse_tmux_id, validate_percent, EmbeddedTmuxBackend, TmuxError,
    WAITAGENT_PANE_PIPE_OWNER_OPTION,
};
use crate::infra::tmux_types::{
    TmuxLayoutGateway, TmuxPaneId, TmuxPaneInfo, TmuxProgram, TmuxSplitSize, TmuxWindowHandle,
    TmuxWindowId, TmuxWorkspaceHandle,
};

impl EmbeddedTmuxBackend {
    pub(crate) fn sync_content_pane_geometries(
        &self,
        workspace: &TmuxWorkspaceHandle,
        current_window_id: Option<&str>,
        geometry: (u16, u16),
    ) -> Result<(), TmuxError> {
        let (width, height) = geometry;
        let output = self.run_on_socket(
            &workspace.socket_name,
            &[
                "list-panes".to_string(),
                "-a".to_string(),
                "-F".to_string(),
                {
                    let role_option = super::WAITAGENT_PANE_ROLE_OPTION;
                    format!(
                        "#{{pane_id}}\t#{{pane_dead}}\t#{{pane_title}}\t#{{pane_width}}\t#{{pane_height}}\t#{{pane_window_id}}\t#{{{role_option}}}"
                    )
                },
            ],
        )?;
        for line in output.stdout.lines() {
            let mut parts = line.split('\t');
            let pane_id = parts.next().unwrap_or_default();
            let pane_dead = parts.next().unwrap_or_default();
            let title = parts.next().unwrap_or_default();
            let pane_width = parts.next().and_then(|value| value.parse::<u16>().ok());
            let pane_height = parts.next().and_then(|value| value.parse::<u16>().ok());
            let window_id = parts.next().unwrap_or_default();
            let role = parts.next().unwrap_or_default();
            if pane_id.is_empty()
                || pane_dead != "0"
                || title == super::WAITAGENT_SIDEBAR_PANE_TITLE
                || title == super::WAITAGENT_FOOTER_PANE_TITLE
                || role != super::WAITAGENT_PANE_ROLE_CONTENT
            {
                continue;
            }
            if pane_width == Some(width) && pane_height == Some(height) {
                continue;
            }
            if current_window_id == Some(window_id) {
                continue;
            }
            let pane = TmuxPaneId::new(pane_id);
            self.resize_window_to_geometry(workspace, window_id, width, height)?;
            self.resize_pane_to_geometry(workspace, &pane, width, height)?;
        }
        Ok(())
    }

    fn resize_window_to_geometry(
        &self,
        workspace: &TmuxWorkspaceHandle,
        window_id: &str,
        width: u16,
        height: u16,
    ) -> Result<(), TmuxError> {
        self.run_workspace_command(
            workspace,
            &[
                "resize-window".to_string(),
                "-t".to_string(),
                window_id.to_string(),
                "-x".to_string(),
                width.to_string(),
                "-y".to_string(),
                height.to_string(),
            ],
        )
        .map(|_| ())
    }

    fn resize_pane_to_geometry(
        &self,
        workspace: &TmuxWorkspaceHandle,
        pane: &TmuxPaneId,
        width: u16,
        height: u16,
    ) -> Result<(), TmuxError> {
        self.run_workspace_command(
            workspace,
            &[
                "resize-pane".to_string(),
                "-t".to_string(),
                pane.as_str().to_string(),
                "-x".to_string(),
                width.to_string(),
                ";".to_string(),
                "resize-pane".to_string(),
                "-t".to_string(),
                pane.as_str().to_string(),
                "-y".to_string(),
                height.to_string(),
            ],
        )
        .map(|_| ())
    }

    pub(crate) fn set_global_hook_on_socket(
        &self,
        socket_name: &str,
        hook_name: &str,
        command: &str,
    ) -> Result<(), TmuxError> {
        self.run_on_socket(
            &crate::infra::tmux::TmuxSocketName::new(socket_name),
            &[
                "set-hook".to_string(),
                "-g".to_string(),
                hook_name.to_string(),
                command.to_string(),
            ],
        )?;
        Ok(())
    }

    #[allow(dead_code)]
    pub(crate) fn break_pane_to_window(
        &self,
        workspace: &TmuxWorkspaceHandle,
        pane: &TmuxPaneId,
        window_name: Option<&str>,
    ) -> Result<(TmuxWindowHandle, TmuxPaneId), TmuxError> {
        let output = self.run_workspace_command(workspace, &break_pane_args(pane, window_name))?;
        parse_break_pane_result(workspace, &output.stdout)
    }

    #[allow(dead_code)]
    #[allow(dead_code)]
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

    pub(crate) fn pane_is_alive(
        &self,
        workspace: &TmuxWorkspaceHandle,
        pane_id: &TmuxPaneId,
    ) -> Result<bool, TmuxError> {
        let output = self.run_workspace_command(
            workspace,
            &[
                "display-message".to_string(),
                "-p".to_string(),
                "-t".to_string(),
                pane_id.as_str().to_string(),
                "#{pane_dead}".to_string(),
            ],
        )?;
        Ok(output.stdout.trim() == "0")
    }

    pub(crate) fn pane_liveness_on_socket(
        &self,
        socket_name: &crate::infra::tmux::TmuxSocketName,
        pane_id: &str,
    ) -> Result<Option<bool>, TmuxError> {
        let output = match self.run_on_socket(
            socket_name,
            &[
                "display-message".to_string(),
                "-p".to_string(),
                "-t".to_string(),
                pane_id.to_string(),
                "#{pane_dead}".to_string(),
            ],
        ) {
            Ok(output) => output,
            Err(error) if error.is_command_failure() => return Ok(None),
            Err(error) => return Err(error),
        };
        Ok(Some(output.stdout.trim() == "0"))
    }

    #[allow(dead_code)]
    pub(crate) fn kill_pane(
        &self,
        workspace: &TmuxWorkspaceHandle,
        pane: &TmuxPaneId,
    ) -> Result<(), TmuxError> {
        self.run_workspace_command(workspace, &kill_pane_args(pane))?;
        Ok(())
    }

    pub(crate) fn clear_pane_pipe_if_owner(
        &self,
        workspace: &TmuxWorkspaceHandle,
        pane: &TmuxPaneId,
        expected_owner: &str,
    ) -> Result<bool, TmuxError> {
        let owner = self.show_pane_option_on_socket(
            &workspace.socket_name,
            pane,
            WAITAGENT_PANE_PIPE_OWNER_OPTION,
        )?;
        if owner.as_deref() != Some(expected_owner) {
            return Ok(false);
        }
        self.run_workspace_command(workspace, &clear_pane_pipe_args(pane))?;
        self.unset_pane_option_on_socket(
            &workspace.socket_name,
            pane,
            WAITAGENT_PANE_PIPE_OWNER_OPTION,
        )?;
        Ok(true)
    }

    pub(crate) fn set_pane_pipe_owned(
        &self,
        workspace: &TmuxWorkspaceHandle,
        pane: &TmuxPaneId,
        owner: &str,
        command: &str,
    ) -> Result<(), TmuxError> {
        self.run_workspace_command(workspace, &clear_pane_pipe_args(pane))?;
        self.set_pane_option_on_socket(
            &workspace.socket_name,
            pane,
            WAITAGENT_PANE_PIPE_OWNER_OPTION,
            owner,
        )?;
        self.run_workspace_command(workspace, &set_pane_pipe_args(pane, command))?;
        Ok(())
    }

    pub(crate) fn set_pane_pipe_owned_if_available(
        &self,
        workspace: &TmuxWorkspaceHandle,
        pane: &TmuxPaneId,
        owner: &str,
        command: &str,
    ) -> Result<bool, TmuxError> {
        let current_owner = self.show_pane_option_on_socket(
            &workspace.socket_name,
            pane,
            WAITAGENT_PANE_PIPE_OWNER_OPTION,
        )?;
        let pipe_state = self.pane_pipe_state(workspace, pane)?;
        if current_owner.is_some() && current_owner.as_deref() != Some(owner) {
            return Ok(false);
        }
        if current_owner.is_none() && pipe_state != "0" {
            return Ok(false);
        }
        self.set_pane_pipe_owned(workspace, pane, owner, command)?;
        Ok(true)
    }

    #[allow(dead_code)]
    pub(crate) fn pane_pipe_state(
        &self,
        workspace: &TmuxWorkspaceHandle,
        pane: &TmuxPaneId,
    ) -> Result<String, TmuxError> {
        let output = self.run_workspace_command(
            workspace,
            &[
                "display-message".to_string(),
                "-p".to_string(),
                "-t".to_string(),
                pane.as_str().to_string(),
                "#{pane_pipe}".to_string(),
            ],
        )?;
        Ok(output.stdout.trim().to_string())
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

    fn unset_pane_option(
        &self,
        workspace: &TmuxWorkspaceHandle,
        pane: &TmuxPaneId,
        option_name: &str,
    ) -> Result<(), Self::Error> {
        let args = vec![
            "set-option".to_string(),
            "-p".to_string(),
            "-u".to_string(),
            "-t".to_string(),
            pane.as_str().to_string(),
            option_name.to_string(),
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

    fn unset_pane_hook(
        &self,
        workspace: &TmuxWorkspaceHandle,
        pane: &TmuxPaneId,
        hook_name: &str,
    ) -> Result<(), Self::Error> {
        let args = vec![
            "set-hook".to_string(),
            "-u".to_string(),
            "-p".to_string(),
            "-t".to_string(),
            pane.as_str().to_string(),
            hook_name.to_string(),
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
            super::exact_session_target(workspace.session_name.as_str()),
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

#[allow(dead_code)]
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

#[allow(dead_code)]
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

#[allow(dead_code)]
fn kill_pane_args(pane: &TmuxPaneId) -> Vec<String> {
    vec![
        "kill-pane".to_string(),
        "-t".to_string(),
        pane.as_str().to_string(),
    ]
}

fn clear_pane_pipe_args(pane: &TmuxPaneId) -> Vec<String> {
    vec![
        "pipe-pane".to_string(),
        "-t".to_string(),
        pane.as_str().to_string(),
    ]
}

fn set_pane_pipe_args(pane: &TmuxPaneId, command: &str) -> Vec<String> {
    vec![
        "pipe-pane".to_string(),
        "-O".to_string(),
        "-t".to_string(),
        pane.as_str().to_string(),
        command.to_string(),
    ]
}

fn set_pane_hook_args(pane: &TmuxPaneId, hook_name: &str, command: &str) -> Vec<String> {
    let target = pane.as_str();
    // Use the session name (everything before ":") for the -t target
    // so the hook fires for all panes in the session, not just one pane.
    let session_target = target.split(':').next().unwrap_or(target);
    vec![
        "set-hook".to_string(),
        "-p".to_string(),
        "-t".to_string(),
        session_target.to_string(),
        hook_name.to_string(),
        command.to_string(),
    ]
}

#[allow(dead_code)]
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
        parse_break_pane_result, set_pane_hook_args, set_pane_pipe_args, swap_panes_args,
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
            clear_pane_pipe_args(&TmuxPaneId::new("%2")),
            vec!["pipe-pane", "-t", "%2"]
        );
        assert_eq!(
            set_pane_pipe_args(&TmuxPaneId::new("%2"), "echo refresh"),
            vec!["pipe-pane", "-O", "-t", "%2", "echo refresh"]
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
