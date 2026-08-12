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
}

/// Signs a server-issued challenge using the SSH private key at the given path.
/// Returns the auth scheme (`ssh-ed25519-challenge` or `ssh-rsa-challenge`) and
/// the SSH `SshSig` PEM bytes.
pub fn sign_challenge(
    challenge: &[u8],
    private_key_path: &Path,
) -> Result<(String, Vec<u8>), OperatorAuthError> {
    let pem = fs::read_to_string(private_key_path)?;
    let private_key = PrivateKey::from_openssh(&pem)?;

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

/// Loads an SSH private key from a file and returns its public key in OpenSSH
/// format. This is used during bootstrap to install the operator's public key
/// on the remote host without uploading the private key.
pub fn public_key_from_private_key_file(
    private_key_path: &Path,
) -> Result<String, OperatorAuthError> {
    let pem = fs::read_to_string(private_key_path)?;
    let private_key = PrivateKey::from_openssh(&pem)?;
    Ok(private_key.public_key().to_openssh()?)
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

        let dir = temp_dir("ed25519");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("key");
        fs::write(&path, ED25519_TEST_KEY_PEM).unwrap();

        let private_key = PrivateKey::from_openssh(ED25519_TEST_KEY_PEM).unwrap();
        let public_key = private_key.public_key().clone();

        let challenge = b"random challenge";
        let (scheme, signature) = sign_challenge(challenge, &path).unwrap();
        assert_eq!(scheme, "ssh-ed25519-challenge");

        verify_challenge(challenge, &scheme, &signature, &public_key).unwrap();
    }

    #[test]
    fn rsa_sign_verify_roundtrip() {
        // ssh_key::PrivateKey::random for RSA is unreliable in tests, so we use
        // a pre-generated fixture key that is only used for this unit test.
        const RSA_TEST_KEY_PEM: &str = include_str!("../../tests/fixtures/rsa_test_key");

        let dir = temp_dir("rsa");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("key");
        fs::write(&path, RSA_TEST_KEY_PEM).unwrap();

        let private_key = PrivateKey::from_openssh(RSA_TEST_KEY_PEM).unwrap();
        let public_key = private_key.public_key().clone();

        let challenge = b"random challenge";
        let (scheme, signature) = sign_challenge(challenge, &path).unwrap();
        assert_eq!(scheme, "ssh-rsa-challenge");

        verify_challenge(challenge, &scheme, &signature, &public_key).unwrap();
    }

    #[test]
    fn verify_fails_with_wrong_key() {
        const ALICE_KEY_PEM: &str = include_str!("../../tests/fixtures/ed25519_test_key");
        const BOB_KEY_PEM: &str = include_str!("../../tests/fixtures/rsa_test_key");

        let dir = temp_dir("alice");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("key");
        fs::write(&path, ALICE_KEY_PEM).unwrap();

        let alice = PrivateKey::from_openssh(ALICE_KEY_PEM).unwrap();
        let bob = PrivateKey::from_openssh(BOB_KEY_PEM).unwrap();

        let challenge = b"random challenge";
        let (scheme, signature) = sign_challenge(challenge, &path).unwrap();

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
