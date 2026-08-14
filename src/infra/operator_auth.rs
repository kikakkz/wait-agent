use ssh_key::{Algorithm, HashAlg, PrivateKey, PublicKey, SshSig};
use std::fs;
use std::path::{Path, PathBuf};

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

/// Ensures an Ed25519 operator private key exists in the OS keyring.
///
/// If the keyring already contains a key, this is a no-op. Otherwise a new
/// Ed25519 key pair is generated and stored under the fixed entry
/// `waitagent` / `operator.private_key`.
pub fn ensure_operator_key_in_keyring() -> Result<(), OperatorAuthError> {
    let entry = KeyringOperatorKeyStore::entry()?;
    match entry.get_password() {
        Ok(_) => return Ok(()),
        Err(keyring::Error::NoEntry) => {}
        Err(error) => {
            return Err(OperatorAuthError::KeyStore(format!(
                "keyring lookup failed: {error}"
            )))
        }
    }
    let mut rng = rand_core::UnwrapErr(getrandom::SysRng);
    let private_key = PrivateKey::random(&mut rng, Algorithm::Ed25519)
        .map_err(|error| OperatorAuthError::KeyStore(format!("key generation failed: {error}")))?;
    let pem = private_key
        .to_openssh(ssh_key::LineEnding::LF)
        .map_err(|error| OperatorAuthError::KeyStore(format!("key encoding failed: {error}")))?;
    entry
        .set_password(pem.as_ref())
        .map_err(|error| OperatorAuthError::KeyStore(format!("keyring set failed: {error}")))?;
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
        .map_err(|error| OperatorAuthError::SignatureFailed(format!("{:?}", error)))?;

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
            std::thread::current().name().unwrap_or("test")
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
