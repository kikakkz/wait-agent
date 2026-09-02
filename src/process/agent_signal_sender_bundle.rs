use crate::lifecycle::LifecycleError;
#[cfg(all(unix, test))]
use std::path::Path;
use std::path::PathBuf;

/// Bundled C sender payload. `build.rs` compiles
/// `src/process/agent_signal_sender_bundle.c` into `OUT_DIR` on non-Windows
/// hosts only, so the bytes can only be embedded on Unix.
#[cfg(unix)]
const AGENT_SIGNAL_SENDER_BYTES: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/waitagent-agent-signal-send"));

/// Resolve the path hook scripts should invoke to send agent signals.
///
/// On Unix the sender is a C binary compiled by `build.rs` and embedded here;
/// it is extracted to the per-user data directory on first use. On Windows the
/// equivalent sender ships as the companion `waitagent-agent-signal-send`
/// binary installed next to this executable (the C bundle is not built on
/// Windows hosts), so no extraction is needed.
#[cfg(unix)]
pub fn extract_agent_signal_sender() -> Result<PathBuf, LifecycleError> {
    let data_dir = data_local_dir().join("waitagent");
    let sender_path = data_dir.join("agent-signal-send");
    let version_path = data_dir.join("agent-signal-send.version");
    let identity = sender_identity();

    let needs_extract = !sender_path.exists()
        || std::fs::read_to_string(&version_path)
            .map(|stored| stored != identity)
            .unwrap_or(true);
    if needs_extract {
        std::fs::create_dir_all(&data_dir).map_err(|error| {
            LifecycleError::Io(
                format!(
                    "failed to create waitagent data directory at {}",
                    data_dir.display()
                ),
                error,
            )
        })?;
        std::fs::write(&sender_path, AGENT_SIGNAL_SENDER_BYTES).map_err(|error| {
            LifecycleError::Io(
                format!(
                    "failed to write agent signal sender to {}",
                    sender_path.display()
                ),
                error,
            )
        })?;
        std::fs::set_permissions(
            &sender_path,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .map_err(|error| {
            LifecycleError::Io(
                format!(
                    "failed to set permissions on agent signal sender at {}",
                    sender_path.display()
                ),
                error,
            )
        })?;
        crate::infra::best_effort::write_file(&version_path, identity);
    }
    Ok(sender_path)
}

#[cfg(windows)]
pub fn extract_agent_signal_sender() -> Result<PathBuf, LifecycleError> {
    let exe = std::env::current_exe().map_err(|error| {
        LifecycleError::Io(
            "failed to locate current executable for agent signal sender".to_string(),
            error,
        )
    })?;
    let sender_path = exe.with_file_name("waitagent-agent-signal-send.exe");
    if sender_path.is_file() {
        return Ok(sender_path);
    }
    Err(LifecycleError::Protocol(format!(
        "agent signal sender not found next to executable at {}",
        sender_path.display()
    )))
}

#[cfg(unix)]
fn sender_identity() -> String {
    format!(
        "len={};hash={:016x}",
        AGENT_SIGNAL_SENDER_BYTES.len(),
        fnv1a64(AGENT_SIGNAL_SENDER_BYTES)
    )
}

#[cfg(unix)]
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(unix)]
fn data_local_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_DATA_HOME") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".local").join("share");
    }
    PathBuf::from("/tmp")
}

#[cfg(all(unix, test))]
#[allow(dead_code)]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

#[cfg(all(test, unix))]
mod tests {
    use super::{fnv1a64, AGENT_SIGNAL_SENDER_BYTES};

    #[test]
    fn bundled_sender_is_non_empty() {
        const _: () = assert!(!AGENT_SIGNAL_SENDER_BYTES.is_empty());
        assert_ne!(fnv1a64(AGENT_SIGNAL_SENDER_BYTES), 0);
    }
}
