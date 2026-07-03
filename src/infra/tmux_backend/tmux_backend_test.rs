mod tests {
    use super::super::EmbeddedTmuxBackend;
    use crate::domain::agent_detector::DetectorRegistry;
    use crate::domain::workspace::{
        WorkspaceInstanceConfig, WorkspaceInstanceId, WorkspaceSessionRole,
    };
    use crate::infra::tmux_backend::process_inspector;
    use crate::infra::tmux_error::tmux_socket_dir;
    use crate::infra::tmux_glue::TmuxGlueBuildStatus;
    use crate::infra::tmux_types::{
        TmuxGateway, TmuxLayoutGateway, TmuxProgram, TmuxSessionGateway, TmuxSessionName,
        TmuxSocketName, TmuxSplitSize, TmuxWindowHandle, TmuxWindowId, TmuxWorkspaceHandle,
    };
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn pane_info_parser_reads_current_command_path_and_pid() {
        let pane = EmbeddedTmuxBackend::pane_info_for_line("%1\t4242\tmain\tbash\t/tmp/demo\t0")
            .expect("pane line should parse");

        assert_eq!(pane.pane_id.as_str(), "%1");
        assert_eq!(pane.pane_pid, Some(4242));
        assert_eq!(pane.title, "main");
        assert_eq!(pane.current_command.as_deref(), Some("bash"));
        assert_eq!(pane.current_path.as_deref(), Some(Path::new("/tmp/demo")));
        assert!(!pane.is_dead);
    }

    #[test]
    fn detector_registry_recognizes_codex_from_process() {
        let registry = DetectorRegistry::default();

        assert_eq!(registry.detect_command_name("codex", None, ""), "codex");

        let argv = vec!["node".to_string(), "/usr/local/bin/codex".to_string()];
        assert_eq!(
            registry.detect_command_name("node", Some(argv.as_slice()), ""),
            "codex"
        );

        assert_eq!(registry.detect_command_name("bash", None, ""), "bash");
    }

    #[test]
    fn detector_registry_does_not_detect_identity_from_pane_text() {
        let registry = DetectorRegistry::default();

        assert_eq!(
            registry.detect_command_name("bash", None, "skip codex"),
            "bash"
        );

        assert_eq!(
            registry.detect_command_name("bash", None, "Type your message here"),
            "bash"
        );

        let stale_codex_then_shell = "│ >_ OpenAI Codex (v0.142.0) │\n\
                                      │ › old prompt                 │\n\
                                      root@host:~#";
        assert_eq!(
            registry.detect_command_name("bash", None, stale_codex_then_shell),
            "bash"
        );
    }

    #[test]
    fn detector_registry_detects_kimi_from_process_only() {
        let registry = DetectorRegistry::default();

        assert_eq!(registry.detect_command_name("kimi", None, ""), "kimi");
        assert_eq!(registry.detect_command_name("kimi-code", None, ""), "kimi");
        assert_eq!(
            registry.detect_command_name(
                "node",
                Some(&["/usr/local/bin/kimi-code".to_string()]),
                ""
            ),
            "kimi"
        );
        assert_eq!(
            registry.detect_command_name("bash", None, "Welcome to Kimi Code!"),
            "bash"
        );
    }

    #[test]
    fn parse_process_children_reads_pid_list() {
        assert_eq!(
            process_inspector::parse_process_children("1279695 1279696\n"),
            vec![1279695, 1279696]
        );
        assert!(process_inspector::parse_process_children("").is_empty());
    }

    #[test]
    fn parse_process_stat_reads_foreground_process_group() {
        let stat =
            "1279306 (bash) S 1279214 1279306 1279214 34828 1279695 4194560 8421 150 0 0 12 3 0 0 20 0 1 0 1 2 3";
        let parsed = process_inspector::parse_process_stat(stat).expect("stat should parse");

        assert_eq!(
            parsed,
            process_inspector::ProcessStat {
                pid: 1279306,
                process_group_id: 1279306,
                tty_nr: 34828,
                foreground_process_group_id: 1279695,
            }
        );
    }

    #[test]
    fn foreground_process_prefers_group_leader_on_same_tty() {
        let shell = process_inspector::ProcessStat {
            pid: 100,
            process_group_id: 100,
            tty_nr: 42,
            foreground_process_group_id: 200,
        };
        let descendants = vec![
            process_inspector::ProcessStat {
                pid: 201,
                process_group_id: 200,
                tty_nr: 42,
                foreground_process_group_id: 200,
            },
            process_inspector::ProcessStat {
                pid: 200,
                process_group_id: 200,
                tty_nr: 42,
                foreground_process_group_id: 200,
            },
            process_inspector::ProcessStat {
                pid: 300,
                process_group_id: 300,
                tty_nr: 99,
                foreground_process_group_id: 300,
            },
        ];

        assert_eq!(
            process_inspector::foreground_process_id_for_shell(&shell, &descendants),
            Some(200)
        );
    }

    fn workspace_config() -> WorkspaceInstanceConfig {
        WorkspaceInstanceConfig {
            workspace_dir: Path::new("/tmp").to_path_buf(),
            workspace_key: "wk-1".to_string(),
            socket_name: "sock-1".to_string(),
            session_name: "sess-1".to_string(),
            session_role: WorkspaceSessionRole::WorkspaceChrome,
            initial_rows: None,
            initial_cols: None,
            initial_program: None,
        }
    }

    fn workspace_handle() -> TmuxWorkspaceHandle {
        TmuxWorkspaceHandle {
            workspace_id: WorkspaceInstanceId::new("wk-1"),
            socket_name: TmuxSocketName::new("sock-1"),
            session_name: TmuxSessionName::new("sess-1"),
        }
    }

    #[test]
    fn embedded_backend_returns_workspace_handle_from_build_env() {
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");

        let handle = backend
            .ensure_workspace(&workspace_config())
            .expect("workspace handle should build");
        kill_server(&backend, &handle);

        assert_eq!(handle.workspace_id.as_str(), "wk-1");
        assert_eq!(handle.socket_name.as_str(), "sock-1");
        assert_eq!(handle.session_name.as_str(), "sess-1");
        assert_eq!(backend.build_status(), &TmuxGlueBuildStatus::Executed);
        assert!(
            backend.artifacts().tmux_binary_path.exists(),
            "embedded tmux binary should be extracted: {}",
            backend.artifacts().tmux_binary_path.display()
        );
        assert!(
            backend
                .artifacts()
                .tmux_binary_path
                .to_string_lossy()
                .contains("waitagent/tmux"),
            "embedded tmux binary should be in waitagent data dir: {}",
            backend.artifacts().tmux_binary_path.display()
        );
    }

    #[test]
    fn system_default_backend_does_not_require_vendored_artifact_files() {
        let backend = EmbeddedTmuxBackend::system_default();

        assert_eq!(backend.artifacts().tmux_binary_path, PathBuf::from("tmux"));
        backend
            .validate_runtime_artifacts()
            .expect("system tmux fallback should skip vendored artifact validation");
    }

    #[test]
    fn embedded_backend_reuses_existing_workspace_session() {
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let config = unique_workspace_config("workspace");
        let workspace = backend
            .ensure_workspace(&config)
            .expect("workspace bootstrap should succeed");
        let workspace_again = backend
            .ensure_workspace(&config)
            .expect("workspace bootstrap should be idempotent");

        let sessions = backend
            .list_sessions_on_socket(&workspace.socket_name)
            .expect("session list should succeed");
        kill_server(&backend, &workspace);

        let matching = sessions
            .into_iter()
            .filter(|record| record.address.session_id() == workspace.session_name.as_str())
            .count();

        assert_eq!(workspace, workspace_again);
        assert_eq!(matching, 1);
    }

    #[test]
    fn embedded_backend_executes_real_window_and_pane_commands() {
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let workspace = backend
            .ensure_workspace(&unique_workspace_config("layout"))
            .expect("workspace bootstrap should succeed");

        let created_window = backend
            .create_window(&workspace, "codex")
            .expect("window handle should build");
        let right = backend
            .split_pane_right(&workspace, &created_window, 24)
            .expect("right pane should build");
        let bottom = backend
            .split_pane_bottom(&workspace, &created_window, 18)
            .expect("bottom pane should build");
        backend
            .select_window(&workspace, &created_window)
            .expect("window selection should succeed");
        backend
            .select_pane(&workspace, &right)
            .expect("pane selection should succeed");
        backend
            .enter_copy_mode(&workspace, &right)
            .expect("copy mode should succeed");

        let panes = backend
            .list_panes(&workspace, &created_window)
            .expect("pane listing should succeed");
        kill_server(&backend, &workspace);

        let active_pane = panes
            .iter()
            .find(|pane| pane.pane_id == right)
            .expect("split pane should exist");

        assert!(created_window.window_id.as_str().starts_with('@'));
        assert!(right.as_str().starts_with('%'));
        assert!(bottom.as_str().starts_with('%'));
        assert_eq!(active_pane.pane_id, right);
    }

    #[test]
    fn target_presentation_pane_prefers_configured_live_main_pane() {
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let workspace = backend
            .ensure_workspace(&unique_workspace_config("presentation-source"))
            .expect("workspace bootstrap should succeed");
        let target_config = WorkspaceInstanceConfig {
            workspace_dir: workspace_config().workspace_dir,
            workspace_key: "target-key".to_string(),
            socket_name: workspace.socket_name.as_str().to_string(),
            session_name: "target-session".to_string(),
            session_role: WorkspaceSessionRole::TargetHost,
            initial_rows: None,
            initial_cols: None,
            initial_program: None,
        };
        let target = backend
            .ensure_workspace(&target_config)
            .expect("target session should be created");
        let workspace_main = backend
            .current_pane(&workspace)
            .expect("workspace main pane should resolve");

        backend
            .run_on_socket(
                &workspace.socket_name,
                &[
                    "set-option".to_string(),
                    "-t".to_string(),
                    target.session_name.as_str().to_string(),
                    "@waitagent_main_pane_id".to_string(),
                    workspace_main.as_str().to_string(),
                ],
            )
            .expect("target main pane option should be set");
        backend
            .run_on_socket(
                &workspace.socket_name,
                &[
                    "set-option".to_string(),
                    "-pt".to_string(),
                    workspace_main.as_str().to_string(),
                    "@waitagent_target_session_name".to_string(),
                    target.session_name.as_str().to_string(),
                ],
            )
            .expect("pane target session option should be set");
        let resolved = backend
            .target_presentation_pane_on_socket(
                workspace.socket_name.as_str(),
                target.session_name.as_str(),
            )
            .expect("presentation pane should resolve");
        backend
            .run_on_socket(
                &workspace.socket_name,
                &[
                    "set-option".to_string(),
                    "-t".to_string(),
                    target.session_name.as_str().to_string(),
                    "@waitagent_main_pane_id".to_string(),
                    "%999999".to_string(),
                ],
            )
            .expect("stale target main pane option should be set");
        let stale_error = backend
            .target_presentation_pane_on_socket(
                workspace.socket_name.as_str(),
                target.session_name.as_str(),
            )
            .expect_err("stale authoritative presentation pane should fail");
        kill_server(&backend, &workspace);

        assert_eq!(resolved, workspace_main);
        assert!(stale_error.to_string().contains("is not live"));
    }

    #[test]
    fn target_presentation_pane_requires_configured_pane() {
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let workspace = backend
            .ensure_workspace(&unique_workspace_config("presentation-required"))
            .expect("workspace bootstrap should succeed");
        let target_config = WorkspaceInstanceConfig {
            workspace_dir: workspace_config().workspace_dir,
            workspace_key: "target-key".to_string(),
            socket_name: workspace.socket_name.as_str().to_string(),
            session_name: "target-session".to_string(),
            session_role: WorkspaceSessionRole::TargetHost,
            initial_rows: None,
            initial_cols: None,
            initial_program: None,
        };
        let target = backend
            .ensure_workspace(&target_config)
            .expect("target session should be created");
        let workspace_main = backend
            .current_pane(&workspace)
            .expect("workspace main pane should resolve");

        backend
            .run_on_socket(
                &workspace.socket_name,
                &[
                    "set-option".to_string(),
                    "-pt".to_string(),
                    workspace_main.as_str().to_string(),
                    "@waitagent_pane_role".to_string(),
                    "content".to_string(),
                ],
            )
            .expect("pane role should be set");
        backend
            .run_on_socket(
                &workspace.socket_name,
                &[
                    "set-option".to_string(),
                    "-pt".to_string(),
                    workspace_main.as_str().to_string(),
                    "@waitagent_session_instance_id".to_string(),
                    target.session_name.as_str().to_string(),
                ],
            )
            .expect("pane owner should be set");

        let error = backend
            .target_presentation_pane_on_socket(
                workspace.socket_name.as_str(),
                target.session_name.as_str(),
            )
            .expect_err("owned content pane without explicit binding should fail");
        kill_server(&backend, &workspace);

        assert!(error
            .to_string()
            .contains("has no authoritative presentation pane"));
    }

    #[test]
    fn target_presentation_pane_rejects_identity_mismatch() {
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let workspace = backend
            .ensure_workspace(&unique_workspace_config("presentation-identity"))
            .expect("workspace bootstrap should succeed");
        let target_config = WorkspaceInstanceConfig {
            workspace_dir: workspace_config().workspace_dir,
            workspace_key: "target-key".to_string(),
            socket_name: workspace.socket_name.as_str().to_string(),
            session_name: "target-session".to_string(),
            session_role: WorkspaceSessionRole::TargetHost,
            initial_rows: None,
            initial_cols: None,
            initial_program: None,
        };
        let target = backend
            .ensure_workspace(&target_config)
            .expect("target session should be created");
        let workspace_main = backend
            .current_pane(&workspace)
            .expect("workspace main pane should resolve");

        backend
            .run_on_socket(
                &workspace.socket_name,
                &[
                    "set-option".to_string(),
                    "-t".to_string(),
                    target.session_name.as_str().to_string(),
                    "@waitagent_main_pane_id".to_string(),
                    workspace_main.as_str().to_string(),
                ],
            )
            .expect("target main pane option should be set");
        backend
            .run_on_socket(
                &workspace.socket_name,
                &[
                    "set-option".to_string(),
                    "-pt".to_string(),
                    workspace_main.as_str().to_string(),
                    "@waitagent_target_session_name".to_string(),
                    "another-target".to_string(),
                ],
            )
            .expect("pane target session option should be set");

        let error = backend
            .target_presentation_pane_on_socket(
                workspace.socket_name.as_str(),
                target.session_name.as_str(),
            )
            .expect_err("identity mismatch should fail");
        kill_server(&backend, &workspace);

        assert!(error.to_string().contains("belongs to target session"));
    }

    #[test]
    fn session_runtime_uses_target_pane_not_stale_presentation_pane() {
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let workspace = backend
            .ensure_workspace(&unique_workspace_config("runtime-target-source"))
            .expect("workspace bootstrap should succeed");
        let target_config = WorkspaceInstanceConfig {
            workspace_dir: workspace_config().workspace_dir,
            workspace_key: "target-key".to_string(),
            socket_name: workspace.socket_name.as_str().to_string(),
            session_name: "target-session".to_string(),
            session_role: WorkspaceSessionRole::TargetHost,
            initial_rows: None,
            initial_cols: None,
            initial_program: None,
        };
        let target = backend
            .ensure_workspace(&target_config)
            .expect("target session should be created");
        let workspace_main = backend
            .current_pane(&workspace)
            .expect("workspace main pane should resolve");

        backend
            .run_on_socket(
                &workspace.socket_name,
                &[
                    "send-keys".to_string(),
                    "-t".to_string(),
                    workspace_main.as_str().to_string(),
                    "Welcome to Kimi Code!".to_string(),
                    "Enter".to_string(),
                    "K2.7 Code thinking  ~".to_string(),
                    "Enter".to_string(),
                ],
            )
            .expect("stale presentation text should be written");
        backend
            .run_on_socket(
                &workspace.socket_name,
                &[
                    "select-pane".to_string(),
                    "-t".to_string(),
                    workspace_main.as_str().to_string(),
                    "-T".to_string(),
                    "Kimi Code".to_string(),
                ],
            )
            .expect("presentation title should be set");
        backend
            .run_on_socket(
                &workspace.socket_name,
                &[
                    "set-option".to_string(),
                    "-t".to_string(),
                    target.session_name.as_str().to_string(),
                    "@waitagent_main_pane_id".to_string(),
                    workspace_main.as_str().to_string(),
                ],
            )
            .expect("target presentation pane should be configured");
        backend
            .run_on_socket(
                &workspace.socket_name,
                &[
                    "set-option".to_string(),
                    "-pt".to_string(),
                    workspace_main.as_str().to_string(),
                    "@waitagent_target_session_name".to_string(),
                    target.session_name.as_str().to_string(),
                ],
            )
            .expect("presentation target option should be set");

        let sessions = backend
            .list_sessions_on_socket(&workspace.socket_name)
            .expect("sessions should list");
        kill_server(&backend, &workspace);

        let target_record = sessions
            .iter()
            .find(|session| session.address.session_id() == target.session_name.as_str())
            .expect("target session should be present");
        assert_eq!(target_record.command_name.as_deref(), Some("bash"));
        assert_eq!(
            target_record.task_state,
            crate::domain::session_catalog::ManagedSessionTaskState::Input
        );
    }

    #[test]
    fn embedded_backend_sets_new_workspace_history_limit_before_session_creation() {
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let workspace = backend
            .ensure_workspace(&unique_workspace_config("history-limit"))
            .expect("workspace bootstrap should succeed");

        let output = backend
            .run_on_socket(
                &workspace.socket_name,
                &[
                    "show-options".to_string(),
                    "-g".to_string(),
                    "history-limit".to_string(),
                ],
            )
            .expect("history-limit should be visible");
        kill_server(&backend, &workspace);

        assert!(output.stdout.contains("history-limit 100000"));
    }

    #[test]
    fn embedded_backend_creates_non_login_shell_for_new_workspace_session() {
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let workspace = backend
            .ensure_workspace(&unique_workspace_config("shell-kind"))
            .expect("workspace bootstrap should succeed");

        let output = backend
            .run_on_socket(
                &workspace.socket_name,
                &[
                    "list-panes".to_string(),
                    "-a".to_string(),
                    "-F".to_string(),
                    "#{pane_pid}".to_string(),
                ],
            )
            .expect("pane pid should resolve");
        let pane_pid = output
            .stdout
            .lines()
            .next()
            .expect("workspace should have a pane")
            .trim()
            .to_string();
        let ps = Command::new("ps")
            .args(["-o", "args=", "-p", &pane_pid])
            .output()
            .expect("ps should run");
        kill_server(&backend, &workspace);

        let command_line = String::from_utf8_lossy(&ps.stdout).trim().to_string();
        assert!(!command_line.starts_with('-'));
        assert!(command_line.contains("bash") || command_line.contains("sh"));
    }

    #[test]
    fn embedded_backend_reports_current_window_and_runs_pane_programs() {
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let workspace = backend
            .ensure_workspace(&unique_workspace_config("pane-prog"))
            .expect("workspace bootstrap should succeed");
        let window = backend
            .current_window(&workspace)
            .expect("current window should resolve");
        let main = backend
            .current_pane(&workspace)
            .expect("current pane should resolve");
        let program =
            TmuxProgram::new("/bin/sh").with_args(vec!["-c".to_string(), "sleep 30".to_string()]);

        let sidebar = backend
            .split_pane_right_with_program(&workspace, &main, TmuxSplitSize::Cells(24), &program)
            .expect("program-backed sidebar pane should spawn");
        backend
            .set_pane_title(&workspace, &sidebar, "waitagent-sidebar")
            .expect("pane title should be set");
        backend
            .set_pane_width(&workspace, &sidebar, 24)
            .expect("sidebar width should be set");
        let footer = backend
            .split_pane_bottom_with_program(
                &workspace,
                &main,
                TmuxSplitSize::Cells(2),
                true,
                &program,
            )
            .expect("program-backed footer pane should spawn");
        backend
            .set_pane_title(&workspace, &footer, "waitagent-footer")
            .expect("footer pane title should be set");
        backend
            .set_pane_height(&workspace, &footer, 2)
            .expect("footer height should be set");
        let panes = backend
            .list_panes(&workspace, &window)
            .expect("pane listing should succeed");
        kill_server(&backend, &workspace);

        assert!(panes.iter().any(|pane| pane.title == "waitagent-sidebar"));
        assert!(panes.iter().any(|pane| pane.title == "waitagent-footer"));
    }

    #[test]
    fn capture_pane_ansi_on_socket_excludes_scrollback_history() {
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let workspace = backend
            .ensure_workspace(&unique_workspace_config("capture-visible"))
            .expect("workspace bootstrap should succeed");
        let main = backend
            .current_pane(&workspace)
            .expect("current pane should resolve");
        let (_, height) = backend
            .pane_dimensions_on_socket(workspace.socket_name.as_str(), main.as_str())
            .expect("pane dimensions should resolve");
        let scroll_lines = (height.max(2) * 3) + 10;

        backend
            .run_on_socket(
                &workspace.socket_name,
                &[
                    "send-keys".to_string(),
                    "-t".to_string(),
                    main.as_str().to_string(),
                    format!(
                        "printf 'history-line\\n'; for i in $(seq 1 {scroll_lines}); do printf 'visible-%02d\\n' \"$i\"; done"
                    ),
                    "Enter".to_string(),
                ],
            )
            .expect("pane should receive scripted output");
        thread::sleep(Duration::from_millis(500));

        let captured = backend
            .capture_pane_ansi_on_socket(workspace.socket_name.as_str(), main.as_str())
            .expect("ansi capture should succeed");
        kill_server(&backend, &workspace);

        assert!(captured.contains("visible-"));
        assert!(!captured.contains("history-line"));
    }

    #[test]
    fn split_percentages_must_be_nonzero() {
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let workspace = workspace_handle();
        let window = TmuxWindowHandle {
            workspace_id: WorkspaceInstanceId::new("wk-1"),
            window_id: TmuxWindowId::new("@3"),
        };

        let error = backend
            .split_pane_right(&workspace, &window, 0)
            .expect_err("zero-width split should fail");

        assert!(error.to_string().contains("right split width"));
    }

    #[test]
    fn embedded_backend_lists_waitagent_sessions_with_workspace_metadata() {
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let config = unique_workspace_config("listing");
        let workspace = backend
            .ensure_workspace(&config)
            .expect("workspace bootstrap should succeed");

        let sessions = backend
            .list_sessions()
            .expect("managed session listing should succeed");
        kill_server(&backend, &workspace);

        let record = sessions
            .into_iter()
            .find(|session| session.address.session_id() == workspace.session_name.as_str())
            .expect("workspace session should be listed");

        assert_eq!(record.address.server_id(), workspace.socket_name.as_str());
        assert_eq!(
            record.workspace_dir.as_deref(),
            Some(config.workspace_dir.as_path())
        );
        assert_eq!(
            record.workspace_key.as_deref(),
            Some(config.workspace_key.as_str())
        );
    }

    #[test]
    fn tmux_socket_dir_matches_tmux_uid_convention() {
        let socket_dir = tmux_socket_dir();
        assert!(socket_dir.to_string_lossy().contains("/tmux-"));
    }

    #[test]
    fn chrome_refresh_signal_wakes_multiple_workspace_waiters() {
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let workspace = backend
            .ensure_workspace(&unique_workspace_config("chrome-refresh"))
            .expect("workspace bootstrap should succeed");
        let (done_tx, done_rx) = mpsc::channel();

        for _ in 0..2 {
            let backend = backend.clone();
            let socket_name = workspace.socket_name.as_str().to_string();
            let session_name = workspace.session_name.as_str().to_string();
            let done_tx = done_tx.clone();
            thread::spawn(move || {
                backend
                    .wait_for_chrome_refresh_on_socket(&socket_name, &session_name)
                    .expect("wait-for should unblock cleanly");
                done_tx
                    .send(())
                    .expect("waiter completion should be reported");
            });
        }

        thread::sleep(Duration::from_millis(100));
        backend
            .signal_chrome_refresh_on_socket(
                workspace.socket_name.as_str(),
                workspace.session_name.as_str(),
            )
            .expect("chrome refresh signal should succeed");

        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("first waiter should wake");
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("second waiter should wake");
        kill_server(&backend, &workspace);
    }

    #[test]
    fn chrome_refresh_signal_buffers_between_wait_calls() {
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let workspace = backend
            .ensure_workspace(&unique_workspace_config("chrome-refresh-buffered"))
            .expect("workspace bootstrap should succeed");
        let backend_for_waiter = backend.clone();
        let socket_name = workspace.socket_name.as_str().to_string();
        let session_name = workspace.session_name.as_str().to_string();
        let (first_done_tx, first_done_rx) = mpsc::channel();
        let (start_second_tx, start_second_rx) = mpsc::channel();
        let (second_done_tx, second_done_rx) = mpsc::channel();
        thread::spawn(move || {
            backend_for_waiter
                .wait_for_chrome_refresh_on_socket(&socket_name, &session_name)
                .expect("first wait should unblock");
            first_done_tx
                .send(())
                .expect("first waiter completion should be reported");
            start_second_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("second wait should be released by the test");
            backend_for_waiter
                .wait_for_chrome_refresh_on_socket(&socket_name, &session_name)
                .expect("second wait should consume buffered refresh");
            second_done_tx
                .send(())
                .expect("second waiter completion should be reported");
        });

        thread::sleep(Duration::from_millis(100));
        backend
            .signal_chrome_refresh_on_socket(
                workspace.socket_name.as_str(),
                workspace.session_name.as_str(),
            )
            .expect("first chrome refresh signal should succeed");
        first_done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("first waiter should wake");

        backend
            .signal_chrome_refresh_on_socket(
                workspace.socket_name.as_str(),
                workspace.session_name.as_str(),
            )
            .expect("second chrome refresh signal should succeed");
        start_second_tx
            .send(())
            .expect("second wait should be released");
        second_done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("second waiter should consume buffered refresh");
        kill_server(&backend, &workspace);
    }

    #[test]
    fn chrome_refresh_owner_exits_when_last_subscriber_disconnects() {
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let workspace = backend
            .ensure_workspace(&unique_workspace_config("chrome-refresh-owner-exit"))
            .expect("workspace bootstrap should succeed");
        let socket_path = super::super::chrome_refresh_owner_socket_path(
            workspace.socket_name.as_str(),
            workspace.session_name.as_str(),
        );
        let backend_for_waiter = backend.clone();
        let socket_name = workspace.socket_name.as_str().to_string();
        let session_name = workspace.session_name.as_str().to_string();
        let (done_tx, done_rx) = mpsc::channel();
        thread::spawn(move || {
            backend_for_waiter
                .wait_for_chrome_refresh_on_socket(&socket_name, &session_name)
                .expect("wait should unblock");
            done_tx.send(()).expect("waiter should report completion");
        });

        thread::sleep(Duration::from_millis(100));
        backend
            .signal_chrome_refresh_on_socket(
                workspace.socket_name.as_str(),
                workspace.session_name.as_str(),
            )
            .expect("chrome refresh signal should succeed");
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("waiter should wake");

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while socket_path.exists() && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !socket_path.exists(),
            "owner socket should be removed after the last subscriber disconnects"
        );
        kill_server(&backend, &workspace);
    }

    #[test]
    fn initial_chrome_ready_signals_wake_sidebar_and_footer_waiters() {
        let backend = EmbeddedTmuxBackend::from_build_env()
            .expect("vendored tmux backend should discover build env");
        let workspace = backend
            .ensure_workspace(&unique_workspace_config("chrome-ready"))
            .expect("workspace bootstrap should succeed");
        let (done_tx, done_rx) = mpsc::channel();

        {
            let backend = backend.clone();
            let socket_name = workspace.socket_name.as_str().to_string();
            let session_name = workspace.session_name.as_str().to_string();
            let done_tx = done_tx.clone();
            thread::spawn(move || {
                backend
                    .wait_for_sidebar_ready_on_socket(&socket_name, &session_name)
                    .expect("sidebar wait-for should unblock cleanly");
                done_tx
                    .send("sidebar")
                    .expect("sidebar waiter completion should be reported");
            });
        }

        {
            let backend = backend.clone();
            let socket_name = workspace.socket_name.as_str().to_string();
            let session_name = workspace.session_name.as_str().to_string();
            let done_tx = done_tx.clone();
            thread::spawn(move || {
                backend
                    .wait_for_footer_ready_on_socket(&socket_name, &session_name)
                    .expect("footer wait-for should unblock cleanly");
                done_tx
                    .send("footer")
                    .expect("footer waiter completion should be reported");
            });
        }

        thread::sleep(Duration::from_millis(100));
        backend
            .signal_sidebar_ready_on_socket(
                workspace.socket_name.as_str(),
                workspace.session_name.as_str(),
            )
            .expect("sidebar ready signal should succeed");
        backend
            .signal_footer_ready_on_socket(
                workspace.socket_name.as_str(),
                workspace.session_name.as_str(),
            )
            .expect("footer ready signal should succeed");

        let first = done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("first waiter should wake");
        let second = done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("second waiter should wake");
        assert_ne!(first, second);
        kill_server(&backend, &workspace);
    }

    fn unique_workspace_config(prefix: &str) -> WorkspaceInstanceConfig {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after unix epoch")
            .as_nanos();
        let workspace_dir = std::env::temp_dir().join(format!("waitagent-{prefix}-{nonce:x}"));
        std::fs::create_dir_all(&workspace_dir)
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

    fn kill_server(backend: &EmbeddedTmuxBackend, workspace: &TmuxWorkspaceHandle) {
        let _ = backend.run_workspace_command(workspace, &["kill-server".to_string()]);
    }
}
