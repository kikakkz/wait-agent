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
        let args = bind_main_pane_zoom_toggle_args(key, pane);
        self.run_workspace_command(workspace, &args)?;
        Ok(())
    }

    fn bind_waitagent_focus_sidebar(
        &self,
        workspace: &TmuxWorkspaceHandle,
        key: &str,
        main: &TmuxPaneId,
        sidebar: &TmuxPaneId,
        sidebar_width: u16,
    ) -> Result<(), Self::Error> {
        let args = bind_waitagent_focus_sidebar_args(key, main, sidebar, sidebar_width);
        self.run_workspace_command(workspace, &args)?;
        Ok(())
    }

    fn bind_waitagent_focus_main(
        &self,
        workspace: &TmuxWorkspaceHandle,
        key: &str,
        main: &TmuxPaneId,
    ) -> Result<(), Self::Error> {
        let args = bind_waitagent_focus_main_args(key, main);
        self.run_workspace_command(workspace, &args)?;
        Ok(())
    }

    fn bind_waitagent_sidebar_back(
        &self,
        workspace: &TmuxWorkspaceHandle,
        key: &str,
        sidebar: &TmuxPaneId,
        main: &TmuxPaneId,
    ) -> Result<(), Self::Error> {
        let args = bind_waitagent_sidebar_back_args(key, sidebar, main);
        self.run_workspace_command(workspace, &args)?;
        Ok(())
    }

    fn bind_waitagent_sidebar_hide(
        &self,
        workspace: &TmuxWorkspaceHandle,
        key: &str,
        sidebar: &TmuxPaneId,
        main: &TmuxPaneId,
        collapsed_width: u16,
    ) -> Result<(), Self::Error> {
        let args = bind_waitagent_sidebar_hide_args(key, sidebar, main, collapsed_width);
        self.run_workspace_command(workspace, &args)?;
        Ok(())
    }

    fn bind_waitagent_footer_action(
        &self,
        workspace: &TmuxWorkspaceHandle,
        key: &str,
        footer: &TmuxPaneId,
        command: &str,
    ) -> Result<(), Self::Error> {
        let args = bind_waitagent_footer_action_args(key, footer, command);
        self.run_workspace_command(workspace, &args)?;
        Ok(())
    }
}

fn bind_main_pane_zoom_toggle_args(key: &str, pane: &TmuxPaneId) -> Vec<String> {
    bind_key_args(
        key,
        false,
        vec![
            "select-pane".to_string(),
            "-t".to_string(),
            pane.as_str().to_string(),
            "\\;".to_string(),
            "resize-pane".to_string(),
            "-t".to_string(),
            pane.as_str().to_string(),
            "-Z".to_string(),
        ],
    )
}

fn bind_waitagent_focus_sidebar_args(
    key: &str,
    main: &TmuxPaneId,
    sidebar: &TmuxPaneId,
    sidebar_width: u16,
) -> Vec<String> {
    bind_key_args(
        key,
        false,
        vec![
            "if-shell".to_string(),
            "-F".to_string(),
            current_pane_is(main),
            focus_sidebar_command(sidebar, sidebar_width),
            format!("send-keys {key}"),
        ],
    )
}

fn bind_waitagent_focus_main_args(key: &str, main: &TmuxPaneId) -> Vec<String> {
    bind_key_args(
        key,
        true,
        vec![
            "select-pane".to_string(),
            "-t".to_string(),
            main.as_str().to_string(),
        ],
    )
}

fn bind_waitagent_sidebar_back_args(
    key: &str,
    sidebar: &TmuxPaneId,
    main: &TmuxPaneId,
) -> Vec<String> {
    bind_key_args(
        key,
        false,
        vec![
            "if-shell".to_string(),
            "-F".to_string(),
            current_pane_is(sidebar),
            format!("select-pane -t {}", main.as_str()),
            format!("send-keys {}", key),
        ],
    )
}

fn bind_waitagent_sidebar_hide_args(
    key: &str,
    sidebar: &TmuxPaneId,
    main: &TmuxPaneId,
    collapsed_width: u16,
) -> Vec<String> {
    bind_key_args(
        key,
        false,
        vec![
            "if-shell".to_string(),
            "-F".to_string(),
            current_pane_is(sidebar),
            format!(
                "resize-pane -t {} -x {} ; select-pane -t {}",
                sidebar.as_str(),
                collapsed_width,
                main.as_str()
            ),
            format!("send-keys {}", key),
        ],
    )
}

fn bind_waitagent_footer_action_args(key: &str, footer: &TmuxPaneId, command: &str) -> Vec<String> {
    bind_key_args(
        key,
        false,
        vec![
            "if-shell".to_string(),
            "-F".to_string(),
            current_pane_is(footer),
            command.to_string(),
            format!("send-keys {}", key),
        ],
    )
}

fn focus_sidebar_command(sidebar: &TmuxPaneId, sidebar_width: u16) -> String {
    format!(
        "resize-pane -t {} -x {} ; select-pane -t {}",
        sidebar.as_str(),
        sidebar_width,
        sidebar.as_str()
    )
}

fn bind_key_args(key: &str, requires_prefix: bool, commands: Vec<String>) -> Vec<String> {
    let mut args = vec!["bind-key".to_string()];
    if !requires_prefix {
        args.push("-n".to_string());
    }
    args.push(key.to_string());
    args.extend(commands);
    args
}

fn current_pane_is(pane: &TmuxPaneId) -> String {
    format!("#{{==:#{{pane_id}},{}}}", pane.as_str())
}

#[cfg(test)]
mod tests {
    use super::{
        bind_main_pane_zoom_toggle_args, bind_waitagent_focus_main_args,
        bind_waitagent_focus_sidebar_args, bind_waitagent_footer_action_args,
        bind_waitagent_sidebar_back_args, bind_waitagent_sidebar_hide_args,
    };
    use crate::infra::tmux_types::TmuxPaneId;

    #[test]
    fn zoom_toggle_binding_targets_the_main_pane_without_prefix() {
        let args = bind_main_pane_zoom_toggle_args("C-o", &TmuxPaneId::new("%1"));

        assert_eq!(
            args,
            vec![
                "bind-key",
                "-n",
                "C-o",
                "select-pane",
                "-t",
                "%1",
                "\\;",
                "resize-pane",
                "-t",
                "%1",
                "-Z",
            ]
        );
    }

    #[test]
    fn sidebar_focus_binding_restores_width_before_selecting_sidebar() {
        let args = bind_waitagent_focus_sidebar_args(
            "Right",
            &TmuxPaneId::new("%1"),
            &TmuxPaneId::new("%2"),
            24,
        );

        assert_eq!(
            args,
            vec![
                "bind-key",
                "-n",
                "Right",
                "if-shell",
                "-F",
                "#{==:#{pane_id},%1}",
                "resize-pane -t %2 -x 24 ; select-pane -t %2",
                "send-keys Right",
            ]
        );
    }

    #[test]
    fn main_focus_binding_uses_prefixed_left_to_return_to_the_main_pane() {
        let args = bind_waitagent_focus_main_args("Left", &TmuxPaneId::new("%1"));

        assert_eq!(args, vec!["bind-key", "Left", "select-pane", "-t", "%1"]);
    }

    #[test]
    fn sidebar_back_binding_only_claims_left_when_sidebar_is_focused() {
        let args = bind_waitagent_sidebar_back_args(
            "Left",
            &TmuxPaneId::new("%2"),
            &TmuxPaneId::new("%1"),
        );

        assert_eq!(
            args,
            vec![
                "bind-key",
                "-n",
                "Left",
                "if-shell",
                "-F",
                "#{==:#{pane_id},%2}",
                "select-pane -t %1",
                "send-keys Left",
            ]
        );
    }

    #[test]
    fn sidebar_hide_binding_collapses_sidebar_and_otherwise_passes_h_through() {
        let args = bind_waitagent_sidebar_hide_args(
            "h",
            &TmuxPaneId::new("%2"),
            &TmuxPaneId::new("%1"),
            1,
        );

        assert_eq!(
            args,
            vec![
                "bind-key",
                "-n",
                "h",
                "if-shell",
                "-F",
                "#{==:#{pane_id},%2}",
                "resize-pane -t %2 -x 1 ; select-pane -t %1",
                "send-keys h",
            ]
        );
    }

    #[test]
    fn footer_action_binding_only_claims_keys_when_footer_is_focused() {
        let args = bind_waitagent_footer_action_args(
            "s",
            &TmuxPaneId::new("%3"),
            "run-shell 'waitagent __footer-menu'",
        );

        assert_eq!(
            args,
            vec![
                "bind-key",
                "-n",
                "s",
                "if-shell",
                "-F",
                "#{==:#{pane_id},%3}",
                "run-shell 'waitagent __footer-menu'",
                "send-keys s",
            ]
        );
    }
}
