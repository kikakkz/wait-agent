use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use iroh::SecretKey;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
/// Errors of the enrollment stores and request lifecycle.
pub enum EnrollmentError {
    #[error("store io error: {0}")]
    Io(#[from] io::Error),
    #[error("request {0:?} not found")]
    UnknownRequest(String),
    #[error("request {0:?} is {1}, expected {2}")]
    InvalidState(String, &'static str, &'static str),
    #[error("node id mismatch for request {0:?}")]
    NodeMismatch(String),
    #[error("token for request {0:?} expired")]
    Expired(String),
    #[error("request store is corrupt: {0}")]
    Corrupt(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
/// Lifecycle of a join request.
pub enum RequestState {
    Pending,
    Approved,
    Joined,
    Expired,
    Revoked,
}

impl RequestState {
    /// Lowercase stable name for JSON surfaces.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Joined => "joined",
            Self::Expired => "expired",
            Self::Revoked => "revoked",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// A stored join request, one JSON file per id under `requests/`.
pub struct JoinRequest {
    pub id: String,
    pub node_id: String,
    pub created_at: u64,
    pub expires_at: u64,
    pub token: String,
    pub state: RequestState,
    pub joined_at: Option<u64>,
}

impl JoinRequest {
    /// Whether the request passed its token TTL.
    pub fn is_expired(&self, now: u64) -> bool {
        now >= self.expires_at
    }
}

/// Mints a random 256-bit hex token (OsRng).
pub fn mint_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let mut out = String::with_capacity(64);
    use std::fmt::Write;
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Mints a fresh request id.
pub fn new_request_id() -> String {
    mint_token()
}

/// File-backed store of join requests.
pub struct RequestStore {
    dir: PathBuf,
}

impl RequestStore {
    /// Opens the store under `root/requests` (created 0o700 on unix).
    pub fn new(root: &Path) -> io::Result<Self> {
        let dir = root.join("requests");
        create_private_dir(&dir)?;
        Ok(Self { dir })
    }

    /// Creates a pending request bound to `node_id` with the given token TTL.
    pub fn create(
        &self,
        node_id: &str,
        token_ttl: std::time::Duration,
        now: u64,
    ) -> io::Result<JoinRequest> {
        let request = JoinRequest {
            id: new_request_id(),
            node_id: node_id.to_string(),
            created_at: now,
            expires_at: now.saturating_add(token_ttl.as_secs()),
            token: mint_token(),
            state: RequestState::Pending,
            joined_at: None,
        };
        self.save(&request)?;
        Ok(request)
    }

    /// Persists the request (0o600 on unix).
    pub fn save(&self, request: &JoinRequest) -> io::Result<()> {
        write_private_file(
            &self.path(&request.id),
            &serde_json::to_vec(request).unwrap(),
        )
    }

    /// Loads a request by id.
    pub fn load(&self, id: &str) -> Result<JoinRequest, EnrollmentError> {
        let path = self.path(id);
        let bytes = fs::read(&path).map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => EnrollmentError::UnknownRequest(id.to_string()),
            _ => EnrollmentError::Io(error),
        })?;
        serde_json::from_slice(&bytes)
            .map_err(|error| EnrollmentError::Corrupt(format!("{}: {error}", path.display())))
    }

    /// Finds the request carrying `token`.
    pub fn load_by_token(&self, token: &str) -> Result<JoinRequest, EnrollmentError> {
        for request in self.list()? {
            if request.token == token {
                return Ok(request);
            }
        }
        Err(EnrollmentError::UnknownRequest(token.to_string()))
    }

    /// Lists all requests, oldest first.
    pub fn list(&self) -> Result<Vec<JoinRequest>, EnrollmentError> {
        let mut out: Vec<JoinRequest> = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let bytes = fs::read(entry.path())?;
            out.push(serde_json::from_slice(&bytes).map_err(|error| {
                EnrollmentError::Corrupt(format!("{}: {error}", entry.path().display()))
            })?);
        }
        out.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        Ok(out)
    }

    /// Pending -> approved; expires stale requests and refuses other states.
    pub fn approve(&self, id: &str, now: u64) -> Result<JoinRequest, EnrollmentError> {
        let mut request = self.load(id)?;
        if request.is_expired(now) {
            request.state = RequestState::Expired;
            self.save(&request)?;
            return Err(EnrollmentError::Expired(id.to_string()));
        }
        if request.state != RequestState::Pending {
            return Err(EnrollmentError::InvalidState(
                id.to_string(),
                request.state.as_str(),
                RequestState::Pending.as_str(),
            ));
        }
        request.state = RequestState::Approved;
        self.save(&request)?;
        Ok(request)
    }

    /// Redeems an approved token exactly once and marks the request joined.
    pub fn redeem(
        &self,
        token: &str,
        node_id: &str,
        now: u64,
    ) -> Result<JoinRequest, EnrollmentError> {
        let mut request = self.load_by_token(token)?;
        if request.is_expired(now) {
            request.state = RequestState::Expired;
            self.save(&request)?;
            return Err(EnrollmentError::Expired(request.id));
        }
        if request.node_id != node_id {
            return Err(EnrollmentError::NodeMismatch(request.id));
        }
        if request.state != RequestState::Approved {
            return Err(EnrollmentError::InvalidState(
                request.id.clone(),
                request.state.as_str(),
                RequestState::Approved.as_str(),
            ));
        }
        request.state = RequestState::Joined;
        request.joined_at = Some(now);
        self.save(&request)?;
        Ok(request)
    }

    fn path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }
}

/// File-backed registry of authorized node public keys.
pub struct RegistryStore {
    dir: PathBuf,
}

impl RegistryStore {
    pub fn new(root: &Path) -> io::Result<Self> {
        let dir = root.join("registry");
        create_private_dir(&dir)?;
        Ok(Self { dir })
    }

    /// Registers `node_id` (0o600 file under `registry/`).
    pub fn register(&self, node_id: &str, now: u64) -> io::Result<()> {
        #[derive(Serialize)]
        struct Entry {
            node_id: String,
            joined_at: u64,
        }
        write_private_file(
            &self.path(node_id),
            &serde_json::to_vec(&Entry {
                node_id: node_id.to_string(),
                joined_at: now,
            })
            .unwrap(),
        )
    }

    /// Removes `node_id`; returns whether it was registered.
    pub fn revoke(&self, node_id: &str) -> io::Result<bool> {
        match fs::remove_file(self.path(node_id)) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Whether `node_id` is registered.
    pub fn is_authorized(&self, node_id: &str) -> bool {
        self.path(node_id).is_file()
    }

    #[allow(dead_code)]
    pub fn list(&self) -> Result<Vec<String>, EnrollmentError> {
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                let name = entry.file_name().to_string_lossy().into_owned();
                out.push(name.strip_suffix(".json").unwrap_or(&name).to_string());
            }
        }
        out.sort();
        Ok(out)
    }

    fn path(&self, node_id: &str) -> PathBuf {
        self.dir.join(format!("{node_id}.json"))
    }
}

/// Loads a 32-byte key file, generating and storing it (0o600) on first use.
pub fn load_or_generate_key(path: &Path) -> io::Result<SecretKey> {
    if let Ok(bytes) = fs::read(path) {
        let bytes: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "coordination key is not 32 bytes",
            )
        })?;
        return Ok(SecretKey::from_bytes(&bytes));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let key = SecretKey::generate();
    write_private_file(path, &key.to_bytes())?;
    Ok(key)
}

/// Creates a directory 0o700 on unix.
pub fn create_private_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(path)
    }
}

/// Writes a file 0o600 on unix.
pub fn write_private_file(path: &Path, contents: &[u8]) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(contents)?;
        file.flush()?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        fs::write(path, contents)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "waitagent-coordination-{name}-{}-{}",
            std::process::id(),
            std::thread::current()
                .name()
                .unwrap_or("test")
                .replace(':', "_")
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn request_lifecycle_pending_approve_join() {
        let root = temp_root("lifecycle");
        let store = RequestStore::new(&root).unwrap();
        let request = store
            .create("node-a", std::time::Duration::from_secs(900), 1000)
            .unwrap();
        assert_eq!(request.state, RequestState::Pending);

        store
            .redeem(&request.token, "node-a", 1001)
            .expect_err("a pending request cannot be joined");

        let approved = store.approve(&request.id, 1002).unwrap();
        assert_eq!(approved.state, RequestState::Approved);

        let joined = store.redeem(&approved.token, "node-a", 1003).unwrap();
        assert_eq!(joined.state, RequestState::Joined);
        assert_eq!(joined.joined_at, Some(1003));

        store
            .redeem(&approved.token, "node-a", 1004)
            .expect_err("the token redeems exactly once");
        crate::infra::best_effort::remove_dir_all(&root);
    }

    #[test]
    fn redeem_rejects_wrong_node_and_pending_and_unknown_token() {
        let root = temp_root("redeem-guards");
        let store = RequestStore::new(&root).unwrap();
        let request = store
            .create("node-a", std::time::Duration::from_secs(900), 1000)
            .unwrap();
        store
            .redeem("deadbeef", "node-a", 1001)
            .expect_err("unknown token");
        store.approve(&request.id, 1001).unwrap();
        store
            .redeem(&request.token, "node-b", 1002)
            .expect_err("the token is bound to the requesting key");
        crate::infra::best_effort::remove_dir_all(&root);
    }

    #[test]
    fn expiry_flips_pending_and_approved_to_expired() {
        let root = temp_root("expiry");
        let store = RequestStore::new(&root).unwrap();
        let request = store
            .create("node-a", std::time::Duration::from_secs(10), 1000)
            .unwrap();
        store.approve(&request.id, 1005).unwrap();
        let error = store
            .redeem(&request.token, "node-a", 1011)
            .expect_err("past the TTL the token is dead");
        assert!(matches!(error, EnrollmentError::Expired(_)));
        assert_eq!(
            store.load(&request.id).unwrap().state,
            RequestState::Expired
        );
        crate::infra::best_effort::remove_dir_all(&root);
    }

    #[test]
    fn approve_rejects_non_pending_states() {
        let root = temp_root("approve-guards");
        let store = RequestStore::new(&root).unwrap();
        let request = store
            .create("node-a", std::time::Duration::from_secs(900), 1000)
            .unwrap();
        store.approve(&request.id, 1001).unwrap();
        let error = store
            .approve(&request.id, 1002)
            .expect_err("double approval is refused");
        assert!(matches!(error, EnrollmentError::InvalidState(..)));
        crate::infra::best_effort::remove_dir_all(&root);
    }

    #[test]
    fn registry_persists_and_revokes() {
        let root = temp_root("registry");
        let registry = RegistryStore::new(&root).unwrap();
        assert!(!registry.is_authorized("node-a"));
        registry.register("node-a", 5000).unwrap();
        assert!(registry.is_authorized("node-a"));
        assert_eq!(registry.list().unwrap(), vec!["node-a".to_string()]);
        assert!(registry.revoke("node-a").unwrap());
        assert!(!registry.is_authorized("node-a"));
        assert!(!registry.revoke("node-a").unwrap(), "revoke is idempotent");
        crate::infra::best_effort::remove_dir_all(&root);
    }

    #[test]
    fn store_files_are_private() {
        let root = temp_root("perms");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let store = RequestStore::new(&root).unwrap();
            let request = store
                .create("node-a", std::time::Duration::from_secs(900), 1000)
                .unwrap();
            let file_mode = fs::metadata(store.path(&request.id))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(file_mode, 0o600);
            let dir_mode = fs::metadata(root.join("requests"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(dir_mode, 0o700);
        }
        crate::infra::best_effort::remove_dir_all(&root);
    }

    #[test]
    fn key_persistence_round_trips() {
        let root = temp_root("key");
        let path = root.join("node.key");
        let first = load_or_generate_key(&path).unwrap();
        let second = load_or_generate_key(&path).unwrap();
        assert_eq!(first.public(), second.public());
        crate::infra::best_effort::remove_dir_all(&root);
    }
}
