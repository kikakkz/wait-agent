use crate::application::layout_service::{
    FOOTER_HEIGHT_CELLS, FOOTER_PANE_TITLE, SIDEBAR_PANE_TITLE, SIDEBAR_WIDTH_CELLS,
};
use crate::application::target_registry_service::{
    DefaultTargetCatalogGateway, TargetRegistryService,
};
use crate::cli::{RemoteMainSlotCommand, RemoteNetworkConfig};
use crate::domain::session_catalog::{ConsoleLocation, ManagedSessionRecord, SessionTransport};
use crate::infra::error_log::ERROR_LOG;
use crate::infra::remote_protocol::ControlPlanePayload;
use crate::infra::tmux::EmbeddedTmuxBackend;
use crate::infra::tmux::TmuxLayoutGateway;
use crate::infra::tmux::TmuxPaneId;
use crate::lifecycle::LifecycleError;
use crate::runtime::current_executable::current_waitagent_executable;
use crate::runtime::remote_authority_connection_runtime::{
    AuthorityConnectionGuard, AuthorityConnectionRequest, AuthorityConnectionStarter,
    AuthorityTransportEvent, QueuedAuthorityStreamSink, QueuedAuthorityStreamStarter,
};
use crate::runtime::remote_authority_transport_runtime::authority_transport_socket_path;
use crate::runtime::remote_main_slot::remote_surface_state::{
    mark_remote_surface_state_from_env, RemoteSurfaceState,
};
use crate::runtime::remote_main_slot_runtime::{RemoteAttachmentBinding, RemoteMainSlotRuntime};
use crate::runtime::remote_observer_runtime::RemoteObserverRuntime;
use crate::runtime::remote_transport_runtime::RemoteConnectionRegistry;
use crate::terminal::{TerminalRuntime, TerminalSize};
use std::io;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(test)]
use std::os::unix::net::UnixStream;

mod slot_pane_helpers;
pub(crate) use slot_pane_helpers::*;

const MAIN_PANE_DIED_HOOK: &str = "pane-died[10]";

pub struct RemoteMainSlotPaneRuntime {
    target_registry: TargetRegistryService<DefaultTargetCatalogGateway>,
    backend: EmbeddedTmuxBackend,
    authority_connections: Box<dyn AuthorityConnectionStarter>,
    external_authority_streams: Option<QueuedAuthorityStreamSink>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RemoteInteractSignal {
    ConsoleInputStarted,
    ConsoleSubmit,
    ManualReturnToPicker,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteInteractSurfaceSpec {
    pub socket_name: String,
    pub surface_scope: String,
    pub target: String,
    pub console_id: String,
    pub console_host_id: String,
    pub console_location: ConsoleLocation,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum AuthorityTransportStatus {
    WaitingForRemoteAuthority,
    Connected,
    Disconnected,
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MirrorReadiness {
    Waiting,
    Ready,
}

impl RemoteMainSlotPaneRuntime {
    pub fn from_build_env_with_external_authority_streams_and_network(
        network: RemoteNetworkConfig,
    ) -> Result<Self, LifecycleError> {
        let current_executable = current_waitagent_executable()?;
        let target_registry = TargetRegistryService::new(
            DefaultTargetCatalogGateway::from_build_env_with_network(network.clone())
                .map_err(remote_pane_error)?,
        );
        let backend = EmbeddedTmuxBackend::from_build_env().map_err(remote_pane_error)?;
        Ok(Self::new_with_external_authority_streams_and_network(
            target_registry,
            backend,
            current_executable,
            network,
        ))
    }

    #[cfg(test)]
    pub fn new(
        target_registry: TargetRegistryService<DefaultTargetCatalogGateway>,
        backend: EmbeddedTmuxBackend,
        authority_connections: Box<dyn AuthorityConnectionStarter>,
        current_executable: PathBuf,
        _network: RemoteNetworkConfig,
    ) -> Self {
        Self::new_with_optional_external_authority_streams(
            target_registry,
            backend,
            authority_connections,
            None,
            current_executable,
        )
    }

    fn new_with_optional_external_authority_streams(
        target_registry: TargetRegistryService<DefaultTargetCatalogGateway>,
        backend: EmbeddedTmuxBackend,
        authority_connections: Box<dyn AuthorityConnectionStarter>,
        external_authority_streams: Option<QueuedAuthorityStreamSink>,
        _current_executable: PathBuf,
    ) -> Self {
        Self {
            target_registry,
            backend,
            authority_connections,
            external_authority_streams,
        }
    }

    #[cfg(test)]
    pub fn new_with_external_authority_streams(
        target_registry: TargetRegistryService<DefaultTargetCatalogGateway>,
        backend: EmbeddedTmuxBackend,
        current_executable: PathBuf,
    ) -> Self {
        Self::new_with_external_authority_streams_and_network(
            target_registry,
            backend,
            current_executable,
            RemoteNetworkConfig::default(),
        )
    }

    pub fn new_with_external_authority_streams_and_network(
        target_registry: TargetRegistryService<DefaultTargetCatalogGateway>,
        backend: EmbeddedTmuxBackend,
        current_executable: PathBuf,
        _network: RemoteNetworkConfig,
    ) -> Self {
        let (starter, sink) = QueuedAuthorityStreamStarter::channel();
        Self::new_with_optional_external_authority_streams(
            target_registry,
            backend,
            Box::new(starter),
            Some(sink),
            current_executable,
        )
    }

    #[cfg(test)]
    pub fn submit_external_authority_stream(
        &self,
        stream: UnixStream,
    ) -> Result<(), LifecycleError> {
        let sink = self.external_authority_stream_submitter()?;
        sink.submit(stream).map_err(|_| {
            LifecycleError::Protocol(
                "remote main-slot external authority stream consumer is unavailable".to_string(),
            )
        })
    }

    pub(crate) fn external_authority_stream_submitter(
        &self,
    ) -> Result<QueuedAuthorityStreamSink, LifecycleError> {
        self.external_authority_streams
            .as_ref()
            .cloned()
            .ok_or_else(|| {
                LifecycleError::Protocol(
                "remote main-slot pane runtime is not configured for external authority streams"
                    .to_string(),
            )
            })
    }

    pub(crate) fn start_authority_connection(
        &self,
        request: AuthorityConnectionRequest,
        registry: RemoteConnectionRegistry,
        tx: mpsc::Sender<AuthorityTransportEvent>,
    ) -> io::Result<Box<dyn AuthorityConnectionGuard>> {
        self.authority_connections
            .start_connection(request, registry, tx)
    }

    pub fn run(&self, command: RemoteMainSlotCommand) -> Result<(), LifecycleError> {
        self.run_surface(main_slot_surface_spec(&command))
    }

    pub(crate) fn run_surface(
        &self,
        spec: RemoteInteractSurfaceSpec,
    ) -> Result<(), LifecycleError> {
        self.run_surface_with_signal_sink(spec, |_| {})
    }

    pub(crate) fn run_surface_with_signal_sink<F>(
        &self,
        spec: RemoteInteractSurfaceSpec,
        mut on_signal: F,
    ) -> Result<(), LifecycleError>
    where
        F: FnMut(RemoteInteractSignal),
    {
        let mut target = self.resolve_remote_target(&spec.target, "remote interact surface")?;
        let terminal = TerminalRuntime::stdio();
        let _raw_mode = terminal.enter_raw_mode()?;
        let _cursor_guard = RemotePaneCursorGuard::hide().map_err(|error| {
            LifecycleError::Io("failed to hide remote interact cursor".to_string(), error)
        })?;

        let registry = RemoteConnectionRegistry::new();
        let remote_runtime = RemoteMainSlotRuntime::with_registry(registry.clone());
        let _ = mark_remote_surface_state_from_env(
            &self.backend,
            &spec.socket_name,
            &target,
            None,
            RemoteSurfaceState::Starting,
        );
        let mailbox = remote_runtime
            .ensure_local_observer_connection(spec.console_host_id.clone())
            .ok_or_else(|| {
                LifecycleError::Protocol(
                    "remote observer connection registry is not available".to_string(),
                )
            })?;

        let raw_input_route = Arc::new(RawPtyInputRoute::default());
        let (event_tx, event_rx) = mpsc::channel();
        spawn_input_thread(
            event_tx.clone(),
            RawInputMode {
                route: raw_input_route.clone(),
                registry: registry.clone(),
            },
        );
        let resize_watcher = spawn_resize_watcher(event_tx.clone()).map_err(remote_pane_error)?;
        spawn_mailbox_watcher(mailbox.clone(), event_tx.clone());

        // Capture terminal size after the SIGWINCH handler is active. Prefer
        // tmux pane metadata over stdio: the remote-main-slot process can be
        // spawned before tmux finishes swapping it into the visible main pane.
        let initial_size = current_remote_surface_size(&spec, &terminal);
        let mut observer = RemoteObserverRuntime::new(
            mailbox.clone(),
            usize::from(initial_size.cols),
            usize::from(initial_size.rows),
        );
        let mut last_synced_size = initial_size;
        let mut raw_output_reader = RemoteRawPtyMailboxReader::new(mailbox);
        let target_presence = Arc::new(Mutex::new(true));
        spawn_target_presence_watcher(
            self.target_registry.clone(),
            self.backend.clone(),
            spec.socket_name.clone(),
            spec.surface_scope.clone(),
            spec.target.clone(),
            target_presence.clone(),
            event_tx.clone(),
        );
        let authority_transport_socket_path =
            authority_transport_socket_path(&spec.socket_name, &spec.surface_scope, &spec.target);
        let authority_tx = authority_transport_event_sender(event_tx.clone());
        let _authority_listener = self
            .start_authority_connection(
                AuthorityConnectionRequest {
                    socket_path: authority_transport_socket_path.clone(),
                    authority_id: target.address.authority_id().to_string(),
                },
                registry.clone(),
                authority_tx,
            )
            .map_err(remote_pane_error)?;
        let waiting_authority_status = AuthorityTransportStatus::WaitingForRemoteAuthority;
        thread::spawn(move || {
            let _keep_resize_watcher_alive = resize_watcher;
            thread::park();
        });
        let mut console_seq = 0u64;
        let mut input_signal_decoder = RemoteInteractInputSignalDecoder::default();
        let mut paused_input_buffer: Vec<Vec<u8>> = Vec::new();
        let mut pending_pty_size: Option<TerminalSize> = None;
        let mut binding = None;
        let mut direct_raw_output_last_seq = None;
        let mut raw_screen_initialized = false;
        let mut mirror_readiness = MirrorReadiness::Waiting;
        let mut resize_acked_size: Option<TerminalSize> = None;
        let mut remote_reported_geometry: Option<TerminalSize> = None;
        let mut geometry_resync_pending: Option<TerminalSize> = None;
        let mut geometry_resync_debounce_active = false;
        let mut resync_applied_size: Option<TerminalSize> = None;
        let mut main_slot_intent_active = false;
        let mut intent_guard = MainSlotGeometryIntentGuard::new(&spec);
        let mut viewer_capacity: TerminalSize = initial_size;
        let mut authority_status = waiting_authority_status.clone();
        let mut authority_generation: Option<u64> = None;
        if remote_runtime.has_connection(target.address.authority_id()) {
            match activate_remote_surface_binding(
                &remote_runtime,
                &target,
                &spec,
                &initial_size,
                &mut observer,
                &mut raw_output_reader,
                &raw_input_route,
                &mut pending_pty_size,
                &mut last_synced_size,
                &mut resize_acked_size,
                &mut raw_screen_initialized,
                event_tx.clone(),
            ) {
                Ok(activated_binding) => {
                    binding = Some(activated_binding);
                }
                Err(error) => {
                    ERROR_LOG.log(format!(
                        "[diag-timing] initial remote activation deferred: {error}"
                    ));
                    authority_status = waiting_authority_status.clone();
                }
            }
        }
        let run_result = (|| -> Result<(), LifecycleError> {
            let mut reconnecting_since: Option<Instant> = None;
            let mut reconnect_animation_frame: u8 = 0;

            if should_draw_remote_snapshot(
                binding.as_ref(),
                &observer.snapshot(),
                &authority_status,
            ) {
                let _ = observer.sync();
                draw_remote_snapshot(
                    &terminal,
                    &target,
                    binding.as_ref(),
                    &observer.snapshot(),
                    &authority_status,
                    None,
                    None,
                    0,
                )?;
            }

            // Track initial connecting time so we can show
            // "connecting to remote... Xs" animation while the
            // gRPC bridge establishes (instead of a static placeholder).
            let mut initial_connecting_since: Option<Instant> = if matches!(
                authority_status,
                AuthorityTransportStatus::WaitingForRemoteAuthority
            ) && binding.is_some()
            {
                Some(Instant::now())
            } else {
                None
            };

            loop {
                let event = if let Some(started) = initial_connecting_since {
                    match event_rx.recv_timeout(slot_pane_helpers::RECONNECT_ANIMATION_INTERVAL) {
                        Ok(event) => {
                            initial_connecting_since = None;
                            event
                        }
                        Err(RecvTimeoutError::Timeout) => {
                            let elapsed = started.elapsed();
                            if elapsed > slot_pane_helpers::INITIAL_CONNECT_TIMEOUT {
                                initial_connecting_since = None;
                                reconnecting_since = Some(Instant::now());
                                authority_status = AuthorityTransportStatus::Disconnected;
                                let _ = mark_remote_surface_state_from_env(
                                    &self.backend,
                                    &spec.socket_name,
                                    &target,
                                    authority_generation,
                                    RemoteSurfaceState::Reconnecting,
                                );
                                continue;
                            }
                            reconnect_animation_frame = (reconnect_animation_frame + 1) % 8;
                            let _ = observer.sync();
                            draw_remote_snapshot(
                                &terminal,
                                &target,
                                binding.as_ref(),
                                &observer.snapshot(),
                                &authority_status,
                                Some(elapsed),
                                None,
                                reconnect_animation_frame,
                            )?;
                            continue;
                        }
                        Err(RecvTimeoutError::Disconnected) => return Ok(()),
                    }
                } else if reconnecting_since.is_some() {
                    // ── reconnecting / verifying-exit state machine ──────────
                    // Phase 1 (0 – 500 ms):  "verifying exit" — short-poll the
                    //     catalog directly to confirm whether the session truly
                    //     exited.  Draw *nothing*; keep the last content visible.
                    // Phase 2 (> 500 ms):     genuine reconnect — the target is
                    //     still in the catalog, so this is a network blip.  Draw
                    //     the reconnecting indicator while waiting.
                    // ──────────────────────────────────────────────────────────
                    const VERIFY_EXIT_PHASE: Duration = Duration::from_millis(500);
                    let elapsed = reconnecting_since.unwrap().elapsed();
                    let phase1_timeout = if elapsed < VERIFY_EXIT_PHASE {
                        Duration::from_millis(10)
                    } else {
                        slot_pane_helpers::RECONNECT_ANIMATION_INTERVAL
                    };
                    match event_rx.recv_timeout(phase1_timeout) {
                        Ok(event) => event,
                        Err(RecvTimeoutError::Timeout) => {
                            let elapsed = reconnecting_since.unwrap().elapsed();
                            // Check catalog directly (not the cached watcher
                            // flag).  The publication runtime updates the
                            // catalog within tens of milliseconds after the
                            // target host sends TargetExited.
                            let target_gone = !target_is_present(&target_presence);
                            if elapsed > slot_pane_helpers::RECONNECT_TIMEOUT || target_gone {
                                if target_gone {
                                    ERROR_LOG.log(
                                        "[diag-timing] target gone during reconnect, shutting down"
                                            .to_string(),
                                    );
                                }
                                let _ = std::io::Write::write_all(
                                    &mut io::stdout(),
                                    CLEAR_SCREEN_HOME_ESCAPE.as_bytes(),
                                );
                                let _ = std::io::Write::flush(&mut io::stdout());
                                let pane_id =
                                    std::env::var("TMUX_PANE").unwrap_or_else(|_| String::new());
                                let _ = signal_clean_remote_target_exit(&spec, &pane_id);
                                return Ok(());
                            }
                            if elapsed >= VERIFY_EXIT_PHASE {
                                reconnect_animation_frame = (reconnect_animation_frame + 1) % 8;
                                let _ = observer.sync();
                                draw_remote_snapshot(
                                    &terminal,
                                    &target,
                                    binding.as_ref(),
                                    &observer.snapshot(),
                                    &authority_status,
                                    None,
                                    Some(elapsed),
                                    reconnect_animation_frame,
                                )?;
                            }
                            continue;
                        }
                        Err(RecvTimeoutError::Disconnected) => return Ok(()),
                    }
                } else {
                    match event_rx.recv() {
                        Ok(event) => event,
                        Err(_) => return Ok(()),
                    }
                };

                match event {
                    RemotePaneEvent::GeometryResyncDue => {
                        geometry_resync_debounce_active = false;
                        let Some(reported) = geometry_resync_pending.take() else {
                            continue;
                        };
                        if reported == current_remote_surface_size(&spec, &terminal) {
                            continue;
                        }
                        ERROR_LOG.log(format!(
                            "[diag] geometry re-sync: applying remote geometry {}x{}",
                            reported.cols, reported.rows
                        ));
                        let own_pane = std::env::var("TMUX_PANE").unwrap_or_default();
                        let mut local_coordinate_done = false;
                        if !own_pane.is_empty() {
                            let own_pane_id = TmuxPaneId::new(own_pane.clone());
                            let own_window = self
                                .backend
                                .pane_session_name_on_socket(&spec.socket_name, &own_pane_id)
                                .unwrap_or_default();
                            let own_window_has_chrome = self
                                .backend
                                .window_has_waitagent_chrome_on_socket(
                                    &spec.socket_name,
                                    &own_pane_id,
                                )
                                .unwrap_or(false);
                            if own_window_has_chrome {
                                // Shared waitagent chrome window: publish the
                                // negotiated geometry as an intent and let the
                                // workspace layout runtime apply it (padding
                                // included). The slot never does window
                                // surgery here, so the layout runtime stays
                                // the single writer for the chrome window.
                                match publish_main_slot_geometry_intent(
                                    &spec,
                                    &own_window,
                                    &own_pane,
                                    reported,
                                ) {
                                    Ok(()) => {
                                        ERROR_LOG.log(format!(
                                            "[diag] geometry re-sync: published main-slot geometry intent {}x{} (session={own_window})",
                                            reported.cols, reported.rows
                                        ));
                                        resync_applied_size = Some(reported);
                                        main_slot_intent_active = true;
                                        intent_guard.arm();
                                        local_coordinate_done = true;
                                    }
                                    Err(error) => {
                                        resync_applied_size = None;
                                        ERROR_LOG.log(format!(
                                            "[diag] geometry re-sync: main-slot geometry intent publish failed: {error}"
                                        ));
                                        continue;
                                    }
                                }
                            } else {
                                resync_applied_size = Some(reported);
                                match self.backend.coordinate_geometry_on_socket(
                                    &spec.socket_name,
                                    &own_pane_id,
                                    reported.cols as usize,
                                    reported.rows as usize,
                                ) {
                                    Ok(_) => {
                                        local_coordinate_done = true;
                                    }
                                    Err(error) => {
                                        resync_applied_size = None;
                                        ERROR_LOG.log(format!(
                                            "[diag] geometry re-sync: local coordinate failed: {error}"
                                        ));
                                        continue;
                                    }
                                }
                            }
                        }
                        if local_coordinate_done {
                            let settled = current_remote_surface_size(&spec, &terminal);
                            if settled != reported {
                                resync_applied_size = None;
                                ERROR_LOG.log(format!(
                                    "[diag] geometry re-sync: pane settled at {}x{} instead of {}x{}; reporting settled size as effective capacity",
                                    settled.cols, settled.rows, reported.cols, reported.rows
                                ));
                                // The pane could not hold the negotiated size
                                // (local layout constraint).  Converge
                                // instead of looping: report the settled size
                                // upstream so the authority re-negotiates to
                                // the size the pane actually has.
                                last_synced_size = settled;
                                viewer_capacity = settled;
                                if let Some(binding) = binding.as_ref() {
                                    sync_or_defer_remote_pty_size(
                                        &remote_runtime,
                                        &target,
                                        binding,
                                        &settled,
                                        &mut pending_pty_size,
                                    )?;
                                    resize_acked_size = None;
                                }
                                continue;
                            }
                        }
                        // Re-open the mirror with the viewer capacity as the
                        // desired geometry, never the negotiated one: the
                        // authority must keep seeing the operator's capacity
                        // or the negotiation collapses to the smaller size.
                        let capacity = viewer_capacity;
                        match activate_remote_surface_binding(
                            &remote_runtime,
                            &target,
                            &spec,
                            &capacity,
                            &mut observer,
                            &mut raw_output_reader,
                            &raw_input_route,
                            &mut pending_pty_size,
                            &mut last_synced_size,
                            &mut resize_acked_size,
                            &mut raw_screen_initialized,
                            event_tx.clone(),
                        ) {
                            Ok(new_binding) => {
                                binding = Some(new_binding);
                                // The pane now renders at the negotiated size;
                                // the gate compares acks against the current
                                // pane size, so align the dedup baseline.
                                last_synced_size = reported;
                            }
                            Err(error) => return Err(error),
                        }
                    }
                    RemotePaneEvent::MailboxUpdated => {
                        let raw = raw_output_reader
                            .sync_and_collect_raw()
                            .map_err(remote_protocol_error)?;
                        // Keep the observer's terminal engine in sync with the
                        // mailbox so that draw_remote_snapshot (used during
                        // reconnection) shows the most recent content rather
                        // than stale placeholder state.
                        let _ = observer.sync();
                        if raw.is_empty() {
                            continue;
                        }
                        render_remote_output_and_mark_ready(
                            &raw,
                            &remote_runtime,
                            &target,
                            binding.as_ref(),
                            &mut pending_pty_size,
                            &mut paused_input_buffer,
                            &mut console_seq,
                            &mut mirror_readiness,
                            &mut reconnecting_since,
                            &mut raw_screen_initialized,
                            &last_synced_size,
                            &mut resize_acked_size,
                        )?;
                    }
                    RemotePaneEvent::Resize => {
                        let size = current_remote_surface_size(&spec, &terminal);
                        if let Some(expected) = resync_applied_size {
                            // Suppress resize handling while our own geometry
                            // coordination settles; intermediate reflow sizes
                            // are not genuine capacity changes.
                            if size == expected {
                                resync_applied_size = None;
                                last_synced_size = size;
                            }
                            continue;
                        }
                        if size != last_synced_size {
                            // With the geometry intent active the pane is
                            // pinned to the negotiated size, so its live size
                            // is no longer the viewer capacity. Derive
                            // capacity from the window minus pinned chrome.
                            let capacity = if main_slot_intent_active {
                                window_main_area_capacity(&spec).unwrap_or(size)
                            } else {
                                size
                            };
                            viewer_capacity = capacity;
                            if let Some(binding) = binding.as_ref() {
                                sync_or_defer_remote_pty_size(
                                    &remote_runtime,
                                    &target,
                                    binding,
                                    &capacity,
                                    &mut pending_pty_size,
                                )?;
                                resize_acked_size = None;
                            }
                        }
                        // last_synced_size always tracks the size we are
                        // currently rendering at; the resize-ack gate compares
                        // acks against it.
                        last_synced_size = size;
                        // When raw PTY mode is active (binding.is_some()), the
                        // remote terminal handles its own rendering. Calling
                        // draw_remote_snapshot here would overwrite the terminal
                        // with a stale snapshot, potentially clobbering claude's
                        // bottom separator line and other UI elements.
                        if binding.is_none() {
                            draw_remote_snapshot(
                                &terminal,
                                &target,
                                None,
                                &observer.snapshot(),
                                &authority_status,
                                None,
                                reconnecting_since.map(|s| s.elapsed()),
                                reconnect_animation_frame,
                            )?;
                        }
                    }
                    RemotePaneEvent::AuthorityTransport(event) => match event {
                        AuthorityTransportEvent::Connected {
                            authority_id,
                            generation,
                        } => {
                            if authority_id != target.address.authority_id() {
                                continue;
                            }
                            let current_target =
                                self.resolve_remote_target(&spec.target, "remote reconnect");
                            let is_present = current_target
                                .as_ref()
                                .ok()
                                .is_some_and(|target| target_is_online(Some(target)));
                            if let Ok(current_target) = current_target {
                                target = current_target;
                            }
                            authority_generation = Some(generation);
                            authority_status = if is_present {
                                AuthorityTransportStatus::Connected
                            } else {
                                AuthorityTransportStatus::Disconnected
                            };
                            if let Some(binding) = binding.as_ref() {
                                let flushed = flush_pending_pty_size(
                                    &remote_runtime,
                                    &target,
                                    binding,
                                    &mut pending_pty_size,
                                )?;
                                if flushed {
                                    resize_acked_size = None;
                                }
                                try_flush_ready_inputs(
                                    &remote_runtime,
                                    &target,
                                    Some(binding),
                                    &mut pending_pty_size,
                                    &mut paused_input_buffer,
                                    &mut console_seq,
                                    &mirror_readiness,
                                    &last_synced_size,
                                    &mut resize_acked_size,
                                )?;
                            }
                            let needs_activation = reconnecting_since.is_some()
                                || binding.is_none()
                                || remote_runtime.is_mirror_pending(&target)
                                || remote_runtime.is_mirror_needed(&target);
                            let mut activated = false;
                            if needs_activation
                                && matches!(authority_status, AuthorityTransportStatus::Connected)
                            {
                                let size = current_remote_surface_size(&spec, &terminal);
                                match activate_remote_surface_binding(
                                    &remote_runtime,
                                    &target,
                                    &spec,
                                    &size,
                                    &mut observer,
                                    &mut raw_output_reader,
                                    &raw_input_route,
                                    &mut pending_pty_size,
                                    &mut last_synced_size,
                                    &mut resize_acked_size,
                                    &mut raw_screen_initialized,
                                    event_tx.clone(),
                                ) {
                                    Ok(activated_binding) => {
                                        binding = Some(activated_binding);
                                        mirror_readiness = MirrorReadiness::Waiting;
                                        activated = true;
                                        let _ = mark_remote_surface_state_from_env(
                                            &self.backend,
                                            &spec.socket_name,
                                            &target,
                                            Some(generation),
                                            RemoteSurfaceState::Connected,
                                        )?;
                                    }
                                    Err(error) => {
                                        authority_status =
                                            AuthorityTransportStatus::Failed(error.to_string());
                                    }
                                }
                            }
                            if !activated
                                && binding.is_some()
                                && matches!(authority_status, AuthorityTransportStatus::Connected)
                            {
                                let _ = mark_remote_surface_state_from_env(
                                    &self.backend,
                                    &spec.socket_name,
                                    &target,
                                    Some(generation),
                                    RemoteSurfaceState::Connected,
                                )?;
                            }
                            // When activation succeeded, raw PTY output has
                            // already been written and the remote terminal
                            // handles its own rendering. Don't overwrite with
                            // a stale snapshot (which would clobber UI elements
                            // like claude's input area separator).
                            if !activated && binding.is_none() {
                                let _ = observer.sync();
                                draw_remote_snapshot(
                                    &terminal,
                                    &target,
                                    binding.as_ref(),
                                    &observer.snapshot(),
                                    &authority_status,
                                    None,
                                    None,
                                    0,
                                )?;
                            }
                        }
                        AuthorityTransportEvent::Disconnected {
                            authority_id,
                            generation,
                        } => {
                            if authority_id != target.address.authority_id()
                                || authority_generation != Some(generation)
                            {
                                continue;
                            }
                            begin_authority_reconnect(
                                &remote_runtime,
                                &target,
                                &mut binding,
                                &raw_input_route,
                                &mut raw_screen_initialized,
                                &mut mirror_readiness,
                                &mut authority_status,
                                &mut reconnecting_since,
                                &mut reconnect_animation_frame,
                            );
                            let _ = mark_remote_surface_state_from_env(
                                &self.backend,
                                &spec.socket_name,
                                &target,
                                Some(generation),
                                RemoteSurfaceState::Reconnecting,
                            )?;
                            // Fall through to the reconnecting branch below.
                            // The first ~500 ms are a "verifying exit" phase
                            // that polls the catalog every 10 ms without
                            // drawing anything; after that we switch to the
                            // full reconnecting indicator.
                            continue;
                        }
                        AuthorityTransportEvent::Failed {
                            authority_id,
                            generation,
                            message,
                        } => {
                            if authority_id != target.address.authority_id()
                                || (generation.is_some() && authority_generation != generation)
                            {
                                continue;
                            }
                            if generation.is_some() {
                                begin_authority_reconnect(
                                    &remote_runtime,
                                    &target,
                                    &mut binding,
                                    &raw_input_route,
                                    &mut raw_screen_initialized,
                                    &mut mirror_readiness,
                                    &mut authority_status,
                                    &mut reconnecting_since,
                                    &mut reconnect_animation_frame,
                                );
                                let _ = mark_remote_surface_state_from_env(
                                    &self.backend,
                                    &spec.socket_name,
                                    &target,
                                    generation,
                                    RemoteSurfaceState::Reconnecting,
                                )?;
                            }
                            authority_status = AuthorityTransportStatus::Failed(message);
                            draw_remote_snapshot(
                                &terminal,
                                &target,
                                binding.as_ref(),
                                &observer.snapshot(),
                                &authority_status,
                                None,
                                None,
                                0,
                            )?;
                        }
                        AuthorityTransportEvent::RawPtyOutput {
                            authority_id,
                            generation,
                            payload,
                        } => {
                            if authority_id != target.address.authority_id()
                                || authority_generation != Some(generation)
                            {
                                continue;
                            }
                            if !output_payload_matches_target(
                                payload.session_id.as_str(),
                                payload.target_id.as_str(),
                                &target,
                            ) {
                                ERROR_LOG.log(format!(
                                    "dropping raw PTY output for wrong target: expected {}:{}, got {}:{}",
                                    target.address.session_id(),
                                    target.address.id().as_str(),
                                    payload.session_id,
                                    payload.target_id
                                ));
                                continue;
                            }
                            let raw = collect_direct_raw_pty_output_payload(
                                &target,
                                &authority_id,
                                &payload,
                                &mut direct_raw_output_last_seq,
                            )
                            .map_err(remote_protocol_error)?;
                            render_remote_output_and_mark_ready(
                                &raw,
                                &remote_runtime,
                                &target,
                                binding.as_ref(),
                                &mut pending_pty_size,
                                &mut paused_input_buffer,
                                &mut console_seq,
                                &mut mirror_readiness,
                                &mut reconnecting_since,
                                &mut raw_screen_initialized,
                                &last_synced_size,
                                &mut resize_acked_size,
                            )?;
                        }
                        AuthorityTransportEvent::Envelope {
                            authority_id,
                            generation,
                            envelope,
                        } => {
                            if authority_id != target.address.authority_id()
                                || authority_generation != Some(generation)
                            {
                                continue;
                            }
                            let output_payload_target = match &envelope.payload {
                                ControlPlanePayload::TargetOutput(p) => {
                                    Some((p.session_id.as_str(), p.target_id.as_str()))
                                }
                                ControlPlanePayload::RawPtyOutput(p) => {
                                    Some((p.session_id.as_str(), p.target_id.as_str()))
                                }
                                ControlPlanePayload::MirrorBootstrapChunk(p) => {
                                    Some((p.session_id.as_str(), p.target_id.as_str()))
                                }
                                ControlPlanePayload::MirrorBootstrapComplete(p) => {
                                    Some((p.session_id.as_str(), p.target_id.as_str()))
                                }
                                ControlPlanePayload::TargetGeometryChanged(p) => {
                                    Some((p.session_id.as_str(), p.target_id.as_str()))
                                }
                                _ => None,
                            };
                            if let Some((payload_session_id, payload_target_id)) =
                                output_payload_target
                            {
                                if !output_payload_matches_target(
                                    payload_session_id,
                                    payload_target_id,
                                    &target,
                                ) {
                                    ERROR_LOG.log(format!(
                                        "dropping authority output for wrong target: expected {}:{}, got {}:{}",
                                        target.address.session_id(),
                                        target.address.id().as_str(),
                                        payload_session_id,
                                        payload_target_id
                                    ));
                                    continue;
                                }
                            }
                            if let ControlPlanePayload::ResizeApplied(payload) = &envelope.payload {
                                if envelope.sender_id != target.address.authority_id() {
                                    continue;
                                }
                                if let Some(binding) = binding.as_ref() {
                                    if payload.resize_authority_console_id != binding.console_id {
                                        continue;
                                    }
                                    let acked_size = TerminalSize {
                                        cols: payload.cols as u16,
                                        rows: payload.rows as u16,
                                        pixel_width: 0,
                                        pixel_height: 0,
                                    };
                                    if acked_size != last_synced_size {
                                        ERROR_LOG.log(format!(
                                            "[diag] resize ack geometry mismatch: requested {}x{}, remote applied {}x{}",
                                            last_synced_size.cols,
                                            last_synced_size.rows,
                                            acked_size.cols,
                                            acked_size.rows
                                        ));
                                    }
                                    resize_acked_size = Some(acked_size);
                                    schedule_geometry_resync(
                                        &spec,
                                        &terminal,
                                        acked_size,
                                        &mut geometry_resync_pending,
                                        &mut geometry_resync_debounce_active,
                                        &event_tx,
                                    );
                                    try_flush_ready_inputs(
                                        &remote_runtime,
                                        &target,
                                        Some(binding),
                                        &mut pending_pty_size,
                                        &mut paused_input_buffer,
                                        &mut console_seq,
                                        &mirror_readiness,
                                        &last_synced_size,
                                        &mut resize_acked_size,
                                    )?;
                                }
                                continue;
                            }
                            if let ControlPlanePayload::TargetGeometryChanged(payload) =
                                &envelope.payload
                            {
                                if envelope.sender_id != target.address.authority_id() {
                                    continue;
                                }
                                let reported = TerminalSize {
                                    cols: payload.cols as u16,
                                    rows: payload.rows as u16,
                                    pixel_width: 0,
                                    pixel_height: 0,
                                };
                                if remote_reported_geometry != Some(reported) {
                                    ERROR_LOG.log(format!(
                                        "[diag] target geometry changed: {} -> {}x{}",
                                        remote_reported_geometry
                                            .map(|size| format!("{}x{}", size.cols, size.rows))
                                            .unwrap_or_else(|| "unknown".to_string()),
                                        reported.cols,
                                        reported.rows
                                    ));
                                    remote_reported_geometry = Some(reported);
                                }
                                schedule_geometry_resync(
                                    &spec,
                                    &terminal,
                                    reported,
                                    &mut geometry_resync_pending,
                                    &mut geometry_resync_debounce_active,
                                    &event_tx,
                                );
                                continue;
                            }
                            if let Some(raw) = collect_direct_raw_pty_output_envelope(
                                &target,
                                &envelope,
                                &mut direct_raw_output_last_seq,
                            )
                            .map_err(remote_protocol_error)?
                            {
                                render_remote_output_and_mark_ready(
                                    &raw,
                                    &remote_runtime,
                                    &target,
                                    binding.as_ref(),
                                    &mut pending_pty_size,
                                    &mut paused_input_buffer,
                                    &mut console_seq,
                                    &mut mirror_readiness,
                                    &mut reconnecting_since,
                                    &mut raw_screen_initialized,
                                    &last_synced_size,
                                    &mut resize_acked_size,
                                )?;
                                continue;
                            }
                            if let Err(e) =
                                apply_authority_envelope(&remote_runtime, &target, &envelope)
                            {
                                if e.to_string().contains("session exit") {
                                    ERROR_LOG.log(
                                        "[diag-timing] authority signalled session exit, shutting down"
                                            .to_string(),
                                    );
                                    let _ = std::io::Write::write_all(
                                        &mut io::stdout(),
                                        CLEAR_SCREEN_HOME_ESCAPE.as_bytes(),
                                    );
                                    let _ = std::io::Write::write_all(
                                        &mut io::stdout(),
                                        b"\r\n-- session exited --\r\n",
                                    );
                                    let _ = std::io::Write::flush(&mut io::stdout());

                                    // Clean remote target exit is an explicit workspace
                                    // state transition, not a pane-crash fallback.
                                    let pane_id = std::env::var("TMUX_PANE")
                                        .unwrap_or_else(|_| String::new());
                                    let _ = mark_remote_surface_state_from_env(
                                        &self.backend,
                                        &spec.socket_name,
                                        &target,
                                        authority_generation,
                                        RemoteSurfaceState::Exited,
                                    )?;
                                    signal_clean_remote_target_exit(&spec, &pane_id)?;
                                    return Ok(());
                                }
                                return Err(remote_protocol_error(e));
                            }
                            // Drain observer mailbox immediately: the mailbox
                            // watcher may never fire because sync() below
                            // consumes envelopes via try_recv(), making the
                            // snapshot appear empty.
                            let output = raw_output_reader
                                .sync_and_collect_raw()
                                .map_err(remote_protocol_error)?;
                            if !output.is_empty() {
                                ERROR_LOG.log(format!(
                                    "[diag-timing] Envelope drain: writing {} bytes to pane",
                                    output.len()
                                ));
                                render_remote_output_and_mark_ready(
                                    &output,
                                    &remote_runtime,
                                    &target,
                                    binding.as_ref(),
                                    &mut pending_pty_size,
                                    &mut paused_input_buffer,
                                    &mut console_seq,
                                    &mut mirror_readiness,
                                    &mut reconnecting_since,
                                    &mut raw_screen_initialized,
                                    &last_synced_size,
                                    &mut resize_acked_size,
                                )?;
                            }
                        }
                    },
                    RemotePaneEvent::TargetPresenceChanged(is_available) => {
                        let mut is_available = is_available;
                        if is_available {
                            let current_target =
                                self.resolve_remote_target(&spec.target, "remote target presence");
                            is_available = current_target
                                .as_ref()
                                .ok()
                                .is_some_and(|target| target_is_online(Some(target)));
                            if let Ok(current_target) = current_target {
                                target = current_target;
                            }
                        }
                        if !is_available {
                            let target_exists_in_catalog = target_is_present(&target_presence);
                            let target_availability = self
                                .resolve_remote_target(&spec.target, "remote target presence")
                                .ok()
                                .as_ref()
                                .and_then(|target| target_availability(Some(target)));
                            let should_exit = should_exit_surface_for_target_presence_loss(
                                target_availability,
                                remote_runtime.has_connection(target.address.authority_id()),
                                reconnecting_since.is_some(),
                            );
                            if should_exit {
                                ERROR_LOG.log(format!(
                                    "[diag-timing] target presence loss classified as exit: in_catalog={} availability={:?} authority_connected={} reconnecting={}",
                                    target_exists_in_catalog,
                                    target_availability,
                                    remote_runtime.has_connection(target.address.authority_id()),
                                    reconnecting_since.is_some(),
                                ));
                                let _ = mark_remote_surface_state_from_env(
                                    &self.backend,
                                    &spec.socket_name,
                                    &target,
                                    authority_generation,
                                    RemoteSurfaceState::Exited,
                                )?;
                                let pane_id =
                                    std::env::var("TMUX_PANE").unwrap_or_else(|_| String::new());
                                let _ = signal_clean_remote_target_exit(&spec, &pane_id);
                                return Ok(());
                            }
                        }
                        if !is_available {
                            // During reconnect: target disappearance is a
                            // catalog side-effect of network jitter. Clear
                            // local state but keep reconnecting.
                            begin_authority_reconnect(
                                &remote_runtime,
                                &target,
                                &mut binding,
                                &raw_input_route,
                                &mut raw_screen_initialized,
                                &mut mirror_readiness,
                                &mut authority_status,
                                &mut reconnecting_since,
                                &mut reconnect_animation_frame,
                            );
                            let _ = mark_remote_surface_state_from_env(
                                &self.backend,
                                &spec.socket_name,
                                &target,
                                authority_generation,
                                RemoteSurfaceState::Reconnecting,
                            )?;
                        }
                        // During reconnect, keep status as Disconnected so the
                        // last known content stays visible with reconnecting bar
                        // instead of downgrading to WaitingForRemoteAuthority
                        // which would force placeholder display.
                        // When target returns, allow upgrade to Connected so the
                        // reactivation logic below can trigger.
                        authority_status = authority_status_from_runtime(
                            &remote_runtime,
                            &target,
                            is_available,
                            &waiting_authority_status,
                        );
                        if reconnecting_since.is_some()
                            && authority_status
                                == AuthorityTransportStatus::WaitingForRemoteAuthority
                        {
                            authority_status = AuthorityTransportStatus::Disconnected;
                        }
                        // When both target and authority transport are back
                        // while reconnecting, reactivate. This handles the
                        // race where Connected arrives before or after
                        // TargetPresenceChanged(true).
                        let mut activated = false;
                        if is_available
                            && binding.is_none()
                            && matches!(authority_status, AuthorityTransportStatus::Connected)
                        {
                            let size = current_remote_surface_size(&spec, &terminal);
                            match activate_remote_surface_binding(
                                &remote_runtime,
                                &target,
                                &spec,
                                &size,
                                &mut observer,
                                &mut raw_output_reader,
                                &raw_input_route,
                                &mut pending_pty_size,
                                &mut last_synced_size,
                                &mut resize_acked_size,
                                &mut raw_screen_initialized,
                                event_tx.clone(),
                            ) {
                                Ok(activated_binding) => {
                                    binding = Some(activated_binding);
                                    mirror_readiness = MirrorReadiness::Waiting;
                                    activated = true;
                                    let _ = mark_remote_surface_state_from_env(
                                        &self.backend,
                                        &spec.socket_name,
                                        &target,
                                        authority_generation,
                                        RemoteSurfaceState::Connected,
                                    )?;
                                }
                                Err(error) => {
                                    authority_status =
                                        AuthorityTransportStatus::Failed(error.to_string());
                                }
                            }
                        }
                        if !activated {
                            let _ = observer.sync();
                            draw_remote_snapshot(
                                &terminal,
                                &target,
                                binding.as_ref(),
                                &observer.snapshot(),
                                &authority_status,
                                None,
                                reconnecting_since.map(|i| i.elapsed()),
                                reconnect_animation_frame,
                            )?;
                        }
                    }
                    RemotePaneEvent::Input {
                        bytes,
                        raw_forwarded,
                    } => {
                        for signal in input_signal_decoder.feed(&spec, &bytes) {
                            on_signal(signal);
                        }
                        if should_exit_surface_locally(&spec, &bytes) {
                            return Ok(());
                        }
                        if slot_pane_helpers::is_local_navigation_sequence(&bytes) {
                            slot_pane_helpers::try_local_navigation(&spec.socket_name, &bytes);
                            continue;
                        }
                        if let Some(binding) = binding.as_ref() {
                            if bytes.is_empty() {
                                continue;
                            }
                            console_seq += 1;
                            if raw_forwarded {
                                // Raw mode sends PTY bytes directly from the stdin thread to avoid
                                // adding the UI event loop to every keystroke.
                                continue;
                            }
                            if mirror_readiness != MirrorReadiness::Ready {
                                paused_input_buffer.push(bytes);
                                continue;
                            }
                            if !is_resize_acked(&last_synced_size, &resize_acked_size) {
                                paused_input_buffer.push(bytes);
                                continue;
                            }
                            if !remote_runtime.has_connection(target.address.authority_id()) {
                                ERROR_LOG.log(format!(
                                    "[diag-timing] remote input deferred until authority registers: authority={}",
                                    target.address.authority_id()
                                ));
                                paused_input_buffer.push(bytes);
                                continue;
                            }
                            if let Err(error) = remote_runtime.send_raw_pty_input(
                                &target,
                                binding,
                                console_seq,
                                bytes.clone(),
                            ) {
                                return Err(error);
                            }
                        } else if !bytes.is_empty() && !raw_forwarded {
                            // During reconnection (binding is None), buffer
                            // input locally so keystrokes aren't lost. The
                            // buffer is flushed when reactivation restores
                            // the binding.
                            paused_input_buffer.push(bytes);
                        }
                    }
                }
            }
        })();
        if let Some(binding) = binding.as_ref() {
            let _ = remote_runtime.close_target(&target, binding);
        }
        let _ = mark_remote_surface_state_from_env(
            &self.backend,
            &spec.socket_name,
            &target,
            authority_generation,
            RemoteSurfaceState::Exited,
        );
        if let Err(error) = &run_result {
            ERROR_LOG.log(format!(
                "[diag-timing] remote pane run_result failed: {error}"
            ));
        }
        run_result
    }

    fn resolve_remote_target(
        &self,
        target_id: &str,
        surface_label: &str,
    ) -> Result<ManagedSessionRecord, LifecycleError> {
        let session = self
            .target_registry
            .find_target(target_id)
            .map_err(remote_pane_error)?
            .ok_or_else(|| {
                LifecycleError::Protocol(format!(
                    "unknown remote target `{}` for {surface_label}",
                    target_id
                ))
            })?;
        if session.address.transport() != &SessionTransport::RemotePeer {
            return Err(LifecycleError::Protocol(format!(
                "target `{}` is not a remote target",
                target_id
            )));
        }
        Ok(session)
    }
}

fn schedule_post_activation_resize_probe(tx: mpsc::Sender<RemotePaneEvent>) {
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(50));
        let _ = tx.send(RemotePaneEvent::Resize);
    });
}

const GEOMETRY_RESYNC_DEBOUNCE: Duration = Duration::from_millis(200);

/// Schedule a debounced local re-sync when the authority-reported geometry
/// differs from the size of the pane this slot is rendering into.  The
/// re-sync resizes the local pane/window to the reported geometry and
/// re-runs the mirror activation (clear + fresh bootstrap), so raw output is
/// never replayed at the wrong size for long.
fn schedule_geometry_resync(
    spec: &RemoteInteractSurfaceSpec,
    terminal: &TerminalRuntime,
    reported: TerminalSize,
    pending: &mut Option<TerminalSize>,
    debounce_active: &mut bool,
    event_tx: &mpsc::Sender<RemotePaneEvent>,
) {
    if reported == current_remote_surface_size(spec, terminal) {
        return;
    }
    *pending = Some(reported);
    if *debounce_active {
        return;
    }
    *debounce_active = true;
    let tx = event_tx.clone();
    thread::spawn(move || {
        thread::sleep(GEOMETRY_RESYNC_DEBOUNCE);
        let _ = tx.send(RemotePaneEvent::GeometryResyncDue);
    });
}

fn current_remote_surface_size(
    spec: &RemoteInteractSurfaceSpec,
    terminal: &TerminalRuntime,
) -> TerminalSize {
    pane_size_from_tmux(spec).unwrap_or_else(|| terminal.current_size_or_default())
}

fn pane_size_from_tmux(spec: &RemoteInteractSurfaceSpec) -> Option<TerminalSize> {
    let pane_id = std::env::var("TMUX_PANE").ok()?;
    if pane_id.trim().is_empty() {
        return None;
    }
    let backend = crate::infra::tmux::EmbeddedTmuxBackend::from_build_env().ok()?;
    let (cols, rows) = backend
        .pane_dimensions_on_socket(&spec.socket_name, pane_id.trim())
        .ok()?;
    if cols == 0 || rows == 0 {
        return None;
    }
    Some(TerminalSize {
        rows: rows.clamp(1, u16::MAX as usize) as u16,
        cols: cols.clamp(1, u16::MAX as usize) as u16,
        pixel_width: 0,
        pixel_height: 0,
    })
}

fn activate_remote_surface_binding(
    remote_runtime: &RemoteMainSlotRuntime,
    target: &ManagedSessionRecord,
    spec: &RemoteInteractSurfaceSpec,
    size: &TerminalSize,
    observer: &mut RemoteObserverRuntime,
    raw_output_reader: &mut RemoteRawPtyMailboxReader,
    raw_input_route: &RawPtyInputRoute,
    pending_pty_size: &mut Option<TerminalSize>,
    last_synced_size: &mut TerminalSize,
    resize_acked_size: &mut Option<TerminalSize>,
    raw_screen_initialized: &mut bool,
    event_tx: mpsc::Sender<RemotePaneEvent>,
) -> Result<RemoteAttachmentBinding, LifecycleError> {
    let (activated_binding, raw) = activate_surface_target_with_mode(
        remote_runtime,
        target,
        spec,
        size,
        observer,
        raw_output_reader,
    )?;
    raw_input_route.activate(target, &activated_binding, &spec.console_host_id);
    schedule_post_activation_resize_probe(event_tx);
    sync_or_defer_remote_pty_size(
        remote_runtime,
        target,
        &activated_binding,
        size,
        pending_pty_size,
    )?;
    *last_synced_size = *size;
    *resize_acked_size = None;
    write_remote_raw_output_with_initial_clear(&raw, raw_screen_initialized)?;
    let _ = flush_pending_pty_size(remote_runtime, target, &activated_binding, pending_pty_size)?;
    if pending_pty_size.is_none() {
        *resize_acked_size = None;
    }
    Ok(activated_binding)
}

fn render_remote_output_and_mark_ready(
    raw: &[u8],
    remote_runtime: &RemoteMainSlotRuntime,
    target: &ManagedSessionRecord,
    binding: Option<&RemoteAttachmentBinding>,
    pending_pty_size: &mut Option<TerminalSize>,
    paused_input_buffer: &mut Vec<Vec<u8>>,
    console_seq: &mut u64,
    mirror_readiness: &mut MirrorReadiness,
    reconnecting_since: &mut Option<Instant>,
    raw_screen_initialized: &mut bool,
    last_synced_size: &TerminalSize,
    resize_acked_size: &mut Option<TerminalSize>,
) -> Result<(), LifecycleError> {
    if raw.is_empty() {
        return Ok(());
    }
    write_remote_raw_output_with_initial_clear(raw, raw_screen_initialized)?;
    mark_mirror_ready(
        remote_runtime,
        target,
        binding,
        pending_pty_size,
        paused_input_buffer,
        console_seq,
        mirror_readiness,
        reconnecting_since,
        last_synced_size,
        resize_acked_size,
    )
}

fn begin_authority_reconnect(
    remote_runtime: &RemoteMainSlotRuntime,
    target: &ManagedSessionRecord,
    binding: &mut Option<RemoteAttachmentBinding>,
    raw_input_route: &RawPtyInputRoute,
    raw_screen_initialized: &mut bool,
    mirror_readiness: &mut MirrorReadiness,
    authority_status: &mut AuthorityTransportStatus,
    reconnecting_since: &mut Option<Instant>,
    reconnect_animation_frame: &mut u8,
) {
    remote_runtime.handle_authority_disconnect(target.address.authority_id());
    *binding = None;
    raw_input_route.clear();
    *raw_screen_initialized = false;
    *mirror_readiness = MirrorReadiness::Waiting;
    *authority_status = AuthorityTransportStatus::Disconnected;
    *reconnecting_since = Some(Instant::now());
    *reconnect_animation_frame = 0;
}

#[cfg(test)]
fn mark_mirror_ready_if_raw_arrived(
    raw: &[u8],
    remote_runtime: &RemoteMainSlotRuntime,
    target: &ManagedSessionRecord,
    binding: Option<&RemoteAttachmentBinding>,
    pending_pty_size: &mut Option<TerminalSize>,
    paused_input_buffer: &mut Vec<Vec<u8>>,
    console_seq: &mut u64,
    mirror_readiness: &mut MirrorReadiness,
    reconnecting_since: &mut Option<Instant>,
    last_synced_size: &TerminalSize,
    resize_acked_size: &mut Option<TerminalSize>,
) -> Result<(), LifecycleError> {
    if raw.is_empty() {
        return Ok(());
    }
    mark_mirror_ready(
        remote_runtime,
        target,
        binding,
        pending_pty_size,
        paused_input_buffer,
        console_seq,
        mirror_readiness,
        reconnecting_since,
        last_synced_size,
        resize_acked_size,
    )
}

fn is_resize_acked(
    last_synced_size: &TerminalSize,
    resize_acked_size: &Option<TerminalSize>,
) -> bool {
    resize_acked_size.as_ref() == Some(last_synced_size)
}

fn try_flush_ready_inputs(
    remote_runtime: &RemoteMainSlotRuntime,
    target: &ManagedSessionRecord,
    binding: Option<&RemoteAttachmentBinding>,
    pending_pty_size: &mut Option<TerminalSize>,
    paused_input_buffer: &mut Vec<Vec<u8>>,
    console_seq: &mut u64,
    mirror_readiness: &MirrorReadiness,
    last_synced_size: &TerminalSize,
    resize_acked_size: &mut Option<TerminalSize>,
) -> Result<(), LifecycleError> {
    if *mirror_readiness != MirrorReadiness::Ready {
        return Ok(());
    }
    if !is_resize_acked(last_synced_size, resize_acked_size) {
        return Ok(());
    }
    let Some(binding) = binding else {
        return Ok(());
    };
    let flushed = flush_pending_pty_size(remote_runtime, target, binding, pending_pty_size)?;
    if flushed {
        // A deferred resize was just sent; wait for its ack before flushing
        // paused keystrokes.
        *resize_acked_size = None;
        return Ok(());
    }
    flush_paused_input(
        remote_runtime,
        target,
        binding,
        paused_input_buffer,
        console_seq,
    )?;
    Ok(())
}

fn mark_mirror_ready(
    remote_runtime: &RemoteMainSlotRuntime,
    target: &ManagedSessionRecord,
    binding: Option<&RemoteAttachmentBinding>,
    pending_pty_size: &mut Option<TerminalSize>,
    paused_input_buffer: &mut Vec<Vec<u8>>,
    console_seq: &mut u64,
    mirror_readiness: &mut MirrorReadiness,
    reconnecting_since: &mut Option<Instant>,
    last_synced_size: &TerminalSize,
    resize_acked_size: &mut Option<TerminalSize>,
) -> Result<(), LifecycleError> {
    if *mirror_readiness == MirrorReadiness::Ready {
        *reconnecting_since = None;
        return Ok(());
    }
    *mirror_readiness = MirrorReadiness::Ready;
    *reconnecting_since = None;
    try_flush_ready_inputs(
        remote_runtime,
        target,
        binding,
        pending_pty_size,
        paused_input_buffer,
        console_seq,
        mirror_readiness,
        last_synced_size,
        resize_acked_size,
    )
}

fn sync_remote_pty_size(
    remote_runtime: &RemoteMainSlotRuntime,
    target: &ManagedSessionRecord,
    binding: &RemoteAttachmentBinding,
    size: &TerminalSize,
) -> Result<(), LifecycleError> {
    remote_runtime.send_pty_resize(
        target,
        binding,
        usize::from(size.cols),
        usize::from(size.rows),
    )
}

fn sync_or_defer_remote_pty_size(
    remote_runtime: &RemoteMainSlotRuntime,
    target: &ManagedSessionRecord,
    binding: &RemoteAttachmentBinding,
    size: &TerminalSize,
    pending_pty_size: &mut Option<TerminalSize>,
) -> Result<(), LifecycleError> {
    if !remote_runtime.has_connection(target.address.authority_id()) {
        ERROR_LOG.log(format!(
            "[diag-timing] remote PTY resize deferred until authority registers: authority={}",
            target.address.authority_id()
        ));
        *pending_pty_size = Some(*size);
        return Ok(());
    }
    match sync_remote_pty_size(remote_runtime, target, binding, size) {
        Ok(()) => {
            *pending_pty_size = None;
            Ok(())
        }
        Err(error)
            if is_remote_authority_unavailable(&error)
                || !remote_runtime.has_connection(target.address.authority_id()) =>
        {
            ERROR_LOG.log(format!(
                "[diag-timing] remote PTY resize deferred after authority unregistered: {error}"
            ));
            *pending_pty_size = Some(*size);
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn flush_pending_pty_size(
    remote_runtime: &RemoteMainSlotRuntime,
    target: &ManagedSessionRecord,
    binding: &RemoteAttachmentBinding,
    pending_pty_size: &mut Option<TerminalSize>,
) -> Result<bool, LifecycleError> {
    let Some(size) = *pending_pty_size else {
        return Ok(false);
    };
    if !remote_runtime.has_connection(target.address.authority_id()) {
        return Ok(false);
    }
    match sync_remote_pty_size(remote_runtime, target, binding, &size) {
        Ok(()) => {
            *pending_pty_size = None;
            Ok(true)
        }
        Err(error)
            if is_remote_authority_unavailable(&error)
                || !remote_runtime.has_connection(target.address.authority_id()) =>
        {
            ERROR_LOG.log(format!(
                "[diag-timing] pending remote PTY resize kept after authority unregistered: {error}"
            ));
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

fn signal_clean_remote_target_exit(
    spec: &RemoteInteractSurfaceSpec,
    pane_id: &str,
) -> Result<(), LifecycleError> {
    if pane_id.is_empty() {
        ERROR_LOG.log(format!(
            "[diag-exit] signal_clean_exit_skip_empty_pane target={} socket={} session={} stage=signal_clean_exit",
            spec.target,
            spec.socket_name,
            spec.surface_scope
        ));
        return Ok(());
    }

    ERROR_LOG.log_exit_latency(format!(
        "[diag-exit] signal_clean_exit_start target={} socket={} session={} pane={} stage=signal_clean_exit",
        spec.target,
        spec.socket_name,
        spec.surface_scope,
        pane_id
    ));

    let backend =
        crate::infra::tmux::EmbeddedTmuxBackend::from_build_env().map_err(remote_pane_error)?;
    let workspace = crate::infra::tmux::TmuxWorkspaceHandle {
        workspace_id: crate::domain::workspace::WorkspaceInstanceId::new(
            spec.surface_scope.clone(),
        ),
        socket_name: crate::infra::tmux::TmuxSocketName::new(spec.socket_name.clone()),
        session_name: crate::infra::tmux::TmuxSessionName::new(spec.surface_scope.clone()),
    };
    let pane = crate::infra::tmux::TmuxPaneId::new(pane_id);
    let _ = backend.unset_pane_hook(&workspace, &pane, MAIN_PANE_DIED_HOOK);

    let waitagent = current_waitagent_executable()?;
    let shell_command = [
        shell_escape(&waitagent.display().to_string()),
        shell_escape("__remote-target-exited"),
        shell_escape("--socket-name"),
        shell_escape(&spec.socket_name),
        shell_escape("--session-name"),
        shell_escape(&spec.surface_scope),
        shell_escape("--target"),
        shell_escape(&spec.target),
        shell_escape("--pane-id"),
        shell_escape(pane_id),
    ]
    .join(" ");
    let result = backend
        .run_socket_command(
            &workspace.socket_name,
            &["run-shell".to_string(), "-b".to_string(), shell_command],
        )
        .map_err(remote_pane_error);
    ERROR_LOG.log_exit_latency(format!(
        "[diag-exit] signal_clean_exit_dispatched target={} socket={} session={} pane={} ok={} stage=signal_clean_exit",
        spec.target,
        spec.socket_name,
        spec.surface_scope,
        pane_id,
        result.is_ok()
    ));
    result
}

fn shell_escape(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

/// Publish the negotiated geometry as the workspace's main-slot geometry
/// intent via the hidden layout command. The command applies the intent
/// synchronously (padding included) through the workspace layout runtime, so
/// on success this pane already renders at `size`.
fn publish_main_slot_geometry_intent(
    spec: &RemoteInteractSurfaceSpec,
    session_name: &str,
    pane_id: &str,
    size: TerminalSize,
) -> Result<(), LifecycleError> {
    let waitagent = current_waitagent_executable()?;
    let status = std::process::Command::new(waitagent)
        .arg("__layout-main-slot-geometry")
        .arg("--socket-name")
        .arg(&spec.socket_name)
        .arg("--session-name")
        .arg(session_name)
        .arg("--pane-id")
        .arg(pane_id)
        .arg("--pane-pid")
        .arg(std::process::id().to_string())
        .arg("--cols")
        .arg(size.cols.to_string())
        .arg("--rows")
        .arg(size.rows.to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(remote_pane_error)?;
    if status.success() {
        Ok(())
    } else {
        Err(remote_pane_error(io::Error::new(
            io::ErrorKind::Other,
            format!("main-slot geometry intent command exited with {status}"),
        )))
    }
}

/// Viewer capacity while the geometry intent is active: the pane is pinned to
/// the negotiated size by padding, so capacity is derived from the window
/// size minus the pinned chrome instead of the coordinated pane size.
fn window_main_area_capacity(spec: &RemoteInteractSurfaceSpec) -> Option<TerminalSize> {
    let pane_id = std::env::var("TMUX_PANE").ok()?.trim().to_string();
    if pane_id.is_empty() {
        return None;
    }
    let backend = crate::infra::tmux::EmbeddedTmuxBackend::from_build_env().ok()?;
    let output = backend
        .run_on_socket(
            &crate::infra::tmux::TmuxSocketName::new(spec.socket_name.clone()),
            &[
                "display-message".to_string(),
                "-p".to_string(),
                "-t".to_string(),
                pane_id,
                "#{window_id}\t#{window_width}\t#{window_height}".to_string(),
            ],
        )
        .ok()?;
    let mut parts = output.stdout.trim().split('\t');
    let window_id = parts.next()?;
    let window_cols = parts.next()?.parse::<usize>().ok()?;
    let window_rows = parts.next()?.parse::<usize>().ok()?;
    let panes = backend
        .list_panes_detailed_on_socket(&spec.socket_name, window_id)
        .ok()?;
    let has_sidebar = panes
        .iter()
        .any(|(_, title, _, _)| title.as_str() == SIDEBAR_PANE_TITLE);
    let has_footer = panes
        .iter()
        .any(|(_, title, _, _)| title.as_str() == FOOTER_PANE_TITLE);
    let chrome_cols = if has_sidebar {
        usize::from(SIDEBAR_WIDTH_CELLS) + 1
    } else {
        0
    };
    let chrome_rows = if has_footer {
        usize::from(FOOTER_HEIGHT_CELLS) + 1
    } else {
        0
    };
    let cols = window_cols.saturating_sub(chrome_cols);
    let rows = window_rows.saturating_sub(chrome_rows);
    if cols == 0 || rows == 0 {
        return None;
    }
    Some(TerminalSize {
        rows: rows.min(u16::MAX as usize) as u16,
        cols: cols.min(u16::MAX as usize) as u16,
        pixel_width: 0,
        pixel_height: 0,
    })
}

/// Best-effort cleanup for the main-slot geometry intent: when the
/// remote-main-slot process exits, clear the intent so the workspace layout
/// reverts to filling the main slot. The layout runtime also self-heals stale
/// intents via the pane-pid check; this only avoids waiting for the next
/// reconcile round.
struct MainSlotGeometryIntentGuard {
    socket_name: String,
    session_name: String,
    armed: bool,
}

impl MainSlotGeometryIntentGuard {
    fn new(spec: &RemoteInteractSurfaceSpec) -> Self {
        Self {
            socket_name: spec.socket_name.clone(),
            session_name: spec.surface_scope.clone(),
            armed: false,
        }
    }

    fn arm(&mut self) {
        self.armed = true;
    }
}

impl Drop for MainSlotGeometryIntentGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Ok(waitagent) = current_waitagent_executable() else {
            return;
        };
        let Ok(backend) = crate::infra::tmux::EmbeddedTmuxBackend::from_build_env() else {
            return;
        };
        let shell_command = [
            shell_escape(&waitagent.display().to_string()),
            shell_escape("__layout-main-slot-geometry"),
            shell_escape("--socket-name"),
            shell_escape(&self.socket_name),
            shell_escape("--session-name"),
            shell_escape(&self.session_name),
            shell_escape("--clear"),
        ]
        .join(" ");
        let _ = backend.run_socket_command(
            &crate::infra::tmux::TmuxSocketName::new(self.socket_name.clone()),
            &["run-shell".to_string(), "-b".to_string(), shell_command],
        );
    }
}

/// Flush any input that was buffered during a reconnection period.
/// Each buffered chunk is sent as a separate PTY input message so
/// that keystrokes queued while the binding was down are not lost.
fn flush_paused_input(
    remote_runtime: &RemoteMainSlotRuntime,
    target: &ManagedSessionRecord,
    binding: &RemoteAttachmentBinding,
    buffer: &mut Vec<Vec<u8>>,
    console_seq: &mut u64,
) -> Result<(), LifecycleError> {
    if buffer.is_empty() {
        return Ok(());
    }
    if !remote_runtime.has_connection(target.address.authority_id()) {
        ERROR_LOG.log(format!(
            "[diag-timing] paused remote input kept until authority registers: authority={}",
            target.address.authority_id()
        ));
        return Ok(());
    }

    let mut flushed = 0usize;
    for chunk in buffer.iter() {
        *console_seq += 1;
        match remote_runtime.send_raw_pty_input(target, binding, *console_seq, chunk.clone()) {
            Ok(()) => {
                flushed += 1;
            }
            Err(error)
                if is_remote_authority_unavailable(&error)
                    || !remote_runtime.has_connection(target.address.authority_id()) =>
            {
                ERROR_LOG.log(format!(
                    "[diag-timing] paused remote input kept after authority unregistered: {error}"
                ));
                break;
            }
            Err(error) => return Err(error),
        }
    }
    if flushed > 0 {
        buffer.drain(..flushed);
    }
    Ok(())
}

fn is_remote_authority_unavailable(error: &LifecycleError) -> bool {
    matches!(error, LifecycleError::Protocol(message) if message.contains("remote control-plane connection for node") && message.contains("is not registered"))
}

#[cfg(test)]
mod remote_main_slot_pane_runtime_test;
