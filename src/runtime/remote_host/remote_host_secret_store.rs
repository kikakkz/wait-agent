#![allow(dead_code)]

use crate::runtime::remote_host::remote_host_home::waitagent_home;
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RemoteHostSecretId(String);

impl RemoteHostSecretId {
    pub fn new(value: impl Into<String>) -> Result<Self, RemoteHostSecretStoreError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(RemoteHostSecretStoreError::new(
                "remote host secret id is required",
            ));
        }
        if value.chars().any(char::is_whitespace) {
            return Err(RemoteHostSecretStoreError::new(
                "remote host secret id must not contain whitespace",
            ));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RemoteHostSecretId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteHostSecretValue(String);

impl RemoteHostSecretValue {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

pub trait RemoteHostSecretStore {
    type Error;

    fn put_secret(
        &self,
        id: &RemoteHostSecretId,
        secret: RemoteHostSecretValue,
    ) -> Result<(), Self::Error>;
    fn get_secret(
        &self,
        id: &RemoteHostSecretId,
    ) -> Result<Option<RemoteHostSecretValue>, Self::Error>;
    fn delete_secret(&self, id: &RemoteHostSecretId) -> Result<(), Self::Error>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteHostSecretStoreError {
    message: String,
}

impl RemoteHostSecretStoreError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for RemoteHostSecretStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RemoteHostSecretStoreError {}

#[derive(Debug, Clone, Default)]
pub struct MemoryRemoteHostSecretStore {
    secrets: Arc<Mutex<HashMap<RemoteHostSecretId, RemoteHostSecretValue>>>,
}

impl RemoteHostSecretStore for MemoryRemoteHostSecretStore {
    type Error = RemoteHostSecretStoreError;

    fn put_secret(
        &self,
        id: &RemoteHostSecretId,
        secret: RemoteHostSecretValue,
    ) -> Result<(), Self::Error> {
        self.secrets
            .lock()
            .expect("remote host memory secret store should not be poisoned")
            .insert(id.clone(), secret);
        Ok(())
    }

    fn get_secret(
        &self,
        id: &RemoteHostSecretId,
    ) -> Result<Option<RemoteHostSecretValue>, Self::Error> {
        Ok(self
            .secrets
            .lock()
            .expect("remote host memory secret store should not be poisoned")
            .get(id)
            .cloned())
    }

    fn delete_secret(&self, id: &RemoteHostSecretId) -> Result<(), Self::Error> {
        self.secrets
            .lock()
            .expect("remote host memory secret store should not be poisoned")
            .remove(id);
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct FileRemoteHostSecretStore {
    root: PathBuf,
}

impl Default for FileRemoteHostSecretStore {
    fn default() -> Self {
        Self::new(waitagent_home().join("secrets").join("remote-host"))
    }
}

impl FileRemoteHostSecretStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn path_for(&self, id: &RemoteHostSecretId) -> Result<PathBuf, RemoteHostSecretStoreError> {
        let mut path = self.root.clone();
        for segment in id.as_str().split('.') {
            if segment.is_empty()
                || segment == "."
                || segment == ".."
                || segment.contains('/')
                || segment.contains('\\')
            {
                return Err(RemoteHostSecretStoreError::new(
                    "remote host secret id contains an invalid path segment",
                ));
            }
            path.push(segment);
        }
        Ok(path)
    }
}

impl RemoteHostSecretStore for FileRemoteHostSecretStore {
    type Error = RemoteHostSecretStoreError;

    fn put_secret(
        &self,
        id: &RemoteHostSecretId,
        secret: RemoteHostSecretValue,
    ) -> Result<(), Self::Error> {
        let path = self.path_for(id)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| RemoteHostSecretStoreError::new(error.to_string()))?;
        }
        write_secret_file(&path, secret.expose_secret())
    }

    fn get_secret(
        &self,
        id: &RemoteHostSecretId,
    ) -> Result<Option<RemoteHostSecretValue>, Self::Error> {
        let path = self.path_for(id)?;
        if !path.exists() {
            return Ok(None);
        }
        let value = fs::read_to_string(path)
            .map_err(|error| RemoteHostSecretStoreError::new(error.to_string()))?;
        Ok(Some(RemoteHostSecretValue::new(value)))
    }

    fn delete_secret(&self, id: &RemoteHostSecretId) -> Result<(), Self::Error> {
        let path = self.path_for(id)?;
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(RemoteHostSecretStoreError::new(error.to_string())),
        }
    }
}

#[cfg(unix)]
fn write_secret_file(path: &Path, value: &str) -> Result<(), RemoteHostSecretStoreError> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| RemoteHostSecretStoreError::new(error.to_string()))?;
    file.write_all(value.as_bytes())
        .map_err(|error| RemoteHostSecretStoreError::new(error.to_string()))
}

#[cfg(not(unix))]
fn write_secret_file(path: &Path, value: &str) -> Result<(), RemoteHostSecretStoreError> {
    fs::write(path, value).map_err(|error| RemoteHostSecretStoreError::new(error.to_string()))
}

#[derive(Debug, Clone, Default)]
pub struct SecretToolRemoteHostSecretStore;

impl RemoteHostSecretStore for SecretToolRemoteHostSecretStore {
    type Error = RemoteHostSecretStoreError;

    fn put_secret(
        &self,
        id: &RemoteHostSecretId,
        secret: RemoteHostSecretValue,
    ) -> Result<(), Self::Error> {
        let mut child = Command::new("secret-tool")
            .arg("store")
            .arg("--label")
            .arg(format!("WaitAgent remote host secret {}", id.as_str()))
            .arg("application")
            .arg("waitagent")
            .arg("kind")
            .arg("remote-host")
            .arg("id")
            .arg(id.as_str())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| RemoteHostSecretStoreError::new(error.to_string()))?;
        if let Some(stdin) = child.stdin.as_mut() {
            stdin
                .write_all(secret.expose_secret().as_bytes())
                .map_err(|error| RemoteHostSecretStoreError::new(error.to_string()))?;
        }
        let output = child
            .wait_with_output()
            .map_err(|error| RemoteHostSecretStoreError::new(error.to_string()))?;
        if !output.status.success() {
            return Err(RemoteHostSecretStoreError::new(format!(
                "secret-tool store failed with status {}",
                output.status
            )));
        }
        Ok(())
    }

    fn get_secret(
        &self,
        id: &RemoteHostSecretId,
    ) -> Result<Option<RemoteHostSecretValue>, Self::Error> {
        let output = Command::new("secret-tool")
            .arg("lookup")
            .arg("application")
            .arg("waitagent")
            .arg("kind")
            .arg("remote-host")
            .arg("id")
            .arg(id.as_str())
            .output()
            .map_err(|error| RemoteHostSecretStoreError::new(error.to_string()))?;
        if output.status.success() {
            let mut value = String::from_utf8_lossy(&output.stdout).into_owned();
            while value.ends_with(['\n', '\r']) {
                value.pop();
            }
            return Ok(Some(RemoteHostSecretValue::new(value)));
        }
        Ok(None)
    }

    fn delete_secret(&self, id: &RemoteHostSecretId) -> Result<(), Self::Error> {
        let output = Command::new("secret-tool")
            .arg("clear")
            .arg("application")
            .arg("waitagent")
            .arg("kind")
            .arg("remote-host")
            .arg("id")
            .arg(id.as_str())
            .output()
            .map_err(|error| RemoteHostSecretStoreError::new(error.to_string()))?;
        if output.status.success() {
            return Ok(());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_host_history_memory_secret_store_round_trips_passwords() {
        let store = MemoryRemoteHostSecretStore::default();
        let id = RemoteHostSecretId::new("waitagent.remote-host.130.ssh-password").unwrap();

        store
            .put_secret(&id, RemoteHostSecretValue::new("12345679"))
            .unwrap();

        assert_eq!(
            store.get_secret(&id).unwrap().unwrap().expose_secret(),
            "12345679"
        );

        store.delete_secret(&id).unwrap();
        assert!(store.get_secret(&id).unwrap().is_none());
    }

    #[test]
    fn remote_host_file_secret_store_round_trips_passwords() {
        let root = std::env::temp_dir().join(format!(
            "waitagent-file-secrets-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let store = FileRemoteHostSecretStore::new(&root);
        let id =
            RemoteHostSecretId::new("waitagent.remote-host.10-1-29-130.kk.ssh-password").unwrap();

        store
            .put_secret(&id, RemoteHostSecretValue::new("12345679"))
            .unwrap();

        assert_eq!(
            store.get_secret(&id).unwrap().unwrap().expose_secret(),
            "12345679"
        );
        assert!(root
            .join("waitagent/remote-host/10-1-29-130/kk/ssh-password")
            .exists());

        store.delete_secret(&id).unwrap();
        assert!(store.get_secret(&id).unwrap().is_none());
        let _ = fs::remove_dir_all(root);
    }
}
