use crate::domain::local_runtime::{
    LocalRuntimeConsumer, LocalRuntimeEventKind, LocalRuntimeProducer,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventDrivenRuntimeModule {
    TmuxHookBridge,
    WorkspaceController,
    SessionCatalogProjector,
    SidebarPaneRuntime,
    FooterPaneRuntime,
    MainSlotRuntime,
    AttachClientRuntime,
    SchedulerRuntime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventDrivenFlow {
    pub producer: LocalRuntimeProducer,
    pub event_kind: LocalRuntimeEventKind,
    pub consumers: Vec<LocalRuntimeConsumer>,
    pub owner: EventDrivenRuntimeModule,
    pub purpose: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoricalPollingPath {
    pub module_path: &'static str,
    pub current_mechanism: &'static str,
    pub replacement_owner: EventDrivenRuntimeModule,
    pub replacement_reason: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventDrivenRuntimeContract {
    pub modules: Vec<EventDrivenRuntimeModule>,
    pub flows: Vec<EventDrivenFlow>,
    pub historical_polling_paths: Vec<HistoricalPollingPath>,
}

pub fn default_event_driven_runtime_contract() -> EventDrivenRuntimeContract {
    EventDrivenRuntimeContract {
        modules: vec![
            EventDrivenRuntimeModule::TmuxHookBridge,
            EventDrivenRuntimeModule::WorkspaceController,
            EventDrivenRuntimeModule::SessionCatalogProjector,
            EventDrivenRuntimeModule::SidebarPaneRuntime,
            EventDrivenRuntimeModule::FooterPaneRuntime,
            EventDrivenRuntimeModule::MainSlotRuntime,
            EventDrivenRuntimeModule::AttachClientRuntime,
            EventDrivenRuntimeModule::SchedulerRuntime,
        ],
        flows: vec![
            EventDrivenFlow {
                producer: LocalRuntimeProducer::TmuxHookBridge,
                event_kind: LocalRuntimeEventKind::TmuxHook,
                consumers: vec![
                    LocalRuntimeConsumer::WorkspaceController,
                    LocalRuntimeConsumer::SessionCatalogProjector,
                ],
                owner: EventDrivenRuntimeModule::TmuxHookBridge,
                purpose: "translate tmux hooks and pane-geometry changes into runtime events",
            },
            EventDrivenFlow {
                producer: LocalRuntimeProducer::SessionCatalogProjector,
                event_kind: LocalRuntimeEventKind::SessionCatalog,
                consumers: vec![
                    LocalRuntimeConsumer::SidebarPaneRuntime,
                    LocalRuntimeConsumer::FooterPaneRuntime,
                    LocalRuntimeConsumer::SchedulerRuntime,
                ],
                owner: EventDrivenRuntimeModule::SessionCatalogProjector,
                purpose: "project authoritative session catalog changes to chrome and scheduler consumers",
            },
            EventDrivenFlow {
                producer: LocalRuntimeProducer::SidebarPaneRuntime,
                event_kind: LocalRuntimeEventKind::Chrome,
                consumers: vec![
                    LocalRuntimeConsumer::WorkspaceController,
                    LocalRuntimeConsumer::FooterPaneRuntime,
                ],
                owner: EventDrivenRuntimeModule::SidebarPaneRuntime,
                purpose: "surface explicit sidebar selection and navigation intent without polling session state",
            },
            EventDrivenFlow {
                producer: LocalRuntimeProducer::WorkspaceController,
                event_kind: LocalRuntimeEventKind::TargetActivation,
                consumers: vec![
                    LocalRuntimeConsumer::MainSlotRuntime,
                    LocalRuntimeConsumer::SidebarPaneRuntime,
                    LocalRuntimeConsumer::FooterPaneRuntime,
                    LocalRuntimeConsumer::SchedulerRuntime,
                ],
                owner: EventDrivenRuntimeModule::WorkspaceController,
                purpose: "request target activation through an explicit runtime event rather than hidden attach semantics",
            },
            EventDrivenFlow {
                producer: LocalRuntimeProducer::MainSlotRuntime,
                event_kind: LocalRuntimeEventKind::TargetActivation,
                consumers: vec![
                    LocalRuntimeConsumer::SidebarPaneRuntime,
                    LocalRuntimeConsumer::FooterPaneRuntime,
                    LocalRuntimeConsumer::SchedulerRuntime,
                ],
                owner: EventDrivenRuntimeModule::MainSlotRuntime,
                purpose: "publish target rebind and activation commit after tmux-native main-slot switching succeeds",
            },
            EventDrivenFlow {
                producer: LocalRuntimeProducer::AttachClientRuntime,
                event_kind: LocalRuntimeEventKind::Attach,
                consumers: vec![
                    LocalRuntimeConsumer::WorkspaceController,
                    LocalRuntimeConsumer::SchedulerRuntime,
                ],
                owner: EventDrivenRuntimeModule::AttachClientRuntime,
                purpose: "route attach input, resize, and daemon-output notifications as explicit runtime events",
            },
            EventDrivenFlow {
                producer: LocalRuntimeProducer::SchedulerRuntime,
                event_kind: LocalRuntimeEventKind::Scheduler,
                consumers: vec![
                    LocalRuntimeConsumer::WorkspaceController,
                    LocalRuntimeConsumer::SidebarPaneRuntime,
                    LocalRuntimeConsumer::FooterPaneRuntime,
                ],
                owner: EventDrivenRuntimeModule::SchedulerRuntime,
                purpose: "emit focus and autoswitch decisions only when upstream runtime events change scheduler state",
            },
        ],
        historical_polling_paths: vec![
            HistoricalPollingPath {
                module_path: "src/runtime/ui_pane_runtime.rs::run_sidebar",
                current_mechanism: "fixed 200ms session-catalog refresh plus input recv_timeout",
                replacement_owner: EventDrivenRuntimeModule::SessionCatalogProjector,
                replacement_reason: "sidebar updates must be driven by catalog and geometry events rather than periodic list_sessions polling",
            },
            HistoricalPollingPath {
                module_path: "src/runtime/ui_pane_runtime.rs::run_footer",
                current_mechanism: "fixed 200ms sleep and unconditional footer redraw pass",
                replacement_owner: EventDrivenRuntimeModule::FooterPaneRuntime,
                replacement_reason: "footer updates must be driven by session and fullscreen projection events",
            },
            HistoricalPollingPath {
                module_path: "src/runtime/workspace_attach_runtime.rs::run",
                current_mechanism: "50ms client tick for resize checks and startup refresh fallback",
                replacement_owner: EventDrivenRuntimeModule::AttachClientRuntime,
                replacement_reason: "attach behavior must react to socket, stdin, and resize events without timeout wakeups",
            },
            HistoricalPollingPath {
                module_path: "src/runtime/workspace_bootstrap_runtime.rs::wait_for_existing_daemon_ready",
                current_mechanism: "sleep-and-retry socket readiness loop",
                replacement_owner: EventDrivenRuntimeModule::WorkspaceController,
                replacement_reason: "bootstrap readiness should move to explicit daemon-ready signalling on the new local path",
            },
            HistoricalPollingPath {
                module_path: "src/app.rs::run_managed_console and run_single_session_passthrough",
                current_mechanism: "50ms event-loop tick for scheduler recompute and resize capture",
                replacement_owner: EventDrivenRuntimeModule::SchedulerRuntime,
                replacement_reason: "scheduler and resize control must be triggered by explicit runtime events on the new path",
            },
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::{default_event_driven_runtime_contract, EventDrivenRuntimeModule};
    use crate::domain::local_runtime::{
        LocalRuntimeConsumer, LocalRuntimeEventKind, LocalRuntimeProducer,
    };

    #[test]
    fn contract_declares_session_catalog_projection_to_sidebar_and_footer() {
        let contract = default_event_driven_runtime_contract();

        assert!(contract.flows.iter().any(|flow| {
            flow.producer == LocalRuntimeProducer::SessionCatalogProjector
                && flow.event_kind == LocalRuntimeEventKind::SessionCatalog
                && flow
                    .consumers
                    .contains(&LocalRuntimeConsumer::SidebarPaneRuntime)
                && flow
                    .consumers
                    .contains(&LocalRuntimeConsumer::FooterPaneRuntime)
        }));
    }

    #[test]
    fn contract_marks_existing_tick_loops_as_historical_paths() {
        let contract = default_event_driven_runtime_contract();

        assert!(contract.historical_polling_paths.iter().any(|path| {
            path.module_path == "src/runtime/ui_pane_runtime.rs::run_sidebar"
                && path.replacement_owner == EventDrivenRuntimeModule::SessionCatalogProjector
        }));
        assert!(contract.historical_polling_paths.iter().any(|path| {
            path.module_path == "src/runtime/workspace_attach_runtime.rs::run"
                && path.replacement_owner == EventDrivenRuntimeModule::AttachClientRuntime
        }));
    }

    #[test]
    fn contract_declares_scheduler_as_event_consumer_and_producer() {
        let contract = default_event_driven_runtime_contract();

        assert!(contract.flows.iter().any(|flow| {
            flow.owner == EventDrivenRuntimeModule::AttachClientRuntime
                && flow
                    .consumers
                    .contains(&LocalRuntimeConsumer::SchedulerRuntime)
        }));
        assert!(contract.flows.iter().any(|flow| {
            flow.owner == EventDrivenRuntimeModule::SchedulerRuntime
                && flow.producer == LocalRuntimeProducer::SchedulerRuntime
        }));
    }

    #[test]
    fn contract_declares_target_activation_flow_through_main_slot_runtime() {
        let contract = default_event_driven_runtime_contract();

        assert!(contract.flows.iter().any(|flow| {
            flow.owner == EventDrivenRuntimeModule::WorkspaceController
                && flow.producer == LocalRuntimeProducer::WorkspaceController
                && flow.event_kind == LocalRuntimeEventKind::TargetActivation
                && flow
                    .consumers
                    .contains(&LocalRuntimeConsumer::MainSlotRuntime)
        }));
        assert!(contract.flows.iter().any(|flow| {
            flow.owner == EventDrivenRuntimeModule::MainSlotRuntime
                && flow.producer == LocalRuntimeProducer::MainSlotRuntime
                && flow.event_kind == LocalRuntimeEventKind::TargetActivation
        }));
    }
}
