use crate::application::target_registry_service::{
    DefaultTargetCatalogGateway, TargetRegistryService,
};
use crate::cli::{RemoteMainSlotCommand, RemoteNetworkConfig};
use crate::domain::session_catalog::{ConsoleLocation, ManagedSessionRecord, SessionTransport};
use crate::lifecycle::LifecycleError;
use crate::runtime::remote_authority_connection_runtime::{
    AuthorityConnectionGuard, AuthorityConnectionRequest, AuthorityConnectionStarter,
    AuthorityTransportEvent, QueuedAuthorityStreamSink, QueuedAuthorityStreamStarter,
};
use crate::runtime::remote_authority_transport_runtime::authority_transport_socket_path;
use crate::runtime::remote_main_slot_runtime::RemoteMainSlotRuntime;
use crate::runtime::remote_observer_runtime::RemoteObserverRuntime;
use crate::runtime::remote_transport_runtime::RemoteConnectionRegistry;
use crate::terminal::TerminalRuntime;
use std::io;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;

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
        let target = self.resolve_remote_target(&spec.target, "remote interact surface")?;
        let mut terminal = TerminalRuntime::stdio();
        let initial_size = terminal.current_size_or_default();
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
        let mut observer = RemoteObserverRuntime::new(
            mailbox.clone(),
            usize::from(initial_size.cols),
            usize::from(initial_size.rows),
        );
        let mut raw_output_reader = RemoteRawPtyMailboxReader::new(mailbox.clone());

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
        spawn_mailbox_watcher(mailbox, event_tx.clone());
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
        let mut binding = None;
        let mut direct_raw_output_last_seq = None;
        let mut raw_screen_initialized = false;
        let mut authority_status = waiting_authority_status.clone();
        // Always attempt activation — output_log replay comes from the
        // local mailbox; no need to wait for authority transport.
        let activated = activate_surface_target_with_mode(
            &remote_runtime,
            &target,
            &spec,
            &initial_size,
            &mut observer,
        )
        .map(Some)?;
        if let Some((activated_binding, raw)) = activated {
            raw_input_route.activate(&target, &activated_binding, &spec.console_host_id);
            write_remote_raw_output_with_initial_clear(&raw, &mut raw_screen_initialized)?;
            binding = Some(activated_binding);
        }
        let run_result = (|| -> Result<(), LifecycleError> {
            if should_draw_remote_snapshot(binding.as_ref()) {
                draw_remote_snapshot(
                    &terminal,
                    &target,
                    binding.as_ref(),
                    &observer.snapshot(),
                    &authority_status,
                )?;
            }

            loop {
                match event_rx.recv() {
                    Ok(RemotePaneEvent::MailboxUpdated) => {
                        let raw = raw_output_reader
                            .sync_and_collect_raw()
                            .map_err(remote_protocol_error)?;
                        if raw.is_empty() {
                            continue;
                        }
                        write_remote_raw_output_with_initial_clear(
                            &raw,
                            &mut raw_screen_initialized,
                        )?;
                    }
                    Ok(RemotePaneEvent::Resize) => {
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
                        if should_draw_remote_snapshot(binding.as_ref()) {
                            draw_remote_snapshot(
                                &terminal,
                                &target,
                                binding.as_ref(),
                                &observer.snapshot(),
                                &authority_status,
                            )?;
                        }
                    }
                    Ok(RemotePaneEvent::AuthorityTransport(event)) => match event {
                        AuthorityTransportEvent::Connected => {
                            authority_status = if target_is_present(&target_presence) {
                                AuthorityTransportStatus::Connected
                            } else {
                                AuthorityTransportStatus::Disconnected
                            };
                            let needs_activation = binding.is_none()
                                || remote_runtime.is_mirror_pending(&target)
                                || remote_runtime.is_mirror_needed(&target);
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
                                    Ok(activated) => {
                                        raw_input_route.activate(
                                            &target,
                                            &activated.0,
                                            &spec.console_host_id,
                                        );
                                        write_remote_raw_output_with_initial_clear(
                                            &activated.1,
                                            &mut raw_screen_initialized,
                                        )?;
                                        binding = Some(activated.0);
                                    }
                                    Err(error) => {
                                        authority_status =
                                            AuthorityTransportStatus::Failed(error.to_string());
                                    }
                                }
                            }
                            if should_draw_remote_snapshot(binding.as_ref()) {
                                draw_remote_snapshot(
                                    &terminal,
                                    &target,
                                    binding.as_ref(),
                                    &observer.snapshot(),
                                    &authority_status,
                                )?;
                            }
                        }
                        AuthorityTransportEvent::Disconnected => {
                            remote_runtime
                                .handle_authority_disconnect(target.address.authority_id());
                            authority_status = authority_status_from_runtime(
                                &remote_runtime,
                                &target,
                                target_is_present(&target_presence),
                                &waiting_authority_status,
                            );
                            binding = None;
                            raw_input_route.clear();
                            draw_remote_snapshot(
                                &terminal,
                                &target,
                                binding.as_ref(),
                                &observer.snapshot(),
                                &authority_status,
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
                            apply_authority_envelope(&remote_runtime, &target, &envelope)
                                .map_err(remote_protocol_error)?;
                        }
                    },
                    Ok(RemotePaneEvent::TargetPresenceChanged(is_present)) => {
                        if should_exit_surface_for_target_presence(&spec, is_present) {
                            return Ok(());
                        }
                        authority_status = authority_status_from_runtime(
                            &remote_runtime,
                            &target,
                            is_present,
                            &waiting_authority_status,
                        );
                        if should_draw_remote_snapshot(binding.as_ref()) {
                            draw_remote_snapshot(
                                &terminal,
                                &target,
                                binding.as_ref(),
                                &observer.snapshot(),
                                &authority_status,
                            )?;
                        }
                    }
                    Ok(RemotePaneEvent::Input {
                        bytes,
                        raw_forwarded,
                    }) => {
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
                        }
                    }
                    Err(_) => return Ok(()),
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

#[cfg(test)]
mod remote_main_slot_pane_runtime_test;
