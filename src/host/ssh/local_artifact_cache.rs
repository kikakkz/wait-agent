//! Local cache for waitagent release artifacts (issue #168; the download
//! itself is a curl subprocess honoring the local download proxy config,
//! issue #172).
//!
//! When a saved host profile sets `install_source = "upload"`, this machine
//! — not the remote host — downloads the release artifact and uploads it to
//! the remote over the SSH exec channel. The cache lives under
//! `~/.waitagent/cache/` and stores one artifact plus a `.sha256` sidecar
//! per (target triple, version) pair, named exactly like the published
//! release asset (`waitagent-<version>-<target>.tar.gz`).
//!
//! Downloads run through `curl` (the engine `scripts/install.sh` uses) with
//! the `all_proxy`/`https_proxy` URLs from
//! `local_download_proxy_toml_store` injected as environment variables;
//! curl applies them natively alongside any operator-exported `no_proxy`.
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

use crate::host::ssh::local_download_proxy_toml_store::{
    LocalDownloadProxyConfig, LocalDownloadProxyStoreError,
};
use crate::host::ssh::remote_host_home::waitagent_home;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

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
    /// The local download proxy config could not be read.
    ProxyStore(LocalDownloadProxyStoreError),
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
            Self::ProxyStore(error) => {
                write!(f, "local download proxy config is invalid: {error}")
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

/// Production fetcher (issue #172): a `curl` subprocess, the same download
/// engine `scripts/install.sh` uses on hosts. The local download proxy
/// config (`~/.waitagent/local-download-proxy.toml`) is loaded per connect
/// and injected as `all_proxy`/`https_proxy` environment variables, which
/// curl honors natively along with any operator-exported `no_proxy`.
#[derive(Debug, Clone, Default)]
pub struct CurlArtifactFetcher {
    proxy: LocalDownloadProxyConfig,
}

impl CurlArtifactFetcher {
    /// Production: load the proxy config from the default store path. A
    /// missing file means a direct connection; a malformed file surfaces as
    /// an error so a typo cannot silently bypass the configured proxy.
    pub fn from_default_store() -> Result<Self, LocalArtifactError> {
        Self::from_store_path(&LocalDownloadProxyConfig::default_path())
    }

    /// Loads the proxy config from `store_path` (test seam included).
    pub fn from_store_path(store_path: &Path) -> Result<Self, LocalArtifactError> {
        let proxy = LocalDownloadProxyConfig::load(store_path)
            .map_err(LocalArtifactError::ProxyStore)?
            .unwrap_or_default();
        Ok(Self { proxy })
    }

    /// Direct-connection fetcher (no proxy config consulted).
    #[cfg(test)]
    fn direct() -> Self {
        Self {
            proxy: LocalDownloadProxyConfig::default(),
        }
    }

    fn run_curl(
        &self,
        url: &str,
        proxy: &LocalDownloadProxyConfig,
    ) -> Result<Vec<u8>, LocalArtifactError> {
        // Flag set mirrors scripts/install.sh: fail on HTTP errors, follow
        // redirects, silent progress; bound only the connection setup, then
        // abort if the transfer stalls (<1 KB/s for 60s).
        let mut command = Command::new("curl");
        command
            .arg("-fsSL")
            .arg("--connect-timeout")
            .arg("30")
            .arg("--speed-limit")
            .arg("1024")
            .arg("--speed-time")
            .arg("60")
            .arg(url);
        for (key, value) in proxy.env_assignments() {
            command.env(key, value);
        }
        let output = command
            .output()
            .map_err(|error| LocalArtifactError::Download {
                url: url.to_string(),
                attempts: vec![format!("failed to spawn curl: {error}")],
            })?;
        if output.status.success() {
            Ok(output.stdout)
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stderr = stderr.trim();
            let detail = if stderr.len() > 400 {
                format!("{}…", &stderr[..400])
            } else {
                stderr.to_string()
            };
            Err(LocalArtifactError::Download {
                url: url.to_string(),
                attempts: vec![format!(
                    "curl exited with {}: {}",
                    output.status,
                    if detail.is_empty() {
                        "no stderr output"
                    } else {
                        detail.as_str()
                    }
                )],
            })
        }
    }
}

impl ArtifactFetcher for CurlArtifactFetcher {
    fn fetch(&self, url: &str) -> Result<Vec<u8>, LocalArtifactError> {
        self.run_curl(url, &self.proxy)
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
        if let Some(artifact) = self.peek_artifact(target, version) {
            return Ok(artifact);
        }
        let data = download_verified(target, version, fetcher)?;
        let artifact = self.dir.join(target.file_name(version));
        let sidecar = sidecar_path(&artifact);
        write_artifact(&artifact, &sidecar, &data)?;
        Ok(artifact)
    }

    /// Pure cache probe used by the remote-download fallback (issue #173):
    /// returns the artifact path only when the artifact and its `.sha256`
    /// sidecar both exist and the recorded digest matches. Never downloads;
    /// a missing, corrupt, or unvalidated cache answers `None`.
    pub fn peek_artifact(&self, target: ArtifactTarget, version: &str) -> Option<PathBuf> {
        if validate_version(version).is_err() {
            return None;
        }
        let artifact = self.dir.join(target.file_name(version));
        let data = fs::read(&artifact).ok()?;
        let sidecar = sidecar_path(&artifact);
        match read_sidecar_digest(&sidecar) {
            Ok(Some(expected)) if expected == sha256_hex(&data) => Some(artifact),
            _ => None,
        }
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

    #[test]
    fn peek_artifact_returns_only_verified_hits() {
        let dir = cache_dir("peek");
        let cache = LocalArtifactCache::new(&dir);
        let url = ArtifactTarget::LinuxX86_64.download_url("0.1.90");
        let fetcher = FakeFetcher::default().serving(&url, sample_artifact());
        cache
            .ensure_artifact(ArtifactTarget::LinuxX86_64, "0.1.90", &fetcher)
            .unwrap();

        let artifact = cache.peek_artifact(ArtifactTarget::LinuxX86_64, "0.1.90");
        assert!(
            artifact.is_some(),
            "a freshly downloaded artifact must peek successfully"
        );

        // Corrupt bytes defeat the recorded digest.
        let path = artifact.unwrap();
        std::fs::write(&path, b"tampered").unwrap();
        assert!(cache
            .peek_artifact(ArtifactTarget::LinuxX86_64, "0.1.90")
            .is_none());

        // Missing sidecar defeats the hit.
        std::fs::write(&path, sample_artifact()).unwrap();
        std::fs::remove_file(path.with_file_name("waitagent-0.1.90-x86_64-linux.tar.gz.sha256"))
            .unwrap();
        assert!(cache
            .peek_artifact(ArtifactTarget::LinuxX86_64, "0.1.90")
            .is_none());

        // Wrong version / unsupported version string: no hit, no panic.
        assert!(cache
            .peek_artifact(ArtifactTarget::LinuxX86_64, "0.0.1")
            .is_none());
        assert!(cache
            .peek_artifact(ArtifactTarget::LinuxX86_64, "../x")
            .is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn peek_artifact_never_downloads() {
        let dir = cache_dir("peek-no-download");
        let cache = LocalArtifactCache::new(&dir);
        let fetcher = FakeFetcher::default();

        assert!(cache
            .peek_artifact(ArtifactTarget::LinuxX86_64, "0.1.90")
            .is_none());
        assert_eq!(
            fetcher.attempted_count(),
            0,
            "a peek is a pure filesystem probe"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn curl_fetcher_reads_a_file_url_offline() {
        // file:// exercises the real curl invocation (spawn, flags, stdout
        // capture) without any network access.
        let path = cache_dir("curl-file-fixture").join("payload.bin");
        std::fs::create_dir_all(path.parent().expect("parent")).unwrap();
        let payload = sample_artifact();
        std::fs::write(&path, &payload).unwrap();

        let fetcher = CurlArtifactFetcher::direct();
        let data = fetcher
            .fetch(&format!("file://{}", path.display()))
            .expect("file fetch should succeed");

        assert_eq!(data, payload);
        let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
    }

    #[test]
    fn curl_fetcher_surfaces_curl_failures() {
        let fetcher = CurlArtifactFetcher::direct();
        let url = "file:///definitely/missing/waitagent.tar.gz".to_string();

        let error = fetcher.fetch(&url).unwrap_err();

        match error {
            LocalArtifactError::Download {
                url: failed,
                attempts,
            } => {
                assert_eq!(failed, url);
                assert!(
                    attempts[0].contains("curl exited with"),
                    "the curl exit must be explained: {}",
                    attempts[0]
                );
            }
            other => panic!("expected a download error, got {other}"),
        }
    }

    #[test]
    fn curl_fetcher_loads_proxy_store_per_construction() {
        // Missing store file → direct connection.
        let missing = cache_dir("curl-proxy-missing").join("local-download-proxy.toml");
        let fetcher = CurlArtifactFetcher::from_store_path(&missing)
            .expect("a missing store means direct connection");
        assert!(!fetcher.proxy.has_proxy());

        // Malformed store file → typed error (no silent direct fallback).
        let malformed = cache_dir("curl-proxy-malformed").join("local-download-proxy.toml");
        std::fs::create_dir_all(malformed.parent().expect("parent")).unwrap();
        std::fs::write(&malformed, "all_proxy = \"x\"\nsurprise = 1\n").unwrap();
        let error = CurlArtifactFetcher::from_store_path(&malformed).unwrap_err();
        assert!(
            matches!(error, LocalArtifactError::ProxyStore(_)),
            "a malformed store must not silently bypass the proxy: {error}"
        );

        // Configured store → the fetcher carries the proxy into downloads.
        let configured = cache_dir("curl-proxy-configured").join("local-download-proxy.toml");
        std::fs::create_dir_all(configured.parent().expect("parent")).unwrap();
        std::fs::write(
            &configured,
            "all_proxy = \"socks5://127.0.0.1:7897\"\nhttps_proxy = \"http://127.0.0.1:7890\"\n",
        )
        .unwrap();
        let fetcher =
            CurlArtifactFetcher::from_store_path(&configured).expect("a valid store loads");
        assert_eq!(
            fetcher.proxy.env_assignments(),
            vec![
                ("all_proxy", "socks5://127.0.0.1:7897".to_string()),
                ("https_proxy", "http://127.0.0.1:7890".to_string()),
            ]
        );
        let _ = std::fs::remove_dir_all(cache_dir("curl-proxy-missing"));
        let _ = std::fs::remove_dir_all(cache_dir("curl-proxy-malformed"));
        let _ = std::fs::remove_dir_all(cache_dir("curl-proxy-configured"));
    }
}
