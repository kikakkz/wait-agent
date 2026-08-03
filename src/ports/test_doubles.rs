#[cfg(test)]
pub mod mocks {
    use crate::domain::session_catalog::ManagedSessionRecord;
    use crate::ports::hooks_config::HooksConfigPort;
    use crate::ports::session_creation::{
        RemoteSessionCreationError, RemoteSessionCreationRequest, SessionCreationPort,
    };
    use crate::ports::target_registry::{TargetRegistryError, TargetRegistryPort};
    use std::sync::{Arc, Mutex};

    #[derive(Default, Clone)]
    pub struct MockSessionCreation {
        pub requests: Arc<Mutex<Vec<RemoteSessionCreationRequest>>>,
        pub reply: Arc<Mutex<Option<Result<ManagedSessionRecord, RemoteSessionCreationError>>>>,
    }

    impl MockSessionCreation {
        pub fn with_reply(reply: Result<ManagedSessionRecord, RemoteSessionCreationError>) -> Self {
            Self {
                requests: Arc::new(Mutex::new(Vec::new())),
                reply: Arc::new(Mutex::new(Some(reply))),
            }
        }
    }

    impl SessionCreationPort for MockSessionCreation {
        fn create_session(
            &self,
            request: RemoteSessionCreationRequest,
        ) -> Result<ManagedSessionRecord, RemoteSessionCreationError> {
            self.requests.lock().unwrap().push(request);
            self.reply.lock().unwrap().take().unwrap_or_else(|| {
                Err(RemoteSessionCreationError::InvalidRequest(
                    "mock not configured".to_string(),
                ))
            })
        }
    }

    #[derive(Default, Clone)]
    pub struct MockTargetRegistry {
        pub targets: Arc<Mutex<Vec<ManagedSessionRecord>>>,
        pub calls: Arc<Mutex<usize>>,
    }

    impl MockTargetRegistry {
        pub fn with_targets(targets: Vec<ManagedSessionRecord>) -> Self {
            Self {
                targets: Arc::new(Mutex::new(targets)),
                calls: Arc::new(Mutex::new(0)),
            }
        }
    }

    impl TargetRegistryPort for MockTargetRegistry {
        fn list_targets(&self) -> Result<Vec<ManagedSessionRecord>, TargetRegistryError> {
            Ok(self.targets.lock().unwrap().clone())
        }

        fn list_targets_on_authority(
            &self,
            authority_id: &str,
        ) -> Result<Vec<ManagedSessionRecord>, TargetRegistryError> {
            *self.calls.lock().unwrap() += 1;
            Ok(self
                .targets
                .lock()
                .unwrap()
                .iter()
                .filter(|target| target.address.authority_id() == authority_id)
                .cloned()
                .collect())
        }
    }

    #[derive(Default, Clone)]
    pub struct MockHooksConfig {
        pub agent: &'static str,
    }

    impl HooksConfigPort for MockHooksConfig {
        fn agent_name(&self) -> &'static str {
            self.agent
        }

        fn reconcile(&self) -> std::io::Result<()> {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::mocks::{MockHooksConfig, MockSessionCreation, MockTargetRegistry};
    use crate::ports::hooks_config::HooksConfigPort;
    use crate::ports::session_creation::SessionCreationPort;
    use crate::ports::target_registry::TargetRegistryPort;

    #[test]
    fn session_creation_port_object_safe() {
        let _: Box<dyn SessionCreationPort> = Box::new(MockSessionCreation::default());
    }

    #[test]
    fn target_registry_port_object_safe() {
        let _: Box<dyn TargetRegistryPort> = Box::new(MockTargetRegistry::default());
    }

    #[test]
    fn hooks_config_port_object_safe() {
        let _: Box<dyn HooksConfigPort> = Box::new(MockHooksConfig::default());
    }
}
