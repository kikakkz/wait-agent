use crate::infra::tmux::{TmuxPaneId, TmuxWindowHandle};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceChromeLayout {
    pub window: TmuxWindowHandle,
    pub main_pane: TmuxPaneId,
    pub sidebar_pane: TmuxPaneId,
    pub footer_pane: TmuxPaneId,
}
