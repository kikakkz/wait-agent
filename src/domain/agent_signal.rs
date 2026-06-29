use crate::domain::session_catalog::ManagedSessionTaskState;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSignalEnvelope {
    pub version: u32,
    pub agent: String,
    pub event: String,
    pub socket: String,
    pub session: String,
    pub pane: String,
    pub token: String,
    #[serde(default)]
    pub payload: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStateSource {
    Hook,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentStateUpdate {
    pub effect: AgentStateEffect,
    pub source: AgentStateSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStateEffect {
    Set(ManagedSessionTaskState),
    Clear,
}

pub trait AgentSignalHandler {
    fn handle(&self, signal: &AgentSignalEnvelope) -> Option<AgentStateUpdate>;
}

pub trait AgentSignalHandlerFactory {
    fn create(&self, agent: &str) -> Option<Box<dyn AgentSignalHandler + Send + Sync>>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct BuiltinAgentSignalHandlerFactory;

impl AgentSignalHandlerFactory for BuiltinAgentSignalHandlerFactory {
    fn create(&self, agent: &str) -> Option<Box<dyn AgentSignalHandler + Send + Sync>> {
        match agent {
            "codex" => Some(Box::new(CodexSignalHandler)),
            "claude" => Some(Box::new(ClaudeSignalHandler)),
            "kimi" => Some(Box::new(KimiSignalHandler)),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CodexSignalHandler;

impl AgentSignalHandler for CodexSignalHandler {
    fn handle(&self, signal: &AgentSignalEnvelope) -> Option<AgentStateUpdate> {
        let state = match signal.event.as_str() {
            "UserPromptSubmit" | "PreToolUse" | "PostToolUse" => ManagedSessionTaskState::Running,
            "PermissionRequest" => ManagedSessionTaskState::Confirm,
            "Stop" => ManagedSessionTaskState::Input,
            _ => return None,
        };
        Some(AgentStateUpdate {
            effect: AgentStateEffect::Set(state),
            source: AgentStateSource::Hook,
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ClaudeSignalHandler;

impl AgentSignalHandler for ClaudeSignalHandler {
    fn handle(&self, signal: &AgentSignalEnvelope) -> Option<AgentStateUpdate> {
        let state = match signal.event.as_str() {
            "UserPromptSubmit" | "PreToolUse" | "PostToolUse" | "PostToolBatch" => {
                ManagedSessionTaskState::Running
            }
            "PermissionRequest" => ManagedSessionTaskState::Confirm,
            "Notification" if notification_mentions_permission(&signal.payload) => {
                ManagedSessionTaskState::Confirm
            }
            "Stop" => ManagedSessionTaskState::Input,
            _ => return None,
        };
        Some(AgentStateUpdate {
            effect: AgentStateEffect::Set(state),
            source: AgentStateSource::Hook,
        })
    }
}

fn notification_mentions_permission(payload: &Value) -> bool {
    let lowered = payload.to_string().to_ascii_lowercase();
    lowered.contains("permission")
        || lowered.contains("approve")
        || lowered.contains("approval")
        || lowered.contains("allow")
}

#[derive(Debug, Clone, Copy)]
pub struct KimiSignalHandler;

impl AgentSignalHandler for KimiSignalHandler {
    fn handle(&self, signal: &AgentSignalEnvelope) -> Option<AgentStateUpdate> {
        let state = match signal.event.as_str() {
            "UserPromptSubmit" | "PreToolUse" | "PostToolUse" | "PostToolUseFailure" => {
                ManagedSessionTaskState::Running
            }
            "PermissionRequest" => ManagedSessionTaskState::Confirm,
            "PermissionResult" => permission_result_state(&signal.payload),
            "Stop" | "StopFailure" | "Interrupt" => ManagedSessionTaskState::Input,
            "SessionEnd" => {
                return Some(AgentStateUpdate {
                    effect: AgentStateEffect::Clear,
                    source: AgentStateSource::Hook,
                });
            }
            _ => return None,
        };
        Some(AgentStateUpdate {
            effect: AgentStateEffect::Set(state),
            source: AgentStateSource::Hook,
        })
    }
}

fn permission_result_state(payload: &Value) -> ManagedSessionTaskState {
    let lowered = payload.to_string().to_ascii_lowercase();
    if lowered.contains("deny")
        || lowered.contains("denied")
        || lowered.contains("reject")
        || lowered.contains("\"approved\":false")
        || lowered.contains("\"allow\":false")
    {
        ManagedSessionTaskState::Input
    } else {
        ManagedSessionTaskState::Running
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signal(event: &str) -> AgentSignalEnvelope {
        AgentSignalEnvelope {
            version: 1,
            agent: "codex".to_string(),
            event: event.to_string(),
            socket: "wa-test".to_string(),
            session: "target".to_string(),
            pane: "%1".to_string(),
            token: "secret".to_string(),
            payload: Value::Null,
        }
    }

    #[test]
    fn codex_handler_maps_lifecycle_events_to_states() {
        let handler = CodexSignalHandler;
        assert_eq!(
            handler
                .handle(&signal("UserPromptSubmit"))
                .map(|u| u.effect),
            Some(AgentStateEffect::Set(ManagedSessionTaskState::Running))
        );
        assert_eq!(
            handler
                .handle(&signal("PermissionRequest"))
                .map(|u| u.effect),
            Some(AgentStateEffect::Set(ManagedSessionTaskState::Confirm))
        );
        assert_eq!(
            handler.handle(&signal("PreToolUse")).map(|u| u.effect),
            Some(AgentStateEffect::Set(ManagedSessionTaskState::Running))
        );
        assert_eq!(
            handler.handle(&signal("PostToolUse")).map(|u| u.effect),
            Some(AgentStateEffect::Set(ManagedSessionTaskState::Running))
        );
        assert_eq!(
            handler.handle(&signal("Stop")).map(|u| u.effect),
            Some(AgentStateEffect::Set(ManagedSessionTaskState::Input))
        );
    }

    #[test]
    fn codex_handler_ignores_unknown_events() {
        assert!(CodexSignalHandler.handle(&signal("SessionStart")).is_none());
    }

    #[test]
    fn builtin_factory_creates_supported_handlers() {
        let factory = BuiltinAgentSignalHandlerFactory;
        assert!(factory.create("codex").is_some());
        assert!(factory.create("claude").is_some());
        assert!(factory.create("kimi").is_some());
        assert!(factory.create("unknown").is_none());
    }

    #[test]
    fn claude_handler_maps_core_events_to_states() {
        let handler = ClaudeSignalHandler;
        let mut notification = signal("Notification");
        notification.agent = "kimi".to_string();
        notification.payload = serde_json::json!({"message": "Claude needs permission"});
        assert_eq!(
            handler
                .handle(&signal("UserPromptSubmit"))
                .map(|u| u.effect),
            Some(AgentStateEffect::Set(ManagedSessionTaskState::Running))
        );
        assert_eq!(
            handler
                .handle(&signal("PermissionRequest"))
                .map(|u| u.effect),
            Some(AgentStateEffect::Set(ManagedSessionTaskState::Confirm))
        );
        assert_eq!(
            handler.handle(&signal("PreToolUse")).map(|u| u.effect),
            Some(AgentStateEffect::Set(ManagedSessionTaskState::Running))
        );
        assert_eq!(
            handler.handle(&notification).map(|u| u.effect),
            Some(AgentStateEffect::Set(ManagedSessionTaskState::Confirm))
        );
        assert_eq!(
            handler.handle(&signal("Stop")).map(|u| u.effect),
            Some(AgentStateEffect::Set(ManagedSessionTaskState::Input))
        );
    }

    #[test]
    fn kimi_handler_maps_turn_and_permission_events_to_states() {
        let handler = KimiSignalHandler;
        assert_eq!(
            handler
                .handle(&signal("UserPromptSubmit"))
                .map(|u| u.effect),
            Some(AgentStateEffect::Set(ManagedSessionTaskState::Running))
        );
        assert_eq!(
            handler
                .handle(&signal("PermissionRequest"))
                .map(|u| u.effect),
            Some(AgentStateEffect::Set(ManagedSessionTaskState::Confirm))
        );
        assert_eq!(
            handler
                .handle(&signal("PermissionResult"))
                .map(|u| u.effect),
            Some(AgentStateEffect::Set(ManagedSessionTaskState::Running))
        );
        assert_eq!(
            handler.handle(&signal("StopFailure")).map(|u| u.effect),
            Some(AgentStateEffect::Set(ManagedSessionTaskState::Input))
        );
        assert_eq!(
            handler.handle(&signal("SessionEnd")).map(|u| u.effect),
            Some(AgentStateEffect::Clear)
        );
    }
}
