use std::path::PathBuf;

pub fn waitagent_home() -> PathBuf {
    if let Some(value) = std::env::var_os("WAITAGENT_HOME") {
        return PathBuf::from(value);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
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
