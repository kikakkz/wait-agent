mod tests {
    use super::super::{
        next_remote_fallback_target, next_target_host_session, remote_main_slot_program,
        split_qualified_target, target_socket_name, CurrentWorkspace, MainSlotRuntime,
        FOOTER_PANE_TITLE, SIDEBAR_PANE_TITLE, WAITAGENT_ACTIVE_TARGET_OPTION,
        WAITAGENT_MAIN_PANE_OPTION,
    };
    use crate::application::target_registry_service::{
        DefaultTargetCatalogGateway, TargetRegistryService,
    };
    use crate::application::workspace_service::WorkspaceService;
    use crate::cli::RemoteNetworkConfig;
    use crate::cli::{ActivateTargetCommand, MainPaneDiedCommand, RemoteTargetExitedCommand};
    use crate::domain::session_catalog::{
        ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState, SessionAvailability,
    };
    use crate::domain::workspace::{
        WorkspaceInstanceConfig, WorkspaceInstanceId, WorkspaceSessionRole,
    };
    use crate::infra::tmux::{
        EmbeddedTmuxBackend, TmuxGateway, TmuxLayoutGateway, TmuxSessionName, TmuxSocketName,
        TmuxWorkspaceHandle,
    };
    use crate::runtime::remote_runtime_owner_runtime::RemoteRuntimeOwnerRuntime;
    use crate::runtime::target_host_runtime::TargetHostRuntime;
    use crate::runtime::workspace_entry_runtime::WorkspaceEntryRuntime;
    use crate::runtime::workspace_layout_runtime::WorkspaceLayoutRuntime;
    use crate::runtime::workspace_runtime::WorkspaceRuntime;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::thread;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    #[test]
    fn next_target_host_session_prefers_another_target_on_same_socket() {
        let sessions = vec![
            session("wa-1", "workspace", WorkspaceSessionRole::WorkspaceChrome),
            session("wa-1", "target-a", WorkspaceSessionRole::TargetHost),
            session("wa-1", "target-b", WorkspaceSessionRole::TargetHost),
            session("wa-2", "target-c", WorkspaceSessionRole::TargetHost),
        ];

        let next = next_target_host_session(&sessions, "wa-1", Some("wa-1:target-a"))
            .expect("fallback target should exist");

        assert_eq!(next.address.qualified_target(), "wa-1:target-b");
    }

    #[test]
    fn next_target_host_session_returns_none_without_same_socket_target_hosts() {
        let sessions = vec![session(
            "wa-1",
            "workspace",
            WorkspaceSessionRole::WorkspaceChrome,
        )];

        assert!(next_target_host_session(&sessions, "wa-1", Some("wa-1:target-a")).is_none());
    }

    #[test]
    fn next_target_host_session_ignores_remote_targets_when_local_target_host_exits() {
        let sessions = vec![
            session("wa-1", "workspace", WorkspaceSessionRole::WorkspaceChrome),
            remote_session("192.168.31.18", "pty1"),
        ];

        assert!(next_target_host_session(&sessions, "wa-1", Some("wa-1:target-a")).is_none());
    }

    #[test]
    fn next_target_host_session_returns_none_when_only_active_target_remains() {
        let sessions = vec![
            session("wa-1", "workspace", WorkspaceSessionRole::WorkspaceChrome),
            session("wa-1", "target-a", WorkspaceSessionRole::TargetHost),
        ];

        assert!(next_target_host_session(&sessions, "wa-1", Some("wa-1:target-a")).is_none());
    }

    #[test]
    fn next_remote_fallback_target_prefers_another_remote_target() {
        let sessions = vec![
            session("wa-1", "workspace", WorkspaceSessionRole::WorkspaceChrome),
            session("wa-1", "target-a", WorkspaceSessionRole::TargetHost),
            remote_session("10.1.29.130#7474", "remote-a"),
            remote_session("10.1.29.130#7474", "remote-b"),
        ];

        let next = next_remote_fallback_target(&sessions, Some("10.1.29.130#7474:remote-a"))
            .expect("remote fallback target should exist");

        assert_eq!(next.address.qualified_target(), "10.1.29.130#7474:remote-b");
    }

    #[test]
    fn next_remote_fallback_target_returns_none_without_another_remote_target() {
        let sessions = vec![
            session("wa-1", "workspace", WorkspaceSessionRole::WorkspaceChrome),
            session("wa-1", "target-a", WorkspaceSessionRole::TargetHost),
            remote_session("10.1.29.130#7474", "remote-a"),
        ];

        assert!(
            next_remote_fallback_target(&sessions, Some("10.1.29.130#7474:remote-a")).is_none()
        );
    }

    #[test]
    fn next_target_host_session_returns_first_target_without_active_target() {
        let sessions = vec![
            session("wa-1", "workspace", WorkspaceSessionRole::WorkspaceChrome),
            session("wa-1", "target-a", WorkspaceSessionRole::TargetHost),
            session("wa-1", "target-b", WorkspaceSessionRole::TargetHost),
        ];

        let next =
            next_target_host_session(&sessions, "wa-1", None).expect("a target should exist");

        assert_eq!(next.address.qualified_target(), "wa-1:target-a");
    }

    #[test]
    fn split_qualified_target_parses_socket_and_session_name() {
        assert_eq!(
            split_qualified_target("wa-1:target-a"),
            Some(("wa-1", "target-a"))
        );
        assert_eq!(target_socket_name("wa-1:target-a"), Some("wa-1"));
    }

    #[test]
    fn split_qualified_target_rejects_missing_parts() {
        assert_eq!(split_qualified_target("wa-1"), None);
        assert_eq!(split_qualified_target("wa-1:"), None);
        assert_eq!(split_qualified_target(":target-a"), None);
    }

    #[test]
    fn remote_main_slot_program_targets_workspace_and_remote_target() {
        let workspace = CurrentWorkspace {
            socket_name: "wa-1".to_string(),
            session_name: "workspace-1".to_string(),
            workspace_dir: PathBuf::from("/tmp/demo"),
        };

        let target = remote_session_with_selector(
            "peer-a",
            "shell-1",
            "remote-peer:peer-a:shell-1",
            ManagedSessionTaskState::Running,
        );
        let program = remote_main_slot_program(
            std::path::Path::new("/tmp/waitagent"),
            &workspace,
            &target,
            &RemoteNetworkConfig::default(),
        );

        assert_eq!(program.program, "/tmp/waitagent");
        assert_eq!(
            program.args,
            vec![
                "--port".to_string(),
                "7474".to_string(),
                "__remote-main-slot".to_string(),
                "--socket-name".to_string(),
                "wa-1".to_string(),
                "--session-name".to_string(),
                "workspace-1".to_string(),
                "--target".to_string(),
                "peer-a:shell-1".to_string(),
            ]
        );
        assert_eq!(program.start_directory, Some(PathBuf::from("/tmp/demo")));
    }

    #[test]
    fn activating_remote_target_respawns_workspace_main_pane_not_detached_target_host() {
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let workspace_config = unique_workspace_config("remote-main-slot");
        let workspace_dir = workspace_config.workspace_dir.clone();
        let waitagent_executable = waitagent_test_executable();
        let entry_runtime = WorkspaceEntryRuntime::new(
            WorkspaceRuntime::new(WorkspaceService::new(backend.clone())),
            WorkspaceLayoutRuntime::new_for_tests(
                backend.clone(),
                waitagent_executable.clone(),
                RemoteNetworkConfig::default(),
            )
            .expect("workspace layout runtime should build"),
        );
        let workspace = entry_runtime
            .bootstrap_workspace(&workspace_dir)
            .expect("workspace bootstrap should succeed");
        let target_host = backend
            .ensure_workspace(
                &WorkspaceInstanceConfig::for_new_target_on_socket_with_size(
                    &workspace_dir,
                    workspace.workspace_handle.socket_name.as_str(),
                    None,
                    None,
                ),
            )
            .expect("target host bootstrap should succeed");

        let runtime = MainSlotRuntime::new(
            backend.clone(),
            TargetHostRuntime::from_build_env(backend.clone())
                .expect("target host runtime should build"),
            WorkspaceLayoutRuntime::new_for_tests(
                backend.clone(),
                waitagent_executable.clone(),
                RemoteNetworkConfig::default(),
            )
            .expect("workspace layout runtime should build"),
            TargetRegistryService::new(
                DefaultTargetCatalogGateway::from_build_env_with_socket_name(
                    workspace.workspace_handle.socket_name.as_str(),
                )
                .expect("target catalog gateway should build"),
            ),
            waitagent_executable.clone(),
            RemoteNetworkConfig::default(),
        );

        let local_target = format!(
            "{}:{}",
            workspace.workspace_handle.socket_name.as_str(),
            target_host.session_name.as_str()
        );
        runtime
            .run_activate_target(ActivateTargetCommand {
                current_socket_name: workspace.workspace_handle.socket_name.as_str().to_string(),
                current_session_name: workspace.workspace_handle.session_name.as_str().to_string(),
                target: local_target.clone(),
            })
            .expect("local target activation should succeed");

        let remote_runtime_owner = RemoteRuntimeOwnerRuntime::new_for_tests(
            waitagent_executable.clone(),
            RemoteNetworkConfig::default(),
        );
        let remote_target = remote_session_with_selector(
            "peer-a",
            "remote-1",
            &local_target,
            ManagedSessionTaskState::Input,
        );
        remote_runtime_owner
            .upsert_session(
                workspace.workspace_handle.socket_name.as_str(),
                "peer-a",
                &remote_target,
            )
            .expect("remote target should be discoverable on workspace socket");

        runtime
            .run_activate_target(ActivateTargetCommand {
                current_socket_name: workspace.workspace_handle.socket_name.as_str().to_string(),
                current_session_name: workspace.workspace_handle.session_name.as_str().to_string(),
                target: remote_target.address.qualified_target(),
            })
            .expect("remote target activation should succeed");

        wait_for_condition(|| {
            let active_target = backend
                .show_session_option(&workspace.workspace_handle, WAITAGENT_ACTIVE_TARGET_OPTION)
                .expect("active target should read");
            active_target.as_deref() == Some(remote_target.address.qualified_target().as_str())
        });

        wait_for_condition(|| {
            workspace_main_pane_command(&backend, &workspace.workspace_handle).as_deref()
                == Some("waitagent")
        });
        wait_for_condition(|| {
            workspace_main_pane_pipe(&backend, &workspace.workspace_handle).as_deref() == Some("0")
        });

        let target_host_handle = TmuxWorkspaceHandle {
            workspace_id: WorkspaceInstanceId::new(target_host.session_name.as_str().to_string()),
            socket_name: TmuxSocketName::new(
                workspace.workspace_handle.socket_name.as_str().to_string(),
            ),
            session_name: TmuxSessionName::new(target_host.session_name.as_str().to_string()),
        };
        let target_host_command =
            workspace_main_pane_command(&backend, &target_host_handle).expect("target host pane");
        kill_server(&backend, &workspace.workspace_handle);
        let _ = fs::remove_dir_all(workspace_dir);

        assert_eq!(target_host_command, "bash");
    }

    #[test]
    fn switching_from_remote_back_to_local_target_restores_local_main_pane() {
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let workspace_config = unique_workspace_config("remote-to-local-switch");
        let workspace_dir = workspace_config.workspace_dir.clone();
        let waitagent_executable = waitagent_test_executable();
        let entry_runtime = WorkspaceEntryRuntime::new(
            WorkspaceRuntime::new(WorkspaceService::new(backend.clone())),
            WorkspaceLayoutRuntime::new_for_tests(
                backend.clone(),
                waitagent_executable.clone(),
                RemoteNetworkConfig::default(),
            )
            .expect("workspace layout runtime should build"),
        );
        let workspace = entry_runtime
            .bootstrap_workspace(&workspace_dir)
            .expect("workspace bootstrap should succeed");
        let target_host = backend
            .ensure_workspace(
                &WorkspaceInstanceConfig::for_new_target_on_socket_with_size(
                    &workspace_dir,
                    workspace.workspace_handle.socket_name.as_str(),
                    None,
                    None,
                ),
            )
            .expect("target host bootstrap should succeed");

        let runtime = MainSlotRuntime::new(
            backend.clone(),
            TargetHostRuntime::from_build_env(backend.clone())
                .expect("target host runtime should build"),
            WorkspaceLayoutRuntime::new_for_tests(
                backend.clone(),
                waitagent_executable.clone(),
                RemoteNetworkConfig::default(),
            )
            .expect("workspace layout runtime should build"),
            TargetRegistryService::new(
                DefaultTargetCatalogGateway::from_build_env_with_socket_name(
                    workspace.workspace_handle.socket_name.as_str(),
                )
                .expect("target catalog gateway should build"),
            ),
            waitagent_executable.clone(),
            RemoteNetworkConfig::default(),
        );

        let local_target = format!(
            "{}:{}",
            workspace.workspace_handle.socket_name.as_str(),
            target_host.session_name.as_str()
        );
        runtime
            .run_activate_target(ActivateTargetCommand {
                current_socket_name: workspace.workspace_handle.socket_name.as_str().to_string(),
                current_session_name: workspace.workspace_handle.session_name.as_str().to_string(),
                target: local_target.clone(),
            })
            .expect("local target activation should succeed");

        let remote_runtime_owner = RemoteRuntimeOwnerRuntime::new_for_tests(
            waitagent_executable.clone(),
            RemoteNetworkConfig::default(),
        );
        let remote_target = remote_session_with_selector(
            "peer-a",
            "remote-switch-1",
            &local_target,
            ManagedSessionTaskState::Input,
        );
        remote_runtime_owner
            .upsert_session(
                workspace.workspace_handle.socket_name.as_str(),
                "peer-a",
                &remote_target,
            )
            .expect("remote target should be discoverable on workspace socket");

        runtime
            .run_activate_target(ActivateTargetCommand {
                current_socket_name: workspace.workspace_handle.socket_name.as_str().to_string(),
                current_session_name: workspace.workspace_handle.session_name.as_str().to_string(),
                target: remote_target.address.qualified_target(),
            })
            .expect("remote target activation should succeed");

        wait_for_condition(|| {
            let active_target = backend
                .show_session_option(&workspace.workspace_handle, WAITAGENT_ACTIVE_TARGET_OPTION)
                .expect("active target should read");
            active_target.as_deref() == Some(remote_target.address.qualified_target().as_str())
        });

        // Now switch back to the local target host
        runtime
            .run_activate_target(ActivateTargetCommand {
                current_socket_name: workspace.workspace_handle.socket_name.as_str().to_string(),
                current_session_name: workspace.workspace_handle.session_name.as_str().to_string(),
                target: local_target.clone(),
            })
            .expect("local target re-activation should succeed");

        wait_for_condition(|| {
            let active_target = backend
                .show_session_option(&workspace.workspace_handle, WAITAGENT_ACTIVE_TARGET_OPTION)
                .expect("active target should read");
            active_target.as_deref() == Some(local_target.as_str())
        });

        // After switching back, verify the @waitagent_main_pane_id is actually set
        // AND that the configured main pane runs bash (not waitagent)
        wait_for_condition(|| {
            let Ok(main_pane_id) = backend
                .show_session_option(&workspace.workspace_handle, WAITAGENT_MAIN_PANE_OPTION)
            else {
                return false;
            };
            let Some(main_pane_id) = main_pane_id.filter(|id| !id.is_empty()) else {
                return false;
            };
            let Ok(window) = backend.current_window(&workspace.workspace_handle) else {
                return false;
            };
            let Ok(panes) = backend.list_panes(&workspace.workspace_handle, &window) else {
                return false;
            };
            let Some(main_pane) = panes.iter().find(|p| p.pane_id.as_str() == main_pane_id) else {
                return false;
            };
            main_pane.current_command.as_deref() == Some("bash")
        });

        // Main pane pipe should be disabled after switching back to local
        assert!(
            workspace_main_pane_pipe(&backend, &workspace.workspace_handle).as_deref() == Some("0"),
            "main pane should have pipe disabled after switching back to local target"
        );

        kill_server(&backend, &workspace.workspace_handle);
        let _ = fs::remove_dir_all(workspace_dir);
    }

    #[test]
    fn local_main_pane_exit_recovers_workspace_without_closing_last_workspace_session() {
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let workspace_config = unique_workspace_config("local-main-pane-exit");
        let workspace_dir = workspace_config.workspace_dir.clone();
        let waitagent_executable = waitagent_test_executable();
        let entry_runtime = WorkspaceEntryRuntime::new(
            WorkspaceRuntime::new(WorkspaceService::new(backend.clone())),
            WorkspaceLayoutRuntime::new_for_tests(
                backend.clone(),
                waitagent_executable.clone(),
                RemoteNetworkConfig::default(),
            )
            .expect("workspace layout runtime should build"),
        );
        let workspace = entry_runtime
            .bootstrap_workspace(&workspace_dir)
            .expect("workspace bootstrap should succeed");
        let target_host = backend
            .ensure_workspace(
                &WorkspaceInstanceConfig::for_new_target_on_socket_with_size(
                    &workspace_dir,
                    workspace.workspace_handle.socket_name.as_str(),
                    None,
                    None,
                ),
            )
            .expect("target host bootstrap should succeed");

        let runtime = MainSlotRuntime::new(
            backend.clone(),
            TargetHostRuntime::from_build_env(backend.clone())
                .expect("target host runtime should build"),
            WorkspaceLayoutRuntime::new_for_tests(
                backend.clone(),
                waitagent_executable.clone(),
                RemoteNetworkConfig::default(),
            )
            .expect("workspace layout runtime should build"),
            TargetRegistryService::new(
                DefaultTargetCatalogGateway::from_build_env_with_socket_name(
                    workspace.workspace_handle.socket_name.as_str(),
                )
                .expect("target catalog gateway should build"),
            ),
            waitagent_executable.clone(),
            RemoteNetworkConfig::default(),
        );

        let local_target = format!(
            "{}:{}",
            workspace.workspace_handle.socket_name.as_str(),
            target_host.session_name.as_str()
        );
        runtime
            .run_activate_target(ActivateTargetCommand {
                current_socket_name: workspace.workspace_handle.socket_name.as_str().to_string(),
                current_session_name: workspace.workspace_handle.session_name.as_str().to_string(),
                target: local_target.clone(),
            })
            .expect("local target activation should succeed");

        let main_pane_id = backend
            .show_session_option(&workspace.workspace_handle, WAITAGENT_MAIN_PANE_OPTION)
            .expect("main pane option should read")
            .expect("main pane option should be populated");

        let _ = backend.run_socket_command(
            &TmuxSocketName::new(workspace.workspace_handle.socket_name.as_str().to_string()),
            &[
                "kill-session".to_string(),
                "-t".to_string(),
                target_host.session_name.as_str().to_string(),
            ],
        );

        runtime
            .run_main_pane_died(MainPaneDiedCommand {
                socket_name: workspace.workspace_handle.socket_name.as_str().to_string(),
                session_name: workspace.workspace_handle.session_name.as_str().to_string(),
                pane_id: main_pane_id,
            })
            .expect("local main pane exit should recover workspace");

        wait_for_condition(|| {
            backend
                .show_session_option(&workspace.workspace_handle, WAITAGENT_MAIN_PANE_OPTION)
                .ok()
                .flatten()
                .and_then(|pane_id| {
                    pane_hook_command(&backend, &workspace.workspace_handle, &pane_id, "pane-died")
                })
                .is_some()
        });

        let active_target = backend
            .show_session_option(&workspace.workspace_handle, WAITAGENT_ACTIVE_TARGET_OPTION)
            .expect("active target should read after recovery");
        assert!(active_target.is_none() || active_target.as_deref() == Some(""));

        let recovered_main_pane = backend
            .show_session_option(&workspace.workspace_handle, WAITAGENT_MAIN_PANE_OPTION)
            .expect("main pane option should read after local recovery")
            .expect("main pane option should remain populated after local recovery");
        assert_eq!(
            pane_option(
                &backend,
                &workspace.workspace_handle,
                &recovered_main_pane,
                "remain-on-exit",
            )
            .as_deref(),
            Some("on")
        );
        let pane_died_hook = pane_hook_command(
            &backend,
            &workspace.workspace_handle,
            &recovered_main_pane,
            "pane-died",
        )
        .expect("local recovery should restore pane-died hook");
        assert!(pane_died_hook.contains("__main-pane-died"));

        let workspace_still_exists = backend.current_window(&workspace.workspace_handle).is_ok();
        assert!(
            workspace_still_exists,
            "workspace session should remain alive"
        );

        let target_handle = TmuxWorkspaceHandle {
            workspace_id: WorkspaceInstanceId::new(target_host.session_name.as_str().to_string()),
            socket_name: TmuxSocketName::new(
                workspace.workspace_handle.socket_name.as_str().to_string(),
            ),
            session_name: TmuxSessionName::new(target_host.session_name.as_str().to_string()),
        };
        let target_exists = backend.current_window(&target_handle).is_ok();
        assert!(!target_exists, "exited target session should be gone");

        kill_server(&backend, &workspace.workspace_handle);
        let _ = fs::remove_dir_all(workspace_dir);
    }

    #[test]
    fn remote_main_pane_exit_prefers_another_remote_target_before_local_target_host() {
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let workspace_config = unique_workspace_config("remote-main-slot-exit");
        let workspace_dir = workspace_config.workspace_dir.clone();
        let waitagent_executable = waitagent_test_executable();
        let entry_runtime = WorkspaceEntryRuntime::new(
            WorkspaceRuntime::new(WorkspaceService::new(backend.clone())),
            WorkspaceLayoutRuntime::new_for_tests(
                backend.clone(),
                waitagent_executable.clone(),
                RemoteNetworkConfig::default(),
            )
            .expect("workspace layout runtime should build"),
        );
        let workspace = entry_runtime
            .bootstrap_workspace(&workspace_dir)
            .expect("workspace bootstrap should succeed");
        let target_host = backend
            .ensure_workspace(
                &WorkspaceInstanceConfig::for_new_target_on_socket_with_size(
                    &workspace_dir,
                    workspace.workspace_handle.socket_name.as_str(),
                    None,
                    None,
                ),
            )
            .expect("target host bootstrap should succeed");

        let runtime = MainSlotRuntime::new(
            backend.clone(),
            TargetHostRuntime::from_build_env(backend.clone())
                .expect("target host runtime should build"),
            WorkspaceLayoutRuntime::new_for_tests(
                backend.clone(),
                waitagent_executable.clone(),
                RemoteNetworkConfig::default(),
            )
            .expect("workspace layout runtime should build"),
            TargetRegistryService::new(
                DefaultTargetCatalogGateway::from_build_env_with_socket_name(
                    workspace.workspace_handle.socket_name.as_str(),
                )
                .expect("target catalog gateway should build"),
            ),
            waitagent_executable.clone(),
            RemoteNetworkConfig::default(),
        );

        let local_target = format!(
            "{}:{}",
            workspace.workspace_handle.socket_name.as_str(),
            target_host.session_name.as_str()
        );
        runtime
            .run_activate_target(ActivateTargetCommand {
                current_socket_name: workspace.workspace_handle.socket_name.as_str().to_string(),
                current_session_name: workspace.workspace_handle.session_name.as_str().to_string(),
                target: local_target.clone(),
            })
            .expect("local target activation should succeed");

        let remote_runtime_owner = RemoteRuntimeOwnerRuntime::new_for_tests(
            waitagent_executable.clone(),
            RemoteNetworkConfig::default(),
        );
        let remote_target = remote_session_with_selector(
            "10.1.29.130#7474",
            "remote-exit-1",
            &local_target,
            ManagedSessionTaskState::Input,
        );
        let remote_target_2 = remote_session_with_selector(
            "10.1.29.130#7474",
            "remote-exit-2",
            &local_target,
            ManagedSessionTaskState::Running,
        );
        remote_runtime_owner
            .upsert_session(
                workspace.workspace_handle.socket_name.as_str(),
                "10.1.29.130#7474",
                &remote_target,
            )
            .expect("first remote target should be discoverable on workspace socket");
        remote_runtime_owner
            .upsert_session(
                workspace.workspace_handle.socket_name.as_str(),
                "10.1.29.130#7474",
                &remote_target_2,
            )
            .expect("second remote target should be discoverable on workspace socket");

        runtime
            .run_activate_target(ActivateTargetCommand {
                current_socket_name: workspace.workspace_handle.socket_name.as_str().to_string(),
                current_session_name: workspace.workspace_handle.session_name.as_str().to_string(),
                target: remote_target.address.qualified_target(),
            })
            .expect("remote target activation should succeed");

        wait_for_condition(|| {
            let active_target = backend
                .show_session_option(&workspace.workspace_handle, WAITAGENT_ACTIVE_TARGET_OPTION)
                .expect("active target should read");
            active_target.as_deref() == Some(remote_target.address.qualified_target().as_str())
        });
        wait_for_condition(|| {
            workspace_main_pane_command(&backend, &workspace.workspace_handle).as_deref()
                == Some("waitagent")
        });

        let main_pane_id = backend
            .show_session_option(&workspace.workspace_handle, WAITAGENT_MAIN_PANE_OPTION)
            .expect("main pane option should read")
            .expect("main pane option should be populated");
        runtime
            .run_remote_target_exited(RemoteTargetExitedCommand {
                socket_name: workspace.workspace_handle.socket_name.as_str().to_string(),
                session_name: workspace.workspace_handle.session_name.as_str().to_string(),
                target: remote_target.address.qualified_target(),
                pane_id: Some(main_pane_id),
            })
            .expect("remote target exit should recover to another remote target");

        wait_for_condition(|| {
            let active_target = backend
                .show_session_option(&workspace.workspace_handle, WAITAGENT_ACTIVE_TARGET_OPTION)
                .expect("active target should read");
            active_target.as_deref() == Some(remote_target_2.address.qualified_target().as_str())
        });
        let snapshot = remote_runtime_owner
            .snapshot(workspace.workspace_handle.socket_name.as_str())
            .expect("remote runtime owner snapshot should succeed");
        let remote_sessions = snapshot
            .sessions
            .iter()
            .filter(|session| {
                session.address.transport()
                    == &crate::domain::session_catalog::SessionTransport::RemotePeer
            })
            .map(|session| session.address.qualified_target())
            .collect::<Vec<_>>();
        assert_eq!(remote_sessions.len(), 1);
        assert_eq!(
            remote_sessions[0],
            remote_target_2.address.qualified_target()
        );
        wait_for_condition(|| {
            workspace_main_pane_command(&backend, &workspace.workspace_handle).as_deref()
                == Some("waitagent")
        });
        let recovered_main_pane = backend
            .show_session_option(&workspace.workspace_handle, WAITAGENT_MAIN_PANE_OPTION)
            .expect("main pane option should read after recovery")
            .expect("main pane option should remain populated after recovery");
        let pane_died_hook = pane_hook_command(
            &backend,
            &workspace.workspace_handle,
            &recovered_main_pane,
            "pane-died",
        )
        .expect("recovered main pane should restore pane-died hook");
        assert!(pane_died_hook.contains("__main-pane-died"));
        assert_eq!(
            pane_option(
                &backend,
                &workspace.workspace_handle,
                &recovered_main_pane,
                "remain-on-exit",
            )
            .as_deref(),
            Some("on")
        );

        kill_server(&backend, &workspace.workspace_handle);
        let _ = fs::remove_dir_all(workspace_dir);
    }

    #[test]
    fn remote_main_pane_exit_recovery_ignores_corrupted_main_pane_option() {
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let workspace_config = unique_workspace_config("remote-main-slot-dead-pane");
        let workspace_dir = workspace_config.workspace_dir.clone();
        let waitagent_executable = waitagent_test_executable();
        let entry_runtime = WorkspaceEntryRuntime::new(
            WorkspaceRuntime::new(WorkspaceService::new(backend.clone())),
            WorkspaceLayoutRuntime::new_for_tests(
                backend.clone(),
                waitagent_executable.clone(),
                RemoteNetworkConfig::default(),
            )
            .expect("workspace layout runtime should build"),
        );
        let workspace = entry_runtime
            .bootstrap_workspace(&workspace_dir)
            .expect("workspace bootstrap should succeed");
        let target_host = backend
            .ensure_workspace(
                &WorkspaceInstanceConfig::for_new_target_on_socket_with_size(
                    &workspace_dir,
                    workspace.workspace_handle.socket_name.as_str(),
                    None,
                    None,
                ),
            )
            .expect("target host bootstrap should succeed");

        let runtime = MainSlotRuntime::new(
            backend.clone(),
            TargetHostRuntime::from_build_env(backend.clone())
                .expect("target host runtime should build"),
            WorkspaceLayoutRuntime::new_for_tests(
                backend.clone(),
                waitagent_executable.clone(),
                RemoteNetworkConfig::default(),
            )
            .expect("workspace layout runtime should build"),
            TargetRegistryService::new(
                DefaultTargetCatalogGateway::from_build_env_with_socket_name(
                    workspace.workspace_handle.socket_name.as_str(),
                )
                .expect("target catalog gateway should build"),
            ),
            waitagent_executable.clone(),
            RemoteNetworkConfig::default(),
        );

        let local_target = format!(
            "{}:{}",
            workspace.workspace_handle.socket_name.as_str(),
            target_host.session_name.as_str()
        );
        runtime
            .run_activate_target(ActivateTargetCommand {
                current_socket_name: workspace.workspace_handle.socket_name.as_str().to_string(),
                current_session_name: workspace.workspace_handle.session_name.as_str().to_string(),
                target: local_target.clone(),
            })
            .expect("local target activation should succeed");

        let remote_runtime_owner = RemoteRuntimeOwnerRuntime::new_for_tests(
            waitagent_executable.clone(),
            RemoteNetworkConfig::default(),
        );
        let remote_target = remote_session_with_selector(
            "peer-a",
            "remote-exit-dead-pane-1",
            &local_target,
            ManagedSessionTaskState::Input,
        );
        remote_runtime_owner
            .upsert_session(
                workspace.workspace_handle.socket_name.as_str(),
                "peer-a",
                &remote_target,
            )
            .expect("remote target should be discoverable on workspace socket");

        runtime
            .run_activate_target(ActivateTargetCommand {
                current_socket_name: workspace.workspace_handle.socket_name.as_str().to_string(),
                current_session_name: workspace.workspace_handle.session_name.as_str().to_string(),
                target: remote_target.address.qualified_target(),
            })
            .expect("remote target activation should succeed");

        wait_for_condition(|| {
            let active_target = backend
                .show_session_option(&workspace.workspace_handle, WAITAGENT_ACTIVE_TARGET_OPTION)
                .expect("active target should read");
            active_target.as_deref() == Some(remote_target.address.qualified_target().as_str())
        });
        wait_for_condition(|| {
            workspace_main_pane_command(&backend, &workspace.workspace_handle).as_deref()
                == Some("waitagent")
        });

        let recovery_pane_id = backend
            .show_session_option(&workspace.workspace_handle, WAITAGENT_MAIN_PANE_OPTION)
            .expect("main pane option should read")
            .expect("main pane option should be populated");
        let detached_target_handle = TmuxWorkspaceHandle {
            workspace_id: WorkspaceInstanceId::new(target_host.session_name.as_str().to_string()),
            socket_name: TmuxSocketName::new(
                workspace.workspace_handle.socket_name.as_str().to_string(),
            ),
            session_name: TmuxSessionName::new(target_host.session_name.as_str().to_string()),
        };
        let detached_target_pane = backend
            .list_panes(
                &detached_target_handle,
                &backend
                    .current_window(&detached_target_handle)
                    .expect("target host window should exist"),
            )
            .expect("target host panes should list")
            .into_iter()
            .find(|pane| !pane.is_dead)
            .expect("target host pane should remain live")
            .pane_id
            .as_str()
            .to_string();

        backend
            .set_session_option(
                &workspace.workspace_handle,
                WAITAGENT_MAIN_PANE_OPTION,
                &detached_target_pane,
            )
            .expect("main pane option should be corrupted to another live pane");

        runtime
            .fallback_after_remote_main_pane_exit(
                &CurrentWorkspace {
                    socket_name: workspace.workspace_handle.socket_name.as_str().to_string(),
                    session_name: workspace.workspace_handle.session_name.as_str().to_string(),
                    workspace_dir: workspace_dir.clone(),
                },
                &workspace.workspace_handle,
                &crate::infra::tmux::TmuxPaneId::new(recovery_pane_id.clone()),
                Some(remote_target.address.qualified_target()),
            )
            .expect("remote main pane fallback should honor the explicit recovery pane");

        wait_for_condition(|| {
            let active_target = backend
                .show_session_option(&workspace.workspace_handle, WAITAGENT_ACTIVE_TARGET_OPTION)
                .expect("active target should read");
            active_target.as_deref() == Some(remote_target.address.qualified_target().as_str())
        });
        wait_for_condition(|| {
            workspace_main_pane_command(&backend, &workspace.workspace_handle).as_deref()
                == Some("waitagent")
        });

        let recovered_main_pane = backend
            .show_session_option(&workspace.workspace_handle, WAITAGENT_MAIN_PANE_OPTION)
            .expect("main pane option should read after recovery")
            .expect("main pane option should remain populated");
        assert!(!recovered_main_pane.is_empty());

        kill_server(&backend, &workspace.workspace_handle);
        let _ = fs::remove_dir_all(workspace_dir);
    }

    fn session(socket: &str, session: &str, role: WorkspaceSessionRole) -> ManagedSessionRecord {
        ManagedSessionRecord {
            address: ManagedSessionAddress::local_tmux(socket, session),
            selector: Some(format!("{socket}:{session}")),
            availability: crate::domain::session_catalog::SessionAvailability::Online,
            workspace_dir: Some(PathBuf::from("/tmp/demo")),
            workspace_key: None,
            session_role: Some(role),
            opened_by: Vec::new(),
            attached_clients: 1,
            window_count: 1,
            command_name: Some("bash".to_string()),
            current_path: Some(PathBuf::from("/tmp/demo")),
            task_state: ManagedSessionTaskState::Input,
        }
    }

    fn remote_session(authority_id: &str, session_id: &str) -> ManagedSessionRecord {
        remote_session_with_selector(
            authority_id,
            session_id,
            &format!("{authority_id}:{session_id}"),
            ManagedSessionTaskState::Running,
        )
    }

    fn remote_session_with_selector(
        authority_id: &str,
        session_id: &str,
        selector: &str,
        task_state: ManagedSessionTaskState,
    ) -> ManagedSessionRecord {
        ManagedSessionRecord {
            address: ManagedSessionAddress::remote_peer(authority_id, session_id),
            selector: Some(selector.to_string()),
            availability: SessionAvailability::Online,
            workspace_dir: None,
            workspace_key: None,
            session_role: Some(WorkspaceSessionRole::TargetHost),
            opened_by: Vec::new(),
            attached_clients: 1,
            window_count: 1,
            command_name: Some("bash".to_string()),
            current_path: None,
            task_state,
        }
    }

    fn unique_workspace_config(prefix: &str) -> WorkspaceInstanceConfig {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after unix epoch")
            .as_nanos();
        let workspace_dir = std::env::temp_dir().join(format!("waitagent-{prefix}-{nonce:x}"));
        fs::create_dir_all(&workspace_dir)
            .expect("temporary workspace directory should be created");
        WorkspaceInstanceConfig {
            workspace_dir,
            workspace_key: format!("{prefix}-{nonce:x}"),
            socket_name: format!("wa-test-{nonce:x}"),
            session_name: format!("waitagent-test-{prefix}-{nonce:x}"),
            session_role: WorkspaceSessionRole::WorkspaceChrome,
            initial_rows: None,
            initial_cols: None,
        }
    }

    fn waitagent_test_executable() -> PathBuf {
        let current_exe = std::env::current_exe().expect("current test executable should exist");
        let executable = current_exe
            .parent()
            .and_then(Path::parent)
            .expect("test executable should live under target/debug/deps")
            .join(format!("waitagent{}", std::env::consts::EXE_SUFFIX));
        assert!(
            executable.exists(),
            "waitagent test executable should exist at {}",
            executable.display()
        );
        executable
    }

    fn workspace_main_pane_command(
        backend: &EmbeddedTmuxBackend,
        workspace: &TmuxWorkspaceHandle,
    ) -> Option<String> {
        let window = backend.current_window(workspace).ok()?;
        let panes = backend.list_panes(workspace, &window).ok()?;
        panes
            .into_iter()
            .find(|pane| {
                !pane.is_dead && pane.title != SIDEBAR_PANE_TITLE && pane.title != FOOTER_PANE_TITLE
            })
            .and_then(|pane| pane.current_command)
    }

    fn workspace_main_pane_pipe(
        backend: &EmbeddedTmuxBackend,
        workspace: &TmuxWorkspaceHandle,
    ) -> Option<String> {
        let pane_id = backend
            .show_session_option(workspace, WAITAGENT_MAIN_PANE_OPTION)
            .ok()
            .flatten()?;
        backend
            .pane_pipe_state(workspace, &crate::infra::tmux::TmuxPaneId::new(pane_id))
            .ok()
    }

    fn pane_option(
        backend: &EmbeddedTmuxBackend,
        workspace: &TmuxWorkspaceHandle,
        pane_id: &str,
        option_name: &str,
    ) -> Option<String> {
        let output = backend
            .run_on_socket(
                &workspace.socket_name,
                &[
                    "show-options".to_string(),
                    "-pqv".to_string(),
                    "-t".to_string(),
                    pane_id.to_string(),
                    option_name.to_string(),
                ],
            )
            .ok()?;
        let value = output.stdout.trim();
        if value.is_empty() {
            None
        } else {
            Some(value.to_string())
        }
    }

    fn pane_hook_command(
        backend: &EmbeddedTmuxBackend,
        workspace: &TmuxWorkspaceHandle,
        pane_id: &str,
        hook_name: &str,
    ) -> Option<String> {
        let output = backend
            .run_on_socket(
                &workspace.socket_name,
                &[
                    "show-hooks".to_string(),
                    "-p".to_string(),
                    "-t".to_string(),
                    pane_id.to_string(),
                ],
            )
            .ok()?;
        output.stdout.lines().find_map(|line| {
            let prefix = format!("{hook_name}[");
            line.strip_prefix(&prefix)
                .and_then(|rest| rest.split_once(']'))
                .map(|(_, command)| command.trim().to_string())
        })
    }

    fn wait_for_condition(predicate: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if predicate() {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        assert!(predicate(), "condition should become true before timeout");
    }

    fn kill_server(backend: &EmbeddedTmuxBackend, workspace: &TmuxWorkspaceHandle) {
        let _ = backend.run_socket_command(
            &TmuxSocketName::new(workspace.socket_name.as_str().to_string()),
            &["kill-server".to_string()],
        );
    }
}
