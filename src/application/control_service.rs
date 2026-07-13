use crate::domain::workspace_layout::WorkspaceChromeLayout;
use crate::infra::tmux::{TmuxControlGateway, TmuxPaneId, TmuxWorkspaceHandle};
use crate::ui::chrome::{TMUX_MENU_BORDER_STYLE, TMUX_MENU_SELECTED_STYLE, TMUX_MENU_STYLE};

const HISTORY_TOGGLE_KEY: &str = "C-o";
const HISTORY_TOGGLE_PREFIX_KEY: &str = "z";
const SIDEBAR_FOCUS_KEY: &str = "C-Right";
const MAIN_FOCUS_KEY: &str = "Left";
const SIDEBAR_TOGGLE_KEY: &str = "C-h";
const SIDEBAR_HIDE_KEY: &str = "h";
const CREATE_SESSION_KEY: &str = "C-n";
const CREATE_SESSION_PREFIX_KEY: &str = "c";
const CONNECT_REMOTE_HOST_KEY: &str = "C-w";
const CREATE_REMOTE_SESSION_KEY: &str = "C-s";
const FOOTER_SESSIONS_KEY: &str = "C-m";
const ERROR_LOG_KEY: &str = "C-e";
const ERROR_LOG_PREFIX_KEY: &str = "E";
// Ctrl-M opens the footer sessions menu when the footer pane is focused.
// Outside the footer pane, the key is forwarded to the active program.
const TMUX_STATUS_OPTION: &str = "status";
const TMUX_STATUS_ON: &str = "on";
const TMUX_STATUS_POSITION_OPTION: &str = "status-position";
const TMUX_STATUS_BOTTOM: &str = "bottom";
const TMUX_AUTOMATIC_RENAME_OPTION: &str = "automatic-rename";
const TMUX_OPTION_OFF: &str = "off";
const TMUX_MENU_STYLE_OPTION: &str = "menu-style";
const TMUX_MENU_SELECTED_STYLE_OPTION: &str = "menu-selected-style";
const TMUX_MENU_BORDER_STYLE_OPTION: &str = "menu-border-style";

pub struct FooterMenuBindings {
    pub create_session_command: String,
    pub connect_remote_host_command: String,
    pub create_remote_session_command: String,
    pub open_sessions_menu_command: String,
    pub error_log_command: String,
}

pub struct SidebarBindings {
    pub toggle_command: String,
    pub hide_command: String,
    pub show_command: String,
}

pub struct ControlService<G> {
    tmux: G,
}

impl<G> ControlService<G>
where
    G: TmuxControlGateway,
{
    pub fn new(tmux: G) -> Self {
        Self { tmux }
    }

    pub fn ensure_native_controls(
        &self,
        workspace: &TmuxWorkspaceHandle,
        layout: &WorkspaceChromeLayout,
        history_toggle_command: &str,
        sidebar_bindings: Option<&SidebarBindings>,
        footer_bindings: Option<&FooterMenuBindings>,
    ) -> Result<(), G::Error> {
        self.configure_session_chrome(workspace, layout)?;
        self.bind_main_pane_history_toggle(workspace, history_toggle_command)?;
        if let Some(sidebar_bindings) = sidebar_bindings {
            self.bind_waitagent_sidebar_controls(workspace, layout, sidebar_bindings)?;
        }
        self.bind_waitagent_footer_controls(workspace, layout, footer_bindings)
    }

    pub fn sync_workspace_controls(
        &self,
        workspace: &TmuxWorkspaceHandle,
        layout: &WorkspaceChromeLayout,
        sidebar_bindings: Option<&SidebarBindings>,
        footer_bindings: Option<&FooterMenuBindings>,
    ) -> Result<(), G::Error> {
        if let Some(sidebar_bindings) = sidebar_bindings {
            self.bind_waitagent_sidebar_controls(workspace, layout, sidebar_bindings)?;
        }
        self.bind_waitagent_footer_controls(workspace, layout, footer_bindings)
    }

    pub fn sync_workspace_controls_sidebar_hidden(
        &self,
        workspace: &TmuxWorkspaceHandle,
        main_pane: &TmuxPaneId,
        footer_pane: &TmuxPaneId,
        sidebar_bindings: &SidebarBindings,
        footer_bindings: Option<&FooterMenuBindings>,
    ) -> Result<(), G::Error> {
        self.tmux.bind_waitagent_sidebar_toggle(
            workspace,
            SIDEBAR_TOGGLE_KEY,
            main_pane,
            &sidebar_bindings.toggle_command,
        )?;
        self.tmux.bind_waitagent_sidebar_show(
            workspace,
            SIDEBAR_FOCUS_KEY,
            main_pane,
            &sidebar_bindings.show_command,
        )?;
        self.bind_waitagent_footer_controls_with_panes(
            workspace,
            footer_pane,
            footer_bindings,
        )
    }

    fn configure_session_chrome(
        &self,
        workspace: &TmuxWorkspaceHandle,
        layout: &WorkspaceChromeLayout,
    ) -> Result<(), G::Error> {
        self.tmux
            .set_session_option(workspace, TMUX_STATUS_OPTION, TMUX_STATUS_ON)?;
        self.tmux
            .set_session_option(workspace, TMUX_STATUS_POSITION_OPTION, TMUX_STATUS_BOTTOM)?;
        self.tmux.set_window_option(
            workspace,
            &layout.window,
            TMUX_AUTOMATIC_RENAME_OPTION,
            TMUX_OPTION_OFF,
        )?;
        self.tmux.set_window_option(
            workspace,
            &layout.window,
            TMUX_MENU_STYLE_OPTION,
            TMUX_MENU_STYLE,
        )?;
        self.tmux.set_window_option(
            workspace,
            &layout.window,
            TMUX_MENU_SELECTED_STYLE_OPTION,
            TMUX_MENU_SELECTED_STYLE,
        )?;
        self.tmux.set_window_option(
            workspace,
            &layout.window,
            TMUX_MENU_BORDER_STYLE_OPTION,
            TMUX_MENU_BORDER_STYLE,
        )
    }

    fn bind_main_pane_history_toggle(
        &self,
        workspace: &TmuxWorkspaceHandle,
        command: &str,
    ) -> Result<(), G::Error> {
        self.tmux
            .bind_key_without_prefix(workspace, HISTORY_TOGGLE_KEY, &[command.to_string()])?;
        self.tmux
            .bind_command_with_prefix(workspace, HISTORY_TOGGLE_PREFIX_KEY, command)?;
        for table in ["copy-mode", "copy-mode-vi"] {
            self.tmux
                .bind_copy_mode_cancel_key(workspace, table, HISTORY_TOGGLE_KEY)?;
            self.tmux
                .bind_copy_mode_cancel_key(workspace, table, "Escape")?;
        }
        Ok(())
    }

    fn bind_waitagent_sidebar_controls(
        &self,
        workspace: &TmuxWorkspaceHandle,
        layout: &WorkspaceChromeLayout,
        sidebar_bindings: &SidebarBindings,
    ) -> Result<(), G::Error> {
        self.tmux.bind_waitagent_sidebar_toggle(
            workspace,
            SIDEBAR_TOGGLE_KEY,
            &layout.main_pane,
            &sidebar_bindings.toggle_command,
        )?;
        self.tmux.bind_waitagent_focus_sidebar(
            workspace,
            SIDEBAR_FOCUS_KEY,
            &layout.main_pane,
            &layout.sidebar_pane,
            layout.sidebar_width,
        )?;
        self.tmux
            .bind_waitagent_focus_main(workspace, MAIN_FOCUS_KEY, &layout.main_pane)?;
        self.tmux.bind_waitagent_sidebar_back(
            workspace,
            MAIN_FOCUS_KEY,
            &layout.sidebar_pane,
            &layout.main_pane,
        )?;
        self.tmux.bind_waitagent_sidebar_hide(
            workspace,
            SIDEBAR_HIDE_KEY,
            &layout.sidebar_pane,
            &sidebar_bindings.hide_command,
        )
    }

    fn bind_waitagent_footer_controls(
        &self,
        workspace: &TmuxWorkspaceHandle,
        layout: &WorkspaceChromeLayout,
        footer_bindings: Option<&FooterMenuBindings>,
    ) -> Result<(), G::Error> {
        self.bind_waitagent_footer_controls_with_panes(
            workspace,
            &layout.footer_pane,
            footer_bindings,
        )
    }

    fn bind_waitagent_footer_controls_with_panes(
        &self,
        workspace: &TmuxWorkspaceHandle,
        footer_pane: &TmuxPaneId,
        footer_bindings: Option<&FooterMenuBindings>,
    ) -> Result<(), G::Error> {
        let Some(footer_bindings) = footer_bindings else {
            return Ok(());
        };

        self.tmux.bind_key_without_prefix(
            workspace,
            CREATE_SESSION_KEY,
            &[footer_bindings.create_session_command.clone()],
        )?;
        self.tmux.bind_command_with_prefix(
            workspace,
            CREATE_SESSION_PREFIX_KEY,
            &footer_bindings.create_session_command,
        )?;
        self.tmux.bind_key_without_prefix(
            workspace,
            CONNECT_REMOTE_HOST_KEY,
            &[footer_bindings.connect_remote_host_command.clone()],
        )?;
        self.tmux.bind_key_without_prefix(
            workspace,
            CREATE_REMOTE_SESSION_KEY,
            &[footer_bindings.create_remote_session_command.clone()],
        )?;
        self.tmux.bind_waitagent_footer_action(
            workspace,
            FOOTER_SESSIONS_KEY,
            footer_pane,
            &footer_bindings.open_sessions_menu_command,
        )?;
        self.tmux.bind_key_without_prefix(
            workspace,
            ERROR_LOG_KEY,
            &[footer_bindings.error_log_command.clone()],
        )?;
        self.tmux.bind_command_with_prefix(
            workspace,
            ERROR_LOG_PREFIX_KEY,
            &footer_bindings.error_log_command,
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{ControlService, FooterMenuBindings, SidebarBindings};
    use crate::domain::workspace::WorkspaceInstanceId;
    use crate::domain::workspace_layout::WorkspaceChromeLayout;
    use crate::infra::tmux::{
        TmuxControlGateway, TmuxGateway, TmuxLayoutGateway, TmuxPaneId, TmuxPaneInfo, TmuxProgram,
        TmuxSessionName, TmuxSocketName, TmuxWindowHandle, TmuxWindowId, TmuxWorkspaceHandle,
    };
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Call {
        SetSessionOption(String, String),
        SetWindowOption(String, String),
        BindWithoutPrefix(String, Vec<String>),
        BindCommandWithPrefix(String, String),
        BindWaitagentFocusSidebar(String, String, String, u16),
        BindWaitagentFocusMain(String, String),
        BindWaitagentSidebarBack(String, String, String),
        BindWaitagentSidebarHide(String, String, String),
        BindWaitagentSidebarToggle(String, String, String),
        BindWaitagentSidebarShow(String, String, String),
        BindWaitagentFooterAction(String, String, String),
        BindCopyModeCancelKey(String, String),
    }

    #[derive(Clone, Default)]
    struct FakeGateway {
        calls: Rc<RefCell<Vec<Call>>>,
    }

    impl FakeGateway {
        fn calls(&self) -> Vec<Call> {
            self.calls.borrow().clone()
        }
    }

    impl TmuxGateway for FakeGateway {
        type Error = &'static str;

        fn ensure_workspace(
            &self,
            _config: &crate::domain::workspace::WorkspaceInstanceConfig,
        ) -> Result<TmuxWorkspaceHandle, Self::Error> {
            unreachable!("not used")
        }

        fn create_window(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            _window_name: &str,
        ) -> Result<TmuxWindowHandle, Self::Error> {
            unreachable!("not used")
        }

        fn split_pane_right(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            _window: &TmuxWindowHandle,
            _width_percent: u8,
        ) -> Result<TmuxPaneId, Self::Error> {
            unreachable!("not used")
        }

        fn split_pane_bottom(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            _window: &TmuxWindowHandle,
            _height_percent: u8,
        ) -> Result<TmuxPaneId, Self::Error> {
            unreachable!("not used")
        }

        fn select_window(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            _window: &TmuxWindowHandle,
        ) -> Result<(), Self::Error> {
            unreachable!("not used")
        }

        fn select_pane(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            _pane: &TmuxPaneId,
        ) -> Result<(), Self::Error> {
            unreachable!("not used")
        }

        fn enter_copy_mode(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            _pane: &TmuxPaneId,
        ) -> Result<(), Self::Error> {
            unreachable!("not used")
        }
    }

    impl TmuxLayoutGateway for FakeGateway {
        fn current_window(
            &self,
            _workspace: &TmuxWorkspaceHandle,
        ) -> Result<TmuxWindowHandle, Self::Error> {
            unreachable!("not used")
        }

        fn current_pane(
            &self,
            _workspace: &TmuxWorkspaceHandle,
        ) -> Result<TmuxPaneId, Self::Error> {
            unreachable!("not used")
        }

        fn list_panes(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            _window: &TmuxWindowHandle,
        ) -> Result<Vec<TmuxPaneInfo>, Self::Error> {
            unreachable!("not used")
        }

        fn split_pane_right_with_program(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            _pane: &TmuxPaneId,
            _width: crate::infra::tmux::TmuxSplitSize,
            _program: &TmuxProgram,
        ) -> Result<TmuxPaneId, Self::Error> {
            unreachable!("not used")
        }

        fn split_pane_bottom_with_program(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            _pane: &TmuxPaneId,
            _height: crate::infra::tmux::TmuxSplitSize,
            _full_width: bool,
            _program: &TmuxProgram,
        ) -> Result<TmuxPaneId, Self::Error> {
            unreachable!("not used")
        }

        fn respawn_pane(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            _pane: &TmuxPaneId,
            _program: &TmuxProgram,
        ) -> Result<(), Self::Error> {
            unreachable!("not used")
        }

        fn set_pane_title(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            _pane: &TmuxPaneId,
            _title: &str,
        ) -> Result<(), Self::Error> {
            unreachable!("not used")
        }

        fn set_pane_width(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            _pane: &TmuxPaneId,
            _width: u16,
        ) -> Result<(), Self::Error> {
            unreachable!("not used")
        }

        fn set_pane_height(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            _pane: &TmuxPaneId,
            _height: u16,
        ) -> Result<(), Self::Error> {
            unreachable!("not used")
        }

        fn set_pane_style(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            _pane: &TmuxPaneId,
            _style: &str,
        ) -> Result<(), Self::Error> {
            unreachable!("not used")
        }

        fn set_pane_option(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            _pane: &TmuxPaneId,
            _option_name: &str,
            _value: &str,
        ) -> Result<(), Self::Error> {
            unreachable!("not used")
        }

        fn unset_pane_option(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            _pane: &TmuxPaneId,
            _option_name: &str,
        ) -> Result<(), Self::Error> {
            unreachable!("not used")
        }

        fn set_session_hook(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            _hook_name: &str,
            _command: &str,
        ) -> Result<(), Self::Error> {
            unreachable!("not used")
        }

        fn set_global_hook(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            _hook_name: &str,
            _command: &str,
        ) -> Result<(), Self::Error> {
            unreachable!("not used")
        }

        fn set_pane_hook(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            _pane: &TmuxPaneId,
            _hook_name: &str,
            _command: &str,
        ) -> Result<(), Self::Error> {
            unreachable!("not used")
        }

        fn unset_pane_hook(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            _pane: &TmuxPaneId,
            _hook_name: &str,
        ) -> Result<(), Self::Error> {
            unreachable!("not used")
        }

        fn set_session_option(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            option_name: &str,
            value: &str,
        ) -> Result<(), Self::Error> {
            self.calls.borrow_mut().push(Call::SetSessionOption(
                option_name.to_string(),
                value.to_string(),
            ));
            Ok(())
        }

        fn set_window_option(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            _window: &TmuxWindowHandle,
            option_name: &str,
            value: &str,
        ) -> Result<(), Self::Error> {
            self.calls.borrow_mut().push(Call::SetWindowOption(
                option_name.to_string(),
                value.to_string(),
            ));
            Ok(())
        }
    }

    impl TmuxControlGateway for FakeGateway {
        fn bind_key_without_prefix(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            key: &str,
            command_and_args: &[String],
        ) -> Result<(), Self::Error> {
            self.calls.borrow_mut().push(Call::BindWithoutPrefix(
                key.to_string(),
                command_and_args.to_vec(),
            ));
            Ok(())
        }

        fn bind_command_with_prefix(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            key: &str,
            command: &str,
        ) -> Result<(), Self::Error> {
            self.calls.borrow_mut().push(Call::BindCommandWithPrefix(
                key.to_string(),
                command.to_string(),
            ));
            Ok(())
        }

        fn bind_waitagent_focus_sidebar(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            key: &str,
            main: &TmuxPaneId,
            sidebar: &TmuxPaneId,
            sidebar_width: u16,
        ) -> Result<(), Self::Error> {
            self.calls
                .borrow_mut()
                .push(Call::BindWaitagentFocusSidebar(
                    key.to_string(),
                    main.as_str().to_string(),
                    sidebar.as_str().to_string(),
                    sidebar_width,
                ));
            Ok(())
        }

        fn bind_waitagent_sidebar_back(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            key: &str,
            sidebar: &TmuxPaneId,
            main: &TmuxPaneId,
        ) -> Result<(), Self::Error> {
            self.calls.borrow_mut().push(Call::BindWaitagentSidebarBack(
                key.to_string(),
                sidebar.as_str().to_string(),
                main.as_str().to_string(),
            ));
            Ok(())
        }

        fn bind_waitagent_focus_main(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            key: &str,
            main: &TmuxPaneId,
        ) -> Result<(), Self::Error> {
            self.calls.borrow_mut().push(Call::BindWaitagentFocusMain(
                key.to_string(),
                main.as_str().to_string(),
            ));
            Ok(())
        }

        fn bind_waitagent_sidebar_hide(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            key: &str,
            sidebar: &TmuxPaneId,
            command: &str,
        ) -> Result<(), Self::Error> {
            self.calls.borrow_mut().push(Call::BindWaitagentSidebarHide(
                key.to_string(),
                sidebar.as_str().to_string(),
                command.to_string(),
            ));
            Ok(())
        }

        fn bind_waitagent_sidebar_toggle(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            key: &str,
            main: &TmuxPaneId,
            command: &str,
        ) -> Result<(), Self::Error> {
            self.calls.borrow_mut().push(Call::BindWaitagentSidebarToggle(
                key.to_string(),
                main.as_str().to_string(),
                command.to_string(),
            ));
            Ok(())
        }

        fn bind_waitagent_sidebar_show(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            key: &str,
            main: &TmuxPaneId,
            command: &str,
        ) -> Result<(), Self::Error> {
            self.calls.borrow_mut().push(Call::BindWaitagentSidebarShow(
                key.to_string(),
                main.as_str().to_string(),
                command.to_string(),
            ));
            Ok(())
        }

        fn bind_waitagent_footer_action(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            key: &str,
            footer: &TmuxPaneId,
            command: &str,
        ) -> Result<(), Self::Error> {
            self.calls
                .borrow_mut()
                .push(Call::BindWaitagentFooterAction(
                    key.to_string(),
                    footer.as_str().to_string(),
                    command.to_string(),
                ));
            Ok(())
        }

        fn bind_copy_mode_cancel_key(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            table: &str,
            key: &str,
        ) -> Result<(), Self::Error> {
            self.calls.borrow_mut().push(Call::BindCopyModeCancelKey(
                table.to_string(),
                key.to_string(),
            ));
            Ok(())
        }
    }

    #[test]
    fn control_service_binds_ctrl_o_to_history_command() {
        let gateway = FakeGateway::default();
        let service = ControlService::new(gateway.clone());
        let workspace = TmuxWorkspaceHandle {
            workspace_id: WorkspaceInstanceId::new("wk-1"),
            socket_name: TmuxSocketName::new("wa-wk-1"),
            session_name: TmuxSessionName::new("waitagent-wk-1"),
        };
        let layout = WorkspaceChromeLayout {
            window: TmuxWindowHandle {
                workspace_id: workspace.workspace_id.clone(),
                window_id: TmuxWindowId::new("@1"),
            },
            main_pane: TmuxPaneId::new("%1"),
            sidebar_pane: TmuxPaneId::new("%2"),
            footer_pane: TmuxPaneId::new("%3"),
            sidebar_width: 32,
        };

        service
            .ensure_native_controls(
                &workspace,
                &layout,
                "run-shell -b \"waitagent __toggle-fullscreen\"",
                Some(&SidebarBindings {
                    toggle_command: "run-shell -b 'waitagent __toggle-sidebar --focus main'"
                        .to_string(),
                    hide_command: "run-shell -b 'waitagent __toggle-sidebar --focus main'"
                        .to_string(),
                    show_command: "run-shell -b 'waitagent __toggle-sidebar --focus sidebar'"
                        .to_string(),
                }),
                Some(&FooterMenuBindings {
                    create_session_command: "run-shell -b 'waitagent __new-target'".to_string(),
                    connect_remote_host_command: r#"display-popup -w 70% -h 70% -E '"waitagent" __connect-remote-host-pane'"#.to_string(),
                    create_remote_session_command: "run-shell -b 'waitagent __new-selected-remote-session'".to_string(),
                    open_sessions_menu_command: "run-shell 'waitagent __footer-menu'".to_string(),
                    error_log_command: "display-popup -w 80% -h 80% -E \"waitagent __error-log && echo '' && echo '--- Press ENTER to close ---' && read -r\"".to_string(),
                }),
            )
            .expect("control configuration should succeed");

        assert_eq!(
            gateway.calls(),
            vec![
                Call::SetSessionOption("status".to_string(), "on".to_string()),
                Call::SetSessionOption("status-position".to_string(), "bottom".to_string()),
                Call::SetWindowOption("automatic-rename".to_string(), "off".to_string()),
                Call::SetWindowOption(
                    "menu-style".to_string(),
                    "fg=colour250,bg=colour235".to_string(),
                ),
                Call::SetWindowOption(
                    "menu-selected-style".to_string(),
                    "fg=colour255,bg=colour31".to_string(),
                ),
                Call::SetWindowOption(
                    "menu-border-style".to_string(),
                    "fg=colour24,bg=colour235".to_string(),
                ),
                Call::BindWithoutPrefix(
                    "C-o".to_string(),
                    vec!["run-shell -b \"waitagent __toggle-fullscreen\"".to_string(),],
                ),
                Call::BindCommandWithPrefix(
                    "z".to_string(),
                    "run-shell -b \"waitagent __toggle-fullscreen\"".to_string(),
                ),
                Call::BindCopyModeCancelKey("copy-mode".to_string(), "C-o".to_string()),
                Call::BindCopyModeCancelKey("copy-mode".to_string(), "Escape".to_string()),
                Call::BindCopyModeCancelKey("copy-mode-vi".to_string(), "C-o".to_string()),
                Call::BindCopyModeCancelKey("copy-mode-vi".to_string(), "Escape".to_string()),
                Call::BindWaitagentSidebarToggle(
                    "C-h".to_string(),
                    "%1".to_string(),
                    "run-shell -b 'waitagent __toggle-sidebar --focus main'".to_string(),
                ),
                Call::BindWaitagentFocusSidebar(
                    "C-Right".to_string(),
                    "%1".to_string(),
                    "%2".to_string(),
                    32,
                ),
                Call::BindWaitagentFocusMain("Left".to_string(), "%1".to_string()),
                Call::BindWaitagentSidebarBack(
                    "Left".to_string(),
                    "%2".to_string(),
                    "%1".to_string(),
                ),
                Call::BindWaitagentSidebarHide(
                    "h".to_string(),
                    "%2".to_string(),
                    "run-shell -b 'waitagent __toggle-sidebar --focus main'".to_string(),
                ),
                Call::BindWithoutPrefix(
                    "C-n".to_string(),
                    vec!["run-shell -b 'waitagent __new-target'".to_string(),],
                ),
                Call::BindCommandWithPrefix(
                    "c".to_string(),
                    "run-shell -b 'waitagent __new-target'".to_string(),
                ),
                Call::BindWithoutPrefix(
                    "C-w".to_string(),
                    vec!["display-popup -w 70% -h 70% -E '\"waitagent\" __connect-remote-host-pane'".to_string(),],
                ),
                Call::BindWithoutPrefix(
                    "C-s".to_string(),
                    vec!["run-shell -b 'waitagent __new-selected-remote-session'".to_string(),],
                ),
                Call::BindWaitagentFooterAction(
                    "C-m".to_string(),
                    "%3".to_string(),
                    "run-shell 'waitagent __footer-menu'".to_string(),
                ),
                Call::BindWithoutPrefix(
                    "C-e".to_string(),
                    vec!["display-popup -w 80% -h 80% -E \"waitagent __error-log && echo '' && echo '--- Press ENTER to close ---' && read -r\"".to_string(),],
                ),
                Call::BindCommandWithPrefix(
                    "E".to_string(),
                    "display-popup -w 80% -h 80% -E \"waitagent __error-log && echo '' && echo '--- Press ENTER to close ---' && read -r\"".to_string(),
                ),
            ]
        );
    }
}
