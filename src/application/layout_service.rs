use crate::domain::workspace_layout::WorkspaceChromeLayout;
use crate::infra::tmux::{
    TmuxLayoutGateway, TmuxPaneId, TmuxPaneInfo, TmuxProgram, TmuxSplitSize, TmuxWorkspaceHandle,
};

pub const SIDEBAR_PANE_TITLE: &str = "waitagent-sidebar";
pub const FOOTER_PANE_TITLE: &str = "waitagent-footer";
const SIDEBAR_PANE_STYLE: &str = "fg=colour250,bg=colour234";
const SESSION_LAYOUT_RECONCILE_HOOKS: [&str; 1] = ["client-resized"];
const GLOBAL_LAYOUT_RECONCILE_HOOKS: [&str; 2] = ["session-created", "session-closed"];
const MAIN_PANE_GLOBAL_REFRESH_HOOKS: [&str; 1] = ["pane-exited"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutFocusBehavior {
    ReturnToMain,
    PreserveCurrent,
}

pub struct LayoutService<G> {
    tmux: G,
    sidebar_width: TmuxSplitSize,
    footer_height: TmuxSplitSize,
}

impl<G> LayoutService<G>
where
    G: TmuxLayoutGateway,
{
    pub fn new(tmux: G) -> Self {
        Self {
            tmux,
            sidebar_width: TmuxSplitSize::Cells(24),
            footer_height: TmuxSplitSize::Cells(1),
        }
    }

    pub fn ensure_workspace_layout(
        &self,
        workspace: &TmuxWorkspaceHandle,
        sidebar_program: &TmuxProgram,
        footer_program: &TmuxProgram,
        focus_behavior: LayoutFocusBehavior,
    ) -> Result<WorkspaceChromeLayout, G::Error> {
        let window = self.tmux.current_window(workspace)?;
        let current_pane = self.tmux.current_pane(workspace)?;
        let panes = self.tmux.list_panes(workspace, &window)?;
        let main_pane = main_pane_id(&panes).unwrap_or_else(|| current_pane.clone());

        let sidebar_pane = match pane_with_title(&panes, SIDEBAR_PANE_TITLE) {
            Some(pane) => {
                if pane.is_dead {
                    self.tmux
                        .respawn_pane(workspace, &pane.pane_id, sidebar_program)?;
                }
                pane.pane_id.clone()
            }
            None => self.tmux.split_pane_right_with_program(
                workspace,
                &main_pane,
                self.sidebar_width.clone(),
                sidebar_program,
            )?,
        };
        self.tmux
            .set_pane_title(workspace, &sidebar_pane, SIDEBAR_PANE_TITLE)?;
        self.tmux
            .set_pane_style(workspace, &sidebar_pane, SIDEBAR_PANE_STYLE)?;
        apply_width(&self.tmux, workspace, &sidebar_pane, &self.sidebar_width)?;

        let panes = self.tmux.list_panes(workspace, &window)?;
        let footer_pane = match pane_with_title(&panes, FOOTER_PANE_TITLE) {
            Some(pane) => {
                if pane.is_dead {
                    self.tmux
                        .respawn_pane(workspace, &pane.pane_id, footer_program)?;
                }
                pane.pane_id.clone()
            }
            None => self.tmux.split_pane_bottom_with_program(
                workspace,
                &main_pane,
                self.footer_height.clone(),
                true,
                footer_program,
            )?,
        };
        self.tmux
            .set_pane_title(workspace, &footer_pane, FOOTER_PANE_TITLE)?;
        apply_height(&self.tmux, workspace, &footer_pane, &self.footer_height)?;
        let target_pane = match focus_behavior {
            LayoutFocusBehavior::ReturnToMain => &main_pane,
            LayoutFocusBehavior::PreserveCurrent => &current_pane,
        };
        self.tmux.select_pane(workspace, target_pane)?;

        Ok(WorkspaceChromeLayout {
            window,
            main_pane,
            sidebar_pane,
            footer_pane,
            sidebar_width: self.sidebar_width.cells_or_default(),
        })
    }

    pub fn ensure_layout_hooks(
        &self,
        workspace: &TmuxWorkspaceHandle,
        main_pane: &TmuxPaneId,
        reconcile_command: &str,
        global_reconcile_command: &str,
        pane_exit_command: &str,
    ) -> Result<(), G::Error> {
        for hook_name in SESSION_LAYOUT_RECONCILE_HOOKS {
            self.tmux
                .set_session_hook(workspace, hook_name, reconcile_command)?;
        }
        for hook_name in GLOBAL_LAYOUT_RECONCILE_HOOKS {
            self.tmux
                .set_global_hook(workspace, hook_name, global_reconcile_command)?;
        }
        for hook_name in MAIN_PANE_GLOBAL_REFRESH_HOOKS {
            self.tmux
                .set_pane_hook(workspace, main_pane, hook_name, pane_exit_command)?;
        }
        Ok(())
    }
}

fn pane_with_title<'a>(panes: &'a [TmuxPaneInfo], title: &str) -> Option<&'a TmuxPaneInfo> {
    panes.iter().find(|pane| pane.title == title)
}

fn main_pane_id(panes: &[TmuxPaneInfo]) -> Option<TmuxPaneId> {
    panes
        .iter()
        .find(|pane| pane.title != SIDEBAR_PANE_TITLE && pane.title != FOOTER_PANE_TITLE)
        .map(|pane| pane.pane_id.clone())
}

fn apply_width<G>(
    tmux: &G,
    workspace: &TmuxWorkspaceHandle,
    pane: &TmuxPaneId,
    width: &TmuxSplitSize,
) -> Result<(), G::Error>
where
    G: TmuxLayoutGateway,
{
    if let TmuxSplitSize::Cells(width) = width {
        tmux.set_pane_width(workspace, pane, *width)?;
    }
    Ok(())
}

fn apply_height<G>(
    tmux: &G,
    workspace: &TmuxWorkspaceHandle,
    pane: &TmuxPaneId,
    height: &TmuxSplitSize,
) -> Result<(), G::Error>
where
    G: TmuxLayoutGateway,
{
    if let TmuxSplitSize::Cells(height) = height {
        tmux.set_pane_height(workspace, pane, *height)?;
    }
    Ok(())
}

trait SplitSizeCells {
    fn cells_or_default(&self) -> u16;
}

impl SplitSizeCells for TmuxSplitSize {
    fn cells_or_default(&self) -> u16 {
        match self {
            Self::Cells(value) => *value,
            Self::Percent(_) => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{LayoutFocusBehavior, LayoutService, FOOTER_PANE_TITLE, SIDEBAR_PANE_TITLE};
    use crate::domain::workspace::WorkspaceInstanceId;
    use crate::infra::tmux::{
        TmuxGateway, TmuxLayoutGateway, TmuxPaneId, TmuxPaneInfo, TmuxProgram, TmuxSessionName,
        TmuxSocketName, TmuxSplitSize, TmuxWindowHandle, TmuxWindowId, TmuxWorkspaceHandle,
    };
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Call {
        SplitRight,
        SplitBottom,
        Respawn(String),
        SetTitle(String),
        SetPaneStyle(String, String),
        SetWidth(String, u16),
        SetHeight(String, u16),
        SetHook(String, String),
        SetPaneHook(String, String, String),
        SetGlobalHook(String, String),
        SetSessionOption(String, String),
        SelectMain(String),
    }

    #[derive(Clone)]
    struct FakeGateway {
        panes: Rc<RefCell<Vec<TmuxPaneInfo>>>,
        calls: Rc<RefCell<Vec<Call>>>,
        current_pane: Rc<RefCell<TmuxPaneId>>,
    }

    impl FakeGateway {
        fn new(panes: Vec<TmuxPaneInfo>) -> Self {
            Self {
                panes: Rc::new(RefCell::new(panes)),
                calls: Rc::new(RefCell::new(Vec::new())),
                current_pane: Rc::new(RefCell::new(TmuxPaneId::new("%1"))),
            }
        }

        fn calls(&self) -> Vec<Call> {
            self.calls.borrow().clone()
        }

        fn with_current_pane(self, pane: &str) -> Self {
            *self.current_pane.borrow_mut() = TmuxPaneId::new(pane);
            self
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
            Ok(())
        }

        fn select_pane(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            pane: &TmuxPaneId,
        ) -> Result<(), Self::Error> {
            self.calls
                .borrow_mut()
                .push(Call::SelectMain(pane.as_str().to_string()));
            Ok(())
        }

        fn toggle_zoom(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            _pane: &TmuxPaneId,
        ) -> Result<(), Self::Error> {
            Ok(())
        }

        fn enter_copy_mode(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            _pane: &TmuxPaneId,
        ) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    impl TmuxLayoutGateway for FakeGateway {
        fn current_window(
            &self,
            workspace: &TmuxWorkspaceHandle,
        ) -> Result<TmuxWindowHandle, Self::Error> {
            Ok(TmuxWindowHandle {
                workspace_id: workspace.workspace_id.clone(),
                window_id: TmuxWindowId::new("@1"),
            })
        }

        fn current_pane(
            &self,
            _workspace: &TmuxWorkspaceHandle,
        ) -> Result<TmuxPaneId, Self::Error> {
            Ok(self.current_pane.borrow().clone())
        }

        fn list_panes(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            _window: &TmuxWindowHandle,
        ) -> Result<Vec<TmuxPaneInfo>, Self::Error> {
            Ok(self.panes.borrow().clone())
        }

        fn split_pane_right_with_program(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            _pane: &TmuxPaneId,
            _width: TmuxSplitSize,
            _program: &TmuxProgram,
        ) -> Result<TmuxPaneId, Self::Error> {
            self.calls.borrow_mut().push(Call::SplitRight);
            let pane = TmuxPaneId::new("%2");
            self.panes.borrow_mut().push(TmuxPaneInfo {
                pane_id: pane.clone(),
                title: SIDEBAR_PANE_TITLE.to_string(),
                current_command: Some("waitagent".to_string()),
                current_path: None,
                is_dead: false,
            });
            Ok(pane)
        }

        fn split_pane_bottom_with_program(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            _pane: &TmuxPaneId,
            _height: TmuxSplitSize,
            _full_width: bool,
            _program: &TmuxProgram,
        ) -> Result<TmuxPaneId, Self::Error> {
            self.calls.borrow_mut().push(Call::SplitBottom);
            let pane = TmuxPaneId::new("%3");
            self.panes.borrow_mut().push(TmuxPaneInfo {
                pane_id: pane.clone(),
                title: FOOTER_PANE_TITLE.to_string(),
                current_command: Some("waitagent".to_string()),
                current_path: None,
                is_dead: false,
            });
            Ok(pane)
        }

        fn respawn_pane(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            pane: &TmuxPaneId,
            _program: &TmuxProgram,
        ) -> Result<(), Self::Error> {
            self.calls
                .borrow_mut()
                .push(Call::Respawn(pane.as_str().to_string()));
            Ok(())
        }

        fn set_pane_title(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            pane: &TmuxPaneId,
            _title: &str,
        ) -> Result<(), Self::Error> {
            self.calls
                .borrow_mut()
                .push(Call::SetTitle(pane.as_str().to_string()));
            Ok(())
        }

        fn set_pane_width(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            pane: &TmuxPaneId,
            width: u16,
        ) -> Result<(), Self::Error> {
            self.calls
                .borrow_mut()
                .push(Call::SetWidth(pane.as_str().to_string(), width));
            Ok(())
        }

        fn set_pane_height(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            pane: &TmuxPaneId,
            height: u16,
        ) -> Result<(), Self::Error> {
            self.calls
                .borrow_mut()
                .push(Call::SetHeight(pane.as_str().to_string(), height));
            Ok(())
        }

        fn set_pane_style(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            pane: &TmuxPaneId,
            style: &str,
        ) -> Result<(), Self::Error> {
            self.calls.borrow_mut().push(Call::SetPaneStyle(
                pane.as_str().to_string(),
                style.to_string(),
            ));
            Ok(())
        }

        fn set_session_hook(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            hook_name: &str,
            command: &str,
        ) -> Result<(), Self::Error> {
            self.calls
                .borrow_mut()
                .push(Call::SetHook(hook_name.to_string(), command.to_string()));
            Ok(())
        }

        fn set_global_hook(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            hook_name: &str,
            command: &str,
        ) -> Result<(), Self::Error> {
            self.calls.borrow_mut().push(Call::SetGlobalHook(
                hook_name.to_string(),
                command.to_string(),
            ));
            Ok(())
        }

        fn set_pane_hook(
            &self,
            _workspace: &TmuxWorkspaceHandle,
            pane: &TmuxPaneId,
            hook_name: &str,
            command: &str,
        ) -> Result<(), Self::Error> {
            self.calls.borrow_mut().push(Call::SetPaneHook(
                pane.as_str().to_string(),
                hook_name.to_string(),
                command.to_string(),
            ));
            Ok(())
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
            _option_name: &str,
            _value: &str,
        ) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    fn workspace() -> TmuxWorkspaceHandle {
        TmuxWorkspaceHandle {
            workspace_id: WorkspaceInstanceId::new("wk-1"),
            socket_name: TmuxSocketName::new("wa-wk-1"),
            session_name: TmuxSessionName::new("waitagent-wk-1"),
        }
    }

    #[test]
    fn layout_service_creates_missing_sidebar_and_footer_and_returns_focus_to_main() {
        let gateway = FakeGateway::new(vec![TmuxPaneInfo {
            pane_id: TmuxPaneId::new("%1"),
            title: String::new(),
            current_command: Some("bash".to_string()),
            current_path: None,
            is_dead: false,
        }]);
        let service = LayoutService::new(gateway.clone());
        let program = TmuxProgram::new("/tmp/waitagent");

        let layout = service
            .ensure_workspace_layout(
                &workspace(),
                &program,
                &program,
                LayoutFocusBehavior::ReturnToMain,
            )
            .expect("layout should be created");

        assert_eq!(layout.main_pane.as_str(), "%1");
        assert_eq!(layout.sidebar_pane.as_str(), "%2");
        assert_eq!(layout.footer_pane.as_str(), "%3");
        assert_eq!(
            gateway.calls(),
            vec![
                Call::SplitRight,
                Call::SetTitle("%2".to_string()),
                Call::SetPaneStyle("%2".to_string(), "fg=colour250,bg=colour234".to_string()),
                Call::SetWidth("%2".to_string(), 24),
                Call::SplitBottom,
                Call::SetTitle("%3".to_string()),
                Call::SetHeight("%3".to_string(), 1),
                Call::SelectMain("%1".to_string()),
            ]
        );
    }

    #[test]
    fn layout_service_respawns_dead_chrome_panes_without_recreating_them() {
        let gateway = FakeGateway::new(vec![
            TmuxPaneInfo {
                pane_id: TmuxPaneId::new("%1"),
                title: String::new(),
                current_command: Some("bash".to_string()),
                current_path: None,
                is_dead: false,
            },
            TmuxPaneInfo {
                pane_id: TmuxPaneId::new("%2"),
                title: SIDEBAR_PANE_TITLE.to_string(),
                current_command: Some("waitagent".to_string()),
                current_path: None,
                is_dead: true,
            },
            TmuxPaneInfo {
                pane_id: TmuxPaneId::new("%3"),
                title: FOOTER_PANE_TITLE.to_string(),
                current_command: Some("waitagent".to_string()),
                current_path: None,
                is_dead: true,
            },
        ]);
        let service = LayoutService::new(gateway.clone());
        let program = TmuxProgram::new("/tmp/waitagent");

        service
            .ensure_workspace_layout(
                &workspace(),
                &program,
                &program,
                LayoutFocusBehavior::ReturnToMain,
            )
            .expect("layout should be restored");

        assert_eq!(
            gateway.calls(),
            vec![
                Call::Respawn("%2".to_string()),
                Call::SetTitle("%2".to_string()),
                Call::SetPaneStyle("%2".to_string(), "fg=colour250,bg=colour234".to_string()),
                Call::SetWidth("%2".to_string(), 24),
                Call::Respawn("%3".to_string()),
                Call::SetTitle("%3".to_string()),
                Call::SetHeight("%3".to_string(), 1),
                Call::SelectMain("%1".to_string()),
            ]
        );
    }

    #[test]
    fn layout_service_preserves_focus_during_reconcile_mode() {
        let gateway = FakeGateway::new(vec![
            TmuxPaneInfo {
                pane_id: TmuxPaneId::new("%1"),
                title: String::new(),
                current_command: Some("bash".to_string()),
                current_path: None,
                is_dead: false,
            },
            TmuxPaneInfo {
                pane_id: TmuxPaneId::new("%2"),
                title: SIDEBAR_PANE_TITLE.to_string(),
                current_command: Some("waitagent".to_string()),
                current_path: None,
                is_dead: false,
            },
            TmuxPaneInfo {
                pane_id: TmuxPaneId::new("%3"),
                title: FOOTER_PANE_TITLE.to_string(),
                current_command: Some("waitagent".to_string()),
                current_path: None,
                is_dead: false,
            },
        ])
        .with_current_pane("%3");
        let service = LayoutService::new(gateway.clone());
        let program = TmuxProgram::new("/tmp/waitagent");

        service
            .ensure_workspace_layout(
                &workspace(),
                &program,
                &program,
                LayoutFocusBehavior::PreserveCurrent,
            )
            .expect("layout should reconcile");

        assert_eq!(
            gateway.calls(),
            vec![
                Call::SetTitle("%2".to_string()),
                Call::SetPaneStyle("%2".to_string(), "fg=colour250,bg=colour234".to_string()),
                Call::SetWidth("%2".to_string(), 24),
                Call::SetTitle("%3".to_string()),
                Call::SetHeight("%3".to_string(), 1),
                Call::SelectMain("%3".to_string()),
            ]
        );
    }

    #[test]
    fn layout_service_registers_native_reconcile_hooks() {
        let gateway = FakeGateway::new(vec![]);
        let service = LayoutService::new(gateway.clone());

        service
            .ensure_layout_hooks(
                &workspace(),
                &TmuxPaneId::new("%1"),
                "run-shell -b 'waitagent __layout-reconcile'",
                "run-shell -b 'waitagent __chrome-refresh-all'",
                "run-shell -b 'waitagent __close-session --socket-name wa-wk-1 --session-name waitagent-wk-1'",
            )
            .expect("hook registration should succeed");

        assert_eq!(
            gateway.calls(),
            vec![
                Call::SetHook(
                    "client-resized".to_string(),
                    "run-shell -b 'waitagent __layout-reconcile'".to_string(),
                ),
                Call::SetGlobalHook(
                    "session-created".to_string(),
                    "run-shell -b 'waitagent __chrome-refresh-all'".to_string(),
                ),
                Call::SetGlobalHook(
                    "session-closed".to_string(),
                    "run-shell -b 'waitagent __chrome-refresh-all'".to_string(),
                ),
                Call::SetPaneHook(
                    "%1".to_string(),
                    "pane-exited".to_string(),
                    "run-shell -b 'waitagent __close-session --socket-name wa-wk-1 --session-name waitagent-wk-1'".to_string(),
                ),
            ]
        );
    }
}
