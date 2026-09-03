use crate::lifecycle::LifecycleError;
use crate::process::agent_signal_sender_bundle::extract_agent_signal_sender;
use std::collections::HashMap;
#[cfg(unix)]
use std::process::Command;

/// Environment variables and `PROMPT_COMMAND` needed by a shell so that
/// `waitagent-agent-signal-send` can report the current working directory back
/// to the node server on every prompt.
#[derive(Debug, Clone)]
pub struct AgentSignalEnv {
    pub socket_path: String,
    pub socket_name: String,
    pub target_session_name: String,
    pub session_id: String,
    pub token: String,
}

impl AgentSignalEnv {
    /// Insert all required variables into the environment map used by
    /// `alacritty_terminal` to spawn a local PTY session.
    pub fn apply_to_hashmap(
        &self,
        env: &mut HashMap<String, String>,
    ) -> Result<(), LifecycleError> {
        env.insert(
            "WAITAGENT_SIGNAL_SOCKET".to_string(),
            self.socket_path.clone(),
        );
        env.insert(
            "WAITAGENT_SOCKET_NAME".to_string(),
            self.socket_name.clone(),
        );
        env.insert(
            "WAITAGENT_TARGET_SESSION_NAME".to_string(),
            self.target_session_name.clone(),
        );
        env.insert("WAITAGENT_SESSION_ID".to_string(), self.session_id.clone());
        // Deprecated alias kept for one release; new code should read WAITAGENT_SESSION_ID.
        env.insert("WAITAGENT_PANE_ID".to_string(), self.session_id.clone());
        env.insert(
            "WAITAGENT_AGENT_SIGNAL_TOKEN".to_string(),
            self.token.clone(),
        );
        env.insert("PROMPT_COMMAND".to_string(), self.prompt_command()?);
        Ok(())
    }

    /// Insert all required variables into a `std::process::Command` used to
    /// spawn an authority-host PTY session.
    ///
    /// Only the Unix authority-host spawn path uses this; the Windows ConPTY
    /// path and the alacritty PTY path use [`AgentSignalEnv::apply_to_hashmap`].
    #[cfg(unix)]
    pub fn apply_to_command(&self, cmd: &mut Command) -> Result<(), LifecycleError> {
        cmd.env("WAITAGENT_SIGNAL_SOCKET", &self.socket_path);
        cmd.env("WAITAGENT_SOCKET_NAME", &self.socket_name);
        cmd.env("WAITAGENT_TARGET_SESSION_NAME", &self.target_session_name);
        cmd.env("WAITAGENT_SESSION_ID", &self.session_id);
        // Deprecated alias kept for one release; new code should read WAITAGENT_SESSION_ID.
        cmd.env("WAITAGENT_PANE_ID", &self.session_id);
        cmd.env("WAITAGENT_AGENT_SIGNAL_TOKEN", &self.token);
        cmd.env("PROMPT_COMMAND", self.prompt_command()?);
        Ok(())
    }

    fn prompt_command(&self) -> Result<String, LifecycleError> {
        let cwd_cmd = self.cwd_command()?;
        match std::env::var("PROMPT_COMMAND") {
            Ok(existing) if !existing.trim().is_empty() => Ok(format!("{cwd_cmd}; {existing}")),
            _ => Ok(cwd_cmd),
        }
    }

    fn cwd_command(&self) -> Result<String, LifecycleError> {
        let sender_path = extract_agent_signal_sender()?;
        let quoted = shell_single_quote(sender_path.to_string_lossy().as_ref());
        Ok(format!("printf '%s' \"$PWD\" | {quoted} cwd"))
    }
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use super::AgentSignalEnv;
    use std::collections::HashMap;

    #[test]
    fn agent_signal_env_serializes_pane_id() {
        let env = AgentSignalEnv {
            socket_path: "/tmp/s".into(),
            socket_name: "ratatui-1".into(),
            target_session_name: "sess".into(),
            session_id: "sess".into(),
            token: "tok".into(),
        };
        let mut map = HashMap::new();
        env.apply_to_hashmap(&mut map).unwrap();
        assert_eq!(map.get("WAITAGENT_PANE_ID"), Some(&"sess".to_string()));
    }

    #[test]
    fn agent_signal_env_serializes_session_id() {
        let env = AgentSignalEnv {
            socket_path: "/tmp/s".into(),
            socket_name: "ratatui-1".into(),
            target_session_name: "sess".into(),
            session_id: "sess".into(),
            token: "tok".into(),
        };
        let mut map = HashMap::new();
        env.apply_to_hashmap(&mut map).unwrap();
        assert_eq!(map.get("WAITAGENT_SESSION_ID"), Some(&"sess".to_string()));
    }
}
