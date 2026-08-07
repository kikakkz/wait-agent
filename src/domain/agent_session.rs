use std::fmt;
use std::path::PathBuf;
use std::time::SystemTime;

/// Metadata for a resumable agent session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSession {
    pub id: String,
    pub title: Option<String>,
    pub last_prompt: Option<String>,
    pub cwd: Option<PathBuf>,
    pub updated_at: Option<SystemTime>,
}

/// A command that can be used to resume a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeCommand {
    pub program: String,
    pub args: Vec<String>,
}

/// Errors that can occur while listing agent sessions.
#[derive(Debug)]
pub enum AgentSessionError {
    /// The agent's data directory could not be located.
    HomeNotFound,
    /// Reading a session index file failed.
    IndexRead(std::io::Error),
    /// A session entry could not be parsed.
    Parse(String),
}

impl fmt::Display for AgentSessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HomeNotFound => write!(f, "agent data directory not found"),
            Self::IndexRead(error) => write!(f, "failed to read session index: {error}"),
            Self::Parse(message) => write!(f, "failed to parse session entry: {message}"),
        }
    }
}

impl std::error::Error for AgentSessionError {}

/// Plugin trait for reading an agent tool's local session list.
///
/// Implementations are intentionally synchronous: the files are local and small,
/// and the trait must also be usable on the remote peer node where an async
/// runtime may not be appropriate for simple filesystem reads.
#[allow(dead_code)]
pub trait AgentSessionProvider: Send + Sync + std::fmt::Debug {
    /// Human-readable agent name, e.g. "kimi", "codex", "claude".
    fn name(&self) -> &'static str;

    /// List all resumable sessions for this agent.
    fn list_sessions(&self) -> Result<Vec<AgentSession>, AgentSessionError>;

    /// Build the CLI command that resumes the given session.
    fn resume_command(&self, session: &AgentSession) -> ResumeCommand;
}

/// Registry that dispatches session-listing queries to the appropriate
/// `AgentSessionProvider` by agent name.
#[derive(Debug)]
pub struct AgentSessionRegistry {
    providers: Vec<Box<dyn AgentSessionProvider>>,
}

#[allow(dead_code)]
impl AgentSessionRegistry {
    pub fn new() -> Self {
        Self {
            providers: Vec::new(),
        }
    }

    pub fn register(&mut self, provider: Box<dyn AgentSessionProvider>) {
        self.providers.push(provider);
    }

    /// Returns the provider registered for `agent`, if any.
    pub fn provider_for(&self, agent: &str) -> Option<&dyn AgentSessionProvider> {
        self.providers
            .iter()
            .find(|provider| provider.name() == agent)
            .map(|provider| provider.as_ref())
    }

    /// Lists sessions for `agent` using the registered provider.
    ///
    /// Returns an empty vector when no provider is registered for the agent.
    pub fn list_for(&self, agent: &str) -> Result<Vec<AgentSession>, AgentSessionError> {
        self.provider_for(agent)
            .map(|provider| provider.list_sessions())
            .unwrap_or(Ok(Vec::new()))
    }
}

impl Default for AgentSessionRegistry {
    fn default() -> Self {
        let mut registry = Self::new();
        registry.register(Box::new(
            super::agent_session_kimi::KimiSessionProvider::new(),
        ));
        registry.register(Box::new(
            super::agent_session_codex::CodexSessionProvider::new(),
        ));
        registry.register(Box::new(
            super::agent_session_claude::ClaudeSessionProvider::new(),
        ));
        registry
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct DummyProvider;

    impl AgentSessionProvider for DummyProvider {
        fn name(&self) -> &'static str {
            "dummy"
        }

        fn list_sessions(&self) -> Result<Vec<AgentSession>, AgentSessionError> {
            Ok(vec![AgentSession {
                id: "dummy-1".into(),
                title: Some("Dummy Session".into()),
                last_prompt: None,
                cwd: Some(PathBuf::from("/tmp")),
                updated_at: None,
            }])
        }

        fn resume_command(&self, session: &AgentSession) -> ResumeCommand {
            ResumeCommand {
                program: "dummy".into(),
                args: vec!["resume".into(), session.id.clone()],
            }
        }
    }

    #[test]
    fn registry_routes_by_agent_name() {
        let mut registry = AgentSessionRegistry::new();
        registry.register(Box::new(DummyProvider));

        let sessions = registry.list_for("dummy").unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "dummy-1");

        assert!(registry.list_for("unknown").unwrap().is_empty());
    }

    #[test]
    fn default_registry_includes_kimi() {
        let registry = AgentSessionRegistry::default();
        assert!(registry.provider_for("kimi").is_some());
    }

    #[test]
    fn default_registry_includes_codex_and_claude() {
        let registry = AgentSessionRegistry::default();
        assert!(registry.provider_for("codex").is_some());
        assert!(registry.provider_for("claude").is_some());
    }
}
