use crate::application::target_registry_service::{
    DefaultTargetCatalogGateway, TargetRegistryService,
};
use crate::cli::{RemoteMainSlotCommand, RemoteNetworkConfig};
use crate::domain::session_catalog::{ConsoleLocation, ManagedSessionRecord, SessionTransport};
use crate::infra::error_log::ERROR_LOG;
use crate::infra::tmux::EmbeddedTmuxBackend;
use crate::infra::tmux::TmuxLayoutGateway;
use crate::lifecycle::LifecycleError;
use crate::runtime::current_executable::current_waitagent_executable;
use crate::runtime::remote_authority_connection_runtime::{
    AuthorityConnectionGuard, AuthorityConnectionRequest, AuthorityConnectionStarter,
    AuthorityTransportEvent, QueuedAuthorityStreamSink, QueuedAuthorityStreamStarter,
};
use crate::runtime::remote_authority_transport_runtime::authority_transport_socket_path;
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
        let mut authority_status = waiting_authority_status.clone();
        let mut authority_generation: Option<u64> = None;
        // Always attempt activation — output_log replay comes from the
        // local mailbox; no need to wait for authority transport.
        match activate_remote_surface_binding(
            &remote_runtime,
            &target,
            &spec,
            &initial_size,
            &mut observer,
            &raw_input_route,
            &mut pending_pty_size,
            &mut last_synced_size,
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
                                return Ok(());
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
                        write_remote_raw_output_with_initial_clear(
                            &raw,
                            &mut raw_screen_initialized,
                        )?;
                    }
                    RemotePaneEvent::Resize => {
                        let size = current_remote_surface_size(&spec, &terminal);
                        let resize_is_user_visible = should_sync_remote_pty_resize(&spec)?;
                        if size != last_synced_size && resize_is_user_visible {
                            if let Some(binding) = binding.as_ref() {
                                sync_or_defer_remote_pty_size(
                                    &remote_runtime,
                                    &target,
                                    binding,
                                    &size,
                                    &mut pending_pty_size,
                                )?;
                                last_synced_size = size;
                            }
                        }
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
                                flush_pending_pty_size(
                                    &remote_runtime,
                                    &target,
                                    binding,
                                    &mut pending_pty_size,
                                )?;
                                if mirror_readiness == MirrorReadiness::Ready {
                                    flush_paused_input(
                                        &remote_runtime,
                                        &target,
                                        binding,
                                        &mut paused_input_buffer,
                                        &mut console_seq,
                                    )?;
                                }
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
                                    &raw_input_route,
                                    &mut pending_pty_size,
                                    &mut last_synced_size,
                                    &mut raw_screen_initialized,
                                    event_tx.clone(),
                                ) {
                                    Ok(activated_binding) => {
                                        binding = Some(activated_binding);
                                        mirror_readiness = MirrorReadiness::Waiting;
                                        activated = true;
                                    }
                                    Err(error) => {
                                        authority_status =
                                            AuthorityTransportStatus::Failed(error.to_string());
                                    }
                                }
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
                            let raw = collect_direct_raw_pty_output_payload(
                                &target,
                                &authority_id,
                                &payload,
                                &mut direct_raw_output_last_seq,
                            )
                            .map_err(remote_protocol_error)?;
                            write_remote_raw_output_with_initial_clear(
                                &raw,
                                &mut raw_screen_initialized,
                            )?;
                            mark_mirror_ready_if_raw_arrived(
                                &raw,
                                &remote_runtime,
                                &target,
                                binding.as_ref(),
                                &mut pending_pty_size,
                                &mut paused_input_buffer,
                                &mut console_seq,
                                &mut mirror_readiness,
                                &mut reconnecting_since,
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
                            if let Some(raw) = collect_direct_raw_pty_output_envelope(
                                &target,
                                &envelope,
                                &mut direct_raw_output_last_seq,
                            )
                            .map_err(remote_protocol_error)?
                            {
                                write_remote_raw_output_with_initial_clear(
                                    &raw,
                                    &mut raw_screen_initialized,
                                )?;
                                mark_mirror_ready_if_raw_arrived(
                                    &raw,
                                    &remote_runtime,
                                    &target,
                                    binding.as_ref(),
                                    &mut pending_pty_size,
                                    &mut paused_input_buffer,
                                    &mut console_seq,
                                    &mut mirror_readiness,
                                    &mut reconnecting_since,
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
                                    signal_clean_remote_target_exit(&spec, &pane_id)?;
                                    return Ok(());
                                }
                                return Err(remote_protocol_error(e));
                            }
                            mark_mirror_ready_if_bootstrap_completed(
                                &envelope,
                                &remote_runtime,
                                &target,
                                binding.as_ref(),
                                &mut pending_pty_size,
                                &mut paused_input_buffer,
                                &mut console_seq,
                                &mut mirror_readiness,
                                &mut reconnecting_since,
                            )?;
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
                                write_remote_raw_output_with_initial_clear(
                                    &output,
                                    &mut raw_screen_initialized,
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
                            let should_exit = should_exit_surface_for_target_presence_loss(
                                target_exists_in_catalog,
                                remote_runtime.has_connection(target.address.authority_id()),
                                reconnecting_since.is_some(),
                            );
                            if should_exit {
                                ERROR_LOG.log(format!(
                                    "[diag-timing] target presence loss classified as exit: in_catalog={} authority_connected={} reconnecting={}",
                                    target_exists_in_catalog,
                                    remote_runtime.has_connection(target.address.authority_id()),
                                    reconnecting_since.is_some(),
                                ));
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
                                &raw_input_route,
                                &mut pending_pty_size,
                                &mut last_synced_size,
                                &mut raw_screen_initialized,
                                event_tx.clone(),
                            ) {
                                Ok(activated_binding) => {
                                    binding = Some(activated_binding);
                                    mirror_readiness = MirrorReadiness::Waiting;
                                    activated = true;
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

fn should_sync_remote_pty_resize(spec: &RemoteInteractSurfaceSpec) -> Result<bool, LifecycleError> {
    if spec.console_location != ConsoleLocation::LocalWorkspace {
        return Ok(true);
    }
    let pane_id = std::env::var("TMUX_PANE").map_err(|error| {
        LifecycleError::Protocol(format!(
            "workspace remote pane resize missing TMUX_PANE for target {}: {error}",
            spec.target
        ))
    })?;
    let pane_id = pane_id.trim();
    if pane_id.is_empty() {
        return Err(LifecycleError::Protocol(format!(
            "workspace remote pane resize has empty TMUX_PANE for target {}",
            spec.target
        )));
    }
    let backend =
        crate::infra::tmux::EmbeddedTmuxBackend::from_build_env().map_err(remote_pane_error)?;
    let workspace = crate::infra::tmux::TmuxWorkspaceHandle {
        workspace_id: crate::domain::workspace::WorkspaceInstanceId::new(
            spec.surface_scope.clone(),
        ),
        socket_name: crate::infra::tmux::TmuxSocketName::new(spec.socket_name.clone()),
        session_name: crate::infra::tmux::TmuxSessionName::new(spec.surface_scope.clone()),
    };
    let main_pane = backend
        .show_session_option(&workspace, "@waitagent_main_pane_id")
        .map_err(remote_pane_error)?;
    let active_target = backend
        .show_session_option(&workspace, "@waitagent_active_target")
        .map_err(remote_pane_error)?;
    let window_active = backend
        .run_on_socket(
            &workspace.socket_name,
            &[
                "display-message".to_string(),
                "-p".to_string(),
                "-t".to_string(),
                pane_id.to_string(),
                "#{window_active}".to_string(),
            ],
        )
        .map_err(remote_pane_error)?
        .stdout
        .trim()
        == "1";
    Ok(should_sync_remote_pty_resize_for_state(
        spec,
        pane_id,
        main_pane.as_deref(),
        active_target.as_deref(),
        window_active,
    ))
}

fn should_sync_remote_pty_resize_for_state(
    spec: &RemoteInteractSurfaceSpec,
    pane_id: &str,
    main_pane: Option<&str>,
    active_target: Option<&str>,
    window_active: bool,
) -> bool {
    if spec.console_location != ConsoleLocation::LocalWorkspace {
        return true;
    }
    window_active && main_pane == Some(pane_id) && active_target == Some(spec.target.as_str())
}

fn activate_remote_surface_binding(
    remote_runtime: &RemoteMainSlotRuntime,
    target: &ManagedSessionRecord,
    spec: &RemoteInteractSurfaceSpec,
    size: &TerminalSize,
    observer: &mut RemoteObserverRuntime,
    raw_input_route: &RawPtyInputRoute,
    pending_pty_size: &mut Option<TerminalSize>,
    last_synced_size: &mut TerminalSize,
    raw_screen_initialized: &mut bool,
    event_tx: mpsc::Sender<RemotePaneEvent>,
) -> Result<RemoteAttachmentBinding, LifecycleError> {
    let (activated_binding, raw) =
        activate_surface_target_with_mode(remote_runtime, target, spec, size, observer)?;
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
    write_remote_raw_output_with_initial_clear(&raw, raw_screen_initialized)?;
    flush_pending_pty_size(remote_runtime, target, &activated_binding, pending_pty_size)?;
    Ok(activated_binding)
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
    )
}

fn mark_mirror_ready_if_bootstrap_completed(
    envelope: &crate::infra::remote_protocol::ProtocolEnvelope<
        crate::infra::remote_protocol::ControlPlanePayload,
    >,
    remote_runtime: &RemoteMainSlotRuntime,
    target: &ManagedSessionRecord,
    binding: Option<&RemoteAttachmentBinding>,
    pending_pty_size: &mut Option<TerminalSize>,
    paused_input_buffer: &mut Vec<Vec<u8>>,
    console_seq: &mut u64,
    mirror_readiness: &mut MirrorReadiness,
    reconnecting_since: &mut Option<Instant>,
) -> Result<(), LifecycleError> {
    if matches!(
        envelope.payload,
        crate::infra::remote_protocol::ControlPlanePayload::MirrorBootstrapComplete(_)
    ) {
        mark_mirror_ready(
            remote_runtime,
            target,
            binding,
            pending_pty_size,
            paused_input_buffer,
            console_seq,
            mirror_readiness,
            reconnecting_since,
        )?;
    }
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
) -> Result<(), LifecycleError> {
    if *mirror_readiness == MirrorReadiness::Ready {
        *reconnecting_since = None;
        return Ok(());
    }
    *mirror_readiness = MirrorReadiness::Ready;
    *reconnecting_since = None;
    if let Some(binding) = binding {
        flush_pending_pty_size(remote_runtime, target, binding, pending_pty_size)?;
        flush_paused_input(
            remote_runtime,
            target,
            binding,
            paused_input_buffer,
            console_seq,
        )?;
    }
    Ok(())
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
        Ok(()) => Ok(()),
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
) -> Result<(), LifecycleError> {
    let Some(size) = *pending_pty_size else {
        return Ok(());
    };
    if !remote_runtime.has_connection(target.address.authority_id()) {
        return Ok(());
    }
    match sync_remote_pty_size(remote_runtime, target, binding, &size) {
        Ok(()) => {
            *pending_pty_size = None;
            Ok(())
        }
        Err(error)
            if is_remote_authority_unavailable(&error)
                || !remote_runtime.has_connection(target.address.authority_id()) =>
        {
            ERROR_LOG.log(format!(
                "[diag-timing] pending remote PTY resize kept after authority unregistered: {error}"
            ));
            Ok(())
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
