mod tests {
    use super::super::{MirrorRouteState, RemoteControlPlaneService};
    use crate::domain::session_catalog::{
        ConsoleLocation, ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState,
        SessionAvailability,
    };
    use crate::infra::remote_protocol::{
        ControlPlaneDestination, ControlPlanePayload, RemoteConsoleDescriptor,
    };

    #[test]
    fn open_target_assigns_attachment_and_pty_resize_authority_without_apply_resize() {
        let mut service = RemoteControlPlaneService::new();
        let target = remote_target("peer-a", "shell-1");

        let messages = service
            .open_target(
                &target,
                console("console-a", "observer-a", ConsoleLocation::LocalWorkspace),
                120,
                40,
            )
            .expect("open should succeed");

        assert_eq!(messages.len(), 3);
        assert!(matches!(
            messages[0].destination,
            ControlPlaneDestination::ObserverNode(ref node) if node == "observer-a"
        ));
        match &messages[0].envelope.payload {
            ControlPlanePayload::OpenTargetOk(payload) => {
                assert_eq!(payload.attachment_id, "attach-1");
                assert_eq!(payload.resize_epoch, 1);
                assert_eq!(payload.resize_authority_console_id, "console-a");
            }
            other => panic!("unexpected payload: {other:?}"),
        }
        assert!(matches!(
            messages[1].destination,
            ControlPlaneDestination::AllOpenedObservers { .. }
        ));
        match &messages[1].envelope.payload {
            ControlPlanePayload::ResizeAuthorityChanged(payload) => {
                assert_eq!(payload.cols, None);
                assert_eq!(payload.rows, None);
            }
            other => panic!("unexpected payload: {other:?}"),
        }
        assert!(matches!(
            messages[2].destination,
            ControlPlaneDestination::AuthorityNode(ref node) if node == "peer-a"
        ));
        match &messages[2].envelope.payload {
            ControlPlanePayload::OpenMirrorRequest(payload) => {
                assert_eq!(payload.session_id, "shell-1");
                assert_eq!(payload.target_id, "remote-peer:peer-a:shell-1");
                assert_eq!(payload.console_id, "console-a");
                assert_eq!(payload.cols, 120);
                assert_eq!(payload.rows, 40);
            }
            other => panic!("unexpected payload: {other:?}"),
        }
    }

    #[test]
    fn raw_pty_input_is_serialized_by_server_sequence() {
        let mut service = RemoteControlPlaneService::new();
        let target = remote_target("peer-a", "shell-1");

        let first_open = service
            .open_target(
                &target,
                console("console-a", "observer-a", ConsoleLocation::LocalWorkspace),
                100,
                30,
            )
            .expect("first open should succeed");
        let second_open = service
            .open_target(
                &target,
                console("console-b", "observer-b", ConsoleLocation::ServerConsole),
                140,
                50,
            )
            .expect("second open should succeed");
        let first_attachment = match &first_open[0].envelope.payload {
            ControlPlanePayload::OpenTargetOk(payload) => payload.attachment_id.clone(),
            other => panic!("unexpected payload: {other:?}"),
        };
        let second_attachment = match &second_open[0].envelope.payload {
            ControlPlanePayload::OpenTargetOk(payload) => payload.attachment_id.clone(),
            other => panic!("unexpected payload: {other:?}"),
        };

        let first_input = service
            .route_raw_pty_input(&target, &first_attachment, 1, b"a".to_vec())
            .expect("first input should route");
        let second_input = service
            .route_raw_pty_input(&target, &second_attachment, 9, b"b".to_vec())
            .expect("second input should route");

        match &first_input.envelope.payload {
            ControlPlanePayload::RawPtyInput(payload) => assert_eq!(payload.input_seq, 1),
            other => panic!("unexpected payload: {other:?}"),
        }
        match &second_input.envelope.payload {
            ControlPlanePayload::RawPtyInput(payload) => assert_eq!(payload.input_seq, 2),
            other => panic!("unexpected payload: {other:?}"),
        }
    }

    #[test]
    fn non_authority_pty_resize_is_rejected() {
        let mut service = RemoteControlPlaneService::new();
        let target = remote_target("peer-a", "shell-1");

        let first_open = service
            .open_target(
                &target,
                console("console-a", "observer-a", ConsoleLocation::LocalWorkspace),
                100,
                30,
            )
            .expect("first open should succeed");
        let second_open = service
            .open_target(
                &target,
                console("console-b", "observer-b", ConsoleLocation::ServerConsole),
                140,
                50,
            )
            .expect("second open should succeed");
        let first_attachment = match &first_open[0].envelope.payload {
            ControlPlanePayload::OpenTargetOk(payload) => payload.attachment_id.clone(),
            other => panic!("unexpected payload: {other:?}"),
        };
        let second_attachment = match &second_open[0].envelope.payload {
            ControlPlanePayload::OpenTargetOk(payload) => payload.attachment_id.clone(),
            other => panic!("unexpected payload: {other:?}"),
        };

        let error = service
            .route_pty_resize_request(&target, &first_attachment, 80, 24)
            .expect_err("older attachment should no longer hold authority");
        assert_eq!(
            error.to_string(),
            format!("attachment `{first_attachment}` does not hold PTY resize authority")
        );

        let routed = service
            .route_pty_resize_request(&target, &second_attachment, 160, 60)
            .expect("authority resize should route");
        match routed.envelope.payload {
            ControlPlanePayload::ApplyResize(payload) => {
                assert_eq!(payload.resize_epoch, 2);
                assert_eq!(payload.cols, 160);
                assert_eq!(payload.rows, 60);
            }
            other => panic!("unexpected payload: {other:?}"),
        }
    }

    #[test]
    fn closing_authority_promotes_most_recent_remaining_attachment() {
        let mut service = RemoteControlPlaneService::new();
        let target = remote_target("peer-a", "shell-1");

        let first_open = service
            .open_target(
                &target,
                console("console-a", "observer-a", ConsoleLocation::LocalWorkspace),
                100,
                30,
            )
            .expect("first open should succeed");
        let second_open = service
            .open_target(
                &target,
                console("console-b", "observer-b", ConsoleLocation::ServerConsole),
                140,
                50,
            )
            .expect("second open should succeed");
        let second_attachment = match &second_open[0].envelope.payload {
            ControlPlanePayload::OpenTargetOk(payload) => payload.attachment_id.clone(),
            other => panic!("unexpected payload: {other:?}"),
        };
        let close_messages = service
            .close_target(&target, &second_attachment)
            .expect("closing the authority attachment should succeed");

        assert_eq!(close_messages.len(), 2);
        match &close_messages[0].envelope.payload {
            ControlPlanePayload::ResizeAuthorityChanged(payload) => {
                assert_eq!(payload.resize_epoch, 3);
                assert_eq!(payload.resize_authority_console_id, "console-a");
                assert_eq!(payload.cols, None);
                assert_eq!(payload.rows, None);
            }
            other => panic!("unexpected payload: {other:?}"),
        }
        match &first_open[0].envelope.payload {
            ControlPlanePayload::OpenTargetOk(payload) => {
                assert_eq!(payload.attachment_id, "attach-1")
            }
            other => panic!("unexpected payload: {other:?}"),
        }
    }

    #[test]
    fn expands_all_opened_observers_into_concrete_node_deliveries() {
        let mut service = RemoteControlPlaneService::new();
        let target = remote_target("peer-a", "shell-1");

        service
            .open_target(
                &target,
                console("console-a", "observer-a", ConsoleLocation::LocalWorkspace),
                100,
                30,
            )
            .expect("first open should succeed");
        let routed = service
            .open_target(
                &target,
                console("console-b", "observer-b", ConsoleLocation::ServerConsole),
                140,
                50,
            )
            .expect("second open should succeed");

        let deliveries = service
            .resolve_node_deliveries(&routed)
            .expect("deliveries should resolve");

        assert_eq!(deliveries.len(), 3);
        assert_eq!(deliveries[0].node_id, "observer-b");
        assert_eq!(deliveries[1].node_id, "observer-a");
        assert_eq!(deliveries[2].node_id, "observer-b");
        assert!(matches!(
            deliveries[1].envelope.payload,
            ControlPlanePayload::ResizeAuthorityChanged(_)
        ));
        assert!(matches!(
            deliveries[2].envelope.payload,
            ControlPlanePayload::ResizeAuthorityChanged(_)
        ));
    }

    #[test]
    fn second_open_reuses_existing_mirror_route_without_duplicate_open_request() {
        let mut service = RemoteControlPlaneService::new();
        let target = remote_target("peer-a", "shell-1");

        service
            .open_target(
                &target,
                console("console-a", "observer-a", ConsoleLocation::LocalWorkspace),
                100,
                30,
            )
            .expect("first open should succeed");
        let second_open = service
            .open_target(
                &target,
                console("console-b", "observer-b", ConsoleLocation::ServerConsole),
                140,
                50,
            )
            .expect("second open should succeed");

        assert_eq!(second_open.len(), 2);
        assert!(!second_open.iter().any(|message| matches!(
            message.envelope.payload,
            ControlPlanePayload::OpenMirrorRequest(_)
        )));
    }

    #[test]
    fn record_mirror_accepted_transitions_from_pending_to_active() {
        let mut service = RemoteControlPlaneService::new();
        let target = remote_target("peer-a", "shell-1");

        service
            .open_target(
                &target,
                console("console-a", "observer-a", ConsoleLocation::LocalWorkspace),
                100,
                30,
            )
            .expect("open should succeed");

        // After open_target, mirror_route should be Pending
        let state = service.session_states.get("shell-1").unwrap();
        assert_eq!(state.mirror_route, MirrorRouteState::Pending);
        assert_eq!(state.authority_node_id.as_deref(), Some("peer-a"));

        service.record_mirror_accepted("shell-1");
        let state = service.session_states.get("shell-1").unwrap();
        assert_eq!(state.mirror_route, MirrorRouteState::Active);
    }

    #[test]
    fn record_mirror_rejected_does_not_affect_existing_attachments() {
        let mut service = RemoteControlPlaneService::new();
        let target = remote_target("peer-a", "shell-1");

        let _first_open = service
            .open_target(
                &target,
                console("console-a", "observer-a", ConsoleLocation::LocalWorkspace),
                100,
                30,
            )
            .expect("first open should succeed");

        service.record_mirror_rejected("shell-1", "target offline".to_string());
        let state = service.session_states.get("shell-1").unwrap();
        assert_eq!(
            state.mirror_route,
            MirrorRouteState::Rejected("target offline".to_string())
        );

        // Second open should NOT trigger another mirror request
        let second_open = service
            .open_target(
                &target,
                console("console-b", "observer-b", ConsoleLocation::ServerConsole),
                140,
                50,
            )
            .expect("second open should succeed");
        assert_eq!(second_open.len(), 2);
        assert!(!second_open.iter().any(|message| matches!(
            message.envelope.payload,
            ControlPlanePayload::OpenMirrorRequest(_)
        )));
    }

    #[test]
    fn authority_disconnect_resets_mirror_route_for_reconnect() {
        let mut service = RemoteControlPlaneService::new();
        let target = remote_target("peer-a", "shell-1");

        service
            .open_target(
                &target,
                console("console-a", "observer-a", ConsoleLocation::LocalWorkspace),
                100,
                30,
            )
            .expect("open should succeed");

        // Simulate mirror accepted
        service.record_mirror_accepted("shell-1");
        let state = service.session_states.get("shell-1").unwrap();
        assert_eq!(state.mirror_route, MirrorRouteState::Active);
        assert_eq!(state.authority_node_id.as_deref(), Some("peer-a"));

        // Authority disconnects
        service.handle_authority_disconnect("peer-a");
        let state = service.session_states.get("shell-1").unwrap();
        assert_eq!(state.mirror_route, MirrorRouteState::None);
        assert_eq!(state.authority_node_id, None);

        // After disconnect, a new close+open should be able to request mirror again
        let attachment = "attach-1";
        service
            .close_target(&target, attachment)
            .expect("close should succeed");

        let reopen = service
            .open_target(
                &target,
                console("console-a", "observer-a", ConsoleLocation::LocalWorkspace),
                100,
                30,
            )
            .expect("reopen should succeed");

        assert!(reopen.iter().any(|message| matches!(
            message.envelope.payload,
            ControlPlanePayload::OpenMirrorRequest(_)
        )));
    }

    #[test]
    fn authority_disconnect_reopens_mirror_even_when_attachments_remain() {
        let mut service = RemoteControlPlaneService::new();
        let target = remote_target("peer-a", "shell-1");

        service
            .open_target(
                &target,
                console("console-a", "observer-a", ConsoleLocation::LocalWorkspace),
                100,
                30,
            )
            .expect("first open should succeed");
        service.record_mirror_accepted("shell-1");
        service
            .open_target(
                &target,
                console("console-b", "observer-b", ConsoleLocation::ServerConsole),
                140,
                50,
            )
            .expect("second open should succeed");

        service.handle_authority_disconnect("peer-a");
        let reopen = service
            .open_target(
                &target,
                console("console-c", "observer-c", ConsoleLocation::ServerConsole),
                160,
                60,
            )
            .expect("open during reconnect should succeed");

        assert!(reopen.iter().any(|message| matches!(
            message.envelope.payload,
            ControlPlanePayload::OpenMirrorRequest(_)
        )));
        let state = service.session_states.get("shell-1").unwrap();
        assert_eq!(state.mirror_route, MirrorRouteState::Pending);
        assert_eq!(state.attachments.len(), 3);
    }

    #[test]
    fn closing_last_attachment_emits_close_mirror_request() {
        let mut service = RemoteControlPlaneService::new();
        let target = remote_target("peer-a", "shell-1");

        let first_open = service
            .open_target(
                &target,
                console("console-a", "observer-a", ConsoleLocation::LocalWorkspace),
                100,
                30,
            )
            .expect("first open should succeed");
        let attachment = match &first_open[0].envelope.payload {
            ControlPlanePayload::OpenTargetOk(payload) => payload.attachment_id.clone(),
            other => panic!("unexpected payload: {other:?}"),
        };

        let close_messages = service
            .close_target(&target, &attachment)
            .expect("closing the last attachment should succeed");

        assert_eq!(close_messages.len(), 1);
        assert!(matches!(
            close_messages[0].destination,
            ControlPlaneDestination::AuthorityNode(ref node) if node == "peer-a"
        ));
        match &close_messages[0].envelope.payload {
            ControlPlanePayload::CloseMirrorRequest(payload) => {
                assert_eq!(payload.session_id, "shell-1");
                assert_eq!(payload.target_id, "remote-peer:peer-a:shell-1");
            }
            other => panic!("unexpected payload: {other:?}"),
        }
    }

    #[test]
    fn target_output_is_fanned_out_to_all_opened_observers() {
        let mut service = RemoteControlPlaneService::new();
        let target = remote_target("peer-a", "shell-1");

        service
            .open_target(
                &target,
                console("console-a", "observer-a", ConsoleLocation::LocalWorkspace),
                100,
                30,
            )
            .expect("first open should succeed");
        service
            .open_target(
                &target,
                console("console-b", "observer-b", ConsoleLocation::ServerConsole),
                140,
                50,
            )
            .expect("second open should succeed");

        let routed = service
            .route_target_output(&target, 7, "pty", b"a".to_vec())
            .expect("output should route");
        let deliveries = service
            .resolve_node_deliveries(&[routed])
            .expect("deliveries should resolve");

        assert_eq!(deliveries.len(), 2);
        assert_eq!(deliveries[0].node_id, "observer-a");
        assert_eq!(deliveries[1].node_id, "observer-b");
        match &deliveries[0].envelope.payload {
            ControlPlanePayload::TargetOutput(payload) => {
                assert_eq!(payload.output_seq, 7);
                assert_eq!(payload.stream, "pty");
            }
            other => panic!("unexpected payload: {other:?}"),
        }
    }

    #[test]
    fn raw_pty_input_routes_bytes_without_base64() {
        let mut service = RemoteControlPlaneService::new();
        let target = remote_target("peer-a", "shell-1");
        let first_open = service
            .open_target(
                &target,
                console("console-a", "observer-a", ConsoleLocation::LocalWorkspace),
                100,
                30,
            )
            .expect("first open should succeed");
        let attachment = match &first_open[0].envelope.payload {
            ControlPlanePayload::OpenTargetOk(payload) => payload.attachment_id.clone(),
            other => panic!("unexpected payload: {other:?}"),
        };

        let routed = service
            .route_raw_pty_input(&target, &attachment, 1, b"\x1b[A".to_vec())
            .expect("raw PTY input should route");

        assert!(matches!(
            routed.destination,
            ControlPlaneDestination::AuthorityNode(ref node) if node == "peer-a"
        ));
        match routed.envelope.payload {
            ControlPlanePayload::RawPtyInput(payload) => {
                assert_eq!(payload.input_seq, 1);
                assert_eq!(payload.input_bytes, b"\x1b[A");
                assert_eq!(payload.console_host_id, "observer-a");
            }
            other => panic!("unexpected payload: {other:?}"),
        }
    }

    #[test]
    fn raw_pty_output_is_fanned_out_to_all_opened_observers() {
        let mut service = RemoteControlPlaneService::new();
        let target = remote_target("peer-a", "shell-1");

        service
            .open_target(
                &target,
                console("console-a", "observer-a", ConsoleLocation::LocalWorkspace),
                100,
                30,
            )
            .expect("first open should succeed");
        service
            .open_target(
                &target,
                console("console-b", "observer-b", ConsoleLocation::ServerConsole),
                140,
                50,
            )
            .expect("second open should succeed");

        let routed = service
            .route_raw_pty_output(&target, 7, b"a".to_vec())
            .expect("raw output should route");
        let deliveries = service
            .resolve_node_deliveries(&[routed])
            .expect("deliveries should resolve");

        assert_eq!(deliveries.len(), 2);
        assert_eq!(deliveries[0].node_id, "observer-a");
        assert_eq!(deliveries[1].node_id, "observer-b");
        match &deliveries[0].envelope.payload {
            ControlPlanePayload::RawPtyOutput(payload) => {
                assert_eq!(payload.output_seq, 7);
                assert_eq!(payload.output_bytes, b"a");
            }
            other => panic!("unexpected payload: {other:?}"),
        }
    }

    fn console(
        console_id: &str,
        host_id: &str,
        location: ConsoleLocation,
    ) -> RemoteConsoleDescriptor {
        RemoteConsoleDescriptor {
            console_id: console_id.to_string(),
            console_host_id: host_id.to_string(),
            location,
        }
    }

    fn remote_target(authority_id: &str, session_id: &str) -> ManagedSessionRecord {
        ManagedSessionRecord {
            address: ManagedSessionAddress::remote_peer(authority_id, session_id),
            selector: None,
            availability: SessionAvailability::Online,
            workspace_dir: None,
            workspace_key: None,
            session_role: None,
            opened_by: Vec::new(),
            attached_clients: 0,
            window_count: 1,
            command_name: Some("bash".to_string()),
            display_command_name: None,
            current_path: None,
            task_state: ManagedSessionTaskState::Running,
        }
    }
}
