use crate::host::ssh::remote_host_home::waitagent_home;
use std::fmt;
use std::fs;
use std::io;
use std::path::PathBuf;

/// Persistent user settings stored in `~/.waitagent/settings.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Settings {
    /// Currently active public endpoint override, if any.
    pub public_endpoint: Option<String>,
    /// Previously used public endpoint values, most recent first.
    pub public_history: Vec<String>,
    /// Whether `public_endpoint` should be written back to disk.
    pub save_public: bool,
}

/// Persistent store for user settings.
#[derive(Debug, Clone)]
pub struct SettingsStore {
    path: PathBuf,
}

impl SettingsStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn default_path() -> PathBuf {
        waitagent_home().join("settings.toml")
    }

    pub fn load(&self) -> Result<Settings, SettingsStoreError> {
        if !self.path.exists() {
            return Ok(Settings::default());
        }
        let text = fs::read_to_string(&self.path).map_err(SettingsStoreError::io)?;
        parse_settings(&text)
    }

    pub fn save(&self, settings: &Settings) -> Result<(), SettingsStoreError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(SettingsStoreError::io)?;
        }
        let content = serialize_settings(settings);
        fs::write(&self.path, content).map_err(SettingsStoreError::io)
    }

    /// Return the saved public endpoint, if the user chose to persist it.
    pub fn saved_public_endpoint(&self) -> Result<Option<String>, SettingsStoreError> {
        let settings = self.load()?;
        if settings.save_public {
            Ok(settings.public_endpoint)
        } else {
            Ok(None)
        }
    }

    /// Clear the public endpoint and history persistence.
    pub fn clear_public(&self) -> Result<(), SettingsStoreError> {
        let mut settings = self.load()?;
        settings.public_endpoint = None;
        settings.save_public = false;
        self.save(&settings)
    }

    /// Set the current public endpoint and optionally persist it for next startup.
    ///
    /// The value is always added to the front of the history list.
    pub fn set_public(&self, endpoint: &str, save: bool) -> Result<(), SettingsStoreError> {
        let mut settings = self.load()?;
        settings.public_endpoint = Some(endpoint.to_string());
        settings.save_public = save;
        settings.public_history.retain(|value| value != endpoint);
        settings.public_history.insert(0, endpoint.to_string());
        const MAX_HISTORY: usize = 20;
        if settings.public_history.len() > MAX_HISTORY {
            settings.public_history.truncate(MAX_HISTORY);
        }
        self.save(&settings)
    }

    /// Return the stored public endpoint history, most recent first.
    pub fn public_history(&self) -> Result<Vec<String>, SettingsStoreError> {
        Ok(self.load()?.public_history)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsStoreError {
    message: String,
}

impl SettingsStoreError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    fn io(error: io::Error) -> Self {
        Self::new(error.to_string())
    }
}

impl fmt::Display for SettingsStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for SettingsStoreError {}

fn serialize_settings(settings: &Settings) -> String {
    let mut out = String::new();
    out.push_str("# WaitAgent user settings\n");
    out.push_str(&format!(
        "save_public = {}\n",
        if settings.save_public {
            "true"
        } else {
            "false"
        }
    ));
    if let Some(endpoint) = &settings.public_endpoint {
        push_string(&mut out, "public_endpoint", endpoint);
    }
    for value in &settings.public_history {
        push_string(&mut out, "public_history", value);
    }
    out
}

fn push_string(out: &mut String, key: &str, value: &str) {
    out.push_str(key);
    out.push_str(" = \"");
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out.push_str("\"\n");
}

fn parse_settings(text: &str) -> Result<Settings, SettingsStoreError> {
    let mut settings = Settings::default();
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = parse_key_value(line)?;
        match key.as_str() {
            "save_public" => {
                settings.save_public = parse_bool(&value)?;
            }
            "public_endpoint" => {
                settings.public_endpoint = Some(value);
            }
            "public_history" => {
                settings.public_history.push(value);
            }
            other => {
                return Err(SettingsStoreError::new(format!(
                    "unknown settings field `{other}`"
                )));
            }
        }
    }
    Ok(settings)
}

fn parse_key_value(line: &str) -> Result<(String, String), SettingsStoreError> {
    let Some((key, value)) = line.split_once('=') else {
        return Err(SettingsStoreError::new(format!(
            "invalid settings line `{line}`"
        )));
    };
    let key = key.trim().to_string();
    let value = value.trim();
    if value.starts_with('"') {
        return Ok((key, parse_quoted(value)?));
    }
    Ok((key, value.to_string()))
}

fn parse_quoted(value: &str) -> Result<String, SettingsStoreError> {
    if !value.ends_with('"') || value.len() < 2 {
        return Err(SettingsStoreError::new("unterminated settings string"));
    }
    let mut out = String::new();
    let mut chars = value[1..value.len() - 1].chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    Ok(out)
}

fn parse_bool(value: &str) -> Result<bool, SettingsStoreError> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        other => Err(SettingsStoreError::new(format!(
            "settings boolean must be true or false, got `{other}`"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "waitagent-settings-{name}-{}-{}",
            std::process::id(),
            std::thread::current()
                .name()
                .unwrap_or("test")
                .replace(":", "_")
        ))
    }

    #[test]
    fn default_path_uses_waitagent_home() {
        let path = SettingsStore::default_path();
        assert!(path.ends_with(PathBuf::from(".waitagent/settings.toml")));
    }

    #[test]
    fn round_trips_settings() {
        let path = unique_path("round-trip.toml");
        let store = SettingsStore::new(&path);

        let settings = Settings {
            public_endpoint: Some("nat.example:17474".to_string()),
            public_history: vec![
                "nat.example:17474".to_string(),
                "old.example:7474".to_string(),
            ],
            save_public: true,
        };
        store.save(&settings).unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded, settings);

        crate::infra::best_effort::remove_file(path);
    }

    #[test]
    fn set_public_updates_history_and_caps() {
        let path = unique_path("history.toml");
        let store = SettingsStore::new(&path);

        for index in 0..25 {
            store
                .set_public(&format!("host{index}:17474"), false)
                .unwrap();
        }
        let loaded = store.load().unwrap();
        assert_eq!(loaded.public_history.len(), 20);
        assert_eq!(loaded.public_history[0], "host24:17474");

        store.set_public("host10:17474", false).unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded.public_history[0], "host10:17474");
        assert!(!loaded.public_history[1..].contains(&"host10:17474".to_string()));

        crate::infra::best_effort::remove_file(path);
    }

    #[test]
    fn clear_public_unsets_saved_value() {
        let path = unique_path("clear.toml");
        let store = SettingsStore::new(&path);

        let mut settings = Settings::default();
        settings.public_endpoint = Some("x:1".to_string());
        settings.save_public = true;
        store.save(&settings).unwrap();

        store.clear_public().unwrap();
        let loaded = store.load().unwrap();
        assert!(loaded.public_endpoint.is_none());
        assert!(!loaded.save_public);

        crate::infra::best_effort::remove_file(path);
    }
}
