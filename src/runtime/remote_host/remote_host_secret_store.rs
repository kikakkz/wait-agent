#![allow(dead_code)]

use crate::runtime::remote_host::remote_host_home::waitagent_home;
use aes::cipher::{block_padding::Pkcs7, BlockModeDecrypt, BlockModeEncrypt, KeyIvInit};
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
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

const FILE_SECRET_HEADER: &str = "waitagent-secret-v2";

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
        write_secret_file(&path, id, secret.expose_secret())
    }

    fn get_secret(
        &self,
        id: &RemoteHostSecretId,
    ) -> Result<Option<RemoteHostSecretValue>, Self::Error> {
        let path = self.path_for(id)?;
        if !path.exists() {
            return Ok(None);
        }
        let value = fs::read_to_string(&path)
            .map_err(|error| RemoteHostSecretStoreError::new(error.to_string()))?;
        let decrypted = decode_secret_file(id, &value)?;
        Ok(Some(RemoteHostSecretValue::new(decrypted)))
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
fn write_secret_file(
    path: &Path,
    id: &RemoteHostSecretId,
    value: &str,
) -> Result<(), RemoteHostSecretStoreError> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| RemoteHostSecretStoreError::new(error.to_string()))?;
    file.write_all(encode_secret_file(id, value)?.as_bytes())
        .map_err(|error| RemoteHostSecretStoreError::new(error.to_string()))
}

#[cfg(not(unix))]
fn write_secret_file(
    path: &Path,
    id: &RemoteHostSecretId,
    value: &str,
) -> Result<(), RemoteHostSecretStoreError> {
    fs::write(path, encode_secret_file(id, value)?)
        .map_err(|error| RemoteHostSecretStoreError::new(error.to_string()))
}

type Aes256CbcEnc = cbc::Encryptor<aes::Aes256>;
type Aes256CbcDec = cbc::Decryptor<aes::Aes256>;

const OPENSSL_SALTED_PREFIX: &[u8; 8] = b"Salted__";
const OPENSSL_SALT_LEN: usize = 8;
const AES_256_KEY_LEN: usize = 32;
const AES_CBC_IV_LEN: usize = 16;
const SECRET_PBKDF2_ITERATIONS: u32 = 1;

fn encode_secret_file(
    id: &RemoteHostSecretId,
    value: &str,
) -> Result<String, RemoteHostSecretStoreError> {
    let payload = encrypt_secret_payload(id, value.as_bytes())?;
    Ok(format!("{}\n{}\n", FILE_SECRET_HEADER, payload))
}

fn decode_secret_file(
    id: &RemoteHostSecretId,
    text: &str,
) -> Result<String, RemoteHostSecretStoreError> {
    let mut lines = text.lines();
    if lines.next() != Some(FILE_SECRET_HEADER) {
        return Err(RemoteHostSecretStoreError::new(
            "encrypted remote host secret is missing header",
        ));
    }
    let payload = lines.collect::<Vec<_>>().join("");
    if payload.trim().is_empty() {
        return Err(RemoteHostSecretStoreError::new(
            "encrypted remote host secret is missing payload",
        ));
    }
    let plain = decrypt_secret_payload(id, payload.trim())?;
    String::from_utf8(plain).map_err(|error| RemoteHostSecretStoreError::new(error.to_string()))
}

fn encrypt_secret_payload(
    id: &RemoteHostSecretId,
    input: &[u8],
) -> Result<String, RemoteHostSecretStoreError> {
    let mut salt = [0u8; OPENSSL_SALT_LEN];
    getrandom::fill(&mut salt)
        .map_err(|error| RemoteHostSecretStoreError::new(error.to_string()))?;
    let (key, iv) = derive_secret_key_iv(id, &salt)?;

    let padded_len = input.len() + AES_CBC_IV_LEN;
    let mut buffer = vec![0u8; padded_len];
    buffer[..input.len()].copy_from_slice(input);
    let ciphertext = Aes256CbcEnc::new(&key.into(), &iv.into())
        .encrypt_padded::<Pkcs7>(&mut buffer, input.len())
        .map_err(|error| RemoteHostSecretStoreError::new(error.to_string()))?;

    let mut framed =
        Vec::with_capacity(OPENSSL_SALTED_PREFIX.len() + salt.len() + ciphertext.len());
    framed.extend_from_slice(OPENSSL_SALTED_PREFIX);
    framed.extend_from_slice(&salt);
    framed.extend_from_slice(ciphertext);
    Ok(BASE64_STANDARD.encode(framed))
}

fn decrypt_secret_payload(
    id: &RemoteHostSecretId,
    payload: &str,
) -> Result<Vec<u8>, RemoteHostSecretStoreError> {
    let mut framed = BASE64_STANDARD
        .decode(payload)
        .map_err(|error| RemoteHostSecretStoreError::new(error.to_string()))?;
    if framed.len() < OPENSSL_SALTED_PREFIX.len() + OPENSSL_SALT_LEN + AES_CBC_IV_LEN
        || &framed[..OPENSSL_SALTED_PREFIX.len()] != OPENSSL_SALTED_PREFIX
    {
        return Err(RemoteHostSecretStoreError::new(
            "encrypted remote host secret has invalid payload",
        ));
    }

    let salt_start = OPENSSL_SALTED_PREFIX.len();
    let salt_end = salt_start + OPENSSL_SALT_LEN;
    let salt = framed[salt_start..salt_end].to_vec();
    let ciphertext = &mut framed[salt_end..];
    let (key, iv) = derive_secret_key_iv(id, &salt)?;
    let plain = Aes256CbcDec::new(&key.into(), &iv.into())
        .decrypt_padded::<Pkcs7>(ciphertext)
        .map_err(|error| RemoteHostSecretStoreError::new(error.to_string()))?;
    Ok(plain.to_vec())
}

fn derive_secret_key_iv(
    id: &RemoteHostSecretId,
    salt: &[u8],
) -> Result<([u8; AES_256_KEY_LEN], [u8; AES_CBC_IV_LEN]), RemoteHostSecretStoreError> {
    let mut key_iv = [0u8; AES_256_KEY_LEN + AES_CBC_IV_LEN];
    pbkdf2::pbkdf2_hmac::<sha2::Sha256>(
        machine_bound_passphrase(id)?.as_bytes(),
        salt,
        SECRET_PBKDF2_ITERATIONS,
        &mut key_iv,
    );

    let mut key = [0u8; AES_256_KEY_LEN];
    let mut iv = [0u8; AES_CBC_IV_LEN];
    key.copy_from_slice(&key_iv[..AES_256_KEY_LEN]);
    iv.copy_from_slice(&key_iv[AES_256_KEY_LEN..]);
    Ok((key, iv))
}

fn machine_bound_passphrase(id: &RemoteHostSecretId) -> Result<String, RemoteHostSecretStoreError> {
    let machine_id = fs::read_to_string("/etc/machine-id")
        .or_else(|_| fs::read_to_string("/var/lib/dbus/machine-id"))
        .unwrap_or_else(|_| "unknown-machine".to_string());
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown-user".to_string());
    let home = std::env::var("HOME").unwrap_or_default();
    Ok(format!(
        "waitagent-secret:{}:{}:{}:{}",
        machine_id.trim(),
        user,
        home,
        id.as_str()
    ))
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
    fn remote_host_file_secret_store_rejects_legacy_plaintext_passwords() {
        let root = std::env::temp_dir().join(format!(
            "waitagent-file-secrets-legacy-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let store = FileRemoteHostSecretStore::new(&root);
        let id = RemoteHostSecretId::new("waitagent.remote-host.legacy.ssh-password").unwrap();
        let path = root.join("waitagent/remote-host/legacy/ssh-password");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "legacy-secret").unwrap();

        let error = store.get_secret(&id).unwrap_err();
        assert_eq!(
            error.to_string(),
            "encrypted remote host secret is missing header"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "legacy-secret");
        let _ = fs::remove_dir_all(root);
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
        let secret_path = root.join("waitagent/remote-host/10-1-29-130/kk/ssh-password");
        assert!(secret_path.exists());
        let stored = fs::read_to_string(&secret_path).unwrap();
        assert!(stored.starts_with(FILE_SECRET_HEADER));
        assert!(!stored.contains("12345679"));
        let payload = stored.lines().nth(1).unwrap();
        let framed = BASE64_STANDARD.decode(payload).unwrap();
        assert!(framed.starts_with(OPENSSL_SALTED_PREFIX));

        store.delete_secret(&id).unwrap();
        assert!(store.get_secret(&id).unwrap().is_none());
        let _ = fs::remove_dir_all(root);
    }
}
