use crate::lifecycle::LifecycleError;
use std::path::{Path, PathBuf};

const AGENT_SIGNAL_SENDER_BYTES: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/waitagent-agent-signal-send"));

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
        #[cfg(unix)]
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
        let _ = std::fs::write(&version_path, identity);
    }
    Ok(sender_path)
}

fn sender_identity() -> String {
    format!(
        "len={};hash={:016x}",
        AGENT_SIGNAL_SENDER_BYTES.len(),
        fnv1a64(AGENT_SIGNAL_SENDER_BYTES)
    )
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

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

#[allow(dead_code)]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::{fnv1a64, AGENT_SIGNAL_SENDER_BYTES};

    #[test]
    fn bundled_sender_is_non_empty() {
        assert!(!AGENT_SIGNAL_SENDER_BYTES.is_empty());
        assert_ne!(fnv1a64(AGENT_SIGNAL_SENDER_BYTES), 0);
    }
}
