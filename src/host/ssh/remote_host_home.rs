use std::path::PathBuf;

/// Returns the current user's home directory. `HOME` is set on Unix; on
/// Windows it is usually absent and `USERPROFILE` is the equivalent.
pub fn user_home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

pub fn waitagent_home() -> PathBuf {
    if let Some(value) = std::env::var_os("WAITAGENT_HOME") {
        return PathBuf::from(value);
    }
    let home = user_home_dir().unwrap_or_else(|| PathBuf::from("."));
    home.join(".waitagent")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_home_uses_dot_waitagent_under_home() {
        let path = waitagent_home();
        assert!(path.ends_with(".waitagent"));
    }
}
