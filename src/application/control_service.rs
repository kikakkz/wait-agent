use crate::domain::workspace_layout::WorkspaceChromeLayout;
use crate::infra::tmux::{TmuxControlGateway, TmuxWorkspaceHandle};

const FULLSCREEN_TOGGLE_KEY: &str = "C-o";
const TMUX_MOUSE_OPTION: &str = "mouse";
const TMUX_OPTION_ON: &str = "on";

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
    ) -> Result<(), G::Error> {
        self.tmux
            .set_session_option(workspace, TMUX_MOUSE_OPTION, TMUX_OPTION_ON)?;
        self.bind_main_pane_fullscreen_toggle(workspace, layout)
    }

    fn bind_main_pane_fullscreen_toggle(
        &self,
        workspace: &TmuxWorkspaceHandle,
        layout: &WorkspaceChromeLayout,
    ) -> Result<(), G::Error> {
        self.tmux
            .bind_main_pane_zoom_toggle(workspace, FULLSCREEN_TOGGLE_KEY, &layout.main_pane)
    }
}

#[cfg(test)]
mod tests {
    use super::ControlService;
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
        BindWithoutPrefix(String, Vec<String>),
        BindMainPaneZoomToggle(String, String),
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

        fn toggle_zoom(
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

        fn set_session_hook(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            _hook_name: &str,
            _command: &str,
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

        fn bind_main_pane_zoom_toggle(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            key: &str,
            pane: &TmuxPaneId,
        ) -> Result<(), Self::Error> {
            self.calls.borrow_mut().push(Call::BindMainPaneZoomToggle(
                key.to_string(),
                pane.as_str().to_string(),
            ));
            Ok(())
        }
    }

    #[test]
    fn control_service_enables_mouse_and_binds_ctrl_o_to_main_pane_zoom() {
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
        };

        service
            .ensure_native_controls(&workspace, &layout)
            .expect("control configuration should succeed");

        assert_eq!(
            gateway.calls(),
            vec![
                Call::SetSessionOption("mouse".to_string(), "on".to_string()),
                Call::BindMainPaneZoomToggle("C-o".to_string(), "%1".to_string()),
            ]
        );
    }
}
