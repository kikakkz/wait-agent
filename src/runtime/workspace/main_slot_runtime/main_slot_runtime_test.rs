mod tests {
    use super::super::{
        next_remote_target, next_remote_target_on_active_authority, next_target_host_session,
        remote_main_slot_program, split_qualified_target, target_socket_name, CurrentWorkspace,
        MainSlotRuntime, FOOTER_PANE_TITLE, MAIN_PANE_DIED_HOOK, SIDEBAR_PANE_TITLE,
        WAITAGENT_ACTIVE_TARGET_OPTION, WAITAGENT_MAIN_PANE_GENERATION_OPTION,
        WAITAGENT_MAIN_PANE_OPTION, WAITAGENT_MAIN_PANE_TRANSITION_OPTION,
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
    use crate::runtime::current_executable::waitagent_test_executable;
    use crate::runtime::network_state_runtime::persist_workspace_network_config;
    use crate::runtime::remote_runtime_owner_runtime::RemoteRuntimeOwnerRuntime;
    use crate::runtime::remote_workspace_socket_registry_runtime::RemoteWorkspaceSocketRegistryRuntime;
    use crate::runtime::target_host_runtime::TargetHostRuntime;
    use crate::runtime::workspace_entry_runtime::WorkspaceEntryRuntime;
    use crate::runtime::workspace_layout_runtime::{
        set_layout_topology_after_options_hook_for_tests, WorkspaceLayoutRuntime,
    };
    use crate::runtime::workspace_runtime::WorkspaceRuntime;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::Barrier;
    use std::thread;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    #[test]
    fn next_target_host_session_prefers_another_target_on_same_socket() {
        let _guard = crate::test_support::integration_test_lock();
        let sessions = vec![
            session("wa-1", "workspace", WorkspaceSessionRole::WorkspaceChrome),
            session("wa-1", "target-a", WorkspaceSessionRole::TargetHost),
            session("wa-1", "target-b", WorkspaceSessionRole::TargetHost),
            session("wa-2", "target-c", WorkspaceSessionRole::TargetHost),
        ];

        let next = next_target_host_session(&sessions, "wa-1", Some("wa-1:target-a"))
            .expect("next target should exist");

        assert_eq!(next.address.qualified_target(), "wa-1:target-b");
    }

    #[test]
    fn next_target_host_session_returns_none_without_same_socket_target_hosts() {
        let _guard = crate::test_support::integration_test_lock();
        let sessions = vec![session(
            "wa-1",
            "workspace",
            WorkspaceSessionRole::WorkspaceChrome,
        )];

        assert!(next_target_host_session(&sessions, "wa-1", Some("wa-1:target-a")).is_none());
    }

    #[test]
    fn next_target_host_session_ignores_remote_targets_when_local_target_host_exits() {
        let _guard = crate::test_support::integration_test_lock();
        let sessions = vec![
            session("wa-1", "workspace", WorkspaceSessionRole::WorkspaceChrome),
            remote_session("192.168.31.18", "pty1"),
        ];

        assert!(next_target_host_session(&sessions, "wa-1", Some("wa-1:target-a")).is_none());
    }

    #[test]
    fn next_target_host_session_returns_none_when_only_active_target_remains() {
        let _guard = crate::test_support::integration_test_lock();
        let sessions = vec![
            session("wa-1", "workspace", WorkspaceSessionRole::WorkspaceChrome),
            session("wa-1", "target-a", WorkspaceSessionRole::TargetHost),
        ];

        assert!(next_target_host_session(&sessions, "wa-1", Some("wa-1:target-a")).is_none());
    }

    #[test]
    fn next_remote_target_prefers_another_remote_target() {
        let _guard = crate::test_support::integration_test_lock();
        let sessions = vec![
            session("wa-1", "workspace", WorkspaceSessionRole::WorkspaceChrome),
            session("wa-1", "target-a", WorkspaceSessionRole::TargetHost),
            remote_session("10.1.29.130#7474", "remote-a"),
            remote_session("10.1.29.130#7474", "remote-b"),
        ];

        let next = next_remote_target(&sessions, Some("10.1.29.130#7474:remote-a"))
            .expect("remote next target should exist");

        assert_eq!(next.address.qualified_target(), "10.1.29.130#7474:remote-b");
    }

    #[test]
    fn next_remote_target_returns_none_without_another_remote_target() {
        let _guard = crate::test_support::integration_test_lock();
        let sessions = vec![
            session("wa-1", "workspace", WorkspaceSessionRole::WorkspaceChrome),
            session("wa-1", "target-a", WorkspaceSessionRole::TargetHost),
            remote_session("10.1.29.130#7474", "remote-a"),
        ];

        assert!(next_remote_target(&sessions, Some("10.1.29.130#7474:remote-a")).is_none());
    }

    #[test]
    fn next_remote_target_on_active_authority_ignores_other_remote_authorities() {
        let _guard = crate::test_support::integration_test_lock();
        let sessions = vec![
            session("wa-1", "workspace", WorkspaceSessionRole::WorkspaceChrome),
            session("wa-1", "target-a", WorkspaceSessionRole::TargetHost),
            remote_session("10.1.29.130#7474", "remote-a"),
            remote_session("peer-a", "remote-b"),
        ];

        assert!(next_remote_target_on_active_authority(
            &sessions,
            Some("10.1.29.130#7474:remote-a")
        )
        .is_none());
    }

    #[test]
    fn next_target_host_session_returns_first_target_without_active_target() {
        let _guard = crate::test_support::integration_test_lock();
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
        let _guard = crate::test_support::integration_test_lock();
        assert_eq!(
            split_qualified_target("wa-1:target-a"),
            Some(("wa-1", "target-a"))
        );
        assert_eq!(target_socket_name("wa-1:target-a"), Some("wa-1"));
    }

    #[test]
    fn split_qualified_target_rejects_missing_parts() {
        let _guard = crate::test_support::integration_test_lock();
        assert_eq!(split_qualified_target("wa-1"), None);
        assert_eq!(split_qualified_target("wa-1:"), None);
        assert_eq!(split_qualified_target(":target-a"), None);
    }

    #[test]
    fn remote_main_slot_program_targets_workspace_and_remote_target() {
        let _guard = crate::test_support::integration_test_lock();
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
        let _guard = crate::test_support::integration_test_lock();
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
            .upsert_session("peer-a", &remote_target)
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

        let remote_main_pane = backend
            .show_session_option(&workspace.workspace_handle, WAITAGENT_MAIN_PANE_OPTION)
            .expect("main pane option should read after remote activation")
            .expect("remote activation should set main pane");
        assert!(
            pane_exists(&backend, &workspace.workspace_handle, &remote_main_pane),
            "remote main pane should be retained even when authority registration fails"
        );
        assert_eq!(
            pane_option(
                &backend,
                &workspace.workspace_handle,
                &remote_main_pane,
                "remain-on-exit",
            )
            .as_deref(),
            Some("on")
        );
        assert!(
            workspace_main_pane_pipe(&backend, &workspace.workspace_handle).as_deref() == Some("0"),
            "remote mirror pane must not keep the main pane output bridge active"
        );
        runtime
            .layout_runtime
            .enable_main_pane_output_bridge(&workspace.workspace_handle)
            .expect("test should be able to enable the bridge option");
        runtime
            .ensure_initial_target_materialized(&workspace.workspace_handle, &workspace_dir)
            .expect("materializing a remote target should succeed");
        assert!(
            workspace_main_pane_pipe(&backend, &workspace.workspace_handle).as_deref() == Some("0"),
            "materializing an active remote target must not reinstall the output bridge"
        );

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
    fn stale_layout_reconcile_during_main_slot_transition_must_not_rewrite_main_pane_metadata() {
        let _guard = crate::test_support::integration_test_lock();
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let workspace_config = unique_workspace_config("stale-layout-transition");
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

        let runtime = Arc::new(MainSlotRuntime::new(
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
        ));

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
            "remote-stale-layout",
            &local_target,
            ManagedSessionTaskState::Input,
        );
        remote_runtime_owner
            .upsert_session("peer-a", &remote_target)
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

        backend
            .show_session_option(&workspace.workspace_handle, WAITAGENT_MAIN_PANE_OPTION)
            .expect("main pane should read after remote activation")
            .expect("remote activation should set main pane");
        let gate = Arc::new(Barrier::new(2));
        let hook_gate = gate.clone();
        let hook_session = workspace.workspace_handle.session_name.as_str().to_string();
        set_layout_topology_after_options_hook_for_tests(Some(Arc::new(move |workspace| {
            if workspace.session_name.as_str() == hook_session.as_str() {
                hook_gate.wait();
                hook_gate.wait();
            }
        })));

        let runtime_for_thread = runtime.clone();
        let workspace_for_thread = workspace.workspace_handle.clone();
        let workspace_dir_for_thread = workspace_dir.clone();
        let reconcile = thread::spawn(move || {
            runtime_for_thread
                .layout_runtime
                .sync_main_slot_bindings(&workspace_for_thread, &workspace_dir_for_thread)
                .expect("stale layout sync should complete");
        });

        gate.wait();
        backend
            .set_session_option(
                &workspace.workspace_handle,
                WAITAGENT_MAIN_PANE_TRANSITION_OPTION,
                "1",
            )
            .expect("transition marker should set");
        backend
            .set_session_option(&workspace.workspace_handle, WAITAGENT_MAIN_PANE_OPTION, "")
            .expect("main pane should clear during transition");
        gate.wait();
        reconcile.join().expect("stale layout thread should join");
        set_layout_topology_after_options_hook_for_tests(None);

        let transition_main_pane = backend
            .show_session_option(&workspace.workspace_handle, WAITAGENT_MAIN_PANE_OPTION)
            .expect("main pane should read during transition");
        assert_eq!(
            transition_main_pane, None,
            "stale layout reconcile must not rewrite main pane metadata after main-slot transition begins"
        );
    }

    #[test]
    fn switching_from_remote_back_to_local_target_restores_local_main_pane() {
        let _guard = crate::test_support::integration_test_lock();
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
            .upsert_session("peer-a", &remote_target)
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

        let remote_main_pane = backend
            .show_session_option(&workspace.workspace_handle, WAITAGENT_MAIN_PANE_OPTION)
            .expect("main pane should read after remote activation")
            .expect("remote activation should set main pane");
        backend
            .set_session_option(
                &workspace.workspace_handle,
                WAITAGENT_MAIN_PANE_TRANSITION_OPTION,
                "1",
            )
            .expect("transition marker should set");
        backend
            .set_session_option(&workspace.workspace_handle, WAITAGENT_MAIN_PANE_OPTION, "")
            .expect("main pane should clear during transition");
        runtime
            .layout_runtime
            .sync_main_slot_bindings(&workspace.workspace_handle, &workspace_dir)
            .expect("layout sync during transition should not rewrite main pane metadata");
        let transition_main_pane = backend
            .show_session_option(&workspace.workspace_handle, WAITAGENT_MAIN_PANE_OPTION)
            .expect("main pane should read during transition");
        assert_eq!(
            transition_main_pane, None,
            "layout reconcile must not infer and write a main pane while main-slot owns a transition"
        );
        backend
            .set_session_option(
                &workspace.workspace_handle,
                WAITAGENT_MAIN_PANE_TRANSITION_OPTION,
                "",
            )
            .expect("transition marker should clear");
        backend
            .set_session_option(
                &workspace.workspace_handle,
                WAITAGENT_MAIN_PANE_OPTION,
                &remote_main_pane,
            )
            .expect("main pane should restore for the rest of the test");

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

        let remote_session_pane_option = format!(
            "@waitagent_session_pane_{}",
            remote_target.address.qualified_target().replace(':', ".")
        );
        let remote_session_pane = backend
            .show_session_option(&workspace.workspace_handle, &remote_session_pane_option)
            .expect("remote session pane option should read");
        if let Some(remote_session_pane) = remote_session_pane.filter(|pane| !pane.is_empty()) {
            assert_eq!(
                pane_option(
                    &backend,
                    &workspace.workspace_handle,
                    &remote_session_pane,
                    "remain-on-exit",
                )
                .as_deref(),
                Some("on"),
                "inactive remote session pane should remain a persistent owned content pane"
            );
            assert!(
                pane_hook_command(
                    &backend,
                    &workspace.workspace_handle,
                    &remote_session_pane,
                    "pane-died[10]",
                )
                .is_none(),
                "inactive remote session pane should not keep the workspace pane-died hook"
            );
        }
        let hidden_windows = backend
            .run_on_socket(
                &workspace.workspace_handle.socket_name,
                &[
                    "list-windows".to_string(),
                    "-F".to_string(),
                    "#{window_name}".to_string(),
                ],
            )
            .expect("windows should list");
        assert!(
            !hidden_windows.stdout.contains("wa-orphan-"),
            "normal session switching should not create orphan windows"
        );
        assert!(
            hidden_windows.stdout.contains("wa-hidden-"),
            "normal session switching should park inactive content panes in owned hidden windows"
        );

        kill_server(&backend, &workspace.workspace_handle);
        let _ = fs::remove_dir_all(workspace_dir);
    }

    #[test]
    fn last_local_main_pane_exit_stops_workspace_server() {
        assert_last_local_main_pane_exit_stops_workspace_server(
            "local-main-pane-exit",
            true,
            false,
        );
    }

    #[test]
    fn stale_generation_dead_current_local_main_pane_exit_stops_workspace_server() {
        assert_last_local_main_pane_exit_stops_workspace_server(
            "local-main-pane-exit-stale-dead-current",
            true,
            true,
        );
    }

    #[test]
    fn legacy_main_pane_died_hook_without_generation_stops_last_local_workspace_server() {
        assert_last_local_main_pane_exit_stops_workspace_server(
            "local-main-pane-exit-legacy-hook",
            false,
            false,
        );
    }

    fn assert_last_local_main_pane_exit_stops_workspace_server(
        workspace_prefix: &str,
        include_generation: bool,
        bump_generation_after_hook: bool,
    ) {
        let _guard = crate::test_support::integration_test_lock();
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let workspace_config = unique_workspace_config(workspace_prefix);
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

        let pane_generation = include_generation.then(|| {
            runtime
                .backend
                .show_session_option(
                    &workspace.workspace_handle,
                    WAITAGENT_MAIN_PANE_GENERATION_OPTION,
                )
                .expect("main pane generation should read")
                .unwrap_or_default()
        });
        if bump_generation_after_hook {
            backend
                .kill_pane(
                    &workspace.workspace_handle,
                    &crate::infra::tmux::TmuxPaneId::new(main_pane_id.clone()),
                )
                .expect("current main pane should become dead");
            wait_for_condition(|| {
                !pane_is_live(&backend, &workspace.workspace_handle, &main_pane_id)
            });
            let current_generation = runtime
                .backend
                .show_session_option(
                    &workspace.workspace_handle,
                    WAITAGENT_MAIN_PANE_GENERATION_OPTION,
                )
                .expect("main pane generation should read before bump")
                .unwrap_or_default();
            let next_generation = current_generation
                .parse::<u64>()
                .expect("main pane generation should be numeric")
                + 1;
            runtime
                .backend
                .set_session_option(
                    &workspace.workspace_handle,
                    WAITAGENT_MAIN_PANE_GENERATION_OPTION,
                    &next_generation.to_string(),
                )
                .expect("main pane generation should bump");
        }
        runtime
            .run_main_pane_died(MainPaneDiedCommand {
                socket_name: workspace.workspace_handle.socket_name.as_str().to_string(),
                session_name: workspace.workspace_handle.session_name.as_str().to_string(),
                pane_id: main_pane_id,
                pane_generation,
            })
            .expect("last local main pane exit should stop workspace");

        wait_for_condition(|| {
            !backend.socket_is_live(&TmuxSocketName::new(
                workspace.workspace_handle.socket_name.as_str().to_string(),
            ))
        });

        let workspace_still_exists = backend.current_window(&workspace.workspace_handle).is_ok();
        assert!(
            !workspace_still_exists,
            "workspace session should be gone after the last local target exits"
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

        let _ = fs::remove_dir_all(workspace_dir);
    }

    #[test]
    fn stale_legacy_main_pane_died_hook_after_workspace_closed_is_ignored() {
        let _guard = crate::test_support::integration_test_lock();
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let workspace_config = unique_workspace_config("stale-legacy-hook-closed-workspace");
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
        let main_pane_id = backend
            .show_session_option(&workspace.workspace_handle, WAITAGENT_MAIN_PANE_OPTION)
            .expect("main pane option should read")
            .expect("main pane option should be populated");

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

        kill_server(&backend, &workspace.workspace_handle);
        runtime
            .run_main_pane_died(MainPaneDiedCommand {
                socket_name: workspace.workspace_handle.socket_name.as_str().to_string(),
                session_name: workspace.workspace_handle.session_name.as_str().to_string(),
                pane_id: main_pane_id,
                pane_generation: None,
            })
            .expect("stale legacy pane-died hook after workspace close should be ignored");

        let _ = fs::remove_dir_all(workspace_dir);
    }

    #[test]
    fn legacy_main_pane_died_hook_on_first_of_four_local_targets_falls_back_to_next_local_target() {
        let _guard = crate::test_support::integration_test_lock();
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let workspace_config = unique_workspace_config("four-local-first-exit-legacy-hook");
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

        let target_hosts = (0..4)
            .map(|_| {
                backend
                    .ensure_workspace(
                        &WorkspaceInstanceConfig::for_new_target_on_socket_with_size(
                            &workspace_dir,
                            workspace.workspace_handle.socket_name.as_str(),
                            None,
                            None,
                        ),
                    )
                    .expect("target host bootstrap should succeed")
            })
            .collect::<Vec<_>>();
        let targets = target_hosts
            .iter()
            .map(|target_host| {
                format!(
                    "{}:{}",
                    workspace.workspace_handle.socket_name.as_str(),
                    target_host.session_name.as_str()
                )
            })
            .collect::<Vec<_>>();

        runtime
            .run_activate_target(ActivateTargetCommand {
                current_socket_name: workspace.workspace_handle.socket_name.as_str().to_string(),
                current_session_name: workspace.workspace_handle.session_name.as_str().to_string(),
                target: targets[0].clone(),
            })
            .expect("first local target activation should succeed");
        wait_for_condition(|| {
            let active_target = backend
                .show_session_option(&workspace.workspace_handle, WAITAGENT_ACTIVE_TARGET_OPTION)
                .expect("active target should read");
            active_target.as_deref() == Some(targets[0].as_str())
        });

        let main_pane_id = backend
            .show_session_option(&workspace.workspace_handle, WAITAGENT_MAIN_PANE_OPTION)
            .expect("main pane option should read")
            .expect("main pane option should be populated");
        let _ = backend.run_socket_command(
            &TmuxSocketName::new(workspace.workspace_handle.socket_name.as_str().to_string()),
            &[
                "kill-session".to_string(),
                "-t".to_string(),
                target_hosts[0].session_name.as_str().to_string(),
            ],
        );

        runtime
            .run_main_pane_died(MainPaneDiedCommand {
                socket_name: workspace.workspace_handle.socket_name.as_str().to_string(),
                session_name: workspace.workspace_handle.session_name.as_str().to_string(),
                pane_id: main_pane_id,
                pane_generation: None,
            })
            .expect("legacy pane-died hook should recover to a remaining local target");

        let remaining_targets = targets[1..].to_vec();
        wait_for_condition(|| {
            let active_target = backend
                .show_session_option(&workspace.workspace_handle, WAITAGENT_ACTIVE_TARGET_OPTION)
                .expect("active target should read after recovery");
            active_target
                .as_ref()
                .is_some_and(|target| remaining_targets.contains(target))
        });
        wait_for_condition(|| {
            workspace_main_pane_command(&backend, &workspace.workspace_handle).as_deref()
                == Some("bash")
        });
        wait_for_condition(|| {
            current_workspace_pane_command(&backend, &workspace.workspace_handle).as_deref()
                == Some("bash")
        });
        assert!(
            backend.socket_is_live(&TmuxSocketName::new(
                workspace.workspace_handle.socket_name.as_str().to_string(),
            )),
            "workspace server should remain alive while local targets remain"
        );

        let exited_target_handle = TmuxWorkspaceHandle {
            workspace_id: WorkspaceInstanceId::new(
                target_hosts[0].session_name.as_str().to_string(),
            ),
            socket_name: TmuxSocketName::new(
                workspace.workspace_handle.socket_name.as_str().to_string(),
            ),
            session_name: TmuxSessionName::new(target_hosts[0].session_name.as_str().to_string()),
        };
        let exited_target_exists = backend.current_window(&exited_target_handle).is_ok();
        assert!(
            !exited_target_exists,
            "exited first target session should be gone"
        );

        let active_target = backend
            .show_session_option(&workspace.workspace_handle, WAITAGENT_ACTIVE_TARGET_OPTION)
            .expect("active target should read after recovery")
            .expect("active target should remain after recovery");
        let active_session = split_qualified_target(&active_target)
            .map(|(_, session)| session)
            .expect("active target should be qualified");
        let current_main_pane = backend
            .show_session_option(&workspace.workspace_handle, WAITAGENT_MAIN_PANE_OPTION)
            .expect("main pane option should read after recovery")
            .expect("main pane option should remain populated after recovery");
        assert_eq!(
            pane_option(
                &backend,
                &workspace.workspace_handle,
                &current_main_pane,
                "@waitagent_target_session_name",
            )
            .as_deref(),
            Some(active_session),
            "workspace main pane must belong to the recovered active target"
        );

        kill_server(&backend, &workspace.workspace_handle);
        let _ = fs::remove_dir_all(workspace_dir);
    }

    #[test]
    fn layout_reconcile_preserves_current_main_pane_generation_hook() {
        let _guard = crate::test_support::integration_test_lock();
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let workspace_config = unique_workspace_config("layout-preserves-generation-hook");
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

        let target = format!(
            "{}:{}",
            workspace.workspace_handle.socket_name.as_str(),
            target_host.session_name.as_str()
        );
        runtime
            .run_activate_target(ActivateTargetCommand {
                current_socket_name: workspace.workspace_handle.socket_name.as_str().to_string(),
                current_session_name: workspace.workspace_handle.session_name.as_str().to_string(),
                target,
            })
            .expect("local target activation should succeed");

        let main_pane_id = backend
            .show_session_option(&workspace.workspace_handle, WAITAGENT_MAIN_PANE_OPTION)
            .expect("main pane option should read")
            .expect("main pane option should be populated");
        let generation = backend
            .show_session_option(
                &workspace.workspace_handle,
                WAITAGENT_MAIN_PANE_GENERATION_OPTION,
            )
            .expect("main pane generation should read")
            .expect("main pane generation should be populated");
        assert_ne!(
            generation, "0",
            "activation should install a state generation"
        );
        let expected_generation_arg = format!("'--pane-generation' '{generation}'");
        let hook_before = pane_hook_command(
            &backend,
            &workspace.workspace_handle,
            &main_pane_id,
            "pane-died[10]",
        )
        .expect("main pane should have a pane-died hook before reconcile");
        assert!(
            hook_before.contains(&expected_generation_arg),
            "main slot should install a generation-scoped pane-died hook: {hook_before}"
        );

        WorkspaceLayoutRuntime::new_for_tests(
            backend.clone(),
            waitagent_executable,
            RemoteNetworkConfig::default(),
        )
        .expect("workspace layout runtime should build")
        .sync_main_slot_bindings(&workspace.workspace_handle, &workspace_dir)
        .expect("layout reconcile should preserve main pane lifecycle hook generation");

        let generation_after = backend
            .show_session_option(
                &workspace.workspace_handle,
                WAITAGENT_MAIN_PANE_GENERATION_OPTION,
            )
            .expect("main pane generation should read after reconcile")
            .expect("main pane generation should remain populated after reconcile");
        assert_eq!(generation_after, generation);
        let hook_after = pane_hook_command(
            &backend,
            &workspace.workspace_handle,
            &main_pane_id,
            "pane-died[10]",
        )
        .expect("main pane should have a pane-died hook after reconcile");
        assert!(
            hook_after.contains(&expected_generation_arg),
            "layout reconcile must not overwrite the hook with generation 0: {hook_after}"
        );

        kill_server(&backend, &workspace.workspace_handle);
        let _ = fs::remove_dir_all(workspace_dir);
    }

    #[test]
    fn stale_layout_reconcile_generation_does_not_rewrite_main_pane_hook() {
        let _guard = crate::test_support::integration_test_lock();
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let workspace_config = unique_workspace_config("stale-layout-generation-skip");
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

        let target = format!(
            "{}:{}",
            workspace.workspace_handle.socket_name.as_str(),
            target_host.session_name.as_str()
        );
        runtime
            .run_activate_target(ActivateTargetCommand {
                current_socket_name: workspace.workspace_handle.socket_name.as_str().to_string(),
                current_session_name: workspace.workspace_handle.session_name.as_str().to_string(),
                target,
            })
            .expect("local target activation should succeed");

        let main_pane_id = backend
            .show_session_option(&workspace.workspace_handle, WAITAGENT_MAIN_PANE_OPTION)
            .expect("main pane option should read")
            .expect("main pane option should be populated");
        let generation = backend
            .show_session_option(
                &workspace.workspace_handle,
                WAITAGENT_MAIN_PANE_GENERATION_OPTION,
            )
            .expect("main pane generation should read")
            .expect("main pane generation should be populated");
        let hook_before = pane_hook_command(
            &backend,
            &workspace.workspace_handle,
            &main_pane_id,
            "pane-died[10]",
        )
        .expect("main pane should have a pane-died hook before stale reconcile");

        WorkspaceLayoutRuntime::new_for_tests(
            backend.clone(),
            waitagent_executable,
            RemoteNetworkConfig::default(),
        )
        .expect("workspace layout runtime should build")
        .run_reconcile(crate::cli::LayoutReconcileCommand {
            socket_name: workspace.workspace_handle.socket_name.as_str().to_string(),
            session_name: workspace.workspace_handle.session_name.as_str().to_string(),
            workspace_dir: workspace_dir.display().to_string(),
            pane_generation: Some("0".to_string()),
        })
        .expect("stale layout reconcile should be ignored");

        let generation_after = backend
            .show_session_option(
                &workspace.workspace_handle,
                WAITAGENT_MAIN_PANE_GENERATION_OPTION,
            )
            .expect("main pane generation should read after stale reconcile")
            .expect("main pane generation should remain populated");
        assert_eq!(generation_after, generation);
        let hook_after = pane_hook_command(
            &backend,
            &workspace.workspace_handle,
            &main_pane_id,
            "pane-died[10]",
        )
        .expect("main pane should have a pane-died hook after stale reconcile");
        assert_eq!(hook_after, hook_before);

        kill_server(&backend, &workspace.workspace_handle);
        let _ = fs::remove_dir_all(workspace_dir);
    }

    #[test]
    fn concurrent_local_target_activation_keeps_main_pane_identity_consistent() {
        let _guard = crate::test_support::integration_test_lock();
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let workspace_config = unique_workspace_config("concurrent-local-activation");
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

        let runtime = Arc::new(MainSlotRuntime::new(
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
        ));

        let target_sessions = (0..4)
            .map(|_| {
                backend
                    .ensure_workspace(
                        &WorkspaceInstanceConfig::for_new_target_on_socket_with_size(
                            &workspace_dir,
                            workspace.workspace_handle.socket_name.as_str(),
                            None,
                            None,
                        ),
                    )
                    .expect("target host bootstrap should succeed")
                    .session_name
                    .as_str()
                    .to_string()
            })
            .collect::<Vec<_>>();

        let targets = target_sessions
            .iter()
            .map(|session_name| {
                format!(
                    "{}:{}",
                    workspace.workspace_handle.socket_name.as_str(),
                    session_name
                )
            })
            .collect::<Vec<_>>();

        let barrier = Arc::new(Barrier::new(targets.len() + 1));
        let mut threads = Vec::new();
        for target in targets.clone() {
            let runtime = runtime.clone();
            let barrier = barrier.clone();
            let socket_name = workspace.workspace_handle.socket_name.as_str().to_string();
            let session_name = workspace.workspace_handle.session_name.as_str().to_string();
            threads.push(thread::spawn(move || {
                barrier.wait();
                runtime.run_activate_target(ActivateTargetCommand {
                    current_socket_name: socket_name,
                    current_session_name: session_name,
                    target,
                })
            }));
        }
        barrier.wait();
        for thread in threads {
            thread
                .join()
                .expect("worker should join")
                .expect("concurrent activation should succeed");
        }

        wait_for_condition(|| {
            let active_target = backend
                .show_session_option(&workspace.workspace_handle, WAITAGENT_ACTIVE_TARGET_OPTION)
                .expect("active target should read");
            let Some(active_target) = active_target else {
                return false;
            };
            let main_pane_id = backend
                .show_session_option(&workspace.workspace_handle, WAITAGENT_MAIN_PANE_OPTION)
                .expect("main pane option should read")
                .expect("main pane should exist");
            let expected_session = split_qualified_target(&active_target)
                .map(|(_, session_name)| session_name.to_string());
            pane_option(
                &backend,
                &workspace.workspace_handle,
                &main_pane_id,
                "@waitagent_target_session_name",
            ) == expected_session
        });

        let active_target = backend
            .show_session_option(&workspace.workspace_handle, WAITAGENT_ACTIVE_TARGET_OPTION)
            .expect("active target should read")
            .expect("active target should be populated");
        let main_pane_id = backend
            .show_session_option(&workspace.workspace_handle, WAITAGENT_MAIN_PANE_OPTION)
            .expect("main pane option should read")
            .expect("main pane should exist");
        let expected_session = split_qualified_target(&active_target)
            .map(|(_, session_name)| session_name.to_string())
            .expect("active target should be qualified");
        assert_eq!(
            pane_option(
                &backend,
                &workspace.workspace_handle,
                &main_pane_id,
                "@waitagent_target_session_name",
            )
            .as_deref(),
            Some(expected_session.as_str())
        );

        kill_server(&backend, &workspace.workspace_handle);
        let _ = fs::remove_dir_all(workspace_dir);
    }

    #[test]
    fn stale_main_pane_died_generation_is_ignored() {
        let _guard = crate::test_support::integration_test_lock();
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let workspace_config = unique_workspace_config("stale-main-pane-generation");
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
        let current_generation = backend
            .show_session_option(
                &workspace.workspace_handle,
                WAITAGENT_MAIN_PANE_GENERATION_OPTION,
            )
            .expect("main pane generation should read")
            .expect("main pane generation should be populated");
        let stale_generation = current_generation
            .parse::<u64>()
            .expect("generation should be numeric")
            .saturating_sub(1)
            .to_string();

        runtime
            .run_main_pane_died(MainPaneDiedCommand {
                socket_name: workspace.workspace_handle.socket_name.as_str().to_string(),
                session_name: workspace.workspace_handle.session_name.as_str().to_string(),
                pane_id: main_pane_id.clone(),
                pane_generation: Some(stale_generation),
            })
            .expect("stale main pane generation should be ignored");

        let active_target = backend
            .show_session_option(&workspace.workspace_handle, WAITAGENT_ACTIVE_TARGET_OPTION)
            .expect("active target should read");
        assert_eq!(active_target.as_deref(), Some(local_target.as_str()));
        let preserved_generation = backend
            .show_session_option(
                &workspace.workspace_handle,
                WAITAGENT_MAIN_PANE_GENERATION_OPTION,
            )
            .expect("main pane generation should read after stale event");
        assert_eq!(
            preserved_generation.as_deref(),
            Some(current_generation.as_str())
        );

        kill_server(&backend, &workspace.workspace_handle);
        let _ = fs::remove_dir_all(workspace_dir);
    }

    #[test]
    fn last_remote_main_pane_exit_activates_local_target_when_one_exists() {
        let _guard = crate::test_support::integration_test_lock();
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let workspace_config = unique_workspace_config("remote-main-slot-last-remote-exit");
        let workspace_dir = workspace_config.workspace_dir.clone();
        let network = unique_remote_network_config(&workspace_config.workspace_key);

        let waitagent_executable = waitagent_test_executable();
        let entry_runtime = WorkspaceEntryRuntime::new(
            WorkspaceRuntime::new(WorkspaceService::new(backend.clone())),
            WorkspaceLayoutRuntime::new_for_tests(
                backend.clone(),
                waitagent_executable.clone(),
                network.clone(),
            )
            .expect("workspace layout runtime should build"),
        );
        let workspace = entry_runtime
            .bootstrap_workspace(&workspace_dir)
            .expect("workspace bootstrap should succeed");
        persist_workspace_network_config(&backend, &workspace.workspace_handle, &network)
            .expect("workspace network config should persist");
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
                network.clone(),
            )
            .expect("workspace layout runtime should build"),
            TargetRegistryService::new(
                DefaultTargetCatalogGateway::from_build_env_with_socket_name(
                    workspace.workspace_handle.socket_name.as_str(),
                )
                .expect("target catalog gateway should build"),
            ),
            waitagent_executable.clone(),
            network.clone(),
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

        let remote_runtime_owner =
            RemoteRuntimeOwnerRuntime::new_for_tests(waitagent_executable.clone(), network.clone());
        let remote_target = remote_session_with_selector(
            "10.1.29.130#7474",
            "remote-last-exit-1",
            &local_target,
            ManagedSessionTaskState::Input,
        );
        remote_runtime_owner
            .upsert_session("10.1.29.130#7474", &remote_target)
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
            .expect("last remote target exit should activate the local workspace target");

        wait_for_condition(|| {
            backend.socket_is_live(&TmuxSocketName::new(
                workspace.workspace_handle.socket_name.as_str().to_string(),
            ))
        });
        wait_for_condition(|| {
            let active_target = backend
                .show_session_option(&workspace.workspace_handle, WAITAGENT_ACTIVE_TARGET_OPTION)
                .expect("active target should read after remote exit");
            active_target.as_deref() == Some(local_target.as_str())
        });
        wait_for_condition(|| {
            workspace_main_pane_command(&backend, &workspace.workspace_handle).as_deref()
                == Some("bash")
        });

        kill_server(&backend, &workspace.workspace_handle);
        let _ = fs::remove_dir_all(workspace_dir);
    }

    #[test]
    fn last_remote_main_pane_exit_restores_local_main_pane() {
        let _guard = crate::test_support::integration_test_lock();
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let workspace_config = unique_workspace_config("remote-main-slot-exit-to-local");
        let workspace_dir = workspace_config.workspace_dir.clone();
        let network = unique_remote_network_config(&workspace_config.workspace_key);

        let waitagent_executable = waitagent_test_executable();
        let entry_runtime = WorkspaceEntryRuntime::new(
            WorkspaceRuntime::new(WorkspaceService::new(backend.clone())),
            WorkspaceLayoutRuntime::new_for_tests(
                backend.clone(),
                waitagent_executable.clone(),
                network.clone(),
            )
            .expect("workspace layout runtime should build"),
        );
        let workspace = entry_runtime
            .bootstrap_workspace(&workspace_dir)
            .expect("workspace bootstrap should succeed");
        persist_workspace_network_config(&backend, &workspace.workspace_handle, &network)
            .expect("workspace network config should persist");
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
                network.clone(),
            )
            .expect("workspace layout runtime should build"),
            TargetRegistryService::new(
                DefaultTargetCatalogGateway::from_build_env_with_socket_name(
                    workspace.workspace_handle.socket_name.as_str(),
                )
                .expect("target catalog gateway should build"),
            ),
            waitagent_executable.clone(),
            network.clone(),
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

        let remote_runtime_owner =
            RemoteRuntimeOwnerRuntime::new_for_tests(waitagent_executable.clone(), network.clone());
        let remote_target = remote_session_with_selector(
            "10.1.29.130#7474",
            "remote-exit-local-1",
            &local_target,
            ManagedSessionTaskState::Input,
        );
        remote_runtime_owner
            .upsert_session("10.1.29.130#7474", &remote_target)
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

        let exited_pane_id = backend
            .show_session_option(&workspace.workspace_handle, WAITAGENT_MAIN_PANE_OPTION)
            .expect("main pane option should read")
            .expect("main pane option should be populated");
        let exited_pane = crate::infra::tmux::TmuxPaneId::new(exited_pane_id.clone());
        backend
            .set_pane_option(
                &workspace.workspace_handle,
                &exited_pane,
                "remain-on-exit",
                "on",
            )
            .expect("remote pane should remain after simulated exit");
        backend
            .unset_pane_hook(
                &workspace.workspace_handle,
                &exited_pane,
                MAIN_PANE_DIED_HOOK,
            )
            .expect("manual remote exit simulation should disable automatic pane-died recovery");
        backend
            .run_on_socket(
                &workspace.workspace_handle.socket_name,
                &[
                    "respawn-pane".to_string(),
                    "-k".to_string(),
                    "-t".to_string(),
                    exited_pane_id.clone(),
                    "exit 0".to_string(),
                ],
            )
            .expect("remote pane should become a retained dead pane");
        wait_for_condition(|| {
            !pane_is_live(&backend, &workspace.workspace_handle, &exited_pane_id)
        });
        runtime
            .run_remote_target_exited(RemoteTargetExitedCommand {
                socket_name: workspace.workspace_handle.socket_name.as_str().to_string(),
                session_name: workspace.workspace_handle.session_name.as_str().to_string(),
                target: remote_target.address.qualified_target(),
                pane_id: Some(exited_pane_id.clone()),
            })
            .expect("last remote target exit should restore the selected local target");

        wait_for_condition(|| {
            backend.socket_is_live(&TmuxSocketName::new(
                workspace.workspace_handle.socket_name.as_str().to_string(),
            ))
        });
        wait_for_condition(|| {
            let active_target = backend
                .show_session_option(&workspace.workspace_handle, WAITAGENT_ACTIVE_TARGET_OPTION)
                .expect("active target should read after remote exit");
            active_target.as_deref() == Some(local_target.as_str())
        });
        wait_for_condition(|| {
            workspace_main_pane_command(&backend, &workspace.workspace_handle).as_deref()
                == Some("bash")
        });
        wait_for_condition(|| !pane_exists(&backend, &workspace.workspace_handle, &exited_pane_id));
        let main_pane_after = backend
            .show_session_option(&workspace.workspace_handle, WAITAGENT_MAIN_PANE_OPTION)
            .expect("main pane option should read after remote exit")
            .expect("main pane option should be populated after remote exit");
        assert!(
            !pane_is_chrome(&backend, &workspace.workspace_handle, &main_pane_after),
            "workspace main pane must not be chrome after remote exit"
        );

        kill_server(&backend, &workspace.workspace_handle);
        let _ = fs::remove_dir_all(workspace_dir);
    }

    #[test]
    fn connect_workspace_last_remote_exit_closes_workspace_even_with_local_support_target() {
        let _guard = crate::test_support::integration_test_lock();
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let workspace_config = unique_workspace_config("connect-remote-exit-closes");
        let workspace_dir = workspace_config.workspace_dir.clone();
        let mut network = unique_remote_network_config(&workspace_config.workspace_key);
        network.connect = Some(format!("10.1.29.130:{}", network.port));

        let waitagent_executable = waitagent_test_executable();
        let entry_runtime = WorkspaceEntryRuntime::new(
            WorkspaceRuntime::new(WorkspaceService::new(backend.clone())),
            WorkspaceLayoutRuntime::new_for_tests(
                backend.clone(),
                waitagent_executable.clone(),
                network.clone(),
            )
            .expect("workspace layout runtime should build"),
        );
        let workspace = entry_runtime
            .bootstrap_workspace(&workspace_dir)
            .expect("workspace bootstrap should succeed");
        persist_workspace_network_config(&backend, &workspace.workspace_handle, &network)
            .expect("workspace network config should persist");
        RemoteWorkspaceSocketRegistryRuntime::new(network.clone())
            .register_workspace_socket(workspace.workspace_handle.socket_name.as_str())
            .expect("workspace socket should register");
        let target_host = backend
            .ensure_workspace(
                &WorkspaceInstanceConfig::for_new_target_on_socket_with_size(
                    &workspace_dir,
                    workspace.workspace_handle.socket_name.as_str(),
                    None,
                    None,
                ),
            )
            .expect("local support target host bootstrap should succeed");

        let runtime = MainSlotRuntime::new(
            backend.clone(),
            TargetHostRuntime::from_build_env(backend.clone())
                .expect("target host runtime should build"),
            WorkspaceLayoutRuntime::new_for_tests(
                backend.clone(),
                waitagent_executable.clone(),
                network.clone(),
            )
            .expect("workspace layout runtime should build"),
            TargetRegistryService::new(
                DefaultTargetCatalogGateway::from_build_env_with_socket_name(
                    workspace.workspace_handle.socket_name.as_str(),
                )
                .expect("target catalog gateway should build"),
            ),
            waitagent_executable.clone(),
            network.clone(),
        );

        let local_target = format!(
            "{}:{}",
            workspace.workspace_handle.socket_name.as_str(),
            target_host.session_name.as_str()
        );
        let remote_runtime_owner =
            RemoteRuntimeOwnerRuntime::new_for_tests(waitagent_executable.clone(), network.clone());
        let remote_target = remote_session_with_selector(
            "10.1.29.130#7474",
            "connect-remote-exit-1",
            &local_target,
            ManagedSessionTaskState::Input,
        );
        remote_runtime_owner
            .upsert_session("10.1.29.130#7474", &remote_target)
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

        let exited_pane_id = backend
            .show_session_option(&workspace.workspace_handle, WAITAGENT_MAIN_PANE_OPTION)
            .expect("main pane option should read")
            .expect("main pane option should be populated");
        runtime
            .run_remote_target_exited(RemoteTargetExitedCommand {
                socket_name: workspace.workspace_handle.socket_name.as_str().to_string(),
                session_name: workspace.workspace_handle.session_name.as_str().to_string(),
                target: remote_target.address.qualified_target(),
                pane_id: Some(exited_pane_id),
            })
            .expect("last connect remote target exit should close workspace");

        wait_for_condition(|| {
            !backend.socket_is_live(&TmuxSocketName::new(
                workspace.workspace_handle.socket_name.as_str().to_string(),
            ))
        });
        assert!(RemoteWorkspaceSocketRegistryRuntime::new(network.clone())
            .live_workspace_socket_names()
            .expect("workspace socket registry should read")
            .is_empty());

        let _ = fs::remove_dir_all(workspace_dir);
    }

    #[test]
    fn last_remote_main_pane_exit_closes_workspace_when_no_target_remains() {
        let _guard = crate::test_support::integration_test_lock();
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let workspace_config = unique_workspace_config("remote-main-slot-no-target-remains");
        let workspace_dir = workspace_config.workspace_dir.clone();
        let network = unique_remote_network_config(&workspace_config.workspace_key);

        let waitagent_executable = waitagent_test_executable();
        let entry_runtime = WorkspaceEntryRuntime::new(
            WorkspaceRuntime::new(WorkspaceService::new(backend.clone())),
            WorkspaceLayoutRuntime::new_for_tests(
                backend.clone(),
                waitagent_executable.clone(),
                network.clone(),
            )
            .expect("workspace layout runtime should build"),
        );
        let workspace = entry_runtime
            .bootstrap_workspace(&workspace_dir)
            .expect("workspace bootstrap should succeed");
        persist_workspace_network_config(&backend, &workspace.workspace_handle, &network)
            .expect("workspace network config should persist");
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
                network.clone(),
            )
            .expect("workspace layout runtime should build"),
            TargetRegistryService::new(
                DefaultTargetCatalogGateway::from_build_env_with_socket_name(
                    workspace.workspace_handle.socket_name.as_str(),
                )
                .expect("target catalog gateway should build"),
            ),
            waitagent_executable.clone(),
            network.clone(),
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

        let remote_runtime_owner =
            RemoteRuntimeOwnerRuntime::new_for_tests(waitagent_executable.clone(), network.clone());
        let remote_target = remote_session_with_selector(
            "10.1.29.130#7474",
            "remote-exit-no-target-remains",
            &local_target,
            ManagedSessionTaskState::Input,
        );
        remote_runtime_owner
            .upsert_session("10.1.29.130#7474", &remote_target)
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

        let exited_pane_id = backend
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
            .run_remote_target_exited(RemoteTargetExitedCommand {
                socket_name: workspace.workspace_handle.socket_name.as_str().to_string(),
                session_name: workspace.workspace_handle.session_name.as_str().to_string(),
                target: remote_target.address.qualified_target(),
                pane_id: Some(exited_pane_id),
            })
            .expect("last remote target exit should close workspace when no target remains");

        wait_for_condition(|| {
            !backend.socket_is_live(&TmuxSocketName::new(
                workspace.workspace_handle.socket_name.as_str().to_string(),
            ))
        });

        let _ = fs::remove_dir_all(workspace_dir);
    }

    #[test]
    fn duplicate_remote_main_pane_exit_events_are_serialized() {
        let _guard = crate::test_support::integration_test_lock();
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let workspace_config = unique_workspace_config("remote-main-slot-exit-serialized");
        let workspace_dir = workspace_config.workspace_dir.clone();
        let network = unique_remote_network_config(&workspace_config.workspace_key);

        let waitagent_executable = waitagent_test_executable();
        let entry_runtime = WorkspaceEntryRuntime::new(
            WorkspaceRuntime::new(WorkspaceService::new(backend.clone())),
            WorkspaceLayoutRuntime::new_for_tests(
                backend.clone(),
                waitagent_executable.clone(),
                network.clone(),
            )
            .expect("workspace layout runtime should build"),
        );
        let workspace = entry_runtime
            .bootstrap_workspace(&workspace_dir)
            .expect("workspace bootstrap should succeed");
        persist_workspace_network_config(&backend, &workspace.workspace_handle, &network)
            .expect("workspace network config should persist");
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

        let runtime = Arc::new(MainSlotRuntime::new(
            backend.clone(),
            TargetHostRuntime::from_build_env(backend.clone())
                .expect("target host runtime should build"),
            WorkspaceLayoutRuntime::new_for_tests(
                backend.clone(),
                waitagent_executable.clone(),
                network.clone(),
            )
            .expect("workspace layout runtime should build"),
            TargetRegistryService::new(
                DefaultTargetCatalogGateway::from_build_env_with_socket_name(
                    workspace.workspace_handle.socket_name.as_str(),
                )
                .expect("target catalog gateway should build"),
            ),
            waitagent_executable.clone(),
            network.clone(),
        ));

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

        let remote_runtime_owner =
            RemoteRuntimeOwnerRuntime::new_for_tests(waitagent_executable.clone(), network.clone());
        let remote_target = remote_session_with_selector(
            "10.1.29.130#7474",
            "remote-exit-serialized-1",
            &local_target,
            ManagedSessionTaskState::Input,
        );
        let remote_target_2 = remote_session_with_selector(
            "10.1.29.130#7474",
            "remote-exit-serialized-2",
            &local_target,
            ManagedSessionTaskState::Running,
        );
        remote_runtime_owner
            .upsert_session("10.1.29.130#7474", &remote_target)
            .expect("first remote target should be discoverable on workspace socket");
        remote_runtime_owner
            .upsert_session("10.1.29.130#7474", &remote_target_2)
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

        let exited_pane_id = backend
            .show_session_option(&workspace.workspace_handle, WAITAGENT_MAIN_PANE_OPTION)
            .expect("main pane option should read")
            .expect("main pane option should be populated");
        remote_runtime_owner
            .remove_session(
                "10.1.29.130#7474",
                "10.1.29.130#7474",
                remote_target.address.session_id(),
            )
            .expect("exited remote target should be removed before pane death recovery");
        backend
            .kill_pane(
                &workspace.workspace_handle,
                &crate::infra::tmux::TmuxPaneId::new(exited_pane_id.clone()),
            )
            .expect("remote main pane should be killable");

        let pane_generation = backend
            .show_session_option(
                &workspace.workspace_handle,
                "@waitagent_main_pane_generation",
            )
            .expect("main pane generation should read")
            .unwrap_or_default();
        let barrier = Arc::new(Barrier::new(3));
        let mut threads = Vec::new();
        for _ in 0..2 {
            let runtime = runtime.clone();
            let barrier = barrier.clone();
            let socket_name = workspace.workspace_handle.socket_name.as_str().to_string();
            let session_name = workspace.workspace_handle.session_name.as_str().to_string();
            let pane_id = exited_pane_id.clone();
            let pane_generation = pane_generation.clone();
            threads.push(thread::spawn(move || {
                barrier.wait();
                runtime.run_main_pane_died(MainPaneDiedCommand {
                    socket_name,
                    session_name,
                    pane_id,
                    pane_generation: Some(pane_generation),
                })
            }));
        }
        barrier.wait();
        for handle in threads {
            handle
                .join()
                .expect("worker should join")
                .expect("duplicate exit handling should succeed");
        }

        wait_for_condition(|| {
            let active_target = backend
                .show_session_option(&workspace.workspace_handle, WAITAGENT_ACTIVE_TARGET_OPTION)
                .expect("active target should read");
            active_target.as_deref() == Some(remote_target_2.address.qualified_target().as_str())
        });
        wait_for_condition(|| {
            workspace_main_pane_command(&backend, &workspace.workspace_handle).as_deref()
                == Some("waitagent")
        });
        wait_for_condition(|| {
            current_workspace_pane_command(&backend, &workspace.workspace_handle).as_deref()
                == Some("waitagent")
        });

        let snapshot = remote_runtime_owner
            .snapshot()
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
        assert_eq!(
            remote_sessions,
            vec![remote_target_2.address.qualified_target()]
        );

        kill_server(&backend, &workspace.workspace_handle);
        let _ = fs::remove_dir_all(workspace_dir);
    }

    #[test]
    fn remote_main_pane_exit_activates_next_remote_target_own_pane() {
        let _guard = crate::test_support::integration_test_lock();
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let workspace_config = unique_workspace_config("remote-main-slot-exit");
        let workspace_dir = workspace_config.workspace_dir.clone();
        let network = unique_remote_network_config(&workspace_config.workspace_key);

        let waitagent_executable = waitagent_test_executable();
        let entry_runtime = WorkspaceEntryRuntime::new(
            WorkspaceRuntime::new(WorkspaceService::new(backend.clone())),
            WorkspaceLayoutRuntime::new_for_tests(
                backend.clone(),
                waitagent_executable.clone(),
                network.clone(),
            )
            .expect("workspace layout runtime should build"),
        );
        let workspace = entry_runtime
            .bootstrap_workspace(&workspace_dir)
            .expect("workspace bootstrap should succeed");
        persist_workspace_network_config(&backend, &workspace.workspace_handle, &network)
            .expect("workspace network config should persist");
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
                network.clone(),
            )
            .expect("workspace layout runtime should build"),
            TargetRegistryService::new(
                DefaultTargetCatalogGateway::from_build_env_with_socket_name(
                    workspace.workspace_handle.socket_name.as_str(),
                )
                .expect("target catalog gateway should build"),
            ),
            waitagent_executable.clone(),
            network.clone(),
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

        let remote_runtime_owner =
            RemoteRuntimeOwnerRuntime::new_for_tests(waitagent_executable.clone(), network.clone());
        let remote_authority = format!("10.1.29.130#{}", network.port);
        let remote_target = remote_session_with_selector(
            remote_authority.as_str(),
            "remote-exit-1",
            &local_target,
            ManagedSessionTaskState::Input,
        );
        let remote_target_2 = remote_session_with_selector(
            remote_authority.as_str(),
            "remote-exit-2",
            &local_target,
            ManagedSessionTaskState::Running,
        );
        remote_runtime_owner
            .upsert_session(remote_authority.as_str(), &remote_target)
            .expect("first remote target should be discoverable on workspace socket");
        remote_runtime_owner
            .upsert_session(remote_authority.as_str(), &remote_target_2)
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
            .run_activate_target(ActivateTargetCommand {
                current_socket_name: workspace.workspace_handle.socket_name.as_str().to_string(),
                current_session_name: workspace.workspace_handle.session_name.as_str().to_string(),
                target: remote_target_2.address.qualified_target(),
            })
            .expect("second remote target activation should succeed");
        wait_for_condition(|| {
            backend
                .show_session_option(&workspace.workspace_handle, WAITAGENT_ACTIVE_TARGET_OPTION)
                .expect("active target should read")
                .as_deref()
                == Some(remote_target_2.address.qualified_target().as_str())
        });
        let next_remote_pane_id = backend
            .show_session_option(&workspace.workspace_handle, WAITAGENT_MAIN_PANE_OPTION)
            .expect("next remote main pane option should read")
            .expect("next remote main pane option should be populated");
        assert_ne!(next_remote_pane_id, main_pane_id);
        runtime
            .run_activate_target(ActivateTargetCommand {
                current_socket_name: workspace.workspace_handle.socket_name.as_str().to_string(),
                current_session_name: workspace.workspace_handle.session_name.as_str().to_string(),
                target: remote_target.address.qualified_target(),
            })
            .expect("first remote target reactivation should succeed");
        wait_for_condition(|| {
            backend
                .show_session_option(&workspace.workspace_handle, WAITAGENT_ACTIVE_TARGET_OPTION)
                .expect("active target should read")
                .as_deref()
                == Some(remote_target.address.qualified_target().as_str())
        });
        let main_pane = crate::infra::tmux::TmuxPaneId::new(main_pane_id.clone());
        backend
            .set_pane_option(
                &workspace.workspace_handle,
                &main_pane,
                "remain-on-exit",
                "on",
            )
            .expect("remote pane should remain after clean exit");
        backend
            .unset_pane_hook(&workspace.workspace_handle, &main_pane, MAIN_PANE_DIED_HOOK)
            .expect("manual remote exit simulation should disable automatic pane-died recovery");
        backend
            .run_on_socket(
                &workspace.workspace_handle.socket_name,
                &[
                    "respawn-pane".to_string(),
                    "-k".to_string(),
                    "-t".to_string(),
                    main_pane_id.clone(),
                    "exit 0".to_string(),
                ],
            )
            .expect("remote pane should become a retained dead pane");
        wait_for_condition(|| !pane_is_live(&backend, &workspace.workspace_handle, &main_pane_id));

        runtime
            .run_remote_target_exited(RemoteTargetExitedCommand {
                socket_name: workspace.workspace_handle.socket_name.as_str().to_string(),
                session_name: workspace.workspace_handle.session_name.as_str().to_string(),
                target: remote_target.address.qualified_target(),
                pane_id: Some(main_pane_id.clone()),
            })
            .expect("remote target exit should recover to another remote target");

        wait_for_condition(|| {
            let active_target = backend
                .show_session_option(&workspace.workspace_handle, WAITAGENT_ACTIVE_TARGET_OPTION)
                .expect("active target should read");
            active_target.as_deref() == Some(remote_target_2.address.qualified_target().as_str())
        });
        let snapshot = remote_runtime_owner
            .snapshot()
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
        assert!(
            !remote_sessions.contains(&remote_target.address.qualified_target()),
            "exited remote target should be removed from the runtime owner snapshot"
        );
        assert!(
            remote_sessions.contains(&remote_target_2.address.qualified_target()),
            "next remote target should remain in the runtime owner snapshot"
        );
        wait_for_condition(|| {
            workspace_main_pane_command(&backend, &workspace.workspace_handle).as_deref()
                == Some("waitagent")
        });
        let recovered_main_pane = backend
            .show_session_option(&workspace.workspace_handle, WAITAGENT_MAIN_PANE_OPTION)
            .expect("main pane option should read after recovery")
            .expect("main pane option should remain populated after recovery");
        assert_eq!(recovered_main_pane, next_remote_pane_id);
        let pane_died_hook = pane_hook_command(
            &backend,
            &workspace.workspace_handle,
            &recovered_main_pane,
            "pane-died[10]",
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
    fn remote_target_exit_activates_existing_next_remote_pane() {
        let _guard = crate::test_support::integration_test_lock();
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let workspace_config = unique_workspace_config("remote-exit-next-remote-pane");
        let workspace_dir = workspace_config.workspace_dir.clone();

        let waitagent_executable = waitagent_test_executable();
        let network = unique_remote_network_config(&workspace_config.workspace_key);
        let entry_runtime = WorkspaceEntryRuntime::new(
            WorkspaceRuntime::new(WorkspaceService::new(backend.clone())),
            WorkspaceLayoutRuntime::new_for_tests(
                backend.clone(),
                waitagent_executable.clone(),
                network.clone(),
            )
            .expect("workspace layout runtime should build"),
        );
        let workspace = entry_runtime
            .bootstrap_workspace(&workspace_dir)
            .expect("workspace bootstrap should succeed");
        persist_workspace_network_config(&backend, &workspace.workspace_handle, &network)
            .expect("workspace network config should persist");
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
                network.clone(),
            )
            .expect("workspace layout runtime should build"),
            TargetRegistryService::new(
                DefaultTargetCatalogGateway::from_build_env_with_socket_name(
                    workspace.workspace_handle.socket_name.as_str(),
                )
                .expect("target catalog gateway should build"),
            ),
            waitagent_executable.clone(),
            network.clone(),
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

        let remote_runtime_owner =
            RemoteRuntimeOwnerRuntime::new_for_tests(waitagent_executable.clone(), network.clone());
        let remote_authority = format!("127.0.0.1#{}", network.port);
        let next_target = remote_session_with_selector(
            remote_authority.as_str(),
            "remote-exit-next-a",
            &local_target,
            ManagedSessionTaskState::Input,
        );
        let exiting_target = remote_session_with_selector(
            remote_authority.as_str(),
            "remote-exit-next-b",
            &local_target,
            ManagedSessionTaskState::Running,
        );
        remote_runtime_owner
            .upsert_session(remote_authority.as_str(), &next_target)
            .expect("next remote target should be discoverable");
        remote_runtime_owner
            .upsert_session(remote_authority.as_str(), &exiting_target)
            .expect("exiting remote target should be discoverable");

        runtime
            .run_activate_target(ActivateTargetCommand {
                current_socket_name: workspace.workspace_handle.socket_name.as_str().to_string(),
                current_session_name: workspace.workspace_handle.session_name.as_str().to_string(),
                target: next_target.address.qualified_target(),
            })
            .expect("next target activation should create its session pane");
        wait_for_condition(|| {
            backend
                .show_session_option(&workspace.workspace_handle, WAITAGENT_ACTIVE_TARGET_OPTION)
                .expect("active target should read")
                .as_deref()
                == Some(next_target.address.qualified_target().as_str())
        });
        let next_pane_option = format!(
            "@waitagent_session_pane_{}",
            next_target.address.qualified_target().replace(':', ".")
        );
        let next_pane_before = backend
            .show_session_option(&workspace.workspace_handle, &next_pane_option)
            .expect("next pane option should read")
            .expect("next pane option should be populated");
        backend
            .run_on_socket(
                &workspace.workspace_handle.socket_name,
                &[
                    "respawn-pane".to_string(),
                    "-k".to_string(),
                    "-t".to_string(),
                    next_pane_before.clone(),
                    "sleep 60".to_string(),
                ],
            )
            .expect("next pane should remain live for activation");
        wait_for_condition(|| {
            pane_is_live(&backend, &workspace.workspace_handle, &next_pane_before)
        });

        runtime
            .run_activate_target(ActivateTargetCommand {
                current_socket_name: workspace.workspace_handle.socket_name.as_str().to_string(),
                current_session_name: workspace.workspace_handle.session_name.as_str().to_string(),
                target: exiting_target.address.qualified_target(),
            })
            .expect("exiting target activation should succeed");
        wait_for_condition(|| {
            backend
                .show_session_option(&workspace.workspace_handle, WAITAGENT_ACTIVE_TARGET_OPTION)
                .expect("active target should read")
                .as_deref()
                == Some(exiting_target.address.qualified_target().as_str())
        });
        let exiting_pane = backend
            .show_session_option(&workspace.workspace_handle, WAITAGENT_MAIN_PANE_OPTION)
            .expect("main pane option should read")
            .expect("main pane option should be populated");
        assert_ne!(next_pane_before, exiting_pane);

        runtime
            .run_remote_target_exited(RemoteTargetExitedCommand {
                socket_name: workspace.workspace_handle.socket_name.as_str().to_string(),
                session_name: workspace.workspace_handle.session_name.as_str().to_string(),
                target: exiting_target.address.qualified_target(),
                pane_id: Some(exiting_pane.clone()),
            })
            .expect("remote target exit should activate existing next pane");

        wait_for_condition(|| {
            backend
                .show_session_option(&workspace.workspace_handle, WAITAGENT_ACTIVE_TARGET_OPTION)
                .expect("active target should read")
                .as_deref()
                == Some(next_target.address.qualified_target().as_str())
        });
        let next_pane_after = backend
            .show_session_option(&workspace.workspace_handle, &next_pane_option)
            .expect("next pane option should read after exit")
            .expect("next pane option should stay populated after exit");
        assert_eq!(next_pane_after, next_pane_before);
        let main_pane_after = backend
            .show_session_option(&workspace.workspace_handle, WAITAGENT_MAIN_PANE_OPTION)
            .expect("main pane option should read after exit")
            .expect("main pane option should remain populated after exit");
        assert_eq!(main_pane_after, next_pane_before);

        kill_server(&backend, &workspace.workspace_handle);
        let _ = fs::remove_dir_all(workspace_dir);
    }

    #[test]
    fn remote_main_pane_exit_uses_pane_identity_when_active_target_already_changed() {
        let _guard = crate::test_support::integration_test_lock();
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let workspace_config = unique_workspace_config("remote-exit-active-target-race");
        let workspace_dir = workspace_config.workspace_dir.clone();

        let waitagent_executable = waitagent_test_executable();
        let network = unique_remote_network_config(&workspace_config.workspace_key);
        let entry_runtime = WorkspaceEntryRuntime::new(
            WorkspaceRuntime::new(WorkspaceService::new(backend.clone())),
            WorkspaceLayoutRuntime::new_for_tests(
                backend.clone(),
                waitagent_executable.clone(),
                network.clone(),
            )
            .expect("workspace layout runtime should build"),
        );
        let workspace = entry_runtime
            .bootstrap_workspace(&workspace_dir)
            .expect("workspace bootstrap should succeed");
        persist_workspace_network_config(&backend, &workspace.workspace_handle, &network)
            .expect("workspace network config should persist");
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
                network.clone(),
            )
            .expect("workspace layout runtime should build"),
            TargetRegistryService::new(
                DefaultTargetCatalogGateway::from_build_env_with_socket_name(
                    workspace.workspace_handle.socket_name.as_str(),
                )
                .expect("target catalog gateway should build"),
            ),
            waitagent_executable.clone(),
            network.clone(),
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

        let remote_runtime_owner =
            RemoteRuntimeOwnerRuntime::new_for_tests(waitagent_executable.clone(), network.clone());
        let remote_authority = format!("127.0.0.1#{}", network.port);
        let next_target = remote_session_with_selector(
            remote_authority.as_str(),
            "remote-exit-race-next",
            &local_target,
            ManagedSessionTaskState::Input,
        );
        let exiting_target = remote_session_with_selector(
            remote_authority.as_str(),
            "remote-exit-race-exiting",
            &local_target,
            ManagedSessionTaskState::Running,
        );
        remote_runtime_owner
            .upsert_session(remote_authority.as_str(), &next_target)
            .expect("next remote target should be discoverable");
        remote_runtime_owner
            .upsert_session(remote_authority.as_str(), &exiting_target)
            .expect("exiting remote target should be discoverable");

        runtime
            .run_activate_target(ActivateTargetCommand {
                current_socket_name: workspace.workspace_handle.socket_name.as_str().to_string(),
                current_session_name: workspace.workspace_handle.session_name.as_str().to_string(),
                target: next_target.address.qualified_target(),
            })
            .expect("next target activation should create its session pane");
        wait_for_condition(|| {
            backend
                .show_session_option(&workspace.workspace_handle, WAITAGENT_ACTIVE_TARGET_OPTION)
                .expect("active target should read")
                .as_deref()
                == Some(next_target.address.qualified_target().as_str())
        });
        let next_pane_option = format!(
            "@waitagent_session_pane_{}",
            next_target.address.qualified_target().replace(':', ".")
        );
        let next_pane_before = backend
            .show_session_option(&workspace.workspace_handle, &next_pane_option)
            .expect("next pane option should read")
            .expect("next pane option should be populated");

        runtime
            .run_activate_target(ActivateTargetCommand {
                current_socket_name: workspace.workspace_handle.socket_name.as_str().to_string(),
                current_session_name: workspace.workspace_handle.session_name.as_str().to_string(),
                target: exiting_target.address.qualified_target(),
            })
            .expect("exiting target activation should succeed");
        wait_for_condition(|| {
            backend
                .show_session_option(&workspace.workspace_handle, WAITAGENT_ACTIVE_TARGET_OPTION)
                .expect("active target should read")
                .as_deref()
                == Some(exiting_target.address.qualified_target().as_str())
        });
        let exiting_pane = backend
            .show_session_option(&workspace.workspace_handle, WAITAGENT_MAIN_PANE_OPTION)
            .expect("main pane option should read")
            .expect("main pane option should be populated");
        assert_ne!(next_pane_before, exiting_pane);
        let exiting_pane_id = crate::infra::tmux::TmuxPaneId::new(exiting_pane.clone());
        backend
            .set_pane_option(
                &workspace.workspace_handle,
                &exiting_pane_id,
                "remain-on-exit",
                "on",
            )
            .expect("remote pane should remain after clean exit");
        backend
            .unset_pane_hook(
                &workspace.workspace_handle,
                &exiting_pane_id,
                MAIN_PANE_DIED_HOOK,
            )
            .expect("manual remote exit simulation should disable automatic pane-died recovery");
        backend
            .run_on_socket(
                &workspace.workspace_handle.socket_name,
                &[
                    "respawn-pane".to_string(),
                    "-k".to_string(),
                    "-t".to_string(),
                    exiting_pane.clone(),
                    "exit 0".to_string(),
                ],
            )
            .expect("remote pane should become a retained dead pane");
        wait_for_condition(|| !pane_is_live(&backend, &workspace.workspace_handle, &exiting_pane));

        backend
            .set_session_option(
                &workspace.workspace_handle,
                WAITAGENT_ACTIVE_TARGET_OPTION,
                next_target.address.qualified_target().as_str(),
            )
            .expect("active target should be able to change before exit event is handled");

        runtime
            .run_remote_target_exited(RemoteTargetExitedCommand {
                socket_name: workspace.workspace_handle.socket_name.as_str().to_string(),
                session_name: workspace.workspace_handle.session_name.as_str().to_string(),
                target: exiting_target.address.qualified_target(),
                pane_id: Some(exiting_pane.clone()),
            })
            .expect("remote target exit should recover based on pane identity");

        wait_for_condition(|| {
            backend
                .show_session_option(&workspace.workspace_handle, WAITAGENT_ACTIVE_TARGET_OPTION)
                .expect("active target should read")
                .as_deref()
                == Some(next_target.address.qualified_target().as_str())
        });
        let main_pane_after = backend
            .show_session_option(&workspace.workspace_handle, WAITAGENT_MAIN_PANE_OPTION)
            .expect("main pane option should read after exit")
            .expect("main pane option should remain populated after exit");
        assert_eq!(main_pane_after, next_pane_before);
        assert!(
            !pane_exists(&backend, &workspace.workspace_handle, &exiting_pane),
            "exiting pane should be destroyed only after replacement main pane is installed"
        );

        kill_server(&backend, &workspace.workspace_handle);
        let _ = fs::remove_dir_all(workspace_dir);
    }

    #[test]
    fn session_pane_lookup_ignores_corrupted_cache_and_chrome_panes() {
        let _guard = crate::test_support::integration_test_lock();
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let workspace_config = unique_workspace_config("remote-exit-sidebar-owner");
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
        let remote_target_a = remote_session_with_selector(
            "10.1.29.130#7474",
            "remote-exit-sidebar-a",
            &local_target,
            ManagedSessionTaskState::Input,
        );
        remote_runtime_owner
            .upsert_session("10.1.29.130#7474", &remote_target_a)
            .expect("first remote target should be discoverable");

        runtime
            .run_activate_target(ActivateTargetCommand {
                current_socket_name: workspace.workspace_handle.socket_name.as_str().to_string(),
                current_session_name: workspace.workspace_handle.session_name.as_str().to_string(),
                target: remote_target_a.address.qualified_target(),
            })
            .expect("first remote target activation should succeed");

        wait_for_condition(|| {
            backend
                .show_session_option(&workspace.workspace_handle, WAITAGENT_ACTIVE_TARGET_OPTION)
                .expect("active target should read")
                .as_deref()
                == Some(remote_target_a.address.qualified_target().as_str())
        });

        let window = backend
            .current_window(&workspace.workspace_handle)
            .expect("workspace window should exist");
        let sidebar_pane = backend
            .list_panes(&workspace.workspace_handle, &window)
            .expect("workspace panes should list")
            .into_iter()
            .find(|pane| pane.title == SIDEBAR_PANE_TITLE && !pane.is_dead)
            .expect("sidebar pane should exist")
            .pane_id;
        let option_name = format!(
            "@waitagent_session_pane_{}",
            remote_target_a.address.qualified_target().replace(':', ".")
        );
        let remote_session_pane = backend
            .show_session_option(&workspace.workspace_handle, &option_name)
            .expect("remote session pane option should read")
            .expect("remote session pane option should be populated");
        assert_ne!(remote_session_pane, sidebar_pane.as_str());
        backend
            .set_session_option(
                &workspace.workspace_handle,
                &option_name,
                sidebar_pane.as_str(),
            )
            .expect("session pane option should accept corrupted sidebar owner");
        let recovered_pane = runtime
            .find_session_pane(
                &workspace.workspace_handle,
                remote_target_a.address.qualified_target().as_str(),
            )
            .expect("session pane lookup should tolerate corrupted cache")
            .expect("owned remote content pane should still be discoverable");
        assert_eq!(recovered_pane.as_str(), remote_session_pane.as_str());
        assert_eq!(
            backend
                .show_session_option(&workspace.workspace_handle, &option_name)
                .expect("session pane option should read")
                .as_deref(),
            None
        );

        backend
            .set_pane_option(
                &workspace.workspace_handle,
                &crate::infra::tmux::TmuxPaneId::new(remote_session_pane.clone()),
                "@waitagent_session_instance_id",
                "different-session",
            )
            .expect("session owner metadata should be corruptible for test");
        let missing_pane = runtime
            .find_session_pane(
                &workspace.workspace_handle,
                remote_target_a.address.qualified_target().as_str(),
            )
            .expect("session pane lookup should tolerate stale owner metadata");
        assert!(
            missing_pane.is_none(),
            "session pane lookup must not borrow another session's content pane"
        );

        kill_server(&backend, &workspace.workspace_handle);
        let _ = fs::remove_dir_all(workspace_dir);
    }

    #[test]
    fn remote_activation_from_sidebar_after_remote_exit_restores_focus_to_remote_main_pane() {
        let _guard = crate::test_support::integration_test_lock();
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let workspace_config = unique_workspace_config("remote-exit-sidebar-focus");
        let workspace_dir = workspace_config.workspace_dir.clone();
        let network = unique_remote_network_config(&workspace_config.workspace_key);

        let waitagent_executable = waitagent_test_executable();
        let entry_runtime = WorkspaceEntryRuntime::new(
            WorkspaceRuntime::new(WorkspaceService::new(backend.clone())),
            WorkspaceLayoutRuntime::new_for_tests(
                backend.clone(),
                waitagent_executable.clone(),
                network.clone(),
            )
            .expect("workspace layout runtime should build"),
        );
        let workspace = entry_runtime
            .bootstrap_workspace(&workspace_dir)
            .expect("workspace bootstrap should succeed");
        persist_workspace_network_config(&backend, &workspace.workspace_handle, &network)
            .expect("workspace network config should persist");
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
                network.clone(),
            )
            .expect("workspace layout runtime should build"),
            TargetRegistryService::new(
                DefaultTargetCatalogGateway::from_build_env_with_socket_name(
                    workspace.workspace_handle.socket_name.as_str(),
                )
                .expect("target catalog gateway should build"),
            ),
            waitagent_executable.clone(),
            network.clone(),
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

        let remote_runtime_owner =
            RemoteRuntimeOwnerRuntime::new_for_tests(waitagent_executable.clone(), network.clone());
        let remote_authority = format!("10.1.29.130#{}", network.port);
        let remote_target_a = remote_session_with_selector(
            &remote_authority,
            "remote-exit-focus-a",
            &local_target,
            ManagedSessionTaskState::Input,
        );
        let remote_target_b = remote_session_with_selector(
            &remote_authority,
            "remote-exit-focus-b",
            &local_target,
            ManagedSessionTaskState::Input,
        );
        remote_runtime_owner
            .upsert_session(&remote_authority, &remote_target_a)
            .expect("first remote target should be discoverable on workspace socket");
        remote_runtime_owner
            .upsert_session(&remote_authority, &remote_target_b)
            .expect("second remote target should be discoverable on workspace socket");

        runtime
            .run_activate_target(ActivateTargetCommand {
                current_socket_name: workspace.workspace_handle.socket_name.as_str().to_string(),
                current_session_name: workspace.workspace_handle.session_name.as_str().to_string(),
                target: remote_target_a.address.qualified_target(),
            })
            .expect("first remote target activation should succeed");

        wait_for_condition(|| {
            let active_target = backend
                .show_session_option(&workspace.workspace_handle, WAITAGENT_ACTIVE_TARGET_OPTION)
                .expect("active target should read");
            active_target.as_deref() == Some(remote_target_a.address.qualified_target().as_str())
        });

        let exited_pane_id = backend
            .show_session_option(&workspace.workspace_handle, WAITAGENT_MAIN_PANE_OPTION)
            .expect("main pane option should read")
            .expect("main pane option should be populated");
        runtime
            .run_remote_target_exited(RemoteTargetExitedCommand {
                socket_name: workspace.workspace_handle.socket_name.as_str().to_string(),
                session_name: workspace.workspace_handle.session_name.as_str().to_string(),
                target: remote_target_a.address.qualified_target(),
                pane_id: Some(exited_pane_id),
            })
            .expect("remote target exit should recover");

        wait_for_condition(|| {
            let active_target = backend
                .show_session_option(&workspace.workspace_handle, WAITAGENT_ACTIVE_TARGET_OPTION)
                .expect("active target should read");
            active_target.as_deref() == Some(remote_target_b.address.qualified_target().as_str())
        });

        runtime
            .run_activate_target(ActivateTargetCommand {
                current_socket_name: workspace.workspace_handle.socket_name.as_str().to_string(),
                current_session_name: workspace.workspace_handle.session_name.as_str().to_string(),
                target: local_target.clone(),
            })
            .expect("local re-activation should succeed");

        wait_for_condition(|| {
            let active_target = backend
                .show_session_option(&workspace.workspace_handle, WAITAGENT_ACTIVE_TARGET_OPTION)
                .expect("active target should read");
            active_target.as_deref() == Some(local_target.as_str())
        });

        let window = backend
            .current_window(&workspace.workspace_handle)
            .expect("workspace window should exist");
        let sidebar_pane = backend
            .list_panes(&workspace.workspace_handle, &window)
            .expect("workspace panes should list")
            .into_iter()
            .find(|pane| pane.title == SIDEBAR_PANE_TITLE && !pane.is_dead)
            .expect("sidebar pane should exist")
            .pane_id;
        backend
            .select_pane(&workspace.workspace_handle, &sidebar_pane)
            .expect("sidebar pane should become current");

        runtime
            .run_activate_target(ActivateTargetCommand {
                current_socket_name: workspace.workspace_handle.socket_name.as_str().to_string(),
                current_session_name: workspace.workspace_handle.session_name.as_str().to_string(),
                target: remote_target_b.address.qualified_target(),
            })
            .expect("second remote target activation should succeed");

        wait_for_condition(|| {
            let active_target = backend
                .show_session_option(&workspace.workspace_handle, WAITAGENT_ACTIVE_TARGET_OPTION)
                .expect("active target should read");
            active_target.as_deref() == Some(remote_target_b.address.qualified_target().as_str())
        });
        wait_for_condition(|| {
            workspace_main_pane_command(&backend, &workspace.workspace_handle).as_deref()
                == Some("waitagent")
        });
        wait_for_condition(|| {
            current_workspace_pane_command(&backend, &workspace.workspace_handle).as_deref()
                == Some("waitagent")
        });

        let current_pane = backend
            .current_pane(&workspace.workspace_handle)
            .expect("current pane should read after remote re-activation");
        let main_pane = backend
            .show_session_option(&workspace.workspace_handle, WAITAGENT_MAIN_PANE_OPTION)
            .expect("main pane option should read after remote re-activation")
            .expect("main pane option should be populated after remote re-activation");
        assert_eq!(current_pane.as_str(), main_pane);

        kill_server(&backend, &workspace.workspace_handle);
        let _ = fs::remove_dir_all(workspace_dir);
    }

    #[test]
    fn remote_main_pane_exit_recovery_ignores_corrupted_main_pane_option() {
        let _guard = crate::test_support::integration_test_lock();
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
            .upsert_session("peer-a", &remote_target)
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
        let recovered_main_pane = backend
            .show_session_option(&workspace.workspace_handle, WAITAGENT_MAIN_PANE_OPTION)
            .expect("main pane option should read after recovery")
            .expect("main pane option should remain populated");
        assert_eq!(recovered_main_pane, recovery_pane_id);
        assert!(
            pane_exists(&backend, &workspace.workspace_handle, &recovered_main_pane),
            "recovery pane should remain available after fallback"
        );

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

    fn unique_remote_network_config(seed: &str) -> RemoteNetworkConfig {
        let hash = seed.bytes().fold(0u16, |acc, byte| {
            acc.wrapping_mul(31).wrapping_add(byte as u16)
        });
        RemoteNetworkConfig {
            port: 20000 + (hash % 20000),
            connect: None,
            node_id: None,
            public_endpoint: None,
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
            initial_program: None,
        }
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

    fn current_workspace_pane_command(
        backend: &EmbeddedTmuxBackend,
        workspace: &TmuxWorkspaceHandle,
    ) -> Option<String> {
        let current_pane = backend.current_pane(workspace).ok()?;
        let window = backend.current_window(workspace).ok()?;
        let panes = backend.list_panes(workspace, &window).ok()?;
        panes
            .into_iter()
            .find(|pane| pane.pane_id == current_pane)
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

    fn pane_exists(
        backend: &EmbeddedTmuxBackend,
        workspace: &TmuxWorkspaceHandle,
        pane_id: &str,
    ) -> bool {
        let output = backend
            .run_on_socket(
                &workspace.socket_name,
                &[
                    "list-panes".to_string(),
                    "-a".to_string(),
                    "-F".to_string(),
                    "#{pane_id}".to_string(),
                ],
            )
            .expect("pane list should read");
        output.stdout.lines().any(|line| line == pane_id)
    }

    fn pane_is_live(
        backend: &EmbeddedTmuxBackend,
        workspace: &TmuxWorkspaceHandle,
        pane_id: &str,
    ) -> bool {
        let output = backend
            .run_on_socket(
                &workspace.socket_name,
                &[
                    "list-panes".to_string(),
                    "-a".to_string(),
                    "-F".to_string(),
                    "#{pane_id}\t#{pane_dead}".to_string(),
                ],
            )
            .expect("pane liveness should list");
        output.stdout.lines().any(|line| {
            let mut parts = line.split('\t');
            parts.next() == Some(pane_id) && parts.next() == Some("0")
        })
    }

    fn pane_is_chrome(
        backend: &EmbeddedTmuxBackend,
        workspace: &TmuxWorkspaceHandle,
        pane_id: &str,
    ) -> bool {
        let output = backend
            .run_on_socket(
                &workspace.socket_name,
                &[
                    "list-panes".to_string(),
                    "-a".to_string(),
                    "-F".to_string(),
                    "#{pane_id}\t#{pane_title}".to_string(),
                ],
            )
            .expect("pane title list should read");
        output.stdout.lines().any(|line| {
            let mut parts = line.split('\t');
            let listed_pane = parts.next().unwrap_or_default();
            let title = parts.next().unwrap_or_default();
            listed_pane == pane_id && (title == SIDEBAR_PANE_TITLE || title == FOOTER_PANE_TITLE)
        })
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
            if let Some(command) = line.strip_prefix(&format!("{hook_name} ")) {
                return Some(command.trim().to_string());
            }
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
