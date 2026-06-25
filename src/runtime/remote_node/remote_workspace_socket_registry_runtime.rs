use crate::cli::RemoteNetworkConfig;
use crate::lifecycle::LifecycleError;
use std::collections::BTreeSet;
use std::fs;
use std::io::{self, ErrorKind};
use std::path::PathBuf;

pub(crate) struct RemoteWorkspaceSocketRegistryRuntime {
    network: RemoteNetworkConfig,
}

impl RemoteWorkspaceSocketRegistryRuntime {
    pub fn new(network: RemoteNetworkConfig) -> Self {
        Self { network }
    }

    pub fn register_workspace_socket(&self, socket_name: &str) -> Result<(), LifecycleError> {
        let mut sockets = self.live_workspace_socket_names()?;
        sockets.insert(socket_name.to_string());
        write_socket_registry(&self.network, &sockets)
    }

    pub fn unregister_workspace_socket(&self, socket_name: &str) -> Result<(), LifecycleError> {
        let mut sockets = self.live_workspace_socket_names()?;
        sockets.remove(socket_name);
        write_socket_registry(&self.network, &sockets)
    }

    pub fn retain_workspace_sockets<F>(
        &self,
        mut is_live: F,
    ) -> Result<BTreeSet<String>, LifecycleError>
    where
        F: FnMut(&str) -> bool,
    {
        let sockets = self.live_workspace_socket_names()?;
        let previous_sockets = sockets.clone();
        let live_sockets = sockets
            .into_iter()
            .filter(|socket_name| is_live(socket_name))
            .collect::<BTreeSet<_>>();
        if live_sockets != previous_sockets {
            write_socket_registry(&self.network, &live_sockets)?;
        }
        Ok(live_sockets)
    }

    pub fn live_workspace_socket_names(&self) -> Result<BTreeSet<String>, LifecycleError> {
        read_socket_registry(&self.network)
    }

    pub fn live_workspace_socket_names_retaining<F>(
        &self,
        is_live: F,
    ) -> Result<BTreeSet<String>, LifecycleError>
    where
        F: FnMut(&str) -> bool,
    {
        self.retain_workspace_sockets(is_live)
    }

    pub fn registry_exists(&self) -> bool {
        workspace_socket_registry_path(&self.network).exists()
    }
}

pub(crate) fn live_workspace_socket_names_for_network(
    network: &RemoteNetworkConfig,
) -> Result<Vec<String>, LifecycleError> {
    Ok(RemoteWorkspaceSocketRegistryRuntime::new(network.clone())
        .live_workspace_socket_names()?
        .into_iter()
        .collect())
}

pub(crate) fn retain_live_workspace_socket_names_for_network<F>(
    network: &RemoteNetworkConfig,
    is_live: F,
) -> Result<Vec<String>, LifecycleError>
where
    F: FnMut(&str) -> bool,
{
    Ok(RemoteWorkspaceSocketRegistryRuntime::new(network.clone())
        .retain_workspace_sockets(is_live)?
        .into_iter()
        .collect())
}

fn read_socket_registry(network: &RemoteNetworkConfig) -> Result<BTreeSet<String>, LifecycleError> {
    let path = workspace_socket_registry_path(network);
    let content = match fs::read_to_string(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(BTreeSet::new()),
        Err(error) => return Err(registry_error(error)),
    };
    Ok(content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToString::to_string)
        .collect())
}

fn write_socket_registry(
    network: &RemoteNetworkConfig,
    sockets: &BTreeSet<String>,
) -> Result<(), LifecycleError> {
    let path = workspace_socket_registry_path(network);
    if sockets.is_empty() {
        match fs::remove_file(path) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(registry_error(error)),
        }
    }
    let mut content = String::new();
    for socket in sockets {
        content.push_str(socket);
        content.push('\n');
    }
    fs::write(path, content).map_err(registry_error)
}

pub(crate) fn workspace_socket_registry_path(network: &RemoteNetworkConfig) -> PathBuf {
    std::env::temp_dir().join(format!(
        "waitagent-live-workspace-sockets-{}.txt",
        sanitize_path_component(&network.listener_addr().to_string())
    ))
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

fn registry_error(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> LifecycleError {
    LifecycleError::Io(
        "remote workspace socket registry operation failed".to_string(),
        io::Error::new(ErrorKind::Other, error.into().to_string()),
    )
}

#[cfg(test)]
mod tests {
    use super::{workspace_socket_registry_path, RemoteWorkspaceSocketRegistryRuntime};
    use crate::cli::RemoteNetworkConfig;

    #[test]
    fn registry_registers_and_unregisters_socket_names_by_network() {
        let network = RemoteNetworkConfig {
            port: 31987,
            connect: None,
            node_id: None,
            public_endpoint: None,
        };
        let path = workspace_socket_registry_path(&network);
        let _ = std::fs::remove_file(&path);
        let registry = RemoteWorkspaceSocketRegistryRuntime::new(network.clone());

        registry
            .register_workspace_socket("wa-a")
            .expect("first socket should register");
        registry
            .register_workspace_socket("wa-b")
            .expect("second socket should register");
        registry
            .register_workspace_socket("wa-a")
            .expect("duplicate socket should be idempotent");

        let sockets = registry
            .live_workspace_socket_names()
            .expect("registry should read");
        assert_eq!(
            sockets.into_iter().collect::<Vec<_>>(),
            vec!["wa-a".to_string(), "wa-b".to_string()]
        );

        registry
            .unregister_workspace_socket("wa-a")
            .expect("socket should unregister");
        let sockets = registry
            .live_workspace_socket_names()
            .expect("registry should read after unregister");
        assert_eq!(sockets.into_iter().collect::<Vec<_>>(), vec!["wa-b"]);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn registry_retain_workspace_sockets_prunes_stale_entries() {
        let network = RemoteNetworkConfig {
            port: 31989,
            connect: None,
            node_id: None,
            public_endpoint: None,
        };
        let path = workspace_socket_registry_path(&network);
        let _ = std::fs::remove_file(&path);
        let registry = RemoteWorkspaceSocketRegistryRuntime::new(network.clone());

        registry
            .register_workspace_socket("wa-live")
            .expect("live socket should register");
        registry
            .register_workspace_socket("wa-stale")
            .expect("stale socket should register");

        let live_sockets = registry
            .retain_workspace_sockets(|socket_name| socket_name == "wa-live")
            .expect("registry should retain live sockets");
        assert_eq!(
            live_sockets.into_iter().collect::<Vec<_>>(),
            vec!["wa-live".to_string()]
        );

        let sockets = registry
            .live_workspace_socket_names()
            .expect("registry should read after retain");
        assert_eq!(sockets.into_iter().collect::<Vec<_>>(), vec!["wa-live"]);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn registry_retain_workspace_sockets_does_not_rewrite_unchanged_registry() {
        let network = RemoteNetworkConfig {
            port: 31990,
            connect: None,
            node_id: None,
            public_endpoint: None,
        };
        let path = workspace_socket_registry_path(&network);
        let _ = std::fs::remove_file(&path);
        let registry = RemoteWorkspaceSocketRegistryRuntime::new(network.clone());

        registry
            .register_workspace_socket("wa-live")
            .expect("live socket should register");
        let before = std::fs::metadata(&path)
            .expect("registry metadata should read")
            .modified()
            .expect("registry mtime should read");

        let live_sockets = registry
            .retain_workspace_sockets(|socket_name| socket_name == "wa-live")
            .expect("unchanged retain should succeed");
        let after = std::fs::metadata(&path)
            .expect("registry metadata should read after retain")
            .modified()
            .expect("registry mtime should read after retain");

        assert_eq!(
            live_sockets.into_iter().collect::<Vec<_>>(),
            vec!["wa-live".to_string()]
        );
        assert_eq!(before, after);

        let _ = std::fs::remove_file(path);
    }
}
