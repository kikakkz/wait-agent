use ssh_key::{Algorithm, HashAlg, PrivateKey, PublicKey, SshSig};
use std::fs;
// `Write` is only exercised by the Unix 0o600-mode write paths; the Windows
// paths use `fs::write`.
#[cfg(unix)]
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Filename-safe fingerprint of a public key: base64(SHA-256 of key data).
pub fn public_key_fingerprint(public_key: &PublicKey) -> String {
    let fingerprint = public_key.fingerprint(HashAlg::Sha256);
    match fingerprint {
        ssh_key::Fingerprint::Sha256(bytes) => base64_encode(bytes),
        _ => unreachable!("SHA-256 fingerprint requested"),
    }
}

fn base64_encode(bytes: impl AsRef<[u8]>) -> String {
    use base64::{prelude::BASE64_URL_SAFE_NO_PAD, Engine};
    BASE64_URL_SAFE_NO_PAD.encode(bytes)
}

const SSHSIG_NAMESPACE: &str = "waitagent@wait-agent.io";

/// Errors that can occur during operator authentication.
#[derive(Debug, thiserror::Error)]
pub enum OperatorAuthError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to load SSH key: {0}")]
    KeyLoad(#[from] ssh_key::Error),
    #[error("unsupported key algorithm: {0}")]
    UnsupportedAlgorithm(String),
    #[error("signature verification failed")]
    VerificationFailed,
    #[error("signature production failed: {0}")]
    SignatureFailed(String),
    #[error("operator key store error: {0}")]
    KeyStore(String),
}

/// Abstracts storage of the operator private key.
pub trait OperatorKeyStore: Send + Sync {
    /// Returns the operator public key in OpenSSH format.
    fn public_key_openssh(&self) -> Result<String, OperatorAuthError>;

    /// Signs a server-issued challenge and returns the auth scheme plus SSH signature PEM.
    fn sign_challenge(&self, challenge: &[u8]) -> Result<(String, Vec<u8>), OperatorAuthError>;
}

const OPERATOR_KEYRING_SERVICE: &str = "waitagent";
const OPERATOR_KEYRING_ACCOUNT: &str = "operator.private_key";

/// Operator key stored in the OS keyring.
#[derive(Debug, Clone, Default)]
pub struct KeyringOperatorKeyStore;

impl KeyringOperatorKeyStore {
    fn entry() -> Result<keyring::Entry, OperatorAuthError> {
        keyring::Entry::new(OPERATOR_KEYRING_SERVICE, OPERATOR_KEYRING_ACCOUNT)
            .map_err(|error| OperatorAuthError::KeyStore(format!("keyring entry failed: {error}")))
    }

    fn load_private_key(&self) -> Result<PrivateKey, OperatorAuthError> {
        let entry = Self::entry()?;
        let pem = entry
            .get_password()
            .map_err(|error| OperatorAuthError::KeyStore(format!("keyring get failed: {error}")))?;
        Ok(PrivateKey::from_openssh(&pem)?)
    }
}

impl OperatorKeyStore for KeyringOperatorKeyStore {
    fn public_key_openssh(&self) -> Result<String, OperatorAuthError> {
        self.load_private_key()?
            .public_key()
            .to_openssh()
            .map_err(Into::into)
    }

    fn sign_challenge(&self, challenge: &[u8]) -> Result<(String, Vec<u8>), OperatorAuthError> {
        let private_key = self.load_private_key()?;
        sign_with_key(challenge, &private_key)
    }
}

/// In-memory operator key store for tests and other environments where the OS
/// keyring is unavailable.
#[derive(Debug, Clone)]
#[cfg(test)]
pub struct MemoryOperatorKeyStore {
    private_key_pem: String,
}

#[cfg(test)]
impl MemoryOperatorKeyStore {
    /// Creates a store from an existing OpenSSH private key PEM.
    pub fn from_pem(pem: impl Into<String>) -> Self {
        Self {
            private_key_pem: pem.into(),
        }
    }

    /// Generates a new Ed25519 key pair and returns its store.
    pub fn generate() -> Result<Self, OperatorAuthError> {
        let mut rng = rand_core::UnwrapErr(getrandom::SysRng);
        let private_key = PrivateKey::random(&mut rng, Algorithm::Ed25519).map_err(|error| {
            OperatorAuthError::KeyStore(format!("key generation failed: {error}"))
        })?;
        let pem = private_key
            .to_openssh(ssh_key::LineEnding::LF)
            .map_err(|error| {
                OperatorAuthError::KeyStore(format!("key encoding failed: {error}"))
            })?;
        Ok(Self::from_pem(pem.to_string()))
    }
}

#[cfg(test)]
impl OperatorKeyStore for MemoryOperatorKeyStore {
    fn public_key_openssh(&self) -> Result<String, OperatorAuthError> {
        let private_key = PrivateKey::from_openssh(&self.private_key_pem)?;
        Ok(private_key.public_key().to_openssh()?)
    }

    fn sign_challenge(&self, challenge: &[u8]) -> Result<(String, Vec<u8>), OperatorAuthError> {
        let private_key = PrivateKey::from_openssh(&self.private_key_pem)?;
        sign_with_key(challenge, &private_key)
    }
}

/// Ensures an Ed25519 operator private key exists on this host: in the OS
/// keyring when available, falling back to a file at
/// `~/.waitagent/operator.key` on headless hosts without a D-Bus secret
/// service.
///
/// If the keyring already contains a key, this is a no-op. Otherwise a new
/// Ed25519 key pair is generated and stored under the fixed keyring entry
/// `waitagent` / `operator.private_key`, or in the fallback file when the
/// keyring cannot hold it.
pub fn ensure_operator_key_in_keyring() -> Result<(), OperatorAuthError> {
    default_operator_key_store()
        .public_key_openssh()
        .map(|_| ())
}

/// Selects the operator key store for this host: the OS keyring when it holds
/// or can store the operator key, otherwise the `~/.waitagent/operator.key`
/// file fallback. The selection is cached for the lifetime of the process so
/// all call sites agree on a single backend.
fn select_operator_key_store() -> Arc<dyn OperatorKeyStore> {
    if let Ok(entry) = KeyringOperatorKeyStore::entry() {
        match entry.get_password() {
            Ok(_) => return Arc::new(KeyringOperatorKeyStore),
            Err(keyring::Error::NoEntry) => {
                match generate_operator_private_key()
                    .and_then(|key| encode_operator_private_key(&key).map(|pem| (key, pem)))
                {
                    Ok((private_key, pem)) => {
                        if entry.set_password(pem.as_str()).is_ok() {
                            return Arc::new(KeyringOperatorKeyStore);
                        }
                        // The keyring cannot store the new key; persist it in
                        // the file fallback instead so remote operator auth
                        // keeps working.
                        match FileOperatorKeyStore::default().store_private_key(&private_key) {
                            Ok(()) => return Arc::new(FileOperatorKeyStore::default()),
                            Err(error) => crate::infra::error_log::ERROR_LOG.log(format!(
                                "[operator-auth] failed to persist operator key: {error}"
                            )),
                        }
                    }
                    Err(error) => crate::infra::error_log::ERROR_LOG.log(format!(
                        "[operator-auth] operator key generation failed: {error}"
                    )),
                }
            }
            Err(_) => {
                // Keyring backend unavailable; fall through to the file store.
            }
        }
    }
    let store = Arc::new(FileOperatorKeyStore::default());
    if let Err(error) = store.load_or_generate() {
        crate::infra::error_log::ERROR_LOG.log(format!(
            "[operator-auth] file operator key store failed: {error}"
        ));
    }
    store
}

/// Returns the operator key store for this host (see
/// [`select_operator_key_store`]).
pub fn default_operator_key_store() -> Arc<dyn OperatorKeyStore> {
    static STORE: std::sync::OnceLock<Arc<dyn OperatorKeyStore>> = std::sync::OnceLock::new();
    STORE.get_or_init(select_operator_key_store).clone()
}

/// Operator key stored in a file under `~/.waitagent/`.
///
/// Used as a fallback on hosts where the OS keyring is unavailable (headless
/// servers without a D-Bus secret service). The private key is stored PEM
/// encoded with owner-only (`0o600`) permissions.
#[derive(Debug, Clone)]
pub struct FileOperatorKeyStore {
    key_path: PathBuf,
}

impl Default for FileOperatorKeyStore {
    fn default() -> Self {
        Self::new(default_operator_key_path())
    }
}

impl FileOperatorKeyStore {
    pub fn new(key_path: impl Into<PathBuf>) -> Self {
        Self {
            key_path: key_path.into(),
        }
    }

    /// Loads the private key from disk, generating and persisting a new
    /// Ed25519 key on first use.
    fn load_or_generate(&self) -> Result<PrivateKey, OperatorAuthError> {
        if let Ok(pem) = fs::read_to_string(&self.key_path) {
            if let Ok(private_key) = PrivateKey::from_openssh(&pem) {
                return Ok(private_key);
            }
        }
        let private_key = generate_operator_private_key()?;
        self.store_private_key(&private_key)?;
        Ok(private_key)
    }

    fn store_private_key(&self, private_key: &PrivateKey) -> Result<(), OperatorAuthError> {
        let pem = encode_operator_private_key(private_key)?;
        if let Some(parent) = self.key_path.parent() {
            fs::create_dir_all(parent)?;
        }
        write_private_key_file(&self.key_path, pem.as_str())
    }
}

impl OperatorKeyStore for FileOperatorKeyStore {
    fn public_key_openssh(&self) -> Result<String, OperatorAuthError> {
        self.load_or_generate()?
            .public_key()
            .to_openssh()
            .map_err(Into::into)
    }

    fn sign_challenge(&self, challenge: &[u8]) -> Result<(String, Vec<u8>), OperatorAuthError> {
        let private_key = self.load_or_generate()?;
        sign_with_key(challenge, &private_key)
    }
}

/// Returns the default `~/.waitagent/operator.key` path.
fn default_operator_key_path() -> PathBuf {
    crate::host::ssh::remote_host_home::waitagent_home().join("operator.key")
}

fn generate_operator_private_key() -> Result<PrivateKey, OperatorAuthError> {
    let mut rng = rand_core::UnwrapErr(getrandom::SysRng);
    PrivateKey::random(&mut rng, Algorithm::Ed25519)
        .map_err(|error| OperatorAuthError::KeyStore(format!("key generation failed: {error}")))
}

fn encode_operator_private_key(private_key: &PrivateKey) -> Result<String, OperatorAuthError> {
    private_key
        .to_openssh(ssh_key::LineEnding::LF)
        .map(|pem| pem.to_string())
        .map_err(|error| OperatorAuthError::KeyStore(format!("key encoding failed: {error}")))
}

#[cfg(unix)]
fn write_private_key_file(path: &Path, contents: &str) -> Result<(), OperatorAuthError> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(contents.as_bytes())?;
    Ok(())
}

#[cfg(not(unix))]
fn write_private_key_file(path: &Path, contents: &str) -> Result<(), OperatorAuthError> {
    fs::write(path, contents)?;
    Ok(())
}

/// Signs a server-issued challenge using the given private key.
fn sign_with_key(
    challenge: &[u8],
    private_key: &PrivateKey,
) -> Result<(String, Vec<u8>), OperatorAuthError> {
    let scheme = auth_scheme_for_algorithm(&private_key.algorithm())?;
    let hash_alg = signing_hash_alg(&private_key.algorithm());
    let signature = private_key
        .sign(SSHSIG_NAMESPACE, hash_alg, challenge)
        .map_err(|error| OperatorAuthError::SignatureFailed(format!("{error:?}")))?;

    Ok((
        scheme,
        signature.to_pem(ssh_key::LineEnding::LF)?.into_bytes(),
    ))
}

/// Verifies that `signature_pem` is a valid SSH signature over `challenge`
/// produced by `public_key` under the given auth scheme.
pub fn verify_challenge(
    challenge: &[u8],
    scheme: &str,
    signature_pem: &[u8],
    public_key: &PublicKey,
) -> Result<(), OperatorAuthError> {
    if !algorithm_matches_scheme(&public_key.algorithm(), scheme) {
        return Err(OperatorAuthError::UnsupportedAlgorithm(format!(
            "public key algorithm {:?} does not match scheme {}",
            public_key.algorithm(),
            scheme
        )));
    }

    let signature_pem =
        std::str::from_utf8(signature_pem).map_err(|_| OperatorAuthError::VerificationFailed)?;
    let signature = SshSig::from_pem(signature_pem)?;
    public_key
        .verify(SSHSIG_NAMESPACE, challenge, &signature)
        .map_err(|_| OperatorAuthError::VerificationFailed)
}

/// Lists all authorized operator public keys in `dir`. Returns tuples of
/// `(fingerprint, public_key)`.
pub fn list_authorized_operators(
    dir: &Path,
) -> Result<Vec<(String, PublicKey)>, OperatorAuthError> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut out = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("pub") {
            continue;
        }
        let text = fs::read_to_string(&path)?;
        let public_key = PublicKey::from_openssh(&text)?;
        out.push((public_key_fingerprint(&public_key), public_key));
    }
    Ok(out)
}

fn signing_hash_alg(algorithm: &Algorithm) -> HashAlg {
    match algorithm {
        Algorithm::Ed25519 => HashAlg::Sha512,
        Algorithm::Rsa { hash } => hash.unwrap_or(HashAlg::Sha256),
        _ => HashAlg::Sha512,
    }
}

fn algorithm_matches_scheme(algorithm: &Algorithm, scheme: &str) -> bool {
    matches!(
        (algorithm, scheme),
        (Algorithm::Ed25519, "ssh-ed25519-challenge")
            | (Algorithm::Rsa { .. }, "ssh-rsa-challenge")
    )
}

fn auth_scheme_for_algorithm(algorithm: &Algorithm) -> Result<String, OperatorAuthError> {
    match algorithm {
        Algorithm::Ed25519 => Ok("ssh-ed25519-challenge".to_string()),
        Algorithm::Rsa { .. } => Ok("ssh-rsa-challenge".to_string()),
        other => Err(OperatorAuthError::UnsupportedAlgorithm(format!(
            "{other:?}"
        ))),
    }
}

/// Returns the default `~/.waitagent/authorized_operators/` directory.
pub fn default_authorized_operators_dir() -> PathBuf {
    crate::host::ssh::remote_host_home::waitagent_home().join("authorized_operators")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "waitagent-operator-auth-{name}-{}-{}",
            std::process::id(),
            std::thread::current()
                .name()
                .unwrap_or("test")
                .replace(":", "_")
        ))
    }

    #[test]
    fn ed25519_sign_verify_roundtrip() {
        const ED25519_TEST_KEY_PEM: &str = include_str!("../../tests/fixtures/ed25519_test_key");

        let private_key = PrivateKey::from_openssh(ED25519_TEST_KEY_PEM).unwrap();
        let public_key = private_key.public_key().clone();

        let challenge = b"random challenge";
        let (scheme, signature) = sign_with_key(challenge, &private_key).unwrap();
        assert_eq!(scheme, "ssh-ed25519-challenge");

        verify_challenge(challenge, &scheme, &signature, &public_key).unwrap();
    }

    #[test]
    fn rsa_sign_verify_roundtrip() {
        // ssh_key::PrivateKey::random for RSA is unreliable in tests, so we use
        // a pre-generated fixture key that is only used for this unit test.
        const RSA_TEST_KEY_PEM: &str = include_str!("../../tests/fixtures/rsa_test_key");

        let private_key = PrivateKey::from_openssh(RSA_TEST_KEY_PEM).unwrap();
        let public_key = private_key.public_key().clone();

        let challenge = b"random challenge";
        let (scheme, signature) = sign_with_key(challenge, &private_key).unwrap();
        assert_eq!(scheme, "ssh-rsa-challenge");

        verify_challenge(challenge, &scheme, &signature, &public_key).unwrap();
    }

    #[test]
    fn verify_fails_with_wrong_key() {
        const ALICE_KEY_PEM: &str = include_str!("../../tests/fixtures/ed25519_test_key");
        const BOB_KEY_PEM: &str = include_str!("../../tests/fixtures/rsa_test_key");

        let alice = PrivateKey::from_openssh(ALICE_KEY_PEM).unwrap();
        let bob = PrivateKey::from_openssh(BOB_KEY_PEM).unwrap();

        let challenge = b"random challenge";
        let (scheme, signature) = sign_with_key(challenge, &alice).unwrap();

        assert!(verify_challenge(challenge, &scheme, &signature, bob.public_key()).is_err());
        let _ = alice;
    }

    #[test]
    fn file_operator_key_store_generates_and_persists() {
        let dir = temp_dir("file-store");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("operator.key");

        let store = FileOperatorKeyStore::new(&path);
        let public_key = store.public_key_openssh().unwrap();

        // A second store over the same file must load the same key.
        let store2 = FileOperatorKeyStore::new(&path);
        assert_eq!(store2.public_key_openssh().unwrap(), public_key);

        // The persisted key must round-trip through the challenge signing path.
        let challenge = b"file store challenge";
        let (scheme, signature) = store2.sign_challenge(challenge).unwrap();
        let public_key_parsed = PublicKey::from_openssh(&public_key).unwrap();
        verify_challenge(challenge, &scheme, &signature, &public_key_parsed).unwrap();

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn list_authorized_operators_finds_written_keys() {
        let dir = temp_dir("authorized");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        const ED25519_TEST_KEY_PEM: &str = include_str!("../../tests/fixtures/ed25519_test_key");
        let private_key = PrivateKey::from_openssh(ED25519_TEST_KEY_PEM).unwrap();
        let public_key = private_key.public_key();
        let fingerprint = public_key_fingerprint(public_key);

        let path = dir.join(format!("{fingerprint}.pub"));
        let public_key_text = public_key.to_openssh().unwrap();
        fs::write(&path, &public_key_text).unwrap();

        let operators = list_authorized_operators(&dir).unwrap();
        assert_eq!(operators.len(), 1);
        assert_eq!(operators[0].0, fingerprint);

        let _ = fs::remove_dir_all(&dir);
    }
}
