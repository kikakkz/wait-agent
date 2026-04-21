use std::collections::HashMap;
use std::env;

const DEFAULT_LISTEN_ADDR: &str = "127.0.0.1:7474";
const DEFAULT_NODE_ID: &str = "local";

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub node: NodeConfig,
    pub network: NetworkConfig,
    pub debug: DebugConfig,
}

impl AppConfig {
    pub fn from_env() -> Self {
        let env_map = env::vars().collect::<HashMap<_, _>>();
        Self::from_env_map(&env_map)
    }

    fn from_env_map(env_map: &HashMap<String, String>) -> Self {
        let node_id =
            read_env(env_map, "WAITAGENT_NODE_ID").unwrap_or_else(|| DEFAULT_NODE_ID.to_string());
        let access_point = read_env(env_map, "WAITAGENT_ACCESS_POINT");
        let listen_addr = read_env(env_map, "WAITAGENT_LISTEN_ADDR")
            .unwrap_or_else(|| DEFAULT_LISTEN_ADDR.to_string());
        let input_trace_path = read_env(env_map, "WAITAGENT_INPUT_TRACE_PATH");
        let output_trace_path = read_env(env_map, "WAITAGENT_OUTPUT_TRACE_PATH");

        Self {
            node: NodeConfig { node_id },
            network: NetworkConfig {
                access_point,
                listen_addr,
            },
            debug: DebugConfig {
                input_trace_path,
                output_trace_path,
            },
        }
    }

    pub fn runtime_for_run(&self, node_id: Option<&str>, connect: Option<&str>) -> Self {
        self.with_overrides(node_id, connect, None)
    }

    pub fn runtime_for_workspace(&self, node_id: Option<&str>, connect: Option<&str>) -> Self {
        self.with_overrides(node_id, connect, None)
    }

    pub fn runtime_for_server(&self, listen: Option<&str>, node_id: Option<&str>) -> Self {
        self.with_overrides(node_id, None, listen)
    }

    pub fn mode_name(&self) -> &'static str {
        if self.network.access_point.is_some() {
            "network-capable"
        } else {
            "local"
        }
    }

    fn with_overrides(
        &self,
        node_id: Option<&str>,
        access_point: Option<&str>,
        listen_addr: Option<&str>,
    ) -> Self {
        let mut config = self.clone();

        if let Some(value) = node_id {
            config.node.node_id = value.to_string();
        }
        if let Some(value) = access_point {
            config.network.access_point = Some(value.to_string());
        }
        if let Some(value) = listen_addr {
            config.network.listen_addr = value.to_string();
        }

        config
    }
}

#[derive(Debug, Clone)]
pub struct NodeConfig {
    pub node_id: String,
}

#[derive(Debug, Clone)]
pub struct NetworkConfig {
    pub access_point: Option<String>,
    pub listen_addr: String,
}

#[derive(Debug, Clone)]
pub struct DebugConfig {
    pub input_trace_path: Option<String>,
    pub output_trace_path: Option<String>,
}

impl NetworkConfig {
    pub fn access_point_display(&self) -> &str {
        self.access_point.as_deref().unwrap_or("disabled")
    }
}

fn read_env(env_map: &HashMap<String, String>, key: &str) -> Option<String> {
    env_map.get(key).cloned().filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::AppConfig;
    use std::collections::HashMap;

    #[test]
    fn reads_access_point_from_env_map() {
        let mut env_map = HashMap::new();
        env_map.insert(
            "WAITAGENT_ACCESS_POINT".to_string(),
            "ws://127.0.0.1:7474".to_string(),
        );

        let config = AppConfig::from_env_map(&env_map);
        assert_eq!(
            config.network.access_point.as_deref(),
            Some("ws://127.0.0.1:7474")
        );
    }

    #[test]
    fn reads_input_trace_path_from_env_map() {
        let mut env_map = HashMap::new();
        env_map.insert(
            "WAITAGENT_INPUT_TRACE_PATH".to_string(),
            "/tmp/waitagent-input.log".to_string(),
        );

        let config = AppConfig::from_env_map(&env_map);
        assert_eq!(
            config.debug.input_trace_path.as_deref(),
            Some("/tmp/waitagent-input.log")
        );
    }

    #[test]
    fn reads_output_trace_path_from_env_map() {
        let mut env_map = HashMap::new();
        env_map.insert(
            "WAITAGENT_OUTPUT_TRACE_PATH".to_string(),
            "/tmp/waitagent-output.log".to_string(),
        );

        let config = AppConfig::from_env_map(&env_map);
        assert_eq!(
            config.debug.output_trace_path.as_deref(),
            Some("/tmp/waitagent-output.log")
        );
    }
}
