//! Local cache for waitagent release artifacts (issue #168).
//!
//! When a saved host profile sets `install_source = "upload"`, this machine
//! — not the remote host — downloads the release artifact and uploads it to
//! the remote over the SSH exec channel. The cache lives under
//! `~/.waitagent/cache/` and stores one artifact plus a `.sha256` sidecar
//! per (target triple, version) pair, named exactly like the published
//! release asset (`waitagent-<version>-<target>.tar.gz`).
//!
//! Trust model: GitHub releases publish no checksum assets, so a first
//! download has no upstream digest to compare against. The freshly
//! downloaded bytes are instead validated structurally (a well-formed
//! `.tar.gz` archive containing the `waitagent` binary), hashed, and the
//! digest is recorded in the sidecar. Every later cache hit must reproduce
//! that digest, so a corrupted or locally tampered copy is detected and
//! re-downloaded. The chain closes on the remote side: the bootstrapper
//! only accepts the install when the installed binary reports the expected
//! version.
//!
//! Concurrency: the cache is plain filesystem state touched only from the
//! per-profile connect worker thread; concurrent connects for *different*
//! profiles may race on the same artifact. Writes go through a temp file +
//! rename on one directory, so readers never observe a torn file — the
//! worst case is a redundant parallel download, never a corrupt cache read.
//! No locks are involved and none may be added without documenting the
//! lock order (m07-concurrency).

use crate::host::ssh::remote_host_home::waitagent_home;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Release download base; mirrors `REPO` in `scripts/install.sh`.
const RELEASE_DOWNLOAD_BASE: &str = "https://github.com/kikakkz/wait-agent/releases/download";

/// Download attempts for one `ensure_artifact` call: an initial download
/// plus one re-download after a fetch/verify failure, then a hard error.
const DOWNLOAD_ATTEMPTS: usize = 2;

/// Release-artifact target triples with published assets
/// (`.github/workflows/release.yaml`). The mapping mirrors the tarball
/// selection in `scripts/install.sh`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactTarget {
    LinuxX86_64,
    MacosAArch64,
}

impl ArtifactTarget {
    /// Maps remote `uname -s` / `uname -m` output to a target, mirroring
    /// `detect_platform` in `scripts/install.sh` (including the Apple
    /// Silicon-only macOS restriction).
    pub fn from_uname(system: &str, machine: &str) -> Result<Self, LocalArtifactError> {
        let os = match system.trim() {
            "Linux" => "linux",
            "Darwin" => "macos",
            _ => {
                return Err(LocalArtifactError::UnsupportedTarget {
                    system: system.trim().to_string(),
                    machine: machine.trim().to_string(),
                });
            }
        };
        let arch = match machine.trim() {
            "x86_64" | "amd64" => "x86_64",
            "aarch64" | "arm64" => "aarch64",
            _ => {
                return Err(LocalArtifactError::UnsupportedTarget {
                    system: system.trim().to_string(),
                    machine: machine.trim().to_string(),
                });
            }
        };
        match (os, arch) {
            ("linux", "x86_64") => Ok(Self::LinuxX86_64),
            ("macos", "aarch64") => Ok(Self::MacosAArch64),
            _ => Err(LocalArtifactError::UnsupportedTarget {
                system: system.trim().to_string(),
                machine: machine.trim().to_string(),
            }),
        }
    }

    /// Cache/archive file name, identical to the published release asset.
    pub fn file_name(self, version: &str) -> String {
        let target = match self {
            Self::LinuxX86_64 => "x86_64-linux",
            Self::MacosAArch64 => "aarch64-macos",
        };
        format!("waitagent-{version}-{target}.tar.gz")
    }

    /// Full release download URL for this target and version.
    pub fn download_url(self, version: &str) -> String {
        format!(
            "{RELEASE_DOWNLOAD_BASE}/v{version}/{}",
            self.file_name(version)
        )
    }
}

#[derive(Debug)]
pub enum LocalArtifactError {
    /// The remote `uname` pair has no published waitagent release asset.
    UnsupportedTarget { system: String, machine: String },
    /// The artifact could not be downloaded and validated within the
    /// attempt budget (network failure or invalid archive bytes).
    Download { url: String, attempts: Vec<String> },
    /// Downloaded bytes are not a well-formed waitagent release archive.
    InvalidArchive(String),
    /// A filesystem operation failed.
    Io(io::Error),
    /// The requested version is not a sane release version string.
    InvalidVersion(String),
}

impl fmt::Display for LocalArtifactError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedTarget { system, machine } => write!(
                f,
                "unsupported remote target `{system}`/`{machine}`: the local upload install source supports linux/x86_64 and macos/aarch64 (the release assets install.sh supports); choose Remote install source for this host"
            ),
            Self::Download { url, attempts } => write!(
                f,
                "failed to download a valid artifact from {url} after {} attempt(s): {}",
                attempts.len(),
                attempts.join("; ")
            ),
            Self::InvalidArchive(reason) => {
                write!(f, "downloaded artifact is not a valid waitagent archive: {reason}")
            }
            Self::Io(error) => write!(f, "artifact cache filesystem error: {error}"),
            Self::InvalidVersion(version) => {
                write!(f, "invalid release version `{version}`")
            }
        }
    }
}

impl std::error::Error for LocalArtifactError {}

/// Fetches a URL into memory. Sealed behind a trait so `ensure_artifact` is
/// unit-testable without network access.
pub trait ArtifactFetcher {
    fn fetch(&self, url: &str) -> Result<Vec<u8>, LocalArtifactError>;
}

/// Production fetcher: plain HTTPS via `ureq` (rustls), matching the
/// existing MSYS2 provisioning download in `src/platform/msys_env.rs`.
#[derive(Debug, Clone, Copy, Default)]
pub struct UreqArtifactFetcher;

impl ArtifactFetcher for UreqArtifactFetcher {
    fn fetch(&self, url: &str) -> Result<Vec<u8>, LocalArtifactError> {
        let response = ureq::get(url)
            .call()
            .map_err(|error| LocalArtifactError::Download {
                url: url.to_string(),
                attempts: vec![format!("http request failed: {error}")],
            })?;
        response
            .into_body()
            .read_to_vec()
            .map_err(|error| LocalArtifactError::Download {
                url: url.to_string(),
                attempts: vec![format!("reading response body failed: {error}")],
            })
    }
}

/// Filesystem cache of verified release artifacts.
#[derive(Debug, Clone)]
pub struct LocalArtifactCache {
    dir: PathBuf,
}

impl Default for LocalArtifactCache {
    fn default() -> Self {
        Self::new(waitagent_home().join("cache"))
    }
}

impl LocalArtifactCache {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// Returns the path to a locally verified artifact for `target` and
    /// `version`, downloading it on a cache miss or when the cached copy
    /// fails its recorded sha256. On a fetch or structural-validation
    /// failure the download is retried once and then surfaces a hard error.
    pub fn ensure_artifact(
        &self,
        target: ArtifactTarget,
        version: &str,
        fetcher: &dyn ArtifactFetcher,
    ) -> Result<PathBuf, LocalArtifactError> {
        validate_version(version)?;
        let artifact = self.dir.join(target.file_name(version));
        let sidecar = sidecar_path(&artifact);
        if artifact.is_file() && sidecar.is_file() {
            let data = fs::read(&artifact).map_err(LocalArtifactError::Io)?;
            match read_sidecar_digest(&sidecar)? {
                Some(expected) if expected == sha256_hex(&data) => return Ok(artifact),
                // Corrupt or tampered cache: fall through and re-download.
                _ => {}
            }
        }
        let data = download_verified(target, version, fetcher)?;
        write_artifact(&artifact, &sidecar, &data)?;
        Ok(artifact)
    }
}

fn validate_version(version: &str) -> Result<(), LocalArtifactError> {
    let valid = !version.is_empty()
        && version
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_' | '+'));
    if valid {
        Ok(())
    } else {
        Err(LocalArtifactError::InvalidVersion(version.to_string()))
    }
}

fn sidecar_path(artifact: &Path) -> PathBuf {
    let mut name = artifact.file_name().map_or_else(
        || "artifact.tar.gz".to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    name.push_str(".sha256");
    artifact.with_file_name(name)
}

fn read_sidecar_digest(sidecar: &Path) -> Result<Option<String>, LocalArtifactError> {
    let text = match fs::read_to_string(sidecar) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(LocalArtifactError::Io(error)),
    };
    // Accept both `sha256sum` output (`<hex>  <name>`) and a bare hex line.
    let digest = text
        .lines()
        .find_map(|line| line.split_whitespace().next())
        .filter(|token| token.len() == 64 && token.chars().all(|ch| ch.is_ascii_hexdigit()))
        .map(str::to_ascii_lowercase);
    Ok(digest)
}

/// Hex-encoded sha256 of `data` (same shape as `msys_env::sha256_hex`).
fn sha256_hex(data: &[u8]) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(data);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Downloads and structurally validates the artifact, retrying once after a
/// failure and then returning a hard error carrying every attempt.
fn download_verified(
    target: ArtifactTarget,
    version: &str,
    fetcher: &dyn ArtifactFetcher,
) -> Result<Vec<u8>, LocalArtifactError> {
    let url = target.download_url(version);
    let mut attempts = Vec::new();
    for _ in 0..DOWNLOAD_ATTEMPTS {
        match fetcher.fetch(&url).and_then(|data| {
            validate_archive(&data)?;
            Ok(data)
        }) {
            Ok(data) => return Ok(data),
            Err(error) => attempts.push(error.to_string()),
        }
    }
    Err(LocalArtifactError::Download { url, attempts })
}

/// A downloaded artifact must at least be a well-formed `.tar.gz` archive
/// that contains the `waitagent` binary, exactly like the release asset
/// layout install.sh unpacks. This catches HTML error pages, truncated
/// transfers, and garbage bytes before they reach the remote host.
fn validate_archive(data: &[u8]) -> Result<(), LocalArtifactError> {
    let decoder = flate2::read::GzDecoder::new(data);
    let mut archive = tar::Archive::new(decoder);
    let mut found_binary = false;
    let entries = archive.entries().map_err(|error| {
        LocalArtifactError::InvalidArchive(format!("unreadable tar.gz: {error}"))
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            LocalArtifactError::InvalidArchive(format!("unreadable tar entry: {error}"))
        })?;
        let path = entry.path().map_err(|error| {
            LocalArtifactError::InvalidArchive(format!("unreadable tar entry path: {error}"))
        })?;
        if path.components().count() == 1
            && path.file_name() == Some(std::ffi::OsStr::new("waitagent"))
        {
            found_binary = true;
        }
    }
    if found_binary {
        Ok(())
    } else {
        Err(LocalArtifactError::InvalidArchive(
            "archive does not contain a `waitagent` binary".to_string(),
        ))
    }
}

/// Writes artifact + sidecar through temp files and renames so a concurrent
/// reader (or a crash mid-write) never sees a partial file.
fn write_artifact(artifact: &Path, sidecar: &Path, data: &[u8]) -> Result<(), LocalArtifactError> {
    if let Some(parent) = artifact.parent() {
        fs::create_dir_all(parent).map_err(LocalArtifactError::Io)?;
    }
    let digest = sha256_hex(data);
    write_via_rename(artifact, data)?;
    let sidecar_contents = format!(
        "{digest}  {}\n",
        artifact.file_name().map_or_else(
            || "artifact".to_string(),
            |name| name.to_string_lossy().into_owned(),
        )
    );
    write_via_rename(sidecar, sidecar_contents.as_bytes())?;
    Ok(())
}

fn write_via_rename(path: &Path, contents: &[u8]) -> Result<(), LocalArtifactError> {
    let temp = path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&temp, contents).map_err(LocalArtifactError::Io)?;
    fs::rename(&temp, path).map_err(LocalArtifactError::Io)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    #[derive(Default)]
    struct FakeFetcher {
        responses: RefCell<HashMap<String, Result<Vec<u8>, String>>>,
        attempted: RefCell<Vec<String>>,
    }

    impl FakeFetcher {
        fn serving(mut self, url: &str, data: Vec<u8>) -> Self {
            self.responses.get_mut().insert(url.to_string(), Ok(data));
            self
        }

        fn failing(mut self, url: &str, error: &str) -> Self {
            self.responses
                .get_mut()
                .insert(url.to_string(), Err(error.to_string()));
            self
        }

        fn attempted_count(&self) -> usize {
            self.attempted.borrow().len()
        }
    }

    impl ArtifactFetcher for FakeFetcher {
        fn fetch(&self, url: &str) -> Result<Vec<u8>, LocalArtifactError> {
            self.attempted.borrow_mut().push(url.to_string());
            let response = self
                .responses
                .borrow()
                .get(url)
                .cloned()
                .unwrap_or_else(|| Err(format!("no fixture for {url}")));
            response.map_err(|error| LocalArtifactError::Download {
                url: url.to_string(),
                attempts: vec![error],
            })
        }
    }

    fn sample_artifact() -> Vec<u8> {
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let mut builder = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        let body = b"#!/bin/sh\necho waitagent\n";
        header.set_size(body.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, "waitagent", &body[..])
            .expect("append waitagent entry");
        let encoder = builder.into_inner().expect("finish tar archive");
        encoder.finish().expect("finish gzip stream")
    }

    fn cache_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "waitagent-artifact-cache-{name}-{}-{}",
            std::process::id(),
            std::thread::current()
                .name()
                .unwrap_or("test")
                .replace(':', "_")
        ))
    }

    #[test]
    fn artifact_target_from_uname_matches_install_sh_platforms() {
        assert_eq!(
            ArtifactTarget::from_uname("Linux", "x86_64").unwrap(),
            ArtifactTarget::LinuxX86_64
        );
        assert_eq!(
            ArtifactTarget::from_uname("Linux", "amd64").unwrap(),
            ArtifactTarget::LinuxX86_64
        );
        assert_eq!(
            ArtifactTarget::from_uname("Darwin", "arm64").unwrap(),
            ArtifactTarget::MacosAArch64
        );
        assert!(ArtifactTarget::from_uname("Darwin", "x86_64").is_err());
        assert!(ArtifactTarget::from_uname("FreeBSD", "x86_64").is_err());
        assert!(ArtifactTarget::from_uname("Linux", "riscv64").is_err());
    }

    #[test]
    fn artifact_target_names_match_release_assets() {
        assert_eq!(
            ArtifactTarget::LinuxX86_64.file_name("0.1.90"),
            "waitagent-0.1.90-x86_64-linux.tar.gz"
        );
        assert_eq!(
            ArtifactTarget::MacosAArch64.file_name("0.1.90"),
            "waitagent-0.1.90-aarch64-macos.tar.gz"
        );
        assert_eq!(
            ArtifactTarget::LinuxX86_64.download_url("0.1.90"),
            "https://github.com/kikakkz/wait-agent/releases/download/v0.1.90/waitagent-0.1.90-x86_64-linux.tar.gz"
        );
    }

    #[test]
    fn ensure_artifact_downloads_and_writes_sha256_sidecar() {
        let dir = cache_dir("download");
        let cache = LocalArtifactCache::new(&dir);
        let url = ArtifactTarget::LinuxX86_64.download_url("0.1.90");
        let artifact = sample_artifact();
        let fetcher = FakeFetcher::default().serving(&url, artifact.clone());

        let path = cache
            .ensure_artifact(ArtifactTarget::LinuxX86_64, "0.1.90", &fetcher)
            .unwrap();

        assert_eq!(path, dir.join("waitagent-0.1.90-x86_64-linux.tar.gz"));
        assert_eq!(std::fs::read(&path).unwrap(), artifact);
        let sidecar = std::fs::read_to_string(
            path.with_file_name("waitagent-0.1.90-x86_64-linux.tar.gz.sha256"),
        )
        .unwrap();
        assert!(sidecar.starts_with(&sha256_hex(&artifact)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_artifact_reuses_verified_cache_hit_without_refetch() {
        let dir = cache_dir("cache-hit");
        let cache = LocalArtifactCache::new(&dir);
        let url = ArtifactTarget::LinuxX86_64.download_url("0.1.90");
        let fetcher = FakeFetcher::default().serving(&url, sample_artifact());

        cache
            .ensure_artifact(ArtifactTarget::LinuxX86_64, "0.1.90", &fetcher)
            .unwrap();
        cache
            .ensure_artifact(ArtifactTarget::LinuxX86_64, "0.1.90", &fetcher)
            .unwrap();

        assert_eq!(fetcher.attempted_count(), 1, "hit must not re-download");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_artifact_redownloads_once_when_cached_copy_is_corrupt() {
        let dir = cache_dir("corrupt");
        let cache = LocalArtifactCache::new(&dir);
        let url = ArtifactTarget::LinuxX86_64.download_url("0.1.90");
        let fetcher = FakeFetcher::default().serving(&url, sample_artifact());
        let path = cache
            .ensure_artifact(ArtifactTarget::LinuxX86_64, "0.1.90", &fetcher)
            .unwrap();

        std::fs::write(&path, b"tampered bytes").unwrap();
        let path = cache
            .ensure_artifact(ArtifactTarget::LinuxX86_64, "0.1.90", &fetcher)
            .unwrap();

        assert_eq!(
            fetcher.attempted_count(),
            2,
            "corrupt cache must trigger exactly one re-download"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            sample_artifact(),
            "the re-downloaded artifact replaces the corrupt copy"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_artifact_redownloads_once_when_sidecar_is_missing() {
        let dir = cache_dir("no-sidecar");
        let cache = LocalArtifactCache::new(&dir);
        let url = ArtifactTarget::LinuxX86_64.download_url("0.1.90");
        let fetcher = FakeFetcher::default().serving(&url, sample_artifact());
        let path = cache
            .ensure_artifact(ArtifactTarget::LinuxX86_64, "0.1.90", &fetcher)
            .unwrap();

        std::fs::remove_file(path.with_file_name("waitagent-0.1.90-x86_64-linux.tar.gz.sha256"))
            .unwrap();
        cache
            .ensure_artifact(ArtifactTarget::LinuxX86_64, "0.1.90", &fetcher)
            .unwrap();

        assert_eq!(fetcher.attempted_count(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_artifact_hard_errors_after_two_failed_downloads() {
        let dir = cache_dir("fetch-fails");
        let cache = LocalArtifactCache::new(&dir);
        let url = ArtifactTarget::LinuxX86_64.download_url("0.1.90");
        let fetcher = FakeFetcher::default().failing(&url, "network unreachable");

        let error = cache
            .ensure_artifact(ArtifactTarget::LinuxX86_64, "0.1.90", &fetcher)
            .unwrap_err();

        assert!(matches!(error, LocalArtifactError::Download { .. }));
        assert_eq!(fetcher.attempted_count(), 2, "one retry then a hard error");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_artifact_rejects_an_archive_without_the_waitagent_binary() {
        let dir = cache_dir("bad-archive");
        let cache = LocalArtifactCache::new(&dir);
        let url = ArtifactTarget::LinuxX86_64.download_url("0.1.90");
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let mut builder = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_size(4);
        header.set_cksum();
        builder
            .append_data(&mut header, "README.md", &b"nope"[..])
            .unwrap();
        let encoder = builder.into_inner().unwrap();
        let bogus = encoder.finish().unwrap();
        let fetcher = FakeFetcher::default().serving(&url, bogus);

        let error = cache
            .ensure_artifact(ArtifactTarget::LinuxX86_64, "0.1.90", &fetcher)
            .unwrap_err();

        assert!(matches!(error, LocalArtifactError::Download { .. }));
        assert_eq!(fetcher.attempted_count(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_artifact_rejects_path_traversal_versions() {
        let dir = cache_dir("bad-version");
        let cache = LocalArtifactCache::new(&dir);

        let error = cache
            .ensure_artifact(
                ArtifactTarget::LinuxX86_64,
                "../0.1.90",
                &FakeFetcher::default(),
            )
            .unwrap_err();

        assert!(matches!(error, LocalArtifactError::InvalidVersion(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn validate_version_accepts_release_versions() {
        assert!(validate_version("0.1.90").is_ok());
        assert!(validate_version("1.0.0-rc.1").is_ok());
        assert!(validate_version("").is_err());
        assert!(validate_version("0.1/90").is_err());
    }
}
