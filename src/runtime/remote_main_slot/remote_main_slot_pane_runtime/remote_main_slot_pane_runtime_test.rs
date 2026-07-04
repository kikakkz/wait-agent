mod tests {
    use super::super::{
        activate_surface_target, activate_surface_target_with_mode, apply_authority_envelope,
        authority_status_from_runtime, authority_transport_event_sender,
        collect_direct_raw_pty_output_envelope, collect_direct_raw_pty_output_payload,
        flush_paused_input, flush_pending_pty_size, main_slot_console_id, main_slot_surface_spec,
        placeholder_lines, should_draw_remote_snapshot, should_exit_surface_for_target_presence,
        should_exit_surface_for_target_presence_loss, should_exit_surface_locally,
        should_sync_remote_pty_resize_for_state, spawn_mailbox_watcher,
        sync_or_defer_remote_pty_size, write_remote_raw_output_with_initial_clear,
        AuthorityTransportStatus, RawPtyInputRoute, RemoteInteractInputSignalDecoder,
        RemoteInteractSignal, RemoteInteractSurfaceSpec, RemoteMainSlotPaneRuntime,
        RemotePaneEvent, RemoteRawPtyMailboxReader, CLEAR_SCREEN_HOME_ESCAPE,
    };
    use crate::application::target_registry_service::{
        DefaultTargetCatalogGateway, TargetRegistryService,
    };
    use crate::cli::{RemoteMainSlotCommand, RemoteNetworkConfig};
    use crate::domain::session_catalog::{
        ConsoleLocation, ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState,
        SessionAvailability,
    };
    use crate::infra::remote_protocol::{
        ControlPlanePayload, MirrorBootstrapChunkPayload, MirrorBootstrapCompletePayload,
        ProtocolEnvelope, RawPtyInputPayload, RawPtyOutputPayload, RemoteConsoleDescriptor,
        TargetOutputPayload,
    };
    use crate::infra::remote_transport_codec::write_registration_frame;
    use crate::infra::tmux::EmbeddedTmuxBackend;
    use crate::runtime::remote_authority_connection_runtime::{
        spawn_authority_listener, AuthorityConnectionRequest, AuthorityTransportEvent,
    };
    use crate::runtime::remote_authority_transport_runtime::{
        authority_transport_socket_path, RemoteAuthorityCommand,
    };
    use crate::runtime::remote_main_slot_runtime::RemoteAttachmentBinding;
    use crate::runtime::remote_main_slot_runtime::RemoteControlPlaneTransportError;
    use crate::runtime::remote_main_slot_runtime::RemoteMainSlotRuntime;
    use crate::runtime::remote_observer_runtime::RemoteObserverRuntime;
    use crate::runtime::remote_observer_runtime::RemoteObserverSnapshot;
    use crate::runtime::remote_transport_runtime::{
        RemoteConnectionRegistry, RemoteControlPlaneConnection,
    };
    use crate::terminal::{ScreenSnapshot, ScreenState, TerminalEngine, TerminalSize};
    use std::fs;
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;
    use std::process;
    use std::sync::{mpsc, Arc, Mutex};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[test]
    fn main_slot_console_id_matches_workspace_main_slot_shape() {
        let command = RemoteMainSlotCommand {
            socket_name: "wa-1".to_string(),
            session_name: "workspace-1".to_string(),
            target: "peer-a:shell-1".to_string(),
        };

        assert_eq!(
            main_slot_console_id(&command),
            "workspace-main-slot:wa-1:workspace-1"
        );
    }

    #[test]
    fn main_slot_surface_spec_marks_local_workspace_console() {
        let command = RemoteMainSlotCommand {
            socket_name: "wa-1".to_string(),
            session_name: "workspace-1".to_string(),
            target: "peer-a:shell-1".to_string(),
        };

        let spec = main_slot_surface_spec(&command);

        assert_eq!(spec.console_id, "workspace-main-slot:wa-1:workspace-1");
        assert_eq!(spec.console_host_id, "wa-1");
        assert_eq!(spec.surface_scope, "workspace-1");
        assert_eq!(spec.console_location, ConsoleLocation::LocalWorkspace);
    }

    #[test]
    fn only_server_console_surface_exits_on_ctrl_right_bracket() {
        let main_slot = RemoteInteractSurfaceSpec {
            socket_name: "wa-1".to_string(),
            surface_scope: "workspace-1".to_string(),
            target: "peer-a:shell-1".to_string(),
            console_id: "workspace-main-slot:wa-1:workspace-1".to_string(),
            console_host_id: "wa-1".to_string(),
            console_location: ConsoleLocation::LocalWorkspace,
        };
        let server_console = RemoteInteractSurfaceSpec {
            console_location: ConsoleLocation::ServerConsole,
            ..main_slot.clone()
        };

        assert!(!should_exit_surface_locally(&main_slot, &[0x1d]));
        assert!(should_exit_surface_locally(&server_console, &[0x1d]));
        assert!(!should_exit_surface_locally(&server_console, b"hello"));
    }

    #[test]
    fn workspace_remote_resize_syncs_only_for_visible_active_content_pane() {
        let spec = RemoteInteractSurfaceSpec {
            socket_name: "wa-1".to_string(),
            surface_scope: "workspace-1".to_string(),
            target: "peer-a:shell-1".to_string(),
            console_id: "workspace-main-slot:wa-1:workspace-1".to_string(),
            console_host_id: "wa-1".to_string(),
            console_location: ConsoleLocation::LocalWorkspace,
        };

        assert!(should_sync_remote_pty_resize_for_state(
            &spec,
            "%4",
            Some("%4"),
            Some("peer-a:shell-1"),
            true
        ));
        assert!(!should_sync_remote_pty_resize_for_state(
            &spec,
            "%4",
            Some("%4"),
            Some("peer-a:shell-1"),
            false
        ));
        assert!(!should_sync_remote_pty_resize_for_state(
            &spec,
            "%4",
            Some("%3"),
            Some("peer-a:shell-1"),
            true
        ));
        assert!(!should_sync_remote_pty_resize_for_state(
            &spec,
            "%4",
            Some("%4"),
            Some("wa-1:local"),
            true
        ));
    }

    #[test]
    fn server_console_resize_sync_is_independent_from_workspace_visibility() {
        let spec = server_console_surface_spec();

        assert!(should_sync_remote_pty_resize_for_state(
            &spec,
            "%4",
            Some("%3"),
            Some("other"),
            false
        ));
    }

    #[test]
    fn raw_remote_draws_placeholder_until_content_appears() {
        let binding = RemoteAttachmentBinding {
            session_id: "shell-1".to_string(),
            target_id: "remote-peer:peer-a:shell-1".to_string(),
            attachment_id: "attach-1".to_string(),
            console_id: "console-a".to_string(),
        };

        let snapshot = empty_snapshot();

        let waiting = AuthorityTransportStatus::WaitingForRemoteAuthority;
        let connected = AuthorityTransportStatus::Connected;

        // No binding → always draw placeholder
        assert!(should_draw_remote_snapshot(None, &snapshot, &waiting));

        // Binding but no visible output + waiting → draw placeholder (no black screen)
        assert!(should_draw_remote_snapshot(
            Some(&binding),
            &snapshot,
            &waiting
        ));

        // Binding, no visible output, but connected → don't draw (raw PTY handles it)
        assert!(!should_draw_remote_snapshot(
            Some(&binding),
            &snapshot,
            &connected
        ));
    }

    fn empty_snapshot() -> RemoteObserverSnapshot {
        let empty_screen = ScreenSnapshot {
            size: TerminalSize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
            lines: Vec::new(),
            styled_lines: Vec::new(),
            active_style_ansi: String::new(),
            scrollback: Vec::new(),
            styled_scrollback: Vec::new(),
            scroll_top: 0,
            scroll_bottom: 24,
            window_title: None,
            cursor_row: 0,
            cursor_col: 0,
            cursor_visible: false,
            alternate_screen: false,
        };
        RemoteObserverSnapshot {
            session_id: None,
            target_id: None,
            attachment_id: None,
            console_id: None,
            availability: None,
            resize_epoch: None,
            resize_authority_console_id: None,
            resize_authority_host_id: None,
            last_output_seq: None,
            has_visible_output: false,
            bootstrap_complete: false,
            screen: ScreenState {
                normal: empty_screen.clone(),
                alternate: empty_screen,
                alternate_screen_active: false,
                application_cursor_keys: false,
            },
        }
    }

    #[test]
    fn server_console_input_decoder_emits_input_started_and_submit() {
        let spec = server_console_surface_spec();
        let mut decoder = RemoteInteractInputSignalDecoder::default();

        assert_eq!(
            decoder.feed(&spec, b"abc\r"),
            vec![
                RemoteInteractSignal::ConsoleInputStarted,
                RemoteInteractSignal::ConsoleSubmit,
            ]
        );
    }

    #[test]
    fn server_console_input_decoder_keeps_partial_submit_sequence_until_complete() {
        let spec = server_console_surface_spec();
        let mut decoder = RemoteInteractInputSignalDecoder::default();

        assert!(decoder.feed(&spec, b"\x1b[13").is_empty());
        assert_eq!(
            decoder.feed(&spec, b"u"),
            vec![
                RemoteInteractSignal::ConsoleInputStarted,
                RemoteInteractSignal::ConsoleSubmit,
            ]
        );
    }

    #[test]
    fn server_console_input_decoder_emits_manual_return_for_ctrl_right_bracket() {
        let spec = server_console_surface_spec();
        let mut decoder = RemoteInteractInputSignalDecoder::default();

        assert_eq!(
            decoder.feed(&spec, &[0x1d]),
            vec![RemoteInteractSignal::ManualReturnToPicker]
        );
    }

    #[test]
    fn new_with_external_authority_streams_keeps_external_sink_under_runtime_ownership() {
        let target_registry = TargetRegistryService::new(
            DefaultTargetCatalogGateway::from_build_env()
                .expect("build env target catalog should exist"),
        );
        let runtime = RemoteMainSlotPaneRuntime::new_with_external_authority_streams(
            target_registry,
            EmbeddedTmuxBackend::from_build_env().expect("tmux backend should build"),
            PathBuf::from("/tmp/waitagent"),
        );

        let (_client, server) = UnixStream::pair().expect("stream pair should open");
        runtime
            .submit_external_authority_stream(server)
            .expect("runtime should accept submitted authority stream");
    }

    #[test]
    fn submitted_external_authority_stream_reaches_authority_connection_runtime() {
        let target_registry = TargetRegistryService::new(
            DefaultTargetCatalogGateway::from_build_env()
                .expect("build env target catalog should exist"),
        );
        let runtime = RemoteMainSlotPaneRuntime::new_with_external_authority_streams(
            target_registry,
            EmbeddedTmuxBackend::from_build_env().expect("tmux backend should build"),
            PathBuf::from("/tmp/waitagent"),
        );
        let registry = RemoteConnectionRegistry::new();
        let (tx, rx) = mpsc::channel();
        let _guard = runtime
            .authority_connections
            .start_connection(
                AuthorityConnectionRequest {
                    socket_path: test_socket_path("pane-external-authority"),
                    authority_id: "peer-a".to_string(),
                },
                registry.clone(),
                tx,
            )
            .expect("authority connection runtime should start");

        let (mut client, server) = UnixStream::pair().expect("stream pair should open");
        runtime
            .submit_external_authority_stream(server)
            .expect("runtime should accept external authority stream");
        write_registration_frame(&mut client, "peer-a").expect("registration frame should encode");

        match rx
            .recv_timeout(Duration::from_secs(1))
            .expect("connected event should arrive")
        {
            AuthorityTransportEvent::Connected { authority_id, .. } => {
                assert_eq!(authority_id, "peer-a");
            }
            other => panic!("unexpected authority event: {other:?}"),
        }
        assert!(registry.has_connection("peer-a"));
    }

    #[test]
    fn runtime_without_external_authority_streams_rejects_submissions() {
        let target_registry = TargetRegistryService::new(
            DefaultTargetCatalogGateway::from_build_env()
                .expect("build env target catalog should exist"),
        );
        let runtime = RemoteMainSlotPaneRuntime::new(
            target_registry,
            EmbeddedTmuxBackend::from_build_env().expect("tmux backend should build"),
            Box::new(crate::runtime::remote_authority_connection_runtime::LocalAuthoritySocketBridgeStarter),
            PathBuf::from("/tmp/waitagent"),
            RemoteNetworkConfig::default(),
        );

        let (_client, server) = UnixStream::pair().expect("stream pair should open");
        let error = runtime
            .submit_external_authority_stream(server)
            .expect_err("default runtime should reject external authority stream submissions");

        assert_eq!(
            error.to_string(),
            "remote main-slot pane runtime is not configured for external authority streams"
        );
    }

    #[test]
    fn placeholder_lines_explain_transport_gap_before_output_arrives() {
        let lines = placeholder_lines(
            &remote_target(),
            Some(&RemoteAttachmentBinding {
                session_id: "shell-1".to_string(),
                target_id: "remote-peer:peer-a:shell-1".to_string(),
                attachment_id: "attach-1".to_string(),
                console_id: "console-a".to_string(),
            }),
            &AuthorityTransportStatus::WaitingForRemoteAuthority,
            TerminalSize {
                rows: 5,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
        );

        assert_eq!(lines.len(), 5);
        assert!(lines[0].contains("remote target bash"));
        assert!(lines[3].contains("waiting for remote authority"));
        assert!(lines[4].contains("live authority node"));
    }

    #[test]
    fn placeholder_lines_surface_authority_transport_failures() {
        let lines = placeholder_lines(
            &remote_target(),
            Some(&RemoteAttachmentBinding {
                session_id: "shell-1".to_string(),
                target_id: "remote-peer:peer-a:shell-1".to_string(),
                attachment_id: "attach-1".to_string(),
                console_id: "console-a".to_string(),
            }),
            &AuthorityTransportStatus::Failed("unexpected authority node".to_string()),
            TerminalSize {
                rows: 5,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
        );

        assert!(lines[3].contains("failed"));
        assert!(lines[4].contains("unexpected authority node"));
    }

    #[test]
    fn placeholder_lines_surface_authority_disconnect() {
        let lines = placeholder_lines(
            &remote_target(),
            Some(&RemoteAttachmentBinding {
                session_id: "shell-1".to_string(),
                target_id: "remote-peer:peer-a:shell-1".to_string(),
                attachment_id: "attach-1".to_string(),
                console_id: "console-a".to_string(),
            }),
            &AuthorityTransportStatus::Disconnected,
            TerminalSize {
                rows: 5,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
        );

        assert!(lines[3].contains("disconnected"));
        assert!(lines[4].contains("waiting for the remote authority"));
    }

    #[test]
    fn render_terminal_safe_remote_line_trims_padding_and_preserves_style() {
        let rendered = super::super::render_terminal_safe_remote_line(
            "\x1b[0;38;5;196mred\x1b[0m     ",
            "red     ",
        );

        assert_eq!(rendered, "\x1b[0;38;5;196mred\x1b[0m");
    }

    #[test]
    fn render_terminal_safe_remote_line_preserves_wide_character_width() {
        let rendered = super::super::render_terminal_safe_remote_line(
            "✨\u{200a}Update available!      ",
            "✨\u{200a}Update available!      ",
        );

        assert_eq!(rendered, "✨\u{200a}Update available!");
    }

    #[test]
    fn next_ansi_escape_len_handles_full_csi_sequences() {
        assert_eq!(
            super::super::next_ansi_escape_len("\x1b[0;38;5;196mred"),
            "\x1b[0;38;5;196m".len()
        );
    }

    #[test]
    fn observe_multi_redraw_replay_through_terminal_engine() {
        let viewport = TerminalSize {
            rows: 21,
            cols: 47,
            pixel_width: 0,
            pixel_height: 0,
        };
        let mut engine = TerminalEngine::new(viewport);

        let placeholder = placeholder_lines(
            &remote_target(),
            None,
            &AuthorityTransportStatus::WaitingForRemoteAuthority,
            viewport,
        );
        let placeholder_refs = placeholder.iter().map(String::as_str).collect::<Vec<_>>();
        let placeholder_render = render_full_frame(&placeholder_refs, false, None);
        engine.feed(placeholder_render.as_bytes());

        let bootstrap_lines = vec![
            "",
            "  ✨\u{200a}Update available! \x1b[2m0.125.0 -> 0.128.0\x1b[0m",
            "",
            "  \x1b[2mRelease notes: \x1b[4mhttps://github.com/openai/code\x1b[0m",
            "",
            "\x1b[0m› 1. Update now (runs `npm install -g",
            "     @openai/codex`)",
            "  2. Skip",
            "  3. Skip until next version",
            "",
            "  \x1b[2mPress enter to continue\x1b[0m",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
        ];
        let bootstrap_render = render_full_frame(&bootstrap_lines, false, Some((10, 25)));
        engine.feed(bootstrap_render.as_bytes());

        let down_lines = vec![
            "",
            "  ✨\u{200a}Update available! \x1b[2m0.125.0 -> 0.128.0\x1b[0m",
            "",
            "  \x1b[2mRelease notes: \x1b[4mhttps://github.com/openai/code\x1b[0m",
            "",
            "  1. Update now (runs `npm install -g",
            "     @openai/codex`)",
            "› 2. Skip",
            "  3. Skip until next version",
            "",
            "  \x1b[2mPress enter to continue\x1b[0m",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
            "",
        ];
        let down_render = render_full_frame(&down_lines, false, Some((7, 9)));
        engine.feed(down_render.as_bytes());

        let snapshot = engine.snapshot();
        eprintln!("multi-redraw line1={:?}", snapshot.lines[0]);
        eprintln!("multi-redraw line2={:?}", snapshot.lines[1]);
        eprintln!("multi-redraw line6={:?}", snapshot.lines[5]);
        eprintln!("multi-redraw line7={:?}", snapshot.lines[6]);
        eprintln!("multi-redraw line8={:?}", snapshot.lines[7]);
        eprintln!("multi-redraw line9={:?}", snapshot.lines[8]);

        assert!(
            snapshot.lines[1].starts_with("  ✨ Update available! 0.125.0 -> 0.128.0"),
            "unexpected line2: {:?}",
            snapshot.lines[1]
        );
        assert_eq!(
            snapshot.lines[5],
            "  1. Update now (runs `npm install -g          "
        );
        assert_eq!(
            snapshot.lines[6],
            "     @openai/codex`)                           "
        );
        assert_eq!(
            snapshot.lines[7],
            "› 2. Skip                                      "
        );
    }

    #[test]
    fn observe_render_helper_on_real_codex_snapshot_lines() {
        let runtime = RemoteMainSlotRuntime::with_registry(RemoteConnectionRegistry::new());
        let mailbox = runtime
            .ensure_local_observer_connection("observer-a")
            .expect("observer loopback registration should succeed");
        runtime.ensure_local_connection("peer-a");

        runtime
            .activate_target(
                &remote_target(),
                RemoteConsoleDescriptor {
                    console_id: "console-a".to_string(),
                    console_host_id: "observer-a".to_string(),
                    location: ConsoleLocation::LocalWorkspace,
                },
                47,
                21,
            )
            .expect("remote activation should succeed");

        let bootstrap_screen = concat!(
            "\n",
            "  ✨\u{200a}Update available! \x1b[2m0.125.0 -> 0.128.0\x1b[0m      \n",
            "\n",
            "  \x1b[2mRelease notes: \x1b[4mhttps://github.com/openai/code\n",
            "\n",
            "\x1b[0m› 1. Update now (runs `npm install -g          \n",
            "     @openai/codex`)   \n",
            "  2. Skip  \n",
            "  3. Skip until next version                  \n",
            "\n",
            "  \x1b[2mPress enter to continue\x1b[0m                    \n",
            "\n\n\n\n\n\n\n\n\n\n",
        );
        let mut bootstrap = String::from("\x1b[2J\x1b[H");
        for (index, line) in bootstrap_screen.lines().enumerate() {
            bootstrap.push_str(&format!("\x1b[{};1H{}", index + 1, line));
        }
        bootstrap.push_str("\x1b[11;26H");
        let redraw = b"\x1b[?2026h\x1b[1;2H\x1b[0m\x1b[m\x1b[K\x1b[2;42H\x1b[0m\x1b[m\x1b[K\x1b[3;2H\x1b[0m\x1b[m\x1b[K\x1b[5;2H\x1b[0m\x1b[m\x1b[K\x1b[6;38H\x1b[0m\x1b[m\x1b[K\x1b[7;21H\x1b[0m\x1b[m\x1b[K\x1b[8;10H\x1b[0m\x1b[m\x1b[K\x1b[9;29H\x1b[0m\x1b[m\x1b[K\x1b[10;2H\x1b[0m\x1b[m\x1b[K\x1b[11;26H\x1b[0m\x1b[m\x1b[K\x1b[12;2H\x1b[0m\x1b[m\x1b[K\x1b[13;2H\x1b[0m\x1b[m\x1b[K\x1b[14;2H\x1b[0m\x1b[m\x1b[K\x1b[15;2H\x1b[0m\x1b[m\x1b[K\x1b[16;2H\x1b[0m\x1b[m\x1b[K\x1b[17;2H\x1b[0m\x1b[m\x1b[K\x1b[18;2H\x1b[0m\x1b[m\x1b[K\x1b[19;2H\x1b[0m\x1b[m\x1b[K\x1b[20;2H\x1b[0m\x1b[m\x1b[K\x1b[21;2H\x1b[0m\x1b[m\x1b[K\x1b[6;1H  1. Update now (runs `npm install -g\x1b[7;6H@openai/codex`)\x1b[8;1H\x1b[;m\xe2\x80\xba 2. Skip\x1b[m\x1b[m\x1b[0m\x1b[?25l\x1b[?2026l";

        runtime
            .send_mirror_bootstrap_chunk(&remote_target(), 1, "pty", bootstrap.into_bytes())
            .expect("bootstrap replay should fan out");
        runtime
            .send_mirror_bootstrap_complete(&remote_target(), 1, false, false, false)
            .expect("bootstrap complete should fan out");
        runtime
            .send_target_output(&remote_target(), 1, "pty", redraw.to_vec())
            .expect("redraw should fan out");

        let mut observer = RemoteObserverRuntime::new(mailbox, 47, 21);
        observer.sync().expect("observer sync should succeed");
        let snapshot = observer.snapshot();
        let active = snapshot.active_screen();

        let rendered2 = super::super::render_terminal_safe_remote_line(
            &active.styled_lines[1],
            &active.lines[1],
        );
        let rendered6 = super::super::render_terminal_safe_remote_line(
            &active.styled_lines[5],
            &active.lines[5],
        );
        let rendered7 = super::super::render_terminal_safe_remote_line(
            &active.styled_lines[6],
            &active.lines[6],
        );
        let rendered8 = super::super::render_terminal_safe_remote_line(
            &active.styled_lines[7],
            &active.lines[7],
        );

        eprintln!("styled2={:?}", active.styled_lines[1]);
        eprintln!("rendered2={:?}", rendered2);
        eprintln!("styled6={:?}", active.styled_lines[5]);
        eprintln!("rendered6={:?}", rendered6);
        eprintln!("styled7={:?}", active.styled_lines[6]);
        eprintln!("rendered7={:?}", rendered7);
        eprintln!("styled8={:?}", active.styled_lines[7]);
        eprintln!("rendered8={:?}", rendered8);

        assert!(rendered2.starts_with("  ✨ Update available!"));
        assert_eq!(rendered6, "  1. Update now (runs `npm install -g");
        assert_eq!(rendered7, "     @openai/codex`)");
        assert_eq!(rendered8, "› 2. Skip");
    }

    #[test]
    fn cursor_visibility_tracks_through_bootstrap_and_target_output() {
        let runtime = RemoteMainSlotRuntime::with_registry(RemoteConnectionRegistry::new());
        let mailbox = runtime
            .ensure_local_observer_connection("observer-a")
            .expect("observer loopback registration should succeed");
        runtime.ensure_local_connection("peer-a");
        let mut observer = RemoteObserverRuntime::new(mailbox, 80, 24);

        // Step 1: Activate to trigger begin_bootstrap
        runtime
            .activate_target(
                &remote_target(),
                RemoteConsoleDescriptor {
                    console_id: "console-a".to_string(),
                    console_host_id: "observer-a".to_string(),
                    location: ConsoleLocation::LocalWorkspace,
                },
                80,
                24,
            )
            .expect("remote activation should succeed");

        // Step 2: Send a simple bootstrap chunk (just clear screen + home)
        let bootstrap = String::from("\x1b[2J\x1b[H");
        runtime
            .send_mirror_bootstrap_chunk(&remote_target(), 1, "pty", bootstrap.into_bytes())
            .expect("bootstrap chunk should fan out");

        // Step 3: Send bootstrap complete with cursor_visible=false
        runtime
            .send_mirror_bootstrap_complete(&remote_target(), 1, false, false, false)
            .expect("bootstrap complete should fan out");

        // Step 4: Sync and check cursor state after bootstrap
        observer.sync().expect("observer sync should succeed");
        let snapshot = observer.snapshot();
        assert!(
            snapshot.has_visible_output,
            "has_visible_output should be true after bootstrap"
        );
        assert!(
            !snapshot.active_screen().cursor_visible,
            "cursor should be hidden after bootstrap with cursor_visible=false"
        );

        // Step 5: Send TargetOutput with \x1b[?25h (cursor show)
        runtime
            .send_target_output(&remote_target(), 1, "pty", b"\x1b[?25h".to_vec())
            .expect("target output should fan out");

        observer.sync().expect("observer sync should succeed");
        let snapshot = observer.snapshot();
        assert!(
            snapshot.has_visible_output,
            "has_visible_output should still be true"
        );
        assert!(
            snapshot.active_screen().cursor_visible,
            "cursor should be visible after \x1b[?25h"
        );

        // Step 6: Send TargetOutput with \x1b[?25l (cursor hide)
        runtime
            .send_target_output(&remote_target(), 2, "pty", b"\x1b[?25l".to_vec())
            .expect("target output should fan out");

        observer.sync().expect("observer sync should succeed");
        let snapshot = observer.snapshot();
        assert!(
            snapshot.has_visible_output,
            "has_visible_output should still be true"
        );
        assert!(
            !snapshot.active_screen().cursor_visible,
            "cursor should be hidden after \x1b[?25l"
        );

        // Step 7: Send TargetOutput with \x1b[?25h AGAIN (cursor show)
        runtime
            .send_target_output(&remote_target(), 3, "pty", b"\x1b[?25h".to_vec())
            .expect("target output should fan out");

        observer.sync().expect("observer sync should succeed");
        let snapshot = observer.snapshot();
        assert!(
            snapshot.active_screen().cursor_visible,
            "cursor should be visible again after second \x1b[?25h"
        );

        // Step 8: Check that cursor_row/col are default (0,0) since we only sent cursor sequences, not positioning
        assert_eq!(
            snapshot.active_screen().cursor_row,
            0,
            "cursor_row should default to 0"
        );
        assert_eq!(
            snapshot.active_screen().cursor_col,
            0,
            "cursor_col should default to 0"
        );
    }

    #[test]
    fn placeholder_lines_show_pending_attachment_before_remote_activation_begins() {
        let lines = placeholder_lines(
            &remote_target(),
            None,
            &AuthorityTransportStatus::WaitingForRemoteAuthority,
            TerminalSize {
                rows: 5,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            },
        );

        assert_eq!(lines[2], "attachment: pending");
    }

    #[test]
    fn activate_surface_target_succeeds_without_authority_connection() {
        let runtime = RemoteMainSlotRuntime::with_registry(RemoteConnectionRegistry::new());
        let mailbox = runtime
            .ensure_local_observer_connection("observer-a")
            .expect("observer loopback registration should succeed");
        let mut observer = RemoteObserverRuntime::new(mailbox, 80, 24);
        let target = remote_target();
        let spec = RemoteInteractSurfaceSpec {
            socket_name: "wa-1".to_string(),
            surface_scope: "workspace-1".to_string(),
            target: target.address.qualified_target(),
            console_id: "workspace-main-slot:wa-1:workspace-1".to_string(),
            console_host_id: "observer-a".to_string(),
            console_location: ConsoleLocation::LocalWorkspace,
        };

        // Activation should succeed even without registered authority
        // connection — output_log replay goes through the local
        // observer mailbox.
        activate_surface_target(
            &runtime,
            &target,
            &spec,
            &TerminalSize {
                cols: 80,
                rows: 24,
                pixel_width: 0,
                pixel_height: 0,
            },
            &mut observer,
        )
        .expect("activation should succeed via local replay even without authority connection");
    }

    #[test]
    fn activate_surface_target_succeeds_after_authority_connection_registration() {
        let runtime = RemoteMainSlotRuntime::with_registry(RemoteConnectionRegistry::new());
        let mailbox = runtime
            .ensure_local_observer_connection("observer-a")
            .expect("observer loopback registration should succeed");
        runtime.ensure_local_connection("peer-a");
        let mut observer = RemoteObserverRuntime::new(mailbox, 80, 24);
        let target = remote_target();
        let spec = RemoteInteractSurfaceSpec {
            socket_name: "wa-1".to_string(),
            surface_scope: "workspace-1".to_string(),
            target: target.address.qualified_target(),
            console_id: "workspace-main-slot:wa-1:workspace-1".to_string(),
            console_host_id: "observer-a".to_string(),
            console_location: ConsoleLocation::LocalWorkspace,
        };

        let binding = activate_surface_target(
            &runtime,
            &target,
            &spec,
            &TerminalSize {
                cols: 80,
                rows: 24,
                pixel_width: 0,
                pixel_height: 0,
            },
            &mut observer,
        )
        .expect("activation should succeed after authority connection exists");

        assert_eq!(binding.attachment_id, "attach-1");
        assert_eq!(
            observer.snapshot().attachment_id.as_deref(),
            Some("attach-1")
        );
    }

    #[test]
    fn raw_activation_leaves_future_bootstrap_bytes_collectable() {
        let runtime = RemoteMainSlotRuntime::with_registry(RemoteConnectionRegistry::new());
        let mailbox = runtime
            .ensure_local_observer_connection("observer-a")
            .expect("observer loopback registration should succeed");
        runtime.ensure_local_connection("peer-a");
        let target = remote_target();
        let mut observer = RemoteObserverRuntime::new(mailbox, 80, 24);
        let spec = RemoteInteractSurfaceSpec {
            socket_name: "wa-1".to_string(),
            surface_scope: "workspace-1".to_string(),
            target: target.address.qualified_target(),
            console_id: "workspace-main-slot:wa-1:workspace-1".to_string(),
            console_host_id: "observer-a".to_string(),
            console_location: ConsoleLocation::LocalWorkspace,
        };

        let (binding, raw) = activate_surface_target_with_mode(
            &runtime,
            &target,
            &spec,
            &TerminalSize {
                cols: 80,
                rows: 24,
                pixel_width: 0,
                pixel_height: 0,
            },
            &mut observer,
        )
        .expect("raw activation should succeed");

        assert_eq!(binding.attachment_id, "attach-1");
        assert!(raw.is_empty());

        runtime
            .send_mirror_bootstrap_chunk(&target, 1, "pty", b"prompt$ ".to_vec())
            .expect("bootstrap should fan out after activation");
        runtime
            .send_mirror_bootstrap_complete(&target, 1, false, false, true)
            .expect("bootstrap complete should fan out");

        assert_eq!(
            observer
                .sync_and_collect_raw()
                .expect("raw bootstrap should collect"),
            b"prompt$ \x1b[?25h"
        );
    }

    #[test]
    fn authority_status_from_runtime_prefers_disconnected_when_target_is_missing() {
        let runtime = RemoteMainSlotRuntime::with_registry(RemoteConnectionRegistry::new());
        runtime.ensure_local_connection("peer-a");

        assert_eq!(
            authority_status_from_runtime(
                &runtime,
                &remote_target(),
                false,
                &AuthorityTransportStatus::WaitingForRemoteAuthority,
            ),
            AuthorityTransportStatus::Disconnected
        );
    }

    #[test]
    fn target_presence_loss_exits_when_target_is_removed_even_if_authority_stays_connected() {
        assert!(should_exit_surface_for_target_presence_loss(
            true == false,
            true,
            false
        ));
        assert!(should_exit_surface_for_target_presence_loss(
            false, true, true
        ));
    }

    #[test]
    fn target_presence_loss_waits_during_reconnect_when_target_still_exists() {
        assert!(!should_exit_surface_for_target_presence_loss(
            true, true, true
        ));
        assert!(!should_exit_surface_for_target_presence_loss(
            true, false, true
        ));
    }

    #[test]
    fn surfaces_exit_when_remote_target_disappears() {
        let main_slot = RemoteInteractSurfaceSpec {
            socket_name: "wa-1".to_string(),
            surface_scope: "workspace-1".to_string(),
            target: "peer-a:shell-1".to_string(),
            console_id: "workspace-main-slot:wa-1:workspace-1".to_string(),
            console_host_id: "wa-1".to_string(),
            console_location: ConsoleLocation::LocalWorkspace,
        };
        let server_console = RemoteInteractSurfaceSpec {
            console_location: ConsoleLocation::ServerConsole,
            ..main_slot.clone()
        };

        assert!(should_exit_surface_for_target_presence(
            &main_slot, false, false, true, false
        ));
        assert!(should_exit_surface_for_target_presence(
            &server_console,
            false,
            false,
            true,
            false
        ));
        assert!(!should_exit_surface_for_target_presence(
            &main_slot, false, true, true, true
        ));
        assert!(!should_exit_surface_for_target_presence(
            &main_slot, true, true, true, false
        ));
    }

    #[test]
    fn authority_transport_socket_path_is_workspace_and_target_scoped() {
        let command = RemoteMainSlotCommand {
            socket_name: "wa-1".to_string(),
            session_name: "workspace-1".to_string(),
            target: "peer-a:shell-1".to_string(),
        };

        let path = authority_transport_socket_path(
            &command.socket_name,
            &command.session_name,
            &command.target,
        );
        let rendered = path.to_string_lossy();

        assert!(rendered.contains("waitagent-remote-"));
        assert!(rendered.ends_with(".sock"));
        assert!(rendered.len() < 108);
    }

    #[test]
    fn authority_target_output_envelope_flows_back_into_observer_terminal_state() {
        let runtime = RemoteMainSlotRuntime::with_registry(RemoteConnectionRegistry::new());
        let mailbox = runtime
            .ensure_local_observer_connection("observer-a")
            .expect("observer loopback registration should succeed");
        runtime.ensure_local_connection("peer-a");
        let target = remote_target();

        runtime
            .activate_target(
                &target,
                crate::infra::remote_protocol::RemoteConsoleDescriptor {
                    console_id: "console-a".to_string(),
                    console_host_id: "observer-a".to_string(),
                    location: crate::domain::session_catalog::ConsoleLocation::LocalWorkspace,
                },
                12,
                4,
            )
            .expect("remote activation should succeed");

        apply_authority_envelope(&runtime, &target, &authority_target_output_envelope(1))
            .expect("authority target_output should apply");

        let mut observer = RemoteObserverRuntime::new(mailbox, 12, 4);
        observer.sync().expect("observer sync should succeed");
        let snapshot = observer.snapshot();
        assert_eq!(snapshot.last_output_seq, Some(1));
        assert_eq!(
            snapshot.active_screen().lines[0],
            "a           ".to_string()
        );
    }

    #[test]
    fn authority_bootstrap_envelope_flows_back_into_observer_terminal_state() {
        let runtime = RemoteMainSlotRuntime::with_registry(RemoteConnectionRegistry::new());
        let mailbox = runtime
            .ensure_local_observer_connection("observer-a")
            .expect("observer loopback registration should succeed");
        runtime.ensure_local_connection("peer-a");
        let target = remote_target();

        runtime
            .activate_target(
                &target,
                crate::infra::remote_protocol::RemoteConsoleDescriptor {
                    console_id: "console-a".to_string(),
                    console_host_id: "observer-a".to_string(),
                    location: crate::domain::session_catalog::ConsoleLocation::LocalWorkspace,
                },
                12,
                4,
            )
            .expect("remote activation should succeed");

        apply_authority_envelope(&runtime, &target, &authority_bootstrap_chunk_envelope(1))
            .expect("authority bootstrap chunk should apply");
        apply_authority_envelope(&runtime, &target, &authority_bootstrap_complete_envelope(1))
            .expect("authority bootstrap complete should apply");

        let mut observer = RemoteObserverRuntime::new(mailbox, 12, 4);
        observer.sync().expect("observer sync should succeed");
        let snapshot = observer.snapshot();
        assert_eq!(snapshot.last_output_seq, None);
        assert!(snapshot.has_visible_output);
        assert!(snapshot.bootstrap_complete);
        assert_eq!(
            snapshot.active_screen().lines[0],
            "a           ".to_string()
        );
    }

    #[test]
    fn authority_target_output_envelope_flows_back_into_server_console_observer_terminal_state() {
        let runtime = RemoteMainSlotRuntime::with_registry(RemoteConnectionRegistry::new());
        let mailbox = runtime
            .ensure_local_observer_connection("server-console:wa-1:console-a")
            .expect("server-console observer loopback registration should succeed");
        runtime.ensure_local_connection("peer-a");
        let target = remote_target();

        runtime
            .activate_target(
                &target,
                crate::infra::remote_protocol::RemoteConsoleDescriptor {
                    console_id: "server-console:wa-1:console-a".to_string(),
                    console_host_id: "server-console:wa-1:console-a".to_string(),
                    location: crate::domain::session_catalog::ConsoleLocation::ServerConsole,
                },
                12,
                4,
            )
            .expect("server-console remote activation should succeed");

        apply_authority_envelope(&runtime, &target, &authority_target_output_envelope(1))
            .expect("authority target_output should apply for server-console observer");

        let mut observer = RemoteObserverRuntime::new(mailbox, 12, 4);
        observer
            .sync()
            .expect("server-console observer sync should succeed");
        let snapshot = observer.snapshot();
        assert_eq!(snapshot.last_output_seq, Some(1));
        assert_eq!(
            snapshot.console_id.as_deref(),
            Some("server-console:wa-1:console-a")
        );
        assert_eq!(
            snapshot.active_screen().lines[0],
            "a           ".to_string()
        );
    }

    #[test]
    fn authority_transport_runtime_round_trips_resize_input_and_output() {
        let registry = RemoteConnectionRegistry::new();
        let runtime = RemoteMainSlotRuntime::with_registry(registry.clone());
        let mailbox = runtime
            .ensure_local_observer_connection("observer-a")
            .expect("observer loopback registration should succeed");
        let authority_mailbox = runtime
            .ensure_local_connection("peer-a")
            .expect("authority loopback registration should succeed");
        let target = remote_target();
        let binding = runtime
            .activate_target(
                &target,
                RemoteConsoleDescriptor {
                    console_id: "console-a".to_string(),
                    console_host_id: "observer-a".to_string(),
                    location: ConsoleLocation::LocalWorkspace,
                },
                12,
                4,
            )
            .expect("remote activation should succeed");
        let authority_open = authority_mailbox.snapshot();
        assert_eq!(authority_open.len(), 1);
        assert_eq!(authority_open[0].message_type, "open_mirror_request");
        let socket_path = authority_transport_socket_path("wa-1", "workspace-1", "peer-a:shell-1");
        let _ = fs::remove_file(&socket_path);
        let (pane_tx, pane_rx) = mpsc::channel();
        let authority_tx = authority_transport_event_sender(pane_tx);
        let _listener = spawn_authority_listener(
            AuthorityConnectionRequest {
                socket_path: socket_path.clone(),
                authority_id: "peer-a".to_string(),
            },
            registry.clone(),
            authority_tx,
        )
        .expect("authority listener should bind");

        let mut authority =
            UnixStream::connect(&socket_path).expect("authority transport should connect");
        write_registration_frame(&mut authority, "peer-a")
            .expect("registration frame should encode");
        match pane_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("transport event should arrive")
        {
            RemotePaneEvent::AuthorityTransport(AuthorityTransportEvent::Connected {
                authority_id,
                ..
            }) => {
                assert_eq!(authority_id, "peer-a");
            }
            other => panic!("unexpected pane event: {other:?}"),
        }

        runtime
            .send_pty_resize(&target, &binding, 160, 50)
            .expect("resize should route");
        assert_eq!(
            match crate::infra::remote_transport_codec::read_control_plane_envelope(&mut authority,)
                .expect("resize command should arrive")
                .payload
            {
                ControlPlanePayload::ApplyResize(payload) => {
                    RemoteAuthorityCommand::ApplyResize(payload)
                }
                other => panic!("unexpected payload: {other:?}"),
            },
            RemoteAuthorityCommand::ApplyResize(
                crate::infra::remote_protocol::ApplyResizePayload {
                    session_id: "shell-1".to_string(),
                    target_id: "remote-peer:peer-a:shell-1".to_string(),
                    resize_epoch: 1,
                    resize_authority_console_id: "console-a".to_string(),
                    cols: 160,
                    rows: 50,
                }
            )
        );

        runtime
            .send_raw_pty_input(&target, &binding, 1, b"a".to_vec())
            .expect("input should route");
        assert_eq!(
            match crate::infra::remote_transport_codec::read_control_plane_envelope(&mut authority,)
                .expect("input command should arrive")
                .payload
            {
                ControlPlanePayload::RawPtyInput(payload) => {
                    RemoteAuthorityCommand::RawPtyInput(payload)
                }
                other => panic!("unexpected payload: {other:?}"),
            },
            RemoteAuthorityCommand::RawPtyInput(
                crate::infra::remote_protocol::RawPtyInputPayload {
                    attachment_id: "attach-1".to_string(),
                    session_id: "shell-1".to_string(),
                    target_id: "remote-peer:peer-a:shell-1".to_string(),
                    console_id: "console-a".to_string(),
                    console_host_id: "observer-a".to_string(),
                    input_seq: 1,
                    input_bytes: b"a".to_vec(),
                }
            )
        );

        crate::infra::remote_transport_codec::write_authority_transport_frame(
            &mut authority,
            &crate::infra::remote_transport_codec::AuthorityTransportFrame::RawPtyOutput(
                RawPtyOutputPayload {
                    session_id: "shell-1".to_string(),
                    target_id: "remote-peer:peer-a:shell-1".to_string(),
                    output_seq: 1,
                    output_bytes: b"b".to_vec(),
                },
            ),
        )
        .expect("raw output should send");
        match pane_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("authority raw output should arrive")
        {
            RemotePaneEvent::AuthorityTransport(AuthorityTransportEvent::RawPtyOutput {
                authority_id,
                payload,
                ..
            }) => {
                assert_eq!(authority_id, "peer-a");
                runtime
                    .send_raw_pty_output(&target, payload.output_seq, payload.output_bytes)
                    .expect("raw authority output should apply");
            }
            other => panic!("unexpected event: {other:?}"),
        }

        let mut raw_reader = RemoteRawPtyMailboxReader::new(mailbox);
        assert_eq!(
            raw_reader
                .sync_and_collect_raw()
                .expect("raw output should be collectable"),
            b"b"
        );
        let _ = fs::remove_file(&socket_path);
    }

    #[test]
    fn mailbox_watcher_emits_update_for_messages_that_arrive_before_thread_starts() {
        let runtime = RemoteMainSlotRuntime::with_registry(RemoteConnectionRegistry::new());
        let mailbox = runtime
            .ensure_local_observer_connection("observer-a")
            .expect("observer loopback registration should succeed");
        runtime.ensure_local_connection("peer-a");
        runtime
            .activate_target(
                &remote_target(),
                RemoteConsoleDescriptor {
                    console_id: "console-a".to_string(),
                    console_host_id: "observer-a".to_string(),
                    location: ConsoleLocation::LocalWorkspace,
                },
                12,
                4,
            )
            .expect("remote activation should succeed");

        let (tx, rx) = mpsc::channel();
        spawn_mailbox_watcher(mailbox, tx);

        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1))
                .expect("watcher should emit for already-buffered mailbox messages"),
            RemotePaneEvent::MailboxUpdated
        );
    }

    #[test]
    fn raw_pty_input_route_sends_bytes_directly_without_base64_target_input() {
        let registry = RemoteConnectionRegistry::new();
        let capture = Arc::new(CapturingRawRemoteConnection::default());
        registry.register_connection("peer-a", capture.clone());
        let route = RawPtyInputRoute::default();
        route.activate(
            &remote_target(),
            &RemoteAttachmentBinding {
                session_id: "shell-1".to_string(),
                target_id: "remote-peer:peer-a:shell-1".to_string(),
                attachment_id: "attach-1".to_string(),
                console_id: "console-a".to_string(),
            },
            "observer-a",
        );

        assert!(route
            .send(&registry, b"ls\r".to_vec())
            .expect("raw route should send"));

        let payloads = capture.raw_inputs();
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0].input_bytes, b"ls\r");
        assert_eq!(payloads[0].console_host_id, "observer-a");
        assert_eq!(payloads[0].input_seq, 1);
    }

    #[test]
    fn raw_pty_input_route_forwards_plain_left_arrow_to_remote_pty() {
        let registry = RemoteConnectionRegistry::new();
        let capture = Arc::new(CapturingRawRemoteConnection::default());
        registry.register_connection("peer-a", capture.clone());
        let route = RawPtyInputRoute::default();
        route.activate(
            &remote_target(),
            &RemoteAttachmentBinding {
                session_id: "shell-1".to_string(),
                target_id: "remote-peer:peer-a:shell-1".to_string(),
                attachment_id: "attach-1".to_string(),
                console_id: "console-a".to_string(),
            },
            "observer-a",
        );

        assert!(route
            .send(&registry, b"\x1b[D".to_vec())
            .expect("plain left arrow should be forwarded"));

        let payloads = capture.raw_inputs();
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0].input_bytes, b"\x1b[D");
    }

    #[test]
    fn raw_pty_input_route_keeps_ctrl_right_for_local_chrome_navigation() {
        let registry = RemoteConnectionRegistry::new();
        let capture = Arc::new(CapturingRawRemoteConnection::default());
        registry.register_connection("peer-a", capture.clone());
        let route = RawPtyInputRoute::default();
        route.activate(
            &remote_target(),
            &RemoteAttachmentBinding {
                session_id: "shell-1".to_string(),
                target_id: "remote-peer:peer-a:shell-1".to_string(),
                attachment_id: "attach-1".to_string(),
                console_id: "console-a".to_string(),
            },
            "observer-a",
        );

        assert!(!route
            .send(&registry, b"\x1b[1;5C".to_vec())
            .expect("ctrl-right should stay local"));

        assert!(capture.raw_inputs().is_empty());
    }

    #[test]
    fn raw_pty_input_route_fails_without_raw_frame_support() {
        let registry = RemoteConnectionRegistry::new();
        registry.register_connection("peer-a", Arc::new(CapturingRemoteConnection::default()));
        let route = RawPtyInputRoute::default();
        route.activate(
            &remote_target(),
            &RemoteAttachmentBinding {
                session_id: "shell-1".to_string(),
                target_id: "remote-peer:peer-a:shell-1".to_string(),
                attachment_id: "attach-1".to_string(),
                console_id: "console-a".to_string(),
            },
            "observer-a",
        );

        let error = route
            .send(&registry, b"x".to_vec())
            .expect_err("raw mode must not fall back to control-plane envelopes");

        assert!(error.to_string().contains("raw PTY input frames"));
    }

    #[test]
    fn direct_raw_pty_output_collects_bytes_without_mailbox_replay() {
        let mut last_output_seq = None;
        let raw = collect_direct_raw_pty_output_envelope(
            &remote_target(),
            &authority_raw_pty_output_envelope(1, b"\x1b[32mok\r\n".to_vec()),
            &mut last_output_seq,
        )
        .expect("raw output should collect")
        .expect("raw output should be returned");

        assert_eq!(raw, b"\x1b[32mok\r\n");
        assert_eq!(last_output_seq, Some(1));

        let duplicate = collect_direct_raw_pty_output_envelope(
            &remote_target(),
            &authority_raw_pty_output_envelope(1, b"again".to_vec()),
            &mut last_output_seq,
        )
        .expect_err("duplicate raw output sequence should be rejected");
        assert!(duplicate.to_string().contains("out-of-order output"));
    }

    #[test]
    fn direct_raw_pty_output_payload_collects_bytes_without_envelope() {
        let mut last_output_seq = None;
        let raw = collect_direct_raw_pty_output_payload(
            &remote_target(),
            "peer-a",
            &RawPtyOutputPayload {
                session_id: "shell-1".to_string(),
                target_id: "remote-peer:peer-a:shell-1".to_string(),
                output_seq: 1,
                output_bytes: b"\x1b[32mok\r\n".to_vec(),
            },
            &mut last_output_seq,
        )
        .expect("raw output should collect");

        assert_eq!(raw, b"\x1b[32mok\r\n");
        assert_eq!(last_output_seq, Some(1));
    }

    #[test]
    fn raw_pty_mailbox_reader_collects_bootstrap_before_direct_output() {
        let registry = RemoteConnectionRegistry::new();
        let mailbox = registry.register_loopback_connection("observer-a");
        let connection = registry
            .connection_for("observer-a")
            .expect("loopback connection should exist");
        connection
            .send(&authority_bootstrap_chunk_envelope_with_bytes(
                1,
                b"prompt$ ".to_vec(),
            ))
            .expect("bootstrap chunk should enqueue");
        connection
            .send(&authority_bootstrap_complete_envelope(1))
            .expect("bootstrap complete should enqueue");
        connection
            .send(&authority_raw_pty_output_envelope(1, b"echo\r\n".to_vec()))
            .expect("raw output should enqueue");
        let mut reader = RemoteRawPtyMailboxReader::new(mailbox);

        let raw = reader
            .sync_and_collect_raw()
            .expect("raw mailbox reader should collect bootstrap and raw output");

        assert_eq!(raw, b"prompt$ \x1b[?25hecho\r\n");
    }

    #[test]
    fn resize_waits_for_authority_registration_then_flushes_latest_size() {
        let registry = RemoteConnectionRegistry::new();
        let runtime = RemoteMainSlotRuntime::with_registry(registry);
        runtime.ensure_local_connection("observer-a");
        let target = remote_target();
        let binding = runtime
            .activate_target_with_raw_pty_mode(
                &target,
                RemoteConsoleDescriptor {
                    console_id: "console-a".to_string(),
                    console_host_id: "observer-a".to_string(),
                    location: ConsoleLocation::LocalWorkspace,
                },
                80,
                24,
                true,
            )
            .expect("activation should not require authority registration");
        let mut pending = None;

        sync_or_defer_remote_pty_size(
            &runtime,
            &target,
            &binding,
            &TerminalSize {
                cols: 132,
                rows: 43,
                pixel_width: 0,
                pixel_height: 0,
            },
            &mut pending,
        )
        .expect("resize should defer while authority is unregistered");

        assert_eq!(
            pending.as_ref().map(|size| (size.cols, size.rows)),
            Some((132, 43))
        );
        let authority_mailbox = runtime
            .ensure_local_connection("peer-a")
            .expect("authority registration should expose mailbox");
        flush_pending_pty_size(&runtime, &target, &binding, &mut pending)
            .expect("pending resize should flush after authority registration");

        assert!(pending.is_none());
        let envelopes = authority_mailbox.snapshot();
        assert_eq!(envelopes.len(), 1);
        match &envelopes[0].payload {
            ControlPlanePayload::ApplyResize(payload) => {
                assert_eq!(payload.cols, 132);
                assert_eq!(payload.rows, 43);
            }
            other => panic!("unexpected payload: {other:?}"),
        }
    }

    #[test]
    fn paused_input_flushes_after_authority_registration() {
        let runtime = RemoteMainSlotRuntime::with_registry(RemoteConnectionRegistry::new());
        runtime.ensure_local_connection("observer-a");
        let target = remote_target();
        let binding = runtime
            .activate_target_with_raw_pty_mode(
                &target,
                RemoteConsoleDescriptor {
                    console_id: "console-a".to_string(),
                    console_host_id: "observer-a".to_string(),
                    location: ConsoleLocation::LocalWorkspace,
                },
                80,
                24,
                true,
            )
            .expect("activation should not require authority registration");
        let mut paused = vec![b"ab".to_vec(), b"c".to_vec()];
        let mut console_seq = 7;

        let authority_mailbox = runtime
            .ensure_local_connection("peer-a")
            .expect("authority registration should expose mailbox");
        flush_paused_input(&runtime, &target, &binding, &mut paused, &mut console_seq)
            .expect("paused input should flush after authority registration");

        assert_eq!(console_seq, 9);
        assert!(paused.is_empty());
        let envelopes = authority_mailbox.snapshot();
        assert_eq!(envelopes.len(), 2);
        let inputs: Vec<_> = envelopes
            .iter()
            .map(|envelope| match &envelope.payload {
                ControlPlanePayload::RawPtyInput(payload) => {
                    (payload.input_seq, payload.input_bytes.clone())
                }
                other => panic!("unexpected payload: {other:?}"),
            })
            .collect();
        assert_eq!(inputs, vec![(1, b"ab".to_vec()), (2, b"c".to_vec())]);
    }

    #[test]
    fn clear_screen_escape_matches_full_terminal_reset_for_raw_activation() {
        assert_eq!(CLEAR_SCREEN_HOME_ESCAPE.as_bytes(), b"\x1b[2J\x1b[H");
    }

    #[test]
    fn empty_raw_output_does_not_mark_screen_initialized() {
        let mut initialized = false;

        write_remote_raw_output_with_initial_clear(b"", &mut initialized)
            .expect("empty raw output should be ignored");

        assert!(!initialized);
    }

    fn authority_target_output_envelope(output_seq: u64) -> ProtocolEnvelope<ControlPlanePayload> {
        ProtocolEnvelope {
            protocol_version: "1.1".to_string(),
            message_id: format!("msg-{output_seq}"),
            message_type: "target_output",
            timestamp: "2026-04-28T00:00:00Z".to_string(),
            sender_id: "peer-a".to_string(),
            correlation_id: None,
            session_id: Some("shell-1".to_string()),
            target_id: Some("remote-peer:peer-a:shell-1".to_string()),
            attachment_id: None,
            console_id: None,
            payload: ControlPlanePayload::TargetOutput(TargetOutputPayload {
                session_id: "shell-1".to_string(),
                target_id: "remote-peer:peer-a:shell-1".to_string(),
                output_seq,
                stream: "pty",
                output_bytes: b"a".to_vec(),
            }),
        }
    }

    fn authority_raw_pty_output_envelope(
        output_seq: u64,
        output_bytes: Vec<u8>,
    ) -> ProtocolEnvelope<ControlPlanePayload> {
        ProtocolEnvelope {
            protocol_version: "1.1".to_string(),
            message_id: format!("raw-msg-{output_seq}"),
            message_type: "raw_pty_output",
            timestamp: "2026-04-28T00:00:00Z".to_string(),
            sender_id: "peer-a".to_string(),
            correlation_id: None,
            session_id: Some("shell-1".to_string()),
            target_id: Some("remote-peer:peer-a:shell-1".to_string()),
            attachment_id: None,
            console_id: None,
            payload: ControlPlanePayload::RawPtyOutput(RawPtyOutputPayload {
                session_id: "shell-1".to_string(),
                target_id: "remote-peer:peer-a:shell-1".to_string(),
                output_seq,
                output_bytes,
            }),
        }
    }

    fn authority_bootstrap_chunk_envelope(chunk_seq: u64) -> ProtocolEnvelope<ControlPlanePayload> {
        authority_bootstrap_chunk_envelope_with_bytes(chunk_seq, b"a".to_vec())
    }

    fn authority_bootstrap_chunk_envelope_with_bytes(
        chunk_seq: u64,
        output_bytes: Vec<u8>,
    ) -> ProtocolEnvelope<ControlPlanePayload> {
        ProtocolEnvelope {
            protocol_version: "1.1".to_string(),
            message_id: format!("bootstrap-{chunk_seq}"),
            message_type: "mirror_bootstrap_chunk",
            timestamp: "2026-04-28T00:00:00Z".to_string(),
            sender_id: "peer-a".to_string(),
            correlation_id: None,
            session_id: Some("shell-1".to_string()),
            target_id: Some("remote-peer:peer-a:shell-1".to_string()),
            attachment_id: None,
            console_id: None,
            payload: ControlPlanePayload::MirrorBootstrapChunk(MirrorBootstrapChunkPayload {
                session_id: "shell-1".to_string(),
                target_id: "remote-peer:peer-a:shell-1".to_string(),
                chunk_seq,
                stream: "pty",
                output_bytes,
            }),
        }
    }

    fn authority_bootstrap_complete_envelope(
        last_chunk_seq: u64,
    ) -> ProtocolEnvelope<ControlPlanePayload> {
        ProtocolEnvelope {
            protocol_version: "1.1".to_string(),
            message_id: format!("bootstrap-complete-{last_chunk_seq}"),
            message_type: "mirror_bootstrap_complete",
            timestamp: "2026-04-28T00:00:00Z".to_string(),
            sender_id: "peer-a".to_string(),
            correlation_id: None,
            session_id: Some("shell-1".to_string()),
            target_id: Some("remote-peer:peer-a:shell-1".to_string()),
            attachment_id: None,
            console_id: None,
            payload: ControlPlanePayload::MirrorBootstrapComplete(MirrorBootstrapCompletePayload {
                session_id: "shell-1".to_string(),
                target_id: "remote-peer:peer-a:shell-1".to_string(),
                last_chunk_seq,
                alternate_screen_active: false,
                application_cursor_keys: false,
                cursor_visible: true,
            }),
        }
    }

    fn remote_target() -> ManagedSessionRecord {
        ManagedSessionRecord {
            address: ManagedSessionAddress::remote_peer("peer-a", "shell-1"),
            selector: None,
            availability: SessionAvailability::Online,
            workspace_dir: None,
            workspace_key: None,
            session_role: None,
            opened_by: Vec::new(),
            attached_clients: 0,
            window_count: 1,
            command_name: Some("bash".to_string()),
            current_path: None,
            task_state: ManagedSessionTaskState::Running,
        }
    }

    fn server_console_surface_spec() -> RemoteInteractSurfaceSpec {
        RemoteInteractSurfaceSpec {
            socket_name: "wa-1".to_string(),
            surface_scope: "server-console:console-a".to_string(),
            target: "peer-a:shell-1".to_string(),
            console_id: "server-console:wa-1:console-a".to_string(),
            console_host_id: "server-console:wa-1:console-a".to_string(),
            console_location: ConsoleLocation::ServerConsole,
        }
    }

    fn test_socket_path(name: &str) -> PathBuf {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        std::env::temp_dir().join(format!(
            "waitagent-test-remote-main-slot-pane-{name}-{}-{millis}.sock",
            process::id()
        ))
    }

    fn render_full_frame(
        lines: &[&str],
        cursor_visible: bool,
        cursor: Option<(usize, usize)>,
    ) -> String {
        let mut frame = String::from("\x1b[?25l\x1b[?7l");
        for (row, line) in lines.iter().enumerate() {
            frame.push_str(&format!("\x1b[{};1H\x1b[2K{}", row + 1, line));
        }
        if let Some((row, col)) = cursor {
            frame.push_str("\x1b[?7h");
            frame.push_str(&format!("\x1b[{};{}H", row + 1, col + 1));
            frame.push_str(if cursor_visible {
                "\x1b[?25h"
            } else {
                "\x1b[?25l"
            });
        } else {
            frame.push_str("\x1b[?7h\x1b[?25l");
        }
        frame
    }

    #[derive(Default)]
    struct CapturingRemoteConnection {
        envelopes: Mutex<Vec<ProtocolEnvelope<ControlPlanePayload>>>,
    }

    impl RemoteControlPlaneConnection for CapturingRemoteConnection {
        fn send(
            &self,
            envelope: &ProtocolEnvelope<ControlPlanePayload>,
        ) -> Result<(), RemoteControlPlaneTransportError> {
            self.envelopes
                .lock()
                .expect("capturing connection mutex should not be poisoned")
                .push(envelope.clone());
            Ok(())
        }
    }

    #[derive(Default)]
    struct CapturingRawRemoteConnection {
        raw_inputs: Mutex<Vec<RawPtyInputPayload>>,
    }

    impl CapturingRawRemoteConnection {
        fn raw_inputs(&self) -> Vec<RawPtyInputPayload> {
            self.raw_inputs
                .lock()
                .expect("capturing raw connection mutex should not be poisoned")
                .clone()
        }
    }

    impl RemoteControlPlaneConnection for CapturingRawRemoteConnection {
        fn send(
            &self,
            _envelope: &ProtocolEnvelope<ControlPlanePayload>,
        ) -> Result<(), RemoteControlPlaneTransportError> {
            panic!("raw input route must not fall back to control-plane envelopes")
        }

        fn send_raw_pty_input(
            &self,
            payload: &RawPtyInputPayload,
        ) -> Result<(), RemoteControlPlaneTransportError> {
            self.raw_inputs
                .lock()
                .expect("capturing raw connection mutex should not be poisoned")
                .push(payload.clone());
            Ok(())
        }
    }
}
