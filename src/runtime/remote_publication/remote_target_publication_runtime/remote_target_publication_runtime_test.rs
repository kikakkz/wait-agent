mod tests {
    use super::super::{
        chrome_refresh_socket_args, is_publishable_discovered_remote_session,
        live_workspace_socket_names_from_sessions, parse_publication_agent_command,
        parse_publication_sender_command, publication_socket_hook_tmux_command,
        published_remote_target_from_local, published_remote_target_record_from_payload,
        remote_target_exited_args, remote_target_publication_agent_args,
        remote_target_publication_agent_socket_path, remote_target_publication_sender_args,
        remote_target_publication_sender_socket_path, remote_target_publication_server_args,
        render_publication_agent_command, render_publication_sender_command,
        socket_lifecycle_publication_action, PublicationAgentCommand, PublicationSenderCommand,
        SocketLifecyclePublicationAction,
    };
    use crate::cli::{default_remote_node_port, RemoteNetworkConfig};
    use crate::domain::session_catalog::{
        ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState, SessionAvailability,
    };
    use crate::domain::workspace::WorkspaceSessionRole;
    use crate::infra::remote_protocol::TargetPublishedPayload;
    use crate::infra::tmux::RemoteTargetPublicationBinding;
    use crate::runtime::remote_target_publication_runtime::RemoteTargetPublicationRuntime;
    use crate::runtime::remote_workspace_socket_registry_runtime::{
        workspace_socket_registry_path, RemoteWorkspaceSocketRegistryRuntime,
    };
    use std::path::PathBuf;

    #[test]
    fn publication_record_uses_remote_identity_and_optional_selector() {
        let record = published_remote_target_record_from_payload(
            "peer-a",
            &TargetPublishedPayload {
                transport_session_id: "shell-1".to_string(),
                node_instance_id: String::new(),
                revision: 0,
                source_session_name: Some("target-host-1".to_string()),
                selector: Some("wa-local:shell-host".to_string()),
                availability: "online",
                session_role: Some("target-host"),
                workspace_key: Some("wk-1".to_string()),
                command_name: Some("codex".to_string()),
                current_path: Some("/tmp/demo".to_string()),
                attached_clients: 2,
                window_count: 3,
                task_state: "confirm",
            },
        )
        .expect("publication payload should build a published record");

        assert_eq!(record.address.authority_id(), "peer-a");
        assert_eq!(record.address.session_id(), "shell-1");
        assert_eq!(record.selector.as_deref(), Some("wa-local:shell-host"));
        assert_eq!(record.availability, SessionAvailability::Online);
        assert_eq!(record.session_role, Some(WorkspaceSessionRole::TargetHost));
        assert_eq!(record.current_path, Some(PathBuf::from("/tmp/demo")));
        assert_eq!(record.task_state, ManagedSessionTaskState::Confirm);
    }

    #[test]
    fn publication_record_rejects_unknown_availability() {
        let error = published_remote_target_record_from_payload(
            "peer-a",
            &TargetPublishedPayload {
                transport_session_id: "shell-1".to_string(),
                node_instance_id: String::new(),
                revision: 0,
                source_session_name: None,
                selector: None,
                availability: "weird",
                session_role: None,
                workspace_key: None,
                command_name: None,
                current_path: None,
                attached_clients: 0,
                window_count: 1,
                task_state: "unknown",
            },
        )
        .expect_err("unknown availability should fail");

        assert!(error
            .to_string()
            .contains("unsupported remote target availability"));
    }

    #[test]
    fn publication_server_args_target_hidden_listener_command() {
        assert_eq!(
            remote_target_publication_server_args("wa-local", &RemoteNetworkConfig::default()),
            vec![
                "--port".to_string(),
                default_remote_node_port().to_string(),
                "__remote-target-publication-server".to_string(),
                "--socket-name".to_string(),
                "wa-local".to_string(),
            ]
        );
    }

    #[test]
    fn publication_agent_args_target_hidden_listener_command() {
        assert_eq!(
            remote_target_publication_agent_args("wa-local", &RemoteNetworkConfig::default()),
            vec![
                "--port".to_string(),
                default_remote_node_port().to_string(),
                "__remote-target-publication-agent".to_string(),
                "--socket-name".to_string(),
                "wa-local".to_string(),
            ]
        );
    }

    #[test]
    fn publication_agent_socket_path_is_scoped_to_socket_name() {
        let path = remote_target_publication_agent_socket_path("wa/local");

        assert!(path
            .to_string_lossy()
            .contains("waitagent-remote-publication-agent-wa_local.sock"));
    }

    #[test]
    fn publication_sender_args_target_hidden_listener_command() {
        assert_eq!(
            remote_target_publication_sender_args("wa-local", &RemoteNetworkConfig::default()),
            vec![
                "--port".to_string(),
                default_remote_node_port().to_string(),
                "__remote-target-publication-sender".to_string(),
                "--socket-name".to_string(),
                "wa-local".to_string(),
            ]
        );
    }

    #[test]
    fn publication_sender_socket_path_is_scoped_to_socket_name() {
        let path = remote_target_publication_sender_socket_path("wa/local");

        assert!(path
            .to_string_lossy()
            .contains("waitagent-remote-publication-sender-wa_local.sock"));
    }

    #[test]
    fn publication_agent_command_round_trips_publish_session() {
        let rendered = render_publication_agent_command(&PublicationAgentCommand::PublishSession {
            session_name: "waitagent-target-1".to_string(),
        });

        let parsed =
            parse_publication_agent_command(rendered.trim()).expect("command should decode");

        assert_eq!(
            parsed,
            PublicationAgentCommand::PublishSession {
                session_name: "waitagent-target-1".to_string(),
            }
        );
    }

    #[test]
    fn publication_agent_command_round_trips_exit_target() {
        let rendered = render_publication_agent_command(&PublicationAgentCommand::ExitTarget {
            authority_id: "peer-a".to_string(),
            transport_session_id: "shell-1".to_string(),
            source_session_name: Some("target-host-1".to_string()),
        });

        let parsed =
            parse_publication_agent_command(rendered.trim()).expect("command should decode");

        assert_eq!(
            parsed,
            PublicationAgentCommand::ExitTarget {
                authority_id: "peer-a".to_string(),
                transport_session_id: "shell-1".to_string(),
                source_session_name: Some("target-host-1".to_string()),
            }
        );
    }

    #[test]
    fn publication_sender_command_round_trips_publish_target() {
        let rendered =
            render_publication_sender_command(&PublicationSenderCommand::PublishTarget {
                authority_id: "peer-a".to_string(),
                transport_session_id: "shell-1".to_string(),
                source_session_name: Some("target-host-1".to_string()),
                selector: Some("wa-local:target-host-1".to_string()),
                availability: "online",
                session_role: Some("target-host"),
                workspace_key: Some("wk-1".to_string()),
                command_name: Some("codex".to_string()),
                current_path: Some("/tmp/demo".to_string()),
                attached_clients: 2,
                window_count: 3,
                task_state: "running",
            });

        let parsed =
            parse_publication_sender_command(rendered.trim()).expect("command should decode");

        assert_eq!(
            parsed,
            PublicationSenderCommand::PublishTarget {
                authority_id: "peer-a".to_string(),
                transport_session_id: "shell-1".to_string(),
                source_session_name: Some("target-host-1".to_string()),
                selector: Some("wa-local:target-host-1".to_string()),
                availability: "online",
                session_role: Some("target-host"),
                workspace_key: Some("wk-1".to_string()),
                command_name: Some("codex".to_string()),
                current_path: Some("/tmp/demo".to_string()),
                attached_clients: 2,
                window_count: 3,
                task_state: "running",
            }
        );
    }

    #[test]
    fn publication_sender_command_round_trips_register_live_session() {
        let rendered =
            render_publication_sender_command(&PublicationSenderCommand::RegisterLiveSession {
                target_session_name: "target-host-1".to_string(),
                authority_id: "peer-a".to_string(),
                target_id: "remote-peer:peer-a:target-host-1".to_string(),
                transport_socket_path: "/tmp/waitagent-remote.sock".to_string(),
            });

        let parsed =
            parse_publication_sender_command(rendered.trim()).expect("command should decode");

        assert_eq!(
            parsed,
            PublicationSenderCommand::RegisterLiveSession {
                target_session_name: "target-host-1".to_string(),
                authority_id: "peer-a".to_string(),
                target_id: "remote-peer:peer-a:target-host-1".to_string(),
                transport_socket_path: "/tmp/waitagent-remote.sock".to_string(),
            }
        );
    }

    #[test]
    fn live_workspace_socket_names_prunes_stale_registry_entries_before_tmux_scan() {
        let network = RemoteNetworkConfig {
            port: 31988,
            connect: None,
            node_id: None,
            public_endpoint: None,
        };
        let path = workspace_socket_registry_path(&network);
        let _ = std::fs::remove_file(&path);
        let registry = RemoteWorkspaceSocketRegistryRuntime::new(network.clone());
        registry
            .register_workspace_socket("wa-registry-a")
            .expect("workspace socket should register");

        let runtime = RemoteTargetPublicationRuntime::from_build_env_with_network(network.clone())
            .expect("publication runtime should build");
        let sockets = runtime
            .live_workspace_socket_names()
            .expect("live workspace sockets should resolve");

        assert!(sockets.is_empty());
        assert!(registry
            .live_workspace_socket_names()
            .expect("workspace socket registry should read")
            .is_empty());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn live_workspace_socket_names_only_include_local_workspace_sessions() {
        let sockets = live_workspace_socket_names_from_sessions(&[
            ManagedSessionRecord {
                address: ManagedSessionAddress::local_tmux("wa-2", "workspace-2"),
                selector: Some("wa-2:workspace-2".to_string()),
                availability: SessionAvailability::Online,
                workspace_dir: Some(PathBuf::from("/tmp/workspace-2")),
                workspace_key: Some("wk-2".to_string()),
                session_role: Some(WorkspaceSessionRole::WorkspaceChrome),
                opened_by: Vec::new(),
                attached_clients: 1,
                window_count: 1,
                command_name: Some("bash".to_string()),
                current_path: None,
                task_state: ManagedSessionTaskState::Running,
            },
            ManagedSessionRecord {
                address: ManagedSessionAddress::local_tmux("wa-1", "target-1"),
                selector: Some("wa-1:target-1".to_string()),
                availability: SessionAvailability::Online,
                workspace_dir: Some(PathBuf::from("/tmp/target-1")),
                workspace_key: Some("wk-1".to_string()),
                session_role: Some(WorkspaceSessionRole::TargetHost),
                opened_by: Vec::new(),
                attached_clients: 0,
                window_count: 1,
                command_name: Some("bash".to_string()),
                current_path: None,
                task_state: ManagedSessionTaskState::Input,
            },
            ManagedSessionRecord {
                address: ManagedSessionAddress::remote_peer("10.1.1.8", "pty1"),
                selector: Some("wa-remote:pty1".to_string()),
                availability: SessionAvailability::Online,
                workspace_dir: None,
                workspace_key: Some("wk-r".to_string()),
                session_role: Some(WorkspaceSessionRole::WorkspaceChrome),
                opened_by: Vec::new(),
                attached_clients: 1,
                window_count: 1,
                command_name: Some("bash".to_string()),
                current_path: None,
                task_state: ManagedSessionTaskState::Running,
            },
            ManagedSessionRecord {
                address: ManagedSessionAddress::local_tmux("wa-1", "workspace-1"),
                selector: Some("wa-1:workspace-1".to_string()),
                availability: SessionAvailability::Online,
                workspace_dir: Some(PathBuf::from("/tmp/workspace-1")),
                workspace_key: Some("wk-1".to_string()),
                session_role: Some(WorkspaceSessionRole::WorkspaceChrome),
                opened_by: Vec::new(),
                attached_clients: 1,
                window_count: 1,
                command_name: Some("bash".to_string()),
                current_path: None,
                task_state: ManagedSessionTaskState::Running,
            },
            ManagedSessionRecord {
                address: ManagedSessionAddress::local_tmux("wa-1", "workspace-1b"),
                selector: Some("wa-1:workspace-1b".to_string()),
                availability: SessionAvailability::Online,
                workspace_dir: Some(PathBuf::from("/tmp/workspace-1b")),
                workspace_key: Some("wk-1b".to_string()),
                session_role: Some(WorkspaceSessionRole::WorkspaceChrome),
                opened_by: Vec::new(),
                attached_clients: 0,
                window_count: 1,
                command_name: Some("bash".to_string()),
                current_path: None,
                task_state: ManagedSessionTaskState::Running,
            },
        ]);

        assert_eq!(sockets, vec!["wa-1".to_string(), "wa-2".to_string()]);
    }

    #[test]
    fn discovered_remote_session_filter_accepts_only_remote_target_hosts() {
        let remote_target = ManagedSessionRecord {
            address: ManagedSessionAddress::remote_peer("10.1.1.8", "pty1"),
            selector: Some("10.1.1.8:pty1".to_string()),
            availability: SessionAvailability::Online,
            workspace_dir: None,
            workspace_key: Some("wk-r".to_string()),
            session_role: Some(WorkspaceSessionRole::TargetHost),
            opened_by: Vec::new(),
            attached_clients: 1,
            window_count: 1,
            command_name: Some("bash".to_string()),
            current_path: None,
            task_state: ManagedSessionTaskState::Running,
        };
        let remote_workspace = ManagedSessionRecord {
            session_role: Some(WorkspaceSessionRole::WorkspaceChrome),
            ..remote_target.clone()
        };
        let local_target = ManagedSessionRecord {
            address: ManagedSessionAddress::local_tmux("wa-1", "target-1"),
            session_role: Some(WorkspaceSessionRole::TargetHost),
            ..remote_target.clone()
        };

        assert!(is_publishable_discovered_remote_session(&remote_target));
        assert!(!is_publishable_discovered_remote_session(&remote_workspace));
        assert!(!is_publishable_discovered_remote_session(&local_target));
    }

    #[test]
    fn publication_agent_command_rejects_unknown_opcode() {
        let error = parse_publication_agent_command("weird")
            .expect_err("unknown publication agent command should fail");

        assert!(error
            .to_string()
            .contains("unsupported remote publication agent command"));
    }

    #[test]
    fn chrome_refresh_socket_args_target_hidden_socket_refresh_command() {
        assert_eq!(
            chrome_refresh_socket_args("wa-local"),
            vec!["__chrome-refresh-socket", "--socket-name", "wa-local"]
        );
    }

    #[test]
    fn remote_target_exited_args_target_hidden_workspace_exit_command() {
        assert_eq!(
            remote_target_exited_args("wa-local", "workspace-1", "peer-a:shell-1"),
            vec![
                "__remote-target-exited",
                "--socket-name",
                "wa-local",
                "--session-name",
                "workspace-1",
                "--target",
                "peer-a:shell-1",
            ]
        );
    }

    #[test]
    fn publication_socket_hook_tmux_command_targets_reconcile_and_socket_refresh() {
        let command = publication_socket_hook_tmux_command(
            "/tmp/wait agent",
            "wa-local",
            &RemoteNetworkConfig::default(),
        );

        assert_eq!(
            command,
            format!(
                "run-shell -b \"'/tmp/wait agent' '--port' '{}' '__socket-lifecycle-hook' '--socket-name' 'wa-local' '--hook-name' '#{{hook}}' '--session-name' '#{{hook_session_name}}' >/dev/null 2>&1\"",
                default_remote_node_port()
            )
        );
    }

    #[test]
    fn client_lifecycle_hooks_prefer_targeted_publish() {
        assert_eq!(
            socket_lifecycle_publication_action(Some("client-attached")),
            SocketLifecyclePublicationAction::TargetedPublish
        );
        assert_eq!(
            socket_lifecycle_publication_action(Some("client-detached")),
            SocketLifecyclePublicationAction::TargetedPublish
        );
        assert_eq!(
            socket_lifecycle_publication_action(Some("session-created")),
            SocketLifecyclePublicationAction::TargetedPublish
        );
    }

    #[test]
    fn session_closed_hook_prefers_targeted_exit() {
        assert_eq!(
            socket_lifecycle_publication_action(Some("session-closed")),
            SocketLifecyclePublicationAction::TargetedExit
        );
    }

    #[test]
    fn unknown_hook_falls_back_to_full_reconcile() {
        assert_eq!(
            socket_lifecycle_publication_action(Some("weird-hook")),
            SocketLifecyclePublicationAction::FullReconcile
        );
    }

    #[test]
    fn reconcile_projects_local_target_host_as_remote_peer() {
        let published = published_remote_target_from_local(
            &RemoteTargetPublicationBinding {
                socket_name: "wa-local".to_string(),
                target_session_name: "shell-host".to_string(),
                authority_id: "peer-a".to_string(),
                transport_session_id: "shell-1".to_string(),
                selector: Some("wa-local:shell-host".to_string()),
            },
            &ManagedSessionRecord {
                address: ManagedSessionAddress::local_tmux("wa-local", "shell-host"),
                selector: Some("wa-local:shell-host".to_string()),
                availability: SessionAvailability::Online,
                workspace_dir: Some(PathBuf::from("/tmp/demo")),
                workspace_key: Some("wk-1".to_string()),
                session_role: Some(WorkspaceSessionRole::TargetHost),
                opened_by: Vec::new(),
                attached_clients: 1,
                window_count: 2,
                command_name: Some("codex".to_string()),
                current_path: Some(PathBuf::from("/tmp/demo")),
                task_state: ManagedSessionTaskState::Running,
            },
        );

        assert_eq!(published.address.authority_id(), "peer-a");
        assert_eq!(published.address.session_id(), "shell-1");
        assert_eq!(published.selector.as_deref(), Some("wa-local:shell-host"));
        assert_eq!(published.command_name.as_deref(), Some("codex"));
        assert_eq!(published.current_path, Some(PathBuf::from("/tmp/demo")));
        assert_eq!(published.workspace_dir, None);
    }
}
