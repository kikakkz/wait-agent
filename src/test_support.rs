use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

#[cfg(test)]
pub(crate) fn integration_test_lock() -> TestGuard {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let guard = match LOCK.get_or_init(|| Mutex::new(())).lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    TestGuard::new(guard)
}

/// RAII guard acquired by integration tests. It keeps the existing global mutex
/// (so tests remain serial) and additionally cleans up test-scoped sidecars,
/// tmux servers, and socket files both before and after each test.
#[cfg(test)]
pub(crate) struct TestGuard {
    _guard: MutexGuard<'static, ()>,
    network: crate::cli::RemoteNetworkConfig,
}

#[cfg(test)]
impl TestGuard {
    fn new(guard: MutexGuard<'static, ()>) -> Self {
        let network = crate::cli::RemoteNetworkConfig::default();
        cleanup_test_artifacts(&network);
        Self {
            _guard: guard,
            network,
        }
    }
}

#[cfg(test)]
impl Drop for TestGuard {
    fn drop(&mut self) {
        cleanup_test_artifacts(&self.network);
    }
}

#[cfg(test)]
fn cleanup_test_artifacts(network: &crate::cli::RemoteNetworkConfig) {
    kill_test_processes(network);
    shutdown_test_owners(network);
    remove_test_socket_files(network);
}

#[cfg(test)]
fn kill_test_processes(network: &crate::cli::RemoteNetworkConfig) {
    let port = network.port;
    let sanitized_port = format!("0_0_0_0_{port}");
    let port_flag = format!("--port {port}");

    // First, terminate anything that looks like a test sidecar or test tmux server.
    send_signal_to_matches(
        "waitagent",
        &["wa-test-", &port_flag, &sanitized_port],
        libc::SIGTERM,
    );
    send_signal_to_matches("tmux", &["-L wa-test-"], libc::SIGTERM);

    // Give the processes a moment to exit cleanly, then hard-kill survivors.
    std::thread::sleep(Duration::from_millis(200));
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline
        && any_matching_processes_remain("waitagent", &["wa-test-", &port_flag, &sanitized_port])
    {
        send_signal_to_matches(
            "waitagent",
            &["wa-test-", &port_flag, &sanitized_port],
            libc::SIGKILL,
        );
        send_signal_to_matches("tmux", &["-L wa-test-"], libc::SIGKILL);
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(test)]
fn send_signal_to_matches(program: &str, cmdline_patterns: &[&str], signal: libc::c_int) {
    let self_pid = std::process::id();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return;
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let pid_str = name.to_string_lossy();
        if !pid_str.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let Ok(pid) = pid_str.parse::<u32>() else {
            continue;
        };
        if pid == self_pid {
            continue;
        }

        let exe = std::fs::read_link(entry.path().join("exe")).ok();
        let exe_name = exe
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().to_string());
        let is_program = match program {
            "waitagent" => exe_name
                .map(|n| n.starts_with("waitagent"))
                .unwrap_or(false),
            _ => exe_name.as_deref() == Some(program),
        };
        if !is_program {
            continue;
        }

        let cmdline = std::fs::read_to_string(entry.path().join("cmdline"))
            .unwrap_or_default()
            .replace('\0', " ");
        if cmdline_patterns
            .iter()
            .any(|pattern| cmdline.contains(pattern))
        {
            unsafe {
                let _ = libc::kill(pid as libc::pid_t, signal);
            }
        }
    }
}

#[cfg(test)]
fn any_matching_processes_remain(program: &str, cmdline_patterns: &[&str]) -> bool {
    let self_pid = std::process::id();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return false;
    };

    for entry in entries.flatten() {
        let pid_str = entry.file_name().to_string_lossy().to_string();
        if !pid_str.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let Ok(pid) = pid_str.parse::<u32>() else {
            continue;
        };
        if pid == self_pid {
            continue;
        }
        let exe = std::fs::read_link(entry.path().join("exe")).ok();
        let exe_name = exe
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().to_string());
        let is_program = match program {
            "waitagent" => exe_name
                .map(|n| n.starts_with("waitagent"))
                .unwrap_or(false),
            _ => exe_name.as_deref() == Some(program),
        };
        if !is_program {
            continue;
        }
        let cmdline = std::fs::read_to_string(entry.path().join("cmdline"))
            .unwrap_or_default()
            .replace('\0', " ");
        if cmdline_patterns
            .iter()
            .any(|pattern| cmdline.contains(pattern))
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
fn shutdown_test_owners(network: &crate::cli::RemoteNetworkConfig) {
    use crate::runtime::remote_node::remote_node_ingress_server_runtime::RemoteNodeIngressServerRuntime;
    use crate::runtime::remote_node::remote_runtime_owner_runtime::RemoteRuntimeOwnerRuntime;

    // Ignore errors: the owners may not be running, and we are only trying to
    // make sure no test-scoped sidecars stay alive.
    let _ = RemoteNodeIngressServerRuntime::shutdown_owner(network);
    let _ = RemoteRuntimeOwnerRuntime::shutdown_owner_if_unused(network);
}

#[cfg(test)]
fn remove_test_socket_files(network: &crate::cli::RemoteNetworkConfig) {
    use crate::runtime::remote_node::remote_node_ingress_server_runtime::remote_node_ingress_owner_socket_path;
    use crate::runtime::remote_node::remote_runtime_owner_runtime::remote_runtime_owner_socket_path;
    use crate::runtime::remote_workspace_socket_registry_runtime::workspace_socket_registry_path;

    let _ = std::fs::remove_file(remote_node_ingress_owner_socket_path(network));
    let _ = std::fs::remove_file(remote_runtime_owner_socket_path(network));
    let _ = std::fs::remove_file(workspace_socket_registry_path(network));
}
