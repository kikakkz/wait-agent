use crate::application::target_registry_service::{
    DefaultTargetCatalogGateway, TargetRegistryService,
};
use crate::cli::{RemoteMainSlotCommand, RemoteNetworkConfig};
use crate::domain::session_catalog::{ConsoleLocation, ManagedSessionRecord, SessionTransport};
use crate::infra::error_log::ERROR_LOG;
use crate::lifecycle::LifecycleError;
use crate::runtime::remote_authority_connection_runtime::{
    AuthorityConnectionGuard, AuthorityConnectionRequest, AuthorityConnectionStarter,
    AuthorityTransportEvent, QueuedAuthorityStreamSink, QueuedAuthorityStreamStarter,
};
use crate::runtime::remote_authority_transport_runtime::authority_transport_socket_path;
use crate::runtime::remote_main_slot_runtime::{RemoteAttachmentBinding, RemoteMainSlotRuntime};
use crate::runtime::remote_observer_runtime::RemoteObserverRuntime;
use crate::runtime::remote_transport_runtime::RemoteConnectionRegistry;
use crate::terminal::TerminalRuntime;
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

pub struct RemoteMainSlotPaneRuntime {
    target_registry: TargetRegistryService<DefaultTargetCatalogGateway>,
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

impl RemoteMainSlotPaneRuntime {
    #[cfg(test)]
    pub fn from_build_env_with_external_authority_streams() -> Result<Self, LifecycleError> {
        Self::from_build_env_with_external_authority_streams_and_network(
            RemoteNetworkConfig::default(),
        )
    }

    pub fn from_build_env_with_external_authority_streams_and_network(
        network: RemoteNetworkConfig,
    ) -> Result<Self, LifecycleError> {
        let current_executable = std::env::current_exe().map_err(|error| {
            LifecycleError::Io(
                "failed to locate current waitagent executable".to_string(),
                error,
            )
        })?;
        let target_registry = TargetRegistryService::new(
            DefaultTargetCatalogGateway::from_build_env().map_err(remote_pane_error)?,
        );
        Ok(Self::new_with_external_authority_streams_and_network(
            target_registry,
            current_executable,
            network,
        ))
    }

    #[cfg(test)]
    pub fn new(
        target_registry: TargetRegistryService<DefaultTargetCatalogGateway>,
        authority_connections: Box<dyn AuthorityConnectionStarter>,
        current_executable: PathBuf,
        _network: RemoteNetworkConfig,
    ) -> Self {
        Self::new_with_optional_external_authority_streams(
            target_registry,
            authority_connections,
            None,
            current_executable,
        )
    }

    fn new_with_optional_external_authority_streams(
        target_registry: TargetRegistryService<DefaultTargetCatalogGateway>,
        authority_connections: Box<dyn AuthorityConnectionStarter>,
        external_authority_streams: Option<QueuedAuthorityStreamSink>,
        _current_executable: PathBuf,
    ) -> Self {
        Self {
            target_registry,
            authority_connections,
            external_authority_streams,
        }
    }

    #[cfg(test)]
    pub fn new_with_external_authority_streams(
        target_registry: TargetRegistryService<DefaultTargetCatalogGateway>,
        current_executable: PathBuf,
    ) -> Self {
        Self::new_with_external_authority_streams_and_network(
            target_registry,
            current_executable,
            RemoteNetworkConfig::default(),
        )
    }

    pub fn new_with_external_authority_streams_and_network(
        target_registry: TargetRegistryService<DefaultTargetCatalogGateway>,
        current_executable: PathBuf,
        _network: RemoteNetworkConfig,
    ) -> Self {
        let (starter, sink) = QueuedAuthorityStreamStarter::channel();
        Self::new_with_optional_external_authority_streams(
            target_registry,
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
        let t_surface = std::time::Instant::now();
        ERROR_LOG.log(format!(
            "[diag-timing] run_surface_with_signal_sink start target={}",
            spec.target
        ));
        let target = self.resolve_remote_target(&spec.target, "remote interact surface")?;
        let mut terminal = TerminalRuntime::stdio();
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

        // Capture terminal size after the SIGWINCH handler is active,
        // so the remote PTY is always sized to match the local display.
        let initial_size = terminal.current_size_or_default();
        let mut observer = RemoteObserverRuntime::new(
            mailbox.clone(),
            usize::from(initial_size.cols),
            usize::from(initial_size.rows),
        );
        let mut raw_output_reader = RemoteRawPtyMailboxReader::new(mailbox);
        let target_presence = Arc::new(Mutex::new(true));
        spawn_target_presence_watcher(
            self.target_registry.clone(),
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
        ERROR_LOG.log(format!(
            "[diag-timing] pane setup complete (threads+authority started), attempting initial activation ({:?})",
            t_surface.elapsed()
        ));
        let mut binding = None;
        let mut direct_raw_output_last_seq = None;
        let mut raw_screen_initialized = false;
        let mut authority_status = waiting_authority_status.clone();
        // Always attempt activation — output_log replay comes from the
        // local mailbox; no need to wait for authority transport.
        let t_before_activate = std::time::Instant::now();
        let activated = activate_surface_target_with_mode(
            &remote_runtime,
            &target,
            &spec,
            &initial_size,
            &mut observer,
        )
        .map(Some)?;
        if let Some((activated_binding, raw)) = activated {
            ERROR_LOG.log(format!(
                "[diag-timing] initial activation got binding ({:?})",
                t_before_activate.elapsed()
            ));
            raw_input_route.activate(&target, &activated_binding, &spec.console_host_id);
            write_remote_raw_output_with_initial_clear(&raw, &mut raw_screen_initialized)?;
            binding = Some(activated_binding);
            flush_paused_input(
                &remote_runtime,
                &target,
                binding.as_ref().unwrap(),
                &paused_input_buffer,
                &mut console_seq,
            )?;
        } else {
            ERROR_LOG.log(format!(
                "[diag-timing] initial activation returned None ({:?})",
                t_before_activate.elapsed()
            ));
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
                ERROR_LOG.log(format!(
                    "[diag-timing] event loop entering initial connecting phase (surface_setup={:?})",
                    t_surface.elapsed()
                ));
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
                    match event_rx.recv_timeout(slot_pane_helpers::RECONNECT_ANIMATION_INTERVAL) {
                        Ok(event) => event,
                        Err(RecvTimeoutError::Timeout) => {
                            let elapsed = reconnecting_since.unwrap().elapsed();
                            // Clean exit: the target is gone from the catalog
                            // AND no connection remains — the session exited,
                            // this is not a transient network blip.
                            let target_gone = !target_is_present(&target_presence)
                                && !remote_runtime.has_connection(target.address.authority_id());
                            if elapsed > slot_pane_helpers::RECONNECT_TIMEOUT || target_gone {
                                if target_gone {
                                    ERROR_LOG.log(
                                        "[diag-timing] target gone during reconnect, shutting down"
                                            .to_string(),
                                    );
                                }
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
                                None,
                                Some(elapsed),
                                reconnect_animation_frame,
                            )?;
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
                        let t_mailbox = std::time::Instant::now();
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
                        let is_first_output = !raw_screen_initialized;
                        if is_first_output {
                            ERROR_LOG.log(format!(
                                "[diag-timing] FIRST OUTPUT received ({} bytes, surface_total={:?})",
                                raw.len(),
                                t_surface.elapsed()
                            ));
                        }
                        write_remote_raw_output_with_initial_clear(
                            &raw,
                            &mut raw_screen_initialized,
                        )?;
                        if is_first_output {
                            ERROR_LOG.log(format!(
                                "[diag-timing] FIRST OUTPUT written to screen (mailbox_handler={:?})",
                                t_mailbox.elapsed()
                            ));
                        }
                    }
                    RemotePaneEvent::Resize => {
                        if let Ok(Some(size)) = terminal.capture_resize() {
                            if let Some(binding) = binding.as_ref() {
                                remote_runtime.send_pty_resize(
                                    &target,
                                    binding,
                                    usize::from(size.cols),
                                    usize::from(size.rows),
                                )?;
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
                        AuthorityTransportEvent::Connected => {
                            let t_conn = std::time::Instant::now();
                            ERROR_LOG.log(format!(
                                "[diag-timing] AuthorityTransportEvent::Connected (elapsed_since_surface_start={:?})",
                                t_surface.elapsed()
                            ));
                            let is_present = target_is_present(&target_presence);
                            // Only clear reconnect when target is also present.
                            // Otherwise, keep reconnecting_since to prevent an
                            // early exit from a stale TargetPresenceChanged(false)
                            // event arriving after the authority reconnects but
                            // before the target reappears in the catalog.
                            if is_present {
                                reconnecting_since = None;
                            }
                            authority_status = if is_present {
                                AuthorityTransportStatus::Connected
                            } else {
                                AuthorityTransportStatus::Disconnected
                            };
                            let needs_activation = binding.is_none()
                                || remote_runtime.is_mirror_pending(&target)
                                || remote_runtime.is_mirror_needed(&target);
                            let mut activated = false;
                            if needs_activation
                                && matches!(authority_status, AuthorityTransportStatus::Connected)
                            {
                                match activate_surface_target_with_mode(
                                    &remote_runtime,
                                    &target,
                                    &spec,
                                    &terminal.current_size_or_default(),
                                    &mut observer,
                                ) {
                                    Ok(result) => {
                                        raw_input_route.activate(
                                            &target,
                                            &result.0,
                                            &spec.console_host_id,
                                        );
                                        write_remote_raw_output_with_initial_clear(
                                            &result.1,
                                            &mut raw_screen_initialized,
                                        )?;
                                        binding = Some(result.0);
                                        activated = true;
                                        ERROR_LOG.log(format!(
                                            "[diag-timing] Connected: reactivation succeeded (handler_elapsed={:?}, surface_total={:?})",
                                            t_conn.elapsed(),
                                            t_surface.elapsed()
                                        ));
                                        flush_paused_input(
                                            &remote_runtime,
                                            &target,
                                            binding.as_ref().unwrap(),
                                            &paused_input_buffer,
                                            &mut console_seq,
                                        )?;
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
                            if !activated {
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
                        AuthorityTransportEvent::Disconnected => {
                            ERROR_LOG.log(format!(
                                "[diag-timing] AuthorityTransportEvent::Disconnected (elapsed_since_surface_start={:?})",
                                t_surface.elapsed()
                            ));
                            remote_runtime
                                .handle_authority_disconnect(target.address.authority_id());
                            let _is_present = target_is_present(&target_presence);
                            authority_status = AuthorityTransportStatus::Disconnected;
                            // Reset screen_initialized so that the recovery
                            // path sends a full clear-screen before writing
                            // new raw PTY output, rather than writing on top
                            // of whatever draw_remote_snapshot left on screen.
                            raw_screen_initialized = false;
                            // Keep binding and last content visible; start reconnecting
                            reconnecting_since = Some(Instant::now());
                            reconnect_animation_frame = 0;
                            let _ = observer.sync();
                            draw_remote_snapshot(
                                &terminal,
                                &target,
                                binding.as_ref(),
                                &observer.snapshot(),
                                &authority_status,
                                None,
                                Some(Duration::ZERO),
                                0,
                            )?;
                        }
                        AuthorityTransportEvent::Failed(message) => {
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
                            payload,
                        } => {
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
                        }
                        AuthorityTransportEvent::Envelope(envelope) => {
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
                                write_remote_raw_output_with_initial_clear(
                                    &output,
                                    &mut raw_screen_initialized,
                                )?;
                            }
                        }
                    },
                    RemotePaneEvent::TargetPresenceChanged(is_present) => {
                        if !is_present && reconnecting_since.is_none() {
                            // The target presence watcher polls at 250ms with a
                            // 4-miss grace (1 second latency). If the authority
                            // transport reconnected during this window, a stale
                            // false event can arrive after `reconnecting_since`
                            // was already cleared. In that case the session must
                            // NOT exit — the watcher will correct on the next poll.
                            if !remote_runtime.has_connection(target.address.authority_id()) {
                                return Ok(());
                            }
                            // Connection is active; the false event is stale.
                            // Skip it — the next poll will send the correct state.
                        }
                        if !is_present && reconnecting_since.is_some() {
                            // The target has disappeared while we are trying to
                            // reconnect.  If no connection exists either, this is
                            // a clean session exit, not a transient network blip.
                            if !remote_runtime.has_connection(target.address.authority_id()) {
                                ERROR_LOG.log(
                                    "[diag-timing] target exited during reconnect, shutting down"
                                        .to_string(),
                                );
                                return Ok(());
                            }
                        }
                        if !is_present {
                            // During reconnect: target disappearance is a
                            // catalog side-effect of network jitter. Clear
                            // local state but keep reconnecting.
                            binding = None;
                            raw_input_route.clear();
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
                            is_present,
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
                        if is_present
                            && binding.is_none()
                            && matches!(authority_status, AuthorityTransportStatus::Connected)
                        {
                            let size = terminal.current_size_or_default();
                            match activate_surface_target_with_mode(
                                &remote_runtime,
                                &target,
                                &spec,
                                &size,
                                &mut observer,
                            ) {
                                Ok(result) => {
                                    reconnecting_since = None;
                                    raw_input_route.activate(
                                        &target,
                                        &result.0,
                                        &spec.console_host_id,
                                    );
                                    write_remote_raw_output_with_initial_clear(
                                        &result.1,
                                        &mut raw_screen_initialized,
                                    )?;
                                    binding = Some(result.0);
                                    activated = true;
                                    flush_paused_input(
                                        &remote_runtime,
                                        &target,
                                        binding.as_ref().unwrap(),
                                        &paused_input_buffer,
                                        &mut console_seq,
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
                            remote_runtime.send_raw_pty_input(
                                &target,
                                binding,
                                console_seq,
                                bytes,
                            )?;
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

/// Flush any input that was buffered during a reconnection period.
/// Each buffered chunk is sent as a separate PTY input message so
/// that keystrokes queued while the binding was down are not lost.
fn flush_paused_input(
    remote_runtime: &RemoteMainSlotRuntime,
    target: &ManagedSessionRecord,
    binding: &RemoteAttachmentBinding,
    buffer: &[Vec<u8>],
    console_seq: &mut u64,
) -> Result<(), LifecycleError> {
    for chunk in buffer {
        *console_seq += 1;
        remote_runtime.send_raw_pty_input(target, binding, *console_seq, chunk.clone())?;
    }
    Ok(())
}

#[cfg(test)]
mod remote_main_slot_pane_runtime_test;
