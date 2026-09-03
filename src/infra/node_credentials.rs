use sha2::{Digest, Sha256};
use std::fs;
use std::io;
// `Write` is only exercised by the Unix 0o600-mode write path; the Windows
// path uses `fs::write`.
#[cfg(unix)]
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::host::ssh::remote_host_home::waitagent_home;

/// Paths to the node's TLS private key and self-signed certificate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeCredentialPaths {
    pub key_path: PathBuf,
    pub cert_path: PathBuf,
}

impl NodeCredentialPaths {
    /// Returns the default credential paths under `~/.waitagent/`.
    pub fn default_paths() -> Self {
        let home = waitagent_home();
        Self {
            key_path: home.join("node.key"),
            cert_path: home.join("node.crt"),
        }
    }

    /// Returns credential paths that are resolved by the remote shell.
    ///
    /// Use these when building SSH remote commands so the remote host's `$HOME`
    /// is expanded on the remote side rather than resolving the control host's
    /// home directory locally.
    pub fn remote_default_paths() -> Self {
        Self {
            key_path: PathBuf::from("$HOME/.waitagent/node.key"),
            cert_path: PathBuf::from("$HOME/.waitagent/node.crt"),
        }
    }
}

/// Errors that can occur while generating or loading node credentials.
#[derive(Debug, thiserror::Error)]
pub enum NodeCredentialsError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("failed to generate certificate: {0}")]
    CertificateGeneration(String),
    #[error("failed to parse certificate: {0}")]
    CertificateParse(String),
    #[error("no end-entity certificate found in PEM file")]
    MissingEndEntityCertificate,
}

/// Ensures that a TLS key pair and self-signed certificate exist at the given
/// paths. If the certificate already exists, its SHA-256 SPKI fingerprint is
/// returned without modifying the files.
pub fn ensure_credentials(paths: &NodeCredentialPaths) -> Result<String, NodeCredentialsError> {
    if paths.cert_path.is_file() {
        return load_cert_fingerprint(&paths.cert_path);
    }
    generate_credentials(paths)
}

/// Generates a new Ed25519 TLS key pair and self-signed certificate, writes
/// them to the given paths, and returns the SHA-256 SPKI fingerprint.
pub fn generate_credentials(paths: &NodeCredentialPaths) -> Result<String, NodeCredentialsError> {
    let key_pair = rcgen::KeyPair::generate(&rcgen::PKCS_ED25519)
        .map_err(|error| NodeCredentialsError::CertificateGeneration(error.to_string()))?;

    let mut params = rcgen::CertificateParams::new(vec!["waitagent".to_string()]);
    params.key_pair = Some(key_pair);
    params.alg = &rcgen::PKCS_ED25519;
    params.is_ca = rcgen::IsCa::NoCa;

    let cert = rcgen::Certificate::from_params(params)
        .map_err(|error| NodeCredentialsError::CertificateGeneration(error.to_string()))?;

    let key_pem = cert.serialize_private_key_pem();
    let cert_pem = cert
        .serialize_pem()
        .map_err(|error| NodeCredentialsError::CertificateGeneration(error.to_string()))?;

    if let Some(parent) = paths.key_path.parent() {
        fs::create_dir_all(parent)?;
    }
    if let Some(parent) = paths.cert_path.parent() {
        fs::create_dir_all(parent)?;
    }

    write_private_key(&paths.key_path, key_pem.as_bytes())?;
    fs::write(&paths.cert_path, cert_pem.as_bytes())?;

    load_cert_fingerprint(&paths.cert_path)
}

fn write_private_key(path: &Path, contents: &[u8]) -> io::Result<()> {
    #[cfg(unix)]
    {
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

/// Loads an existing certificate and returns the lowercase hex SHA-256
/// fingerprint of its SubjectPublicKeyInfo.
pub fn load_cert_fingerprint(cert_path: &Path) -> Result<String, NodeCredentialsError> {
    let pem = fs::read_to_string(cert_path)?;
    let mut reader = pem.as_bytes();
    let certs: Vec<_> = rustls_pemfile::certs(&mut reader)
        .collect::<Result<_, _>>()
        .map_err(|error| NodeCredentialsError::CertificateParse(error.to_string()))?;

    let cert = certs
        .into_iter()
        .next()
        .ok_or(NodeCredentialsError::MissingEndEntityCertificate)?;

    let spki = extract_spki_from_cert_der(&cert)?;
    let fingerprint = Sha256::digest(&spki);
    Ok(hex_encode(&fingerprint))
}

/// Extracts the SubjectPublicKeyInfo (SPKI) DER bytes from an X.509 certificate.
///
/// This is a focused DER walker that understands just enough of the
/// TBSCertificate layout to reach the SPKI field. It is intentionally not a
/// general-purpose X.509 parser.
pub(crate) fn extract_spki_from_cert_der(cert_der: &[u8]) -> Result<Vec<u8>, NodeCredentialsError> {
    // Enter the outer Certificate sequence.
    let cert_contents = read_der_sequence(cert_der, &mut 0).ok_or_else(|| {
        NodeCredentialsError::CertificateParse("invalid certificate DER".to_string())
    })?;
    // Enter the TBSCertificate sequence.
    let tbs = read_der_sequence(cert_contents, &mut 0).ok_or_else(|| {
        NodeCredentialsError::CertificateParse("invalid TBSCertificate".to_string())
    })?;
    if tbs.is_empty() {
        return Err(NodeCredentialsError::CertificateParse(
            "empty TBSCertificate".to_string(),
        ));
    }

    let mut tbs_offset = 0;
    // version [0] is optional; if present it precedes serialNumber.
    if tbs.get(tbs_offset) == Some(&0xA0) {
        skip_der_element(tbs, &mut tbs_offset).ok_or_else(|| {
            NodeCredentialsError::CertificateParse("invalid version field".to_string())
        })?;
    }
    // serialNumber INTEGER
    skip_der_element(tbs, &mut tbs_offset).ok_or_else(|| {
        NodeCredentialsError::CertificateParse("invalid serialNumber".to_string())
    })?;
    // signature AlgorithmIdentifier
    skip_der_sequence(tbs, &mut tbs_offset)
        .ok_or_else(|| NodeCredentialsError::CertificateParse("invalid signature".to_string()))?;
    // issuer Name
    skip_der_sequence(tbs, &mut tbs_offset)
        .ok_or_else(|| NodeCredentialsError::CertificateParse("invalid issuer".to_string()))?;
    // validity Validity
    skip_der_sequence(tbs, &mut tbs_offset)
        .ok_or_else(|| NodeCredentialsError::CertificateParse("invalid validity".to_string()))?;
    // subject Name
    skip_der_sequence(tbs, &mut tbs_offset)
        .ok_or_else(|| NodeCredentialsError::CertificateParse("invalid subject".to_string()))?;
    // subjectPublicKeyInfo
    let spki_start = tbs_offset;
    skip_der_sequence(tbs, &mut tbs_offset).ok_or_else(|| {
        NodeCredentialsError::CertificateParse("invalid subjectPublicKeyInfo".to_string())
    })?;
    Ok(tbs[spki_start..tbs_offset].to_vec())
}

fn read_der_length(buf: &[u8], offset: &mut usize) -> Option<usize> {
    if *offset >= buf.len() {
        return None;
    }
    let first = buf[*offset];
    *offset += 1;
    if first & 0x80 == 0 {
        Some(first as usize)
    } else {
        let num_bytes = (first & 0x7F) as usize;
        if num_bytes == 0 || *offset + num_bytes > buf.len() {
            return None;
        }
        let mut len = 0usize;
        for i in 0..num_bytes {
            len = (len << 8) | buf[*offset + i] as usize;
        }
        *offset += num_bytes;
        Some(len)
    }
}

fn read_der_element<'a>(buf: &'a [u8], offset: &mut usize) -> Option<&'a [u8]> {
    if *offset >= buf.len() {
        return None;
    }
    let _tag = buf[*offset];
    *offset += 1;
    let len = read_der_length(buf, offset)?;
    let end = (*offset).checked_add(len)?;
    if end > buf.len() {
        return None;
    }
    let element = &buf[*offset..end];
    *offset = end;
    Some(element)
}

fn read_der_sequence<'a>(buf: &'a [u8], offset: &mut usize) -> Option<&'a [u8]> {
    if *offset >= buf.len() || buf[*offset] != 0x30 {
        return None;
    }
    read_der_element(buf, offset)
}

fn skip_der_element(buf: &[u8], offset: &mut usize) -> Option<()> {
    read_der_element(buf, offset)?;
    Some(())
}

fn skip_der_sequence(buf: &[u8], offset: &mut usize) -> Option<()> {
    read_der_sequence(buf, offset)?;
    Some(())
}

fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_paths(name: &str) -> NodeCredentialPaths {
        let dir = std::env::temp_dir().join(format!(
            "waitagent-node-credentials-{name}-{}-{}",
            std::process::id(),
            std::thread::current()
                .name()
                .unwrap_or("test")
                .replace(":", "_")
        ));
        let _ = fs::remove_dir_all(&dir);
        NodeCredentialPaths {
            key_path: dir.join("node.key"),
            cert_path: dir.join("node.crt"),
        }
    }

    #[test]
    fn fingerprint_roundtrip() {
        let paths = temp_paths("roundtrip");
        let fingerprint = generate_credentials(&paths).unwrap();
        assert_eq!(fingerprint.len(), 64);
        assert!(fingerprint.chars().all(|ch| ch.is_ascii_hexdigit()));

        let loaded = load_cert_fingerprint(&paths.cert_path).unwrap();
        assert_eq!(loaded, fingerprint);

        crate::infra::best_effort::remove_file(&paths.key_path);
        crate::infra::best_effort::remove_file(&paths.cert_path);
    }

    #[test]
    fn ensure_credentials_skips_existing() {
        let paths = temp_paths("ensure");
        let first = ensure_credentials(&paths).unwrap();
        let second = ensure_credentials(&paths).unwrap();
        assert_eq!(first, second);

        crate::infra::best_effort::remove_file(&paths.key_path);
        crate::infra::best_effort::remove_file(&paths.cert_path);
    }

    #[cfg(unix)]
    #[test]
    fn key_file_has_600_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let paths = temp_paths("perms");
        generate_credentials(&paths).unwrap();
        let metadata = fs::metadata(&paths.key_path).unwrap();
        let mode = metadata.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        crate::infra::best_effort::remove_file(&paths.key_path);
        crate::infra::best_effort::remove_file(&paths.cert_path);
    }
}
