use crate::application::target_registry_service::{
    DefaultTargetCatalogGateway, TargetRegistryService,
};
use crate::cli::{AttachCommand, RemoteServerConsoleCommand, ServerConsoleCommand};
use crate::domain::session_catalog::{
    ConsoleLocation, ManagedSessionRecord, ManagedSessionTaskState, SessionAvailability,
    SessionTransport,
};
use crate::domain::workspace::WorkspaceSessionRole;
use crate::infra::remote_protocol::{ControlPlanePayload, ProtocolEnvelope};
use crate::lifecycle::LifecycleError;
use crate::runtime::remote_authority_transport_runtime::authority_transport_socket_path;
use crate::runtime::remote_main_slot_pane_runtime::{
    RemoteInteractSurfaceSpec, RemoteMainSlotPaneRuntime,
};
use crate::runtime::remote_node_session_runtime::{
    spawn_remote_node_session_listener, RemoteNodePublicationSink, RemoteNodeSessionError,
};
use crate::runtime::remote_target_publication_runtime::RemoteTargetPublicationRuntime;
use crate::runtime::workspace_command_runtime::WorkspaceCommandRuntime;
use crate::terminal::TerminalRuntime;
use std::io::{self, Read, Write};
use std::sync::Arc;

pub struct RemoteServerConsoleRuntime {
    target_registry: TargetRegistryService<DefaultTargetCatalogGateway>,
    surface_runtime: RemoteMainSlotPaneRuntime,
    publication_runtime: RemoteTargetPublicationRuntime,
    workspace_runtime: WorkspaceCommandRuntime,
}

impl RemoteServerConsoleRuntime {
    pub fn from_build_env() -> Result<Self, LifecycleError> {
        Ok(Self {
            target_registry: TargetRegistryService::new(
                DefaultTargetCatalogGateway::from_build_env().map_err(target_catalog_error)?,
            ),
            surface_runtime:
                RemoteMainSlotPaneRuntime::from_build_env_with_external_authority_streams()?,
            publication_runtime: RemoteTargetPublicationRuntime::from_build_env()?,
            workspace_runtime: WorkspaceCommandRuntime::from_build_env()?,
        })
    }

    pub fn run(&self, command: RemoteServerConsoleCommand) -> Result<(), LifecycleError> {
        let mut state = ServerConsoleState::default();
        let mut initial_target = command.target.clone();

        loop {
            let target = match initial_target.take() {
                Some(target) => self.resolve_activation_target(&target)?,
                None => match self.select_activation_target(&command, &mut state)? {
                    Some(target) => target,
                    None => return Ok(()),
                },
            };
            let trace =
                ServerConsoleInteractionSurface::for_target(self, &command, &target).run()?;
            state.apply_interaction_trace(&trace);
        }
    }

    pub fn run_public(&self, command: ServerConsoleCommand) -> Result<(), LifecycleError> {
        self.run(RemoteServerConsoleCommand {
            socket_name: command.socket_name,
            console_name: command.console_name,
            target: command.target,
        })
    }

    fn resolve_activation_target(
        &self,
        target: &str,
    ) -> Result<ManagedSessionRecord, LifecycleError> {
        self.target_registry
            .find_activation_target(target)
            .map_err(target_catalog_error)?
            .ok_or_else(|| {
                LifecycleError::Protocol(format!("unknown server-console target `{target}`"))
            })
    }

    fn select_activation_target(
        &self,
        command: &RemoteServerConsoleCommand,
        state: &mut ServerConsoleState,
    ) -> Result<Option<ManagedSessionRecord>, LifecycleError> {
        let targets = self
            .target_registry
            .list_activation_targets()
            .map_err(target_catalog_error)?;
        if targets.is_empty() {
            return Err(LifecycleError::Protocol(
                "no activation targets are currently available for the server console".to_string(),
            ));
        }

        let terminal = TerminalRuntime::stdio();
        let _raw_mode = terminal.enter_raw_mode()?;
        let _alternate_screen = terminal.enter_alternate_screen()?;
        state.reconcile_targets(&targets);
        let mut selected_index = state.selected_index(&targets).unwrap_or(0);
        let mut pending = Vec::new();
        let mut stdin = io::stdin().lock();
        let mut buffer = [0u8; 64];

        draw_activation_picker(&terminal, command, state, &targets, selected_index)?;

        loop {
            let read = stdin.read(&mut buffer).map_err(|error| {
                LifecycleError::Io(
                    "failed to read server-console activation input".to_string(),
                    error,
                )
            })?;
            if read == 0 {
                return Ok(None);
            }

            let actions = picker_actions(&mut pending, &buffer[..read]);
            if actions.is_empty() {
                continue;
            }

            for action in actions {
                match action {
                    PickerAction::Previous => {
                        selected_index =
                            (selected_index + targets.len().saturating_sub(1)) % targets.len();
                        state.select_target(targets[selected_index].address.qualified_target());
                    }
                    PickerAction::Next => {
                        selected_index = (selected_index + 1) % targets.len();
                        state.select_target(targets[selected_index].address.qualified_target());
                    }
                    PickerAction::Submit => {
                        let target = targets[selected_index].clone();
                        state.focus_target(target.address.qualified_target());
                        return Ok(Some(target));
                    }
                    PickerAction::Cancel => return Ok(None),
                }
            }

            draw_activation_picker(&terminal, command, state, &targets, selected_index)?;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ServerConsoleInteractionEvent {
    TargetOpened(String),
    ReturnedToPicker,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct ServerConsoleInteractionTrace {
    events: Vec<ServerConsoleInteractionEvent>,
}

impl ServerConsoleInteractionTrace {
    fn target_opened(target: String) -> Self {
        Self {
            events: vec![ServerConsoleInteractionEvent::TargetOpened(target)],
        }
    }

    fn push(&mut self, event: ServerConsoleInteractionEvent) {
        self.events.push(event);
    }
}

enum ServerConsoleInteractionSurface<'a> {
    Local(LocalServerConsoleInteractionSurface<'a>),
    Remote(RemoteServerConsoleInteractionSurface<'a>),
}

impl<'a> ServerConsoleInteractionSurface<'a> {
    fn for_target(
        runtime: &'a RemoteServerConsoleRuntime,
        command: &'a RemoteServerConsoleCommand,
        target: &'a ManagedSessionRecord,
    ) -> Self {
        match interaction_surface_kind_for_target(target) {
            ServerConsoleInteractionSurfaceKind::LocalAttach => {
                Self::Local(LocalServerConsoleInteractionSurface {
                    workspace_runtime: &runtime.workspace_runtime,
                    target,
                })
            }
            ServerConsoleInteractionSurfaceKind::RemoteInteract => {
                Self::Remote(RemoteServerConsoleInteractionSurface {
                    surface_runtime: &runtime.surface_runtime,
                    publication_runtime: &runtime.publication_runtime,
                    command,
                    target,
                })
            }
        }
    }

    fn run(&self) -> Result<ServerConsoleInteractionTrace, LifecycleError> {
        match self {
            Self::Local(surface) => surface.run(),
            Self::Remote(surface) => surface.run(),
        }
    }
}

fn interaction_surface_kind_for_target(
    target: &ManagedSessionRecord,
) -> ServerConsoleInteractionSurfaceKind {
    match target.address.transport() {
        SessionTransport::LocalTmux => ServerConsoleInteractionSurfaceKind::LocalAttach,
        SessionTransport::RemotePeer => ServerConsoleInteractionSurfaceKind::RemoteInteract,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServerConsoleInteractionSurfaceKind {
    LocalAttach,
    RemoteInteract,
}

struct LocalServerConsoleInteractionSurface<'a> {
    workspace_runtime: &'a WorkspaceCommandRuntime,
    target: &'a ManagedSessionRecord,
}

impl<'a> LocalServerConsoleInteractionSurface<'a> {
    fn run(&self) -> Result<ServerConsoleInteractionTrace, LifecycleError> {
        let qualified_target = self.target.address.qualified_target();
        let mut trace = ServerConsoleInteractionTrace::target_opened(qualified_target.clone());
        self.workspace_runtime.run_attach(AttachCommand {
            target: Some(qualified_target),
        })?;
        trace.push(ServerConsoleInteractionEvent::ReturnedToPicker);
        Ok(trace)
    }
}

struct RemoteServerConsoleInteractionSurface<'a> {
    surface_runtime: &'a RemoteMainSlotPaneRuntime,
    publication_runtime: &'a RemoteTargetPublicationRuntime,
    command: &'a RemoteServerConsoleCommand,
    target: &'a ManagedSessionRecord,
}

impl<'a> RemoteServerConsoleInteractionSurface<'a> {
    fn run(&self) -> Result<ServerConsoleInteractionTrace, LifecycleError> {
        let qualified_target = self.target.address.qualified_target();
        let mut trace = ServerConsoleInteractionTrace::target_opened(qualified_target.clone());
        let spec = server_console_surface_spec(self.command, &qualified_target);
        let socket_path =
            authority_transport_socket_path(&spec.socket_name, &spec.surface_scope, &spec.target);
        let submitter = self.surface_runtime.external_authority_stream_submitter()?;
        let publication_sink: Arc<dyn RemoteNodePublicationSink> =
            Arc::new(LiveRemotePublicationSink {
                runtime: self.publication_runtime.clone(),
                socket_name: spec.socket_name.clone(),
            });
        let _authority_ingress =
            spawn_remote_node_session_listener(socket_path, submitter, publication_sink).map_err(
                |error| {
                    LifecycleError::Io(
                        "failed to start remote server-console authority ingress".to_string(),
                        error,
                    )
                },
            )?;
        self.surface_runtime.run_surface(spec)?;
        trace.push(ServerConsoleInteractionEvent::ReturnedToPicker);
        Ok(trace)
    }
}

pub(crate) fn server_console_surface_spec(
    command: &RemoteServerConsoleCommand,
    target: &str,
) -> RemoteInteractSurfaceSpec {
    let console_id = server_console_id(command);
    RemoteInteractSurfaceSpec {
        socket_name: command.socket_name.clone(),
        surface_scope: format!("server-console:{}", command.console_name),
        target: target.to_string(),
        console_id: console_id.clone(),
        console_host_id: console_id,
        console_location: ConsoleLocation::ServerConsole,
    }
}

fn server_console_id(command: &RemoteServerConsoleCommand) -> String {
    format!(
        "server-console:{}:{}",
        command.socket_name, command.console_name
    )
}

struct LiveRemotePublicationSink {
    runtime: RemoteTargetPublicationRuntime,
    socket_name: String,
}

impl RemoteNodePublicationSink for LiveRemotePublicationSink {
    fn publish(
        &self,
        envelope: ProtocolEnvelope<ControlPlanePayload>,
    ) -> Result<(), RemoteNodeSessionError> {
        self.runtime
            .apply_live_publication_envelope(&self.socket_name, envelope)
            .map_err(|error| RemoteNodeSessionError::new(error.to_string()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickerAction {
    Previous,
    Next,
    Submit,
    Cancel,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct ServerConsoleState {
    focused_target: Option<String>,
    selected_target: Option<String>,
    scheduling: ServerConsoleSchedulingState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ServerConsoleSchedulingPolicy {
    #[default]
    ManualOnly,
}

impl ServerConsoleSchedulingPolicy {
    fn label(&self) -> &'static str {
        match self {
            Self::ManualOnly => "manual-only",
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct ServerConsoleSchedulingState {
    policy: ServerConsoleSchedulingPolicy,
    waiting_queue: Vec<String>,
}

impl ServerConsoleState {
    fn apply_interaction_trace(&mut self, trace: &ServerConsoleInteractionTrace) {
        for event in &trace.events {
            match event {
                ServerConsoleInteractionEvent::TargetOpened(target) => {
                    self.focus_target(target.clone());
                }
                ServerConsoleInteractionEvent::ReturnedToPicker => {}
            }
        }
    }

    fn focus_target(&mut self, target: String) {
        self.focused_target = Some(target.clone());
        self.selected_target = Some(target);
    }

    fn select_target(&mut self, target: String) {
        self.selected_target = Some(target);
    }

    fn reconcile_targets(&mut self, targets: &[ManagedSessionRecord]) {
        if self
            .focused_target
            .as_ref()
            .map(|target| !contains_target(targets, target))
            == Some(true)
        {
            self.focused_target = None;
        }

        if self
            .selected_target
            .as_ref()
            .map(|target| !contains_target(targets, target))
            == Some(true)
        {
            self.selected_target = self.focused_target.clone();
        }

        if self.selected_target.is_none() {
            self.selected_target = self.focused_target.clone().or_else(|| {
                targets
                    .first()
                    .map(|target| target.address.qualified_target())
            });
        }

        self.scheduling
            .reconcile(self.focused_target.as_deref(), targets);
    }

    fn selected_index(&self, targets: &[ManagedSessionRecord]) -> Option<usize> {
        self.selected_target
            .as_ref()
            .and_then(|target| target_index(targets, target))
            .or_else(|| {
                self.focused_target
                    .as_ref()
                    .and_then(|target| target_index(targets, target))
            })
    }

    fn focused_target_label(&self, targets: &[ManagedSessionRecord]) -> String {
        self.focused_target
            .as_ref()
            .and_then(|target| {
                targets
                    .iter()
                    .find(|candidate| candidate.address.qualified_target() == *target)
            })
            .map(server_console_target_label)
            .unwrap_or_else(|| "(none)".to_string())
    }

    fn scheduling_line(&self, targets: &[ManagedSessionRecord]) -> String {
        self.scheduling.summary_line(targets)
    }

    fn queue_position_for(&self, target: &ManagedSessionRecord) -> Option<usize> {
        self.scheduling
            .waiting_queue
            .iter()
            .position(|queued| *queued == target.address.qualified_target())
            .map(|index| index + 1)
    }
}

impl ServerConsoleSchedulingState {
    fn reconcile(&mut self, focused_target: Option<&str>, targets: &[ManagedSessionRecord]) {
        self.waiting_queue = targets
            .iter()
            .filter(|target| is_waiting_activation_target(target))
            .filter(|target| Some(target.address.qualified_target().as_str()) != focused_target)
            .map(|target| target.address.qualified_target())
            .collect();
    }

    fn summary_line(&self, targets: &[ManagedSessionRecord]) -> String {
        let next = self
            .waiting_queue
            .first()
            .and_then(|target| {
                targets
                    .iter()
                    .find(|candidate| candidate.address.qualified_target() == *target)
            })
            .map(server_console_target_label)
            .unwrap_or_else(|| "(none)".to_string());
        format!(
            "scheduling: {} | waiting: {} queued | next: {}",
            self.policy.label(),
            self.waiting_queue.len(),
            next
        )
    }
}

fn draw_activation_picker(
    terminal: &TerminalRuntime,
    command: &RemoteServerConsoleCommand,
    state: &ServerConsoleState,
    targets: &[ManagedSessionRecord],
    selected_index: usize,
) -> Result<(), LifecycleError> {
    let viewport = terminal.current_size_or_default();
    let rows = usize::from(viewport.rows.max(1));
    let width = usize::from(viewport.cols.max(1));
    let header_rows = 6usize;
    let list_rows = rows.saturating_sub(header_rows).max(1);
    let start = selection_window_start(targets.len(), list_rows, selected_index);
    let end = (start + list_rows).min(targets.len());
    let mut stdout = io::stdout().lock();

    write!(stdout, "\x1b[2J\x1b[H").map_err(|error| {
        LifecycleError::Io(
            "failed to clear server-console activation surface".to_string(),
            error,
        )
    })?;

    let lines = vec![
        fit_line(
            format!(
                "server console {} [{} targets]",
                command.console_name,
                targets.len()
            ),
            width,
        ),
        fit_line(
            format!("focus: {}", state.focused_target_label(targets)),
            width,
        ),
        fit_line(state.scheduling_line(targets), width),
        fit_line(
            "up/down or j/k to move, enter to open, q to cancel".to_string(),
            width,
        ),
        fit_line(
            "inside a remote target, press Ctrl-] to return here".to_string(),
            width,
        ),
        String::new(),
    ];
    for (row, line) in lines.into_iter().enumerate() {
        write!(stdout, "\x1b[{};1H{}\x1b[K", row + 1, line).map_err(|error| {
            LifecycleError::Io(
                "failed to draw server-console activation header".to_string(),
                error,
            )
        })?;
    }

    for row in 0..list_rows {
        let target = targets.get(start + row);
        let line = target
            .map(|target| {
                activation_target_line(
                    target,
                    start + row == selected_index,
                    state.queue_position_for(target),
                    width,
                )
            })
            .unwrap_or_default();
        write!(stdout, "\x1b[{};1H{}\x1b[K", row + header_rows + 1, line).map_err(|error| {
            LifecycleError::Io(
                "failed to draw server-console activation target row".to_string(),
                error,
            )
        })?;
    }

    for row in (header_rows + end.saturating_sub(start))..rows {
        write!(stdout, "\x1b[{};1H\x1b[K", row + 1).map_err(|error| {
            LifecycleError::Io(
                "failed to clear server-console activation row".to_string(),
                error,
            )
        })?;
    }

    stdout.flush().map_err(|error| {
        LifecycleError::Io(
            "failed to flush server-console activation surface".to_string(),
            error,
        )
    })
}

fn activation_target_line(
    target: &ManagedSessionRecord,
    is_selected: bool,
    queue_position: Option<usize>,
    width: usize,
) -> String {
    let marker = if is_selected { ">" } else { " " };
    let queue_marker = queue_position
        .map(|position| format!(" q{position}"))
        .unwrap_or_default();
    let current_path = target
        .current_path
        .as_ref()
        .or(target.workspace_dir.as_ref())
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "-".to_string());
    fit_line(
        format!(
            "{marker} {} [{}{}] {} cwd:{}",
            server_console_target_label(target),
            target.task_state.short_label(),
            queue_marker,
            target.address.qualified_target(),
            current_path
        ),
        width,
    )
}

fn server_console_target_label(target: &ManagedSessionRecord) -> String {
    let role = match target.session_role {
        Some(WorkspaceSessionRole::TargetHost) => "target",
        Some(WorkspaceSessionRole::WorkspaceChrome) => "workspace",
        None => "target",
    };
    format!("{}:{role}", target.display_label())
}

fn fit_line(line: String, width: usize) -> String {
    line.chars().take(width).collect()
}

fn is_waiting_activation_target(target: &ManagedSessionRecord) -> bool {
    target.availability != SessionAvailability::Exited
        && matches!(
            target.task_state,
            ManagedSessionTaskState::Input | ManagedSessionTaskState::Confirm
        )
}

fn contains_target(targets: &[ManagedSessionRecord], target: &str) -> bool {
    target_index(targets, target).is_some()
}

fn target_index(targets: &[ManagedSessionRecord], target: &str) -> Option<usize> {
    targets
        .iter()
        .position(|candidate| candidate.address.qualified_target() == target)
}

fn selection_window_start(total: usize, visible: usize, selected: usize) -> usize {
    if total <= visible {
        return 0;
    }
    let half = visible / 2;
    selected.saturating_sub(half).min(total - visible)
}

fn picker_actions(pending: &mut Vec<u8>, bytes: &[u8]) -> Vec<PickerAction> {
    pending.extend_from_slice(bytes);
    let mut actions = Vec::new();

    loop {
        if pending.starts_with(b"\x1b[A") || pending.starts_with(b"\x1bOA") {
            pending.drain(..3);
            actions.push(PickerAction::Previous);
        } else if pending.starts_with(b"\x1b[B") || pending.starts_with(b"\x1bOB") {
            pending.drain(..3);
            actions.push(PickerAction::Next);
        } else if pending.starts_with(b"\x1bOM") || pending.starts_with(b"\x1b[13u") {
            let drain = if pending.starts_with(b"\x1bOM") { 3 } else { 5 };
            pending.drain(..drain);
            actions.push(PickerAction::Submit);
        } else if pending.first() == Some(&b'k') {
            pending.drain(..1);
            actions.push(PickerAction::Previous);
        } else if pending.first() == Some(&b'j') {
            pending.drain(..1);
            actions.push(PickerAction::Next);
        } else if pending.first() == Some(&b'\r') || pending.first() == Some(&b'\n') {
            pending.drain(..1);
            actions.push(PickerAction::Submit);
        } else if pending.first() == Some(&b'q') || pending.first() == Some(&0x03) {
            pending.drain(..1);
            actions.push(PickerAction::Cancel);
        } else if is_partial_picker_sequence(pending) || pending.is_empty() {
            break;
        } else {
            pending.drain(..1);
        }
    }

    actions
}

fn is_partial_picker_sequence(pending: &[u8]) -> bool {
    [
        b"\x1b[".as_slice(),
        b"\x1bO".as_slice(),
        b"\x1b[1".as_slice(),
        b"\x1b[13".as_slice(),
    ]
    .iter()
    .any(|pattern| pattern.starts_with(pending))
}

fn target_catalog_error(error: crate::infra::tmux::TmuxError) -> LifecycleError {
    LifecycleError::Io(
        "failed to inspect shared activation target catalog".to_string(),
        io::Error::new(io::ErrorKind::Other, error.to_string()),
    )
}

#[cfg(test)]
mod tests {
    use super::{
        interaction_surface_kind_for_target, picker_actions, selection_window_start,
        server_console_surface_spec, PickerAction, ServerConsoleInteractionEvent,
        ServerConsoleInteractionSurfaceKind, ServerConsoleInteractionTrace, ServerConsoleState,
    };
    use crate::cli::RemoteServerConsoleCommand;
    use crate::domain::session_catalog::{
        ConsoleLocation, ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState,
        SessionAvailability,
    };
    use crate::domain::workspace::WorkspaceSessionRole;
    use std::path::PathBuf;

    #[test]
    fn server_console_surface_spec_marks_server_console_location() {
        let spec = server_console_surface_spec(
            &RemoteServerConsoleCommand {
                socket_name: "wa-1".to_string(),
                console_name: "console-a".to_string(),
                target: None,
            },
            "peer-a:shell-1",
        );

        assert_eq!(spec.console_id, "server-console:wa-1:console-a");
        assert_eq!(spec.console_host_id, "server-console:wa-1:console-a");
        assert_eq!(spec.surface_scope, "server-console:console-a");
        assert_eq!(spec.target, "peer-a:shell-1");
        assert_eq!(spec.console_location, ConsoleLocation::ServerConsole);
    }

    #[test]
    fn picker_actions_support_arrows_vim_keys_and_cancel() {
        let mut pending = Vec::new();

        assert_eq!(
            picker_actions(&mut pending, b"\x1b[B"),
            vec![PickerAction::Next]
        );
        assert_eq!(
            picker_actions(&mut pending, b"k"),
            vec![PickerAction::Previous]
        );
        assert_eq!(
            picker_actions(&mut pending, b"\r"),
            vec![PickerAction::Submit]
        );
        assert_eq!(
            picker_actions(&mut pending, b"q"),
            vec![PickerAction::Cancel]
        );
    }

    #[test]
    fn selection_window_keeps_selected_row_visible() {
        assert_eq!(selection_window_start(3, 5, 0), 0);
        assert_eq!(selection_window_start(10, 4, 0), 0);
        assert_eq!(selection_window_start(10, 4, 5), 3);
        assert_eq!(selection_window_start(10, 4, 9), 6);
    }

    #[test]
    fn server_console_state_prefers_focused_target_for_selection() {
        let targets = vec![
            local_target("wa-1", "target-1"),
            remote_target("peer-a", "shell-1"),
        ];
        let mut state = ServerConsoleState::default();
        state.focus_target("peer-a:shell-1".to_string());
        state.reconcile_targets(&targets);

        assert_eq!(state.selected_index(&targets), Some(1));
        assert_eq!(state.focused_target_label(&targets), "bash@remote:target");
    }

    #[test]
    fn server_console_state_releases_focus_when_target_disappears() {
        let mut state = ServerConsoleState::default();
        state.focus_target("peer-a:shell-1".to_string());

        let targets = vec![local_target("wa-1", "target-1")];
        state.reconcile_targets(&targets);

        assert_eq!(state.focused_target, None);
        assert_eq!(state.selected_index(&targets), Some(0));
        assert_eq!(state.focused_target_label(&targets), "(none)");
    }

    #[test]
    fn server_console_scheduling_queue_tracks_waiting_targets_in_catalog_order() {
        let targets = vec![
            running_local_target("wa-1", "target-1"),
            input_target("peer-a", "shell-1"),
            confirm_target("peer-b", "shell-2"),
        ];
        let mut state = ServerConsoleState::default();
        state.focus_target("wa-1:target-1".to_string());
        state.reconcile_targets(&targets);

        assert_eq!(
            state.scheduling.waiting_queue,
            vec!["peer-a:shell-1".to_string(), "peer-b:shell-2".to_string()]
        );
        assert_eq!(
            state.scheduling_line(&targets),
            "scheduling: manual-only | waiting: 2 queued | next: bash@remote:target"
        );
        assert_eq!(state.queue_position_for(&targets[0]), None);
        assert_eq!(state.queue_position_for(&targets[1]), Some(1));
        assert_eq!(state.queue_position_for(&targets[2]), Some(2));
    }

    #[test]
    fn interaction_trace_updates_focused_target_from_open_event() {
        let mut state = ServerConsoleState::default();
        let trace = ServerConsoleInteractionTrace {
            events: vec![
                ServerConsoleInteractionEvent::TargetOpened("peer-a:shell-1".to_string()),
                ServerConsoleInteractionEvent::ReturnedToPicker,
            ],
        };

        state.apply_interaction_trace(&trace);

        assert_eq!(state.focused_target.as_deref(), Some("peer-a:shell-1"));
        assert_eq!(state.selected_target.as_deref(), Some("peer-a:shell-1"));
    }

    #[test]
    fn interaction_surface_selects_local_attach_for_local_targets() {
        let target = local_target("wa-1", "target-1");

        assert_eq!(
            interaction_surface_kind_for_target(&target),
            ServerConsoleInteractionSurfaceKind::LocalAttach
        );
    }

    #[test]
    fn interaction_surface_selects_remote_interact_for_remote_targets() {
        let target = remote_target("peer-a", "shell-1");

        assert_eq!(
            interaction_surface_kind_for_target(&target),
            ServerConsoleInteractionSurfaceKind::RemoteInteract
        );
    }

    fn local_target(socket_name: &str, session_name: &str) -> ManagedSessionRecord {
        ManagedSessionRecord {
            address: ManagedSessionAddress::local_tmux(socket_name, session_name),
            selector: Some(format!("{socket_name}:{session_name}")),
            availability: SessionAvailability::Online,
            workspace_dir: Some(PathBuf::from("/tmp/local")),
            workspace_key: None,
            session_role: Some(WorkspaceSessionRole::TargetHost),
            opened_by: Vec::new(),
            attached_clients: 1,
            window_count: 1,
            command_name: Some("bash".to_string()),
            current_path: Some(PathBuf::from("/tmp/local")),
            task_state: ManagedSessionTaskState::Input,
        }
    }

    fn remote_target(authority_id: &str, session_id: &str) -> ManagedSessionRecord {
        ManagedSessionRecord {
            address: ManagedSessionAddress::remote_peer(authority_id, session_id),
            selector: None,
            availability: SessionAvailability::Online,
            workspace_dir: Some(PathBuf::from("/tmp/remote")),
            workspace_key: None,
            session_role: None,
            opened_by: Vec::new(),
            attached_clients: 0,
            window_count: 1,
            command_name: Some("bash".to_string()),
            current_path: Some(PathBuf::from("/tmp/remote")),
            task_state: ManagedSessionTaskState::Running,
        }
    }

    fn running_local_target(socket_name: &str, session_name: &str) -> ManagedSessionRecord {
        ManagedSessionRecord {
            task_state: ManagedSessionTaskState::Running,
            ..local_target(socket_name, session_name)
        }
    }

    fn input_target(authority_id: &str, session_id: &str) -> ManagedSessionRecord {
        ManagedSessionRecord {
            task_state: ManagedSessionTaskState::Input,
            ..remote_target(authority_id, session_id)
        }
    }

    fn confirm_target(authority_id: &str, session_id: &str) -> ManagedSessionRecord {
        ManagedSessionRecord {
            task_state: ManagedSessionTaskState::Confirm,
            ..remote_target(authority_id, session_id)
        }
    }
}
