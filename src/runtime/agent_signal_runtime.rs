use crate::domain::agent_detector::DetectorRegistry;
use crate::domain::agent_signal::{AgentSignalEnvelope, AgentStateEffect};
#[cfg(test)]
use crate::domain::workspace::{WorkspaceInstanceConfig, WorkspaceSessionRole};
use crate::infra::error_log::ERROR_LOG;
use crate::infra::tmux::{
    EmbeddedTmuxBackend, TmuxLayoutGateway, TmuxSessionName, TmuxSocketName, TmuxWorkspaceHandle,
    WAITAGENT_AGENT_SIGNAL_AGENT_OPTION, WAITAGENT_AGENT_SIGNAL_PANE_OPTION,
    WAITAGENT_AGENT_SIGNAL_STATE_OPTION, WAITAGENT_AGENT_SIGNAL_TOKEN_OPTION,
    WAITAGENT_AGENT_SIGNAL_UPDATED_AT_OPTION,
};
use crate::lifecycle::LifecycleError;
use crate::runtime::remote_target_publication_runtime::RemoteTargetPublicationRuntime;
use crate::runtime::workspace_layout_runtime::WorkspaceLayoutRuntime;
use std::fs;
use std::io::{self, ErrorKind};
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub struct AgentSignalRuntime {
    backend: EmbeddedTmuxBackend,
    layout_runtime: WorkspaceLayoutRuntime,
    publication_runtime: RemoteTargetPublicationRuntime,
    socket_name: String,
    socket_path: PathBuf,
    registry: DetectorRegistry,
}

impl AgentSignalRuntime {
    pub fn new(
        backend: EmbeddedTmuxBackend,
        layout_runtime: WorkspaceLayoutRuntime,
        publication_runtime: RemoteTargetPublicationRuntime,
        socket_name: impl Into<String>,
    ) -> Self {
        let socket_name = socket_name.into();
        Self {
            backend,
            layout_runtime,
            publication_runtime,
            socket_path: agent_signal_socket_path(&socket_name),
            socket_name,
            registry: DetectorRegistry::default(),
        }
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub fn start_background(self) -> Result<thread::JoinHandle<()>, LifecycleError> {
        let socket = bind_signal_socket(&self.socket_path).map_err(agent_signal_error)?;
        let socket_path = self.socket_path.clone();
        let handle = thread::spawn(move || {
            self.run_loop(socket);
            let _ = fs::remove_file(socket_path);
        });
        Ok(handle)
    }

    fn run_loop(self, socket: UnixDatagram) {
        let mut buf = [0u8; 64 * 1024];
        while self
            .backend
            .socket_is_live(&TmuxSocketName::new(&self.socket_name))
        {
            match socket.recv(&mut buf) {
                Ok(len) => self.handle_bytes(&buf[..len]),
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(25));
                }
                Err(error) => {
                    ERROR_LOG.log(format!("[agent-signal] recv failed: {error}"));
                    thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }

    fn handle_bytes(&self, bytes: &[u8]) {
        let Ok(signal) = serde_json::from_slice::<AgentSignalEnvelope>(bytes) else {
            ERROR_LOG.log("[agent-signal] ignored invalid JSON".to_string());
            return;
        };
        if let Err(error) = self.apply_signal(signal) {
            ERROR_LOG.log(format!("[agent-signal] ignored signal: {error}"));
        }
    }

    fn apply_signal(&self, signal: AgentSignalEnvelope) -> Result<(), String> {
        if signal.version != 1 {
            return Err(format!("unsupported version {}", signal.version));
        }
        if signal.socket != self.socket_name {
            return Err("socket mismatch".to_string());
        }
        let workspace = TmuxWorkspaceHandle {
            workspace_id: crate::domain::workspace::WorkspaceInstanceId::new(
                signal.session.clone(),
            ),
            socket_name: TmuxSocketName::new(&signal.socket),
            session_name: TmuxSessionName::new(signal.session.clone()),
        };
        let expected_token = self
            .backend
            .show_session_option(&workspace, WAITAGENT_AGENT_SIGNAL_TOKEN_OPTION)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "session has no signal token".to_string())?;
        if expected_token != signal.token {
            return Err("token mismatch".to_string());
        }
        if !self.pane_matches(&signal)? {
            return Err("pane mismatch".to_string());
        }
        let effect = self
            .registry
            .signal_state_effect(&signal.agent, &signal.event, &signal.payload)
            .ok_or_else(|| format!("unsupported event `{}`", signal.event))?;
        self.apply_state_update(&workspace, &signal, effect)
            .map_err(|error| error.to_string())?;
        self.refresh(&signal);
        Ok(())
    }

    fn pane_matches(&self, signal: &AgentSignalEnvelope) -> Result<bool, String> {
        let authoritative_pane = self
            .backend
            .target_presentation_pane_on_socket(&signal.socket, &signal.session)
            .or_else(|_| {
                self.backend
                    .target_main_pane_on_socket(&signal.socket, &signal.session)
            })
            .map_err(|error| error.to_string())?;
        if authoritative_pane.as_str() == signal.pane {
            return Ok(true);
        }
        let signal_pane = crate::infra::tmux::TmuxPaneId::new(signal.pane.clone());
        let signal_target = self
            .backend
            .show_pane_option_on_socket(
                &TmuxSocketName::new(signal.socket.clone()),
                &signal_pane,
                "@waitagent_target_session_name",
            )
            .map_err(|error| error.to_string())?;
        Ok(signal_target.as_deref() == Some(signal.session.as_str()))
    }

    fn apply_state_update(
        &self,
        workspace: &TmuxWorkspaceHandle,
        signal: &AgentSignalEnvelope,
        effect: AgentStateEffect,
    ) -> Result<(), crate::infra::tmux::TmuxError> {
        match effect {
            AgentStateEffect::Set(state) => {
                self.backend.set_session_option(
                    workspace,
                    WAITAGENT_AGENT_SIGNAL_AGENT_OPTION,
                    &signal.agent,
                )?;
                self.backend.set_session_option(
                    workspace,
                    WAITAGENT_AGENT_SIGNAL_PANE_OPTION,
                    &signal.pane,
                )?;
                self.backend.set_session_option(
                    workspace,
                    WAITAGENT_AGENT_SIGNAL_STATE_OPTION,
                    state.as_str(),
                )?;
                self.backend.set_session_option(
                    workspace,
                    WAITAGENT_AGENT_SIGNAL_UPDATED_AT_OPTION,
                    &now_millis().to_string(),
                )?;
            }
            AgentStateEffect::Clear => {
                self.backend
                    .clear_session_option(workspace, WAITAGENT_AGENT_SIGNAL_AGENT_OPTION)?;
                self.backend
                    .clear_session_option(workspace, WAITAGENT_AGENT_SIGNAL_PANE_OPTION)?;
                self.backend
                    .clear_session_option(workspace, WAITAGENT_AGENT_SIGNAL_STATE_OPTION)?;
                self.backend
                    .clear_session_option(workspace, WAITAGENT_AGENT_SIGNAL_UPDATED_AT_OPTION)?;
            }
        }
        Ok(())
    }

    fn refresh(&self, signal: &AgentSignalEnvelope) {
        let _ = self
            .layout_runtime
            .run_chrome_refresh_signal_on_socket(&signal.socket);
        let _ = self
            .publication_runtime
            .signal_source_session_refresh(&signal.socket, &signal.session);
        let _ = self
            .publication_runtime
            .signal_local_runtime_changed(&signal.socket);
    }
}

pub fn agent_signal_socket_path(socket_name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "waitagent-agent-signal-{}.sock",
        sanitize_path_component(socket_name)
    ))
}

pub fn generate_agent_signal_token() -> String {
    let mut bytes = [0u8; 16];
    if getrandom::fill(&mut bytes).is_ok() {
        return bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    }
    format!("{}-{}", std::process::id(), now_millis())
}

fn bind_signal_socket(path: &Path) -> io::Result<UnixDatagram> {
    if path.exists() {
        fs::remove_file(path)?;
    }
    let socket = UnixDatagram::bind(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    socket.set_nonblocking(true)?;
    Ok(socket)
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

fn sanitize_path_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn agent_signal_error(error: io::Error) -> LifecycleError {
    LifecycleError::Io("agent signal runtime failed".to_string(), error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::RemoteNetworkConfig;
    use crate::domain::session_catalog::ManagedSessionTaskState;
    use crate::infra::tmux::{TmuxGateway, TmuxSessionGateway};
    use crate::runtime::remote_target_publication_runtime::RemoteTargetPublicationRuntime;
    use crate::runtime::workspace_layout_runtime::WorkspaceLayoutRuntime;
    use serde_json::Value;

    #[test]
    fn socket_path_is_short_and_sanitized() {
        let path = agent_signal_socket_path("wa-a/b:c");
        let path_text = path.to_string_lossy();
        assert!(path_text.contains("waitagent-agent-signal-wa-a_b_c.sock"));
        assert!(path_text.len() < 100);
    }

    #[test]
    fn generated_tokens_are_non_empty_and_distinct() {
        let first = generate_agent_signal_token();
        let second = generate_agent_signal_token();
        assert!(!first.is_empty());
        assert_ne!(first, second);
    }

    #[test]
    fn codex_signal_accepts_target_pane() {
        let fixture = SignalRuntimeFixture::new("agent-signal-owned-pane");
        let signal = fixture.signal("UserPromptSubmit", fixture.target_shell_pane.as_str());

        fixture
            .runtime
            .apply_signal(signal)
            .expect("target pane signal should apply");

        let agent = fixture
            .backend
            .show_session_option(&fixture.target, WAITAGENT_AGENT_SIGNAL_AGENT_OPTION)
            .expect("agent option should read");
        let pane = fixture
            .backend
            .show_session_option(&fixture.target, WAITAGENT_AGENT_SIGNAL_PANE_OPTION)
            .expect("pane option should read");
        let state = fixture
            .backend
            .show_session_option(&fixture.target, WAITAGENT_AGENT_SIGNAL_STATE_OPTION)
            .expect("state option should read");

        assert_eq!(agent.as_deref(), Some("codex"));
        assert_eq!(pane.as_deref(), Some(fixture.target_shell_pane.as_str()));
        assert_eq!(
            state.as_deref(),
            Some(ManagedSessionTaskState::Running.as_str())
        );
    }

    #[test]
    fn codex_signal_rejects_presentation_pane() {
        let fixture = SignalRuntimeFixture::new("agent-signal-wrong-pane");
        fixture
            .backend
            .set_pane_option_on_socket(
                &fixture.target.socket_name,
                &fixture.content_pane,
                "@waitagent_target_session_name",
                "other-target",
            )
            .expect("content target should be changed");
        let signal = fixture.signal("UserPromptSubmit", fixture.content_pane.as_str());

        let error = fixture
            .runtime
            .apply_signal(signal)
            .expect_err("presentation pane should be rejected");
        let state = fixture
            .backend
            .show_session_option(&fixture.target, WAITAGENT_AGENT_SIGNAL_STATE_OPTION)
            .expect("state option should read");

        assert_eq!(error, "pane mismatch");
        assert_eq!(state, None);
    }

    #[test]
    fn codex_signal_accepts_workspace_content_pane_bound_to_target() {
        let fixture = SignalRuntimeFixture::new("agent-signal-content-pane");
        let signal = fixture.signal("UserPromptSubmit", fixture.content_pane.as_str());

        fixture
            .runtime
            .apply_signal(signal)
            .expect("target-bound content pane signal should apply");

        let pane = fixture
            .backend
            .show_session_option(&fixture.target, WAITAGENT_AGENT_SIGNAL_PANE_OPTION)
            .expect("pane option should read");
        let state = fixture
            .backend
            .show_session_option(&fixture.target, WAITAGENT_AGENT_SIGNAL_STATE_OPTION)
            .expect("state option should read");

        assert_eq!(pane.as_deref(), Some(fixture.content_pane.as_str()));
        assert_eq!(
            state.as_deref(),
            Some(ManagedSessionTaskState::Running.as_str())
        );
    }

    #[test]
    fn kimi_session_end_clears_agent_signal_metadata() {
        assert_agent_session_end_clears_metadata("kimi", "agent-signal-kimi-session-end");
    }

    #[test]
    fn claude_session_end_clears_agent_signal_metadata() {
        assert_agent_session_end_clears_metadata("claude", "agent-signal-claude-session-end");
    }

    fn assert_agent_session_end_clears_metadata(agent_name: &str, fixture_name: &str) {
        let fixture = SignalRuntimeFixture::new(fixture_name);
        let mut running = fixture.signal("UserPromptSubmit", fixture.target_shell_pane.as_str());
        running.agent = agent_name.to_string();
        fixture
            .runtime
            .apply_signal(running)
            .expect("running signal should apply");

        let mut ended = fixture.signal("SessionEnd", fixture.target_shell_pane.as_str());
        ended.agent = agent_name.to_string();
        fixture
            .runtime
            .apply_signal(ended)
            .expect("session end should clear metadata");

        let agent = fixture
            .backend
            .show_session_option(&fixture.target, WAITAGENT_AGENT_SIGNAL_AGENT_OPTION)
            .expect("agent option should read");
        let pane = fixture
            .backend
            .show_session_option(&fixture.target, WAITAGENT_AGENT_SIGNAL_PANE_OPTION)
            .expect("pane option should read");
        let state = fixture
            .backend
            .show_session_option(&fixture.target, WAITAGENT_AGENT_SIGNAL_STATE_OPTION)
            .expect("state option should read");
        let updated_at = fixture
            .backend
            .show_session_option(&fixture.target, WAITAGENT_AGENT_SIGNAL_UPDATED_AT_OPTION)
            .expect("updated_at option should read");
        let token = fixture
            .backend
            .show_session_option(&fixture.target, WAITAGENT_AGENT_SIGNAL_TOKEN_OPTION)
            .expect("token option should read");

        assert_eq!(agent, None);
        assert_eq!(pane, None);
        assert_eq!(state, None);
        assert_eq!(updated_at, None);
        assert_eq!(token.as_deref(), Some(fixture.token.as_str()));
    }

    struct SignalRuntimeFixture {
        backend: EmbeddedTmuxBackend,
        runtime: AgentSignalRuntime,
        target: TmuxWorkspaceHandle,
        content_pane: crate::infra::tmux::TmuxPaneId,
        target_shell_pane: crate::infra::tmux::TmuxPaneId,
        token: String,
    }

    impl SignalRuntimeFixture {
        fn new(prefix: &str) -> Self {
            let backend = EmbeddedTmuxBackend::from_build_env()
                .expect("vendored tmux backend should discover build env");
            let workspace = backend
                .ensure_workspace(&unique_workspace_config(
                    prefix,
                    WorkspaceSessionRole::WorkspaceChrome,
                ))
                .expect("workspace should be created");
            let target_config = WorkspaceInstanceConfig {
                workspace_dir: workspace_config_dir(),
                workspace_key: format!("{prefix}-target"),
                socket_name: workspace.socket_name.as_str().to_string(),
                session_name: format!("target-{prefix}"),
                session_role: WorkspaceSessionRole::TargetHost,
                initial_rows: None,
                initial_cols: None,
                initial_program: None,
            };
            let target = backend
                .ensure_workspace(&target_config)
                .expect("target session should be created");
            let content_pane = backend
                .current_pane(&workspace)
                .expect("workspace content pane should resolve");
            let target_shell_pane = backend
                .current_pane(&target)
                .expect("target shell pane should resolve");
            let token = "test-token".to_string();

            backend
                .set_session_option(&target, WAITAGENT_AGENT_SIGNAL_TOKEN_OPTION, &token)
                .expect("token option should be set");
            backend
                .set_pane_option_on_socket(
                    &workspace.socket_name,
                    &content_pane,
                    "@waitagent_pane_role",
                    "content",
                )
                .expect("content role should be set");
            backend
                .set_pane_option_on_socket(
                    &workspace.socket_name,
                    &content_pane,
                    "@waitagent_session_instance_id",
                    target.session_name.as_str(),
                )
                .expect("content owner should be set");
            backend
                .set_pane_option_on_socket(
                    &workspace.socket_name,
                    &content_pane,
                    "@waitagent_target_session_name",
                    target.session_name.as_str(),
                )
                .expect("content target should be set");

            let layout_runtime = WorkspaceLayoutRuntime::new_for_tests(
                backend.clone(),
                PathBuf::from("/usr/local/bin/waitagent"),
                RemoteNetworkConfig::default(),
            )
            .expect("layout runtime should build");
            let publication_runtime =
                RemoteTargetPublicationRuntime::new_for_route_tests_without_remote_runtime_owner()
                    .expect("publication runtime should build");
            let runtime = AgentSignalRuntime::new(
                backend.clone(),
                layout_runtime,
                publication_runtime,
                workspace.socket_name.as_str(),
            );

            Self {
                backend,
                runtime,
                target,
                content_pane,
                target_shell_pane,
                token,
            }
        }

        fn signal(&self, event: &str, pane: &str) -> AgentSignalEnvelope {
            AgentSignalEnvelope {
                version: 1,
                agent: "codex".to_string(),
                event: event.to_string(),
                socket: self.target.socket_name.as_str().to_string(),
                session: self.target.session_name.as_str().to_string(),
                pane: pane.to_string(),
                token: self.token.clone(),
                payload: Value::Null,
            }
        }
    }

    impl Drop for SignalRuntimeFixture {
        fn drop(&mut self) {
            let _ = self.backend.kill_server(&self.target.socket_name);
        }
    }

    fn unique_workspace_config(
        prefix: &str,
        session_role: WorkspaceSessionRole,
    ) -> WorkspaceInstanceConfig {
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
            session_role,
            initial_rows: None,
            initial_cols: None,
            initial_program: None,
        }
    }

    fn workspace_config_dir() -> PathBuf {
        std::env::temp_dir()
    }
}
