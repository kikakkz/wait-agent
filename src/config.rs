use std::collections::HashMap;
use std::env;

const DEFAULT_LISTEN_ADDR: &str = "127.0.0.1:7474";
const DEFAULT_NODE_ID: &str = "local";

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub node: NodeConfig,
    pub network: NetworkConfig,
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

        // Keep proxy shape stable from day one so local-first code does not need
        // a command-surface break when network mode becomes real.
        let proxy = ProxyConfig {
            default_proxy: read_env(env_map, "WAITAGENT_PROXY"),
            all_proxy: read_env(env_map, "WAITAGENT_ALL_PROXY"),
            http_proxy: read_env(env_map, "WAITAGENT_HTTP_PROXY"),
            https_proxy: read_env(env_map, "WAITAGENT_HTTPS_PROXY"),
        };

        Self {
            node: NodeConfig { node_id },
            network: NetworkConfig {
                access_point,
                listen_addr,
                proxy,
            },
        }
    }

    pub fn runtime_for_run(&self, node_id: Option<&str>, connect: Option<&str>) -> Self {
        self.with_overrides(node_id, connect, None)
    }

    pub fn runtime_for_local_attach(&self) -> Self {
        self.clone()
    }

    pub fn runtime_for_attach_server(&self, server_addr: &str) -> Self {
        self.with_overrides(None, Some(server_addr), None)
    }

    pub fn runtime_for_server(&self, listen: Option<&str>, node_id: Option<&str>) -> Self {
        self.with_overrides(node_id, None, listen)
    }

    pub fn runtime_for_client(
        &self,
        connect: Option<&str>,
        node_id: Option<&str>,
        proxy: Option<&str>,
        all_proxy: Option<&str>,
        http_proxy: Option<&str>,
        https_proxy: Option<&str>,
    ) -> Self {
        let mut config = self.with_overrides(node_id, connect, None);

        if let Some(value) = proxy {
            config.network.proxy.default_proxy = Some(value.to_string());
        }
        if let Some(value) = all_proxy {
            config.network.proxy.all_proxy = Some(value.to_string());
        }
        if let Some(value) = http_proxy {
            config.network.proxy.http_proxy = Some(value.to_string());
        }
        if let Some(value) = https_proxy {
            config.network.proxy.https_proxy = Some(value.to_string());
        }

        config
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
    pub proxy: ProxyConfig,
}

impl NetworkConfig {
    pub fn access_point_display(&self) -> &str {
        self.access_point.as_deref().unwrap_or("disabled")
    }
}

#[derive(Debug, Clone, Default)]
pub struct ProxyConfig {
    pub default_proxy: Option<String>,
    pub all_proxy: Option<String>,
    pub http_proxy: Option<String>,
    pub https_proxy: Option<String>,
}

impl ProxyConfig {
    pub fn describe(&self) -> String {
        if let Some(value) = self.default_proxy.as_deref() {
            return format!("default={value}");
        }
        if let Some(value) = self.all_proxy.as_deref() {
            return format!("all={value}");
        }

        let mut parts = Vec::new();
        if let Some(value) = self.http_proxy.as_deref() {
            parts.push(format!("http={value}"));
        }
        if let Some(value) = self.https_proxy.as_deref() {
            parts.push(format!("https={value}"));
        }

        if parts.is_empty() {
            "disabled".to_string()
        } else {
            parts.join(",")
        }
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
    fn reads_proxy_and_access_point_from_env_map() {
        let mut env_map = HashMap::new();
        env_map.insert(
            "WAITAGENT_ACCESS_POINT".to_string(),
            "ws://127.0.0.1:7474".to_string(),
        );
        env_map.insert(
            "WAITAGENT_PROXY".to_string(),
            "socks5://127.0.0.1:7897".to_string(),
        );

        let config = AppConfig::from_env_map(&env_map);
        assert_eq!(
            config.network.access_point.as_deref(),
            Some("ws://127.0.0.1:7474")
        );
        assert_eq!(
            config.network.proxy.default_proxy.as_deref(),
            Some("socks5://127.0.0.1:7897")
        );
    }
}
