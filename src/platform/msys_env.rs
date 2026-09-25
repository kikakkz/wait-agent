//! Provisioning of the vendored MSYS2 runtime used by Windows local sessions.
//!
//! Windows local sessions default to a waitagent-provisioned MSYS2 bash
//! (cygwin 3.5 + a current OpenSSH) because the older MSYS ssh shipped with Git
//! for Windows does not put the ConPTY console into raw mode, so Ctrl+C is
//! delivered to ssh as `CTRL_C_EVENT` and kills it. The vendored runtime
//! forwards Ctrl+C correctly.
//!
//! Provisioning downloads a fixed-version `msys2-base` tarball (~50 MB,
//! sha256-verified) from a list of mirrors, unpacks it under
//! `%LOCALAPPDATA%\waitagent\msys64`, then uses its own pacman to install
//! `openssh` and `git`. A marker file records the base version; when the
//! marker is missing or stale the provisioner runs again. Any failure is
//! logged and local sessions fall back to the system shell resolution, so
//! provisioning never blocks or breaks normal operation.

#[cfg(any(windows, test))]
use std::path::{Path, PathBuf};

#[cfg(windows)]
use std::io::Read as _;

#[cfg(windows)]
use std::io::Write as _;

/// Base image version suffix of the vendored MSYS2 tarball.
#[cfg(any(windows, test))]
pub(crate) const BASE_VERSION: &str = "20240727";

/// Extra pacman packages installed into the vendored runtime on top of the
/// base image.
#[cfg(windows)]
pub(crate) const EXTRA_PACKAGES: &[&str] = &["openssh", "git"];

/// Mirrors tried in order; the tarball path is appended to each base URL.
#[cfg(any(windows, test))]
pub(crate) const MIRRORS: &[&str] = &[
    "https://repo.msys2.org",
    "https://mirrors.tuna.tsinghua.edu.cn/msys2",
    "https://mirrors.ustc.edu.cn/msys2",
    "https://mirrors.aliyun.com/msys2",
];

/// Path of the tarball relative to each mirror base URL.
#[cfg(any(windows, test))]
pub(crate) const TARBALL_PATH: &str = "distrib/x86_64/msys2-base-x86_64-20240727.tar.xz";

/// sha256 of the vendored base tarball (measured from the official download).
#[cfg(any(windows, test))]
pub(crate) const BASE_SHA256: &str =
    "da19afcc9b635967b3bfc0db119bf848ed6c28fdde8e4678d9f838d678939060";

/// Marker file name, stored next to the provision directory. The first line
/// records the base version that was provisioned successfully.
#[cfg(any(windows, test))]
pub(crate) const MARKER_FILE_NAME: &str = ".msys-provision-v1";

/// Sentinel created while provisioning runs so concurrent node starts and
/// crash recovery can tell "in progress" apart from "never attempted".
#[cfg(any(windows, test))]
pub(crate) const PROGRESS_FILE_NAME: &str = ".msys-provision-v1.progress";

/// Overall timeout for the pacman step. The first pacman run initializes the
/// keyring, syncs all package databases, and installs two packages; on slow
/// mirror routes the database sync alone exceeds 10 minutes, so allow 20.
#[cfg(windows)]
pub(crate) const PACMAN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1200);

/// Whether the vendored runtime can be used for new local sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(any(windows, test))]
pub(crate) enum ProvisionState {
    /// Marker present and version matches [`BASE_VERSION`].
    Ready,
    /// No usable marker yet: never provisioned, or a run is in progress.
    Provisioning,
    /// Marker present but records a different base version; re-provision.
    Failed,
}

/// Errors of the provisioning pipeline. All failures are non-fatal: the
/// caller logs them and Windows local sessions keep using the system shell.
#[derive(Debug, thiserror::Error)]
#[cfg(any(windows, test))]
pub(crate) enum ProvisionError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("download failed from all mirrors: {0}")]
    Download(String),
    #[error("sha256 mismatch: expected {expected}, got {actual}")]
    Checksum { expected: String, actual: String },
    #[error("unsafe path in archive: {0}")]
    UnsafePath(String),
    #[cfg(windows)]
    #[error("pacman timed out")]
    PacmanTimeout,
}

/// Directory that holds the vendored runtime for a given home directory
/// (`<home>\waitagent\msys64`).
#[cfg(any(windows, test))]
pub(crate) fn provision_dir_for(home: &Path) -> PathBuf {
    home.join("waitagent").join("msys64")
}

#[cfg(any(windows, test))]
fn marker_path_for(home: &Path) -> PathBuf {
    home.join("waitagent").join(MARKER_FILE_NAME)
}

#[cfg(any(windows, test))]
fn progress_path_for(home: &Path) -> PathBuf {
    home.join("waitagent").join(PROGRESS_FILE_NAME)
}

/// Version recorded by the marker file, if a readable marker exists.
#[cfg(any(windows, test))]
fn marker_version(home: &Path) -> Option<String> {
    let contents = std::fs::read_to_string(marker_path_for(home)).ok()?;
    contents.lines().next().map(str::trim).map(str::to_string)
}

/// Provision state derived from the marker/progress files under `home`.
#[cfg(any(windows, test))]
pub(crate) fn provision_state_at(home: &Path) -> ProvisionState {
    match marker_version(home) {
        Some(version) if version == BASE_VERSION => ProvisionState::Ready,
        Some(_) => ProvisionState::Failed,
        None => ProvisionState::Provisioning,
    }
}

/// Full tarball URLs, one per mirror, in fallback order.
#[cfg(any(windows, test))]
pub(crate) fn tarball_urls() -> Vec<String> {
    MIRRORS
        .iter()
        .map(|mirror| format!("{mirror}/{TARBALL_PATH}"))
        .collect()
}

/// Hex-encoded sha256 of `data`.
#[cfg(any(windows, test))]
pub(crate) fn sha256_hex(data: &[u8]) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(data);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Validates downloaded tarball bytes against the pinned sha256.
#[cfg(any(windows, test))]
pub(crate) fn verify_tarball(data: &[u8]) -> Result<(), ProvisionError> {
    let actual = sha256_hex(data);
    if actual == BASE_SHA256 {
        Ok(())
    } else {
        Err(ProvisionError::Checksum {
            expected: BASE_SHA256.to_string(),
            actual,
        })
    }
}

/// Rejects archive entries that could escape the unpack destination:
/// absolute paths, `..` components, Windows path prefixes, and backslashes
/// (tar entries normally use `/`, but on Windows `\` is also a separator,
/// so `..\` must not slip through).
#[cfg(any(windows, test))]
pub(crate) fn ensure_safe_entry_path(path: &Path) -> Result<(), ProvisionError> {
    use std::path::Component;
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(ProvisionError::UnsafePath(path.display().to_string()));
            }
        }
    }
    if path.to_string_lossy().contains('\\') {
        return Err(ProvisionError::UnsafePath(path.display().to_string()));
    }
    Ok(())
}

/// Maps a sanitized tar entry path to its location under the provision
/// directory. The MSYS2 base tarball wraps every entry in a top-level
/// `msys64/` directory (`msys64/usr/bin/bash.exe`), which must be stripped so
/// the runtime lands directly in the provision directory. Returns `None` for
/// the wrapper directory entry itself (`msys64`); entries without the prefix
/// are kept as-is. Must run after [`ensure_safe_entry_path`] so stripped
/// `..`/absolute segments were already rejected.
#[cfg(any(windows, test))]
pub(crate) fn strip_tarball_prefix(path: &Path) -> Option<PathBuf> {
    let mut components = path.components();
    match components.next() {
        Some(std::path::Component::Normal(first)) if first == "msys64" => {
            let rest = components.as_path().to_path_buf();
            if rest.as_os_str().is_empty() {
                None
            } else {
                Some(rest)
            }
        }
        _ => Some(path.to_path_buf()),
    }
}

/// Value to place in the child process `PATH` so `ssh`/`git` resolve to the
/// vendored runtime: the vendored `usr\bin` prepended to the existing PATH.
/// Pure function so the formatting rule is unit-testable off Windows.
#[cfg(any(windows, test))]
pub(crate) fn vendored_path_value(vendored_bin: &Path, existing: Option<&str>) -> String {
    match existing {
        Some(existing) if !existing.is_empty() => {
            format!("{};{existing}", vendored_bin.display())
        }
        _ => vendored_bin.to_string_lossy().into_owned(),
    }
}

/// Download abstraction so unit tests can inject a fake transport.
#[cfg(any(windows, test))]
pub(crate) trait Fetcher {
    fn fetch(&self, url: &str) -> Result<Vec<u8>, ProvisionError>;
}

/// Downloads the tarball from each mirror in order until one succeeds, and
/// verifies the sha256 of the result.
#[cfg(any(windows, test))]
pub(crate) fn download_tarball(fetcher: &dyn Fetcher) -> Result<Vec<u8>, ProvisionError> {
    let mut last_error = String::new();
    for url in tarball_urls() {
        match fetcher.fetch(&url) {
            Ok(data) => {
                verify_tarball(&data)?;
                return Ok(data);
            }
            Err(error) => last_error = format!("{url}: {error}"),
        }
    }
    Err(ProvisionError::Download(last_error))
}

/// ureq-based production fetcher.
#[cfg(windows)]
struct UreqFetcher;

#[cfg(windows)]
impl Fetcher for UreqFetcher {
    fn fetch(&self, url: &str) -> Result<Vec<u8>, ProvisionError> {
        let response = ureq::get(url)
            .call()
            .map_err(|error| ProvisionError::Download(error.to_string()))?;
        let mut data = Vec::new();
        response
            .into_body()
            .into_reader()
            .read_to_end(&mut data)
            .map_err(ProvisionError::Io)?;
        Ok(data)
    }
}

/// Home directory that owns the provision state: `%LOCALAPPDATA%`, falling
/// back to `%USERPROFILE%\AppData\Local`.
#[cfg(windows)]
pub(crate) fn default_home() -> Option<PathBuf> {
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        return Some(PathBuf::from(local_app_data));
    }
    std::env::var_os("USERPROFILE")
        .map(|profile| PathBuf::from(profile).join("AppData").join("Local"))
}

/// Path to the vendored `bash.exe` when the runtime is ready. The marker
/// alone is not enough: the runtime directory must actually contain
/// `usr\bin\bash.exe`, otherwise session spawning would fail.
#[cfg(any(windows, test))]
pub(crate) fn vendored_bash_at(home: &Path) -> Option<PathBuf> {
    if provision_state_at(home) != ProvisionState::Ready {
        return None;
    }
    let bash = provision_dir_for(home).join(r"usr\bin\bash.exe");
    if bash.is_file() {
        Some(bash)
    } else {
        None
    }
}

/// Path to the vendored `bash.exe` when the runtime is ready (default home).
#[cfg(windows)]
pub(crate) fn vendored_bash() -> Option<PathBuf> {
    default_home().and_then(|home| vendored_bash_at(&home))
}

/// Directory to prepend to the session `PATH` when the vendored runtime is
/// ready (`<msys64>\usr\bin`).
#[cfg(windows)]
pub(crate) fn vendored_bin_dir() -> Option<PathBuf> {
    vendored_bash().and_then(|bash| bash.parent().map(Path::to_path_buf))
}

/// Current provision state for the default home.
#[cfg(windows)]
fn provision_state() -> ProvisionState {
    default_home()
        .map(|home| provision_state_at(&home))
        .unwrap_or(ProvisionState::Provisioning)
}

/// Spawns a background thread that provisions the vendored runtime when the
/// marker is missing or stale. No-op when already ready, when another thread
/// in this process already started provisioning, or when the home directory
/// cannot be determined. Never blocks or fails the caller.
#[cfg(windows)]
pub(crate) fn provision_in_background() {
    use std::sync::atomic::{AtomicBool, Ordering};

    static STARTED: AtomicBool = AtomicBool::new(false);
    if provision_state() == ProvisionState::Ready {
        return;
    }
    if STARTED.swap(true, Ordering::Relaxed) {
        return;
    }
    std::thread::spawn(|| {
        if let Err(error) = provision_run(std::io::sink(), false) {
            crate::infra::error_log::ERROR_LOG.log_warn(format!(
                "[msys-env] background provisioning failed: {error}"
            ));
        }
    });
}

/// Runs the full provisioning pipeline, writing human-readable progress lines
/// to `out`. Shared by the background trigger and the `__provision-msys`
/// hidden subcommand.
#[cfg(windows)]
pub(crate) fn provision_sync(out: impl std::io::Write) -> Result<(), ProvisionError> {
    provision_run(out, true)
}

/// `show_pacman_output` inherits pacman's stdout/stderr so the interactive
/// `__provision-msys` run shows download/install progress; the background
/// trigger keeps them nulled.
#[cfg(windows)]
fn provision_run(
    mut out: impl std::io::Write,
    show_pacman_output: bool,
) -> Result<(), ProvisionError> {
    let home = default_home()
        .ok_or_else(|| ProvisionError::Download("%LOCALAPPDATA% is not set".to_string()))?;
    if provision_state_at(&home) == ProvisionState::Ready {
        writeln_ok(&mut out, "MSYS2 runtime already provisioned")?;
        return Ok(());
    }
    let state_dir = home.join("waitagent");
    std::fs::create_dir_all(&state_dir)?;
    let progress_path = progress_path_for(&home);
    std::fs::write(&progress_path, "")?;
    let result = provision_inner(&home, &mut out, show_pacman_output);
    if result.is_ok() {
        let _ = std::fs::remove_file(&progress_path);
    }
    result
}

#[cfg(windows)]
fn writeln_ok(out: &mut impl std::io::Write, line: &str) -> Result<(), ProvisionError> {
    writeln!(out, "{line}").map_err(ProvisionError::Io)
}

/// Runs the pipeline. On failure the half-products from this run are removed
/// (`unpack_tarball` drops a destination it created) and the progress
/// sentinel stays behind, which keeps the state at
/// [`ProvisionState::Provisioning`] for the next attempt.
#[cfg(windows)]
fn provision_inner(
    home: &Path,
    out: &mut impl std::io::Write,
    show_pacman_output: bool,
) -> Result<(), ProvisionError> {
    let dest = provision_dir_for(home);

    writeln_ok(out, "Downloading MSYS2 base tarball (~50 MB)...")?;
    let data = download_tarball(&UreqFetcher)?;

    // The marker is missing or stale at this point (provision_sync returns
    // early when ready), so any existing runtime directory is a leftover from
    // an interrupted or corrupted run: remove it and unpack from scratch
    // rather than merging into it.
    if dest.exists() {
        writeln_ok(out, "Removing existing MSYS2 runtime directory...")?;
        std::fs::remove_dir_all(&dest)?;
    }

    writeln_ok(out, "Unpacking MSYS2 runtime...")?;
    unpack_tarball(&data, &dest)?;

    writeln_ok(
        out,
        "Installing openssh and git via pacman (may take several minutes)...",
    )?;
    run_pacman(&dest, show_pacman_output)?;
    configure_home_lookup(&dest)?;

    let marker = format!("{BASE_VERSION}\n");
    std::fs::write(marker_path_for(home), marker)?;
    writeln_ok(out, "MSYS2 runtime provisioned")?;
    Ok(())
}

/// Streams `tar.xz` bytes into `dest`, sanitizing every entry path and
/// stripping the tarball's top-level `msys64/` wrapper directory. Only
/// regular files and directories are extracted; MSYS2 stores symlinks as
/// regular `.lnk` files, which pass through as regular files. On error the
/// destination directory is removed.
#[cfg(windows)]
fn unpack_tarball(data: &[u8], dest: &Path) -> Result<(), ProvisionError> {
    let decoder = xz2::read::XzDecoder::new(data);
    let mut archive = tar::Archive::new(decoder);
    let result = (|| -> Result<(), ProvisionError> {
        for entry in archive.entries()? {
            let mut entry = entry?;
            let path = entry.path()?.into_owned();
            ensure_safe_entry_path(&path)?;
            let Some(relative) = strip_tarball_prefix(&path) else {
                continue;
            };
            let entry_type = entry.header().entry_type();
            if entry_type.is_dir() {
                std::fs::create_dir_all(dest.join(&relative))?;
            } else if entry_type.is_file() {
                let target = dest.join(&relative);
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let mut file = std::fs::File::create(&target)?;
                std::io::copy(&mut entry, &mut file)?;
            }
            // Other entry types (hard links, devices) are not expected in the
            // MSYS2 base image and are skipped.
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(dest);
    }
    result
}

/// Runs `bash -lc "pacman -Sy --noconfirm openssh git"` inside the unpacked
/// runtime, polling with a timeout so a stuck pacman cannot hang forever.
#[cfg(any(windows, test))]
fn rewrite_nsswitch_home(content: &str) -> String {
    content
        .lines()
        .map(|line| {
            if line.starts_with("db_home:") {
                "db_home: env windows".to_string()
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Point the cygwin home lookup at the user's real profile.
///
/// The MSYS2 base image ships `db_home: cygwin desc`, which resolves every
/// session's home to `<msys>\home\<user>`; vendored ssh would then miss the
/// user's `~/.ssh` keys and config. `env windows` makes cygwin honor the
/// session's `HOME` environment variable (which `local_session` sets to the
/// user profile), falling back to the Windows profile — the same behavior
/// Git for Windows users expect.
#[cfg(windows)]
fn configure_home_lookup(msys_dir: &Path) -> Result<(), ProvisionError> {
    let nsswitch = msys_dir.join(r"etc\nsswitch.conf");
    let content = std::fs::read_to_string(&nsswitch)?;
    std::fs::write(&nsswitch, rewrite_nsswitch_home(&content))?;
    Ok(())
}

/// With `show_output` pacman inherits stdout/stderr so the interactive
/// `__provision-msys` run shows download and install progress.
#[cfg(windows)]
fn run_pacman(msys_dir: &Path, show_output: bool) -> Result<(), ProvisionError> {
    let bash = msys_dir.join(r"usr\bin\bash.exe");
    let command = format!("pacman -Sy --noconfirm {}", EXTRA_PACKAGES.join(" "));
    let stdio = if show_output {
        std::process::Stdio::inherit()
    } else {
        std::process::Stdio::null()
    };
    let mut child = std::process::Command::new(&bash)
        .arg("-lc")
        .arg(&command)
        .current_dir(msys_dir)
        .env("MSYSTEM", "MSYS")
        .stdout(stdio)
        .stderr(std::process::Stdio::null())
        .spawn()?;

    let deadline = std::time::Instant::now() + PACMAN_TIMEOUT;
    loop {
        match child.try_wait()? {
            Some(status) => {
                if status.success() {
                    return Ok(());
                }
                return Err(ProvisionError::Download(format!(
                    "pacman exited with {status}"
                )));
            }
            None => {
                if std::time::Instant::now() >= deadline {
                    let pid = child.id();
                    let _ = child.kill();
                    let _ = child.wait();
                    // `bash -lc` spawns pacman as a grandchild; killing bash
                    // alone would leave pacman behind on Windows, so tear
                    // down the whole process tree.
                    let _ = std::process::Command::new("taskkill")
                        .args(["/PID", &pid.to_string(), "/T", "/F"])
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status();
                    return Err(ProvisionError::PacmanTimeout);
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "waitagent-msys-env-test-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        // `waitagent` is the state dir that holds marker/progress files.
        std::fs::create_dir_all(dir.join("waitagent")).expect("create temp home");
        dir
    }

    struct FakeFetcher {
        responses: std::collections::HashMap<String, Result<Vec<u8>, String>>,
        attempted: std::cell::RefCell<Vec<String>>,
    }

    impl FakeFetcher {
        fn new() -> Self {
            Self {
                responses: std::collections::HashMap::new(),
                attempted: std::cell::RefCell::new(Vec::new()),
            }
        }

        fn serving(&mut self, url: &str, data: Vec<u8>) -> &mut Self {
            self.responses.insert(url.to_string(), Ok(data));
            self
        }

        fn failing(&mut self, url: &str) -> &mut Self {
            self.responses
                .insert(url.to_string(), Err("connection refused".to_string()));
            self
        }

        fn attempted(&self) -> Vec<String> {
            self.attempted.borrow().clone()
        }
    }

    impl Fetcher for FakeFetcher {
        fn fetch(&self, url: &str) -> Result<Vec<u8>, ProvisionError> {
            self.attempted.borrow_mut().push(url.to_string());
            match self.responses.get(url) {
                Some(Ok(data)) => Ok(data.clone()),
                Some(Err(message)) => Err(ProvisionError::Download(format!("{url}: {message}"))),
                None => Err(ProvisionError::Download(format!("{url}: no route"))),
            }
        }
    }

    #[test]
    fn marker_state_machine() {
        let home = temp_home("marker-state");
        assert_eq!(provision_state_at(&home), ProvisionState::Provisioning);

        // Progress sentinel alone still means "not ready".
        std::fs::write(progress_path_for(&home), "").expect("write progress");
        assert_eq!(provision_state_at(&home), ProvisionState::Provisioning);

        // Stale version means failed / needs re-provision.
        std::fs::write(marker_path_for(&home), "20230101\n").expect("write stale marker");
        assert_eq!(provision_state_at(&home), ProvisionState::Failed);

        // Current version means ready.
        std::fs::write(marker_path_for(&home), format!("{BASE_VERSION}\n"))
            .expect("write current marker");
        assert_eq!(provision_state_at(&home), ProvisionState::Ready);

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn mirror_fallback_order() {
        let urls = tarball_urls();
        assert_eq!(urls.len(), 4);
        assert_eq!(
            urls[0],
            "https://repo.msys2.org/distrib/x86_64/msys2-base-x86_64-20240727.tar.xz"
        );
        assert_eq!(
            urls[1],
            "https://mirrors.tuna.tsinghua.edu.cn/msys2/distrib/x86_64/msys2-base-x86_64-20240727.tar.xz"
        );
        assert_eq!(
            urls[2],
            "https://mirrors.ustc.edu.cn/msys2/distrib/x86_64/msys2-base-x86_64-20240727.tar.xz"
        );
        assert_eq!(
            urls[3],
            "https://mirrors.aliyun.com/msys2/distrib/x86_64/msys2-base-x86_64-20240727.tar.xz"
        );
    }

    #[test]
    fn sanitize_rejects_absolute_and_parent_paths() {
        assert!(ensure_safe_entry_path(Path::new("usr/bin/bash.exe")).is_ok());
        assert!(ensure_safe_entry_path(Path::new("./usr/./bin")).is_ok());
        assert!(ensure_safe_entry_path(Path::new("/etc/passwd")).is_err());
        assert!(ensure_safe_entry_path(Path::new("../escape")).is_err());
        assert!(ensure_safe_entry_path(Path::new("usr/../../escape")).is_err());
        assert!(ensure_safe_entry_path(Path::new(r"..\escape")).is_err());
        assert!(ensure_safe_entry_path(Path::new(r"usr\..\escape")).is_err());
        // `Prefix` components (`C:`) only parse on Windows, where this is
        // rejected; the backslash rule above keeps the check meaningful on
        // every host.
        #[cfg(windows)]
        assert!(ensure_safe_entry_path(Path::new("C:/Windows/system32")).is_err());
    }

    #[test]
    fn strip_tarball_prefix_maps_wrapped_entries() {
        // The tarball wraps everything in a top-level `msys64/` directory.
        assert_eq!(
            strip_tarball_prefix(Path::new("msys64/usr/bin/bash.exe")),
            Some(PathBuf::from("usr/bin/bash.exe"))
        );
        assert_eq!(
            strip_tarball_prefix(Path::new("msys64/usr")),
            Some(PathBuf::from("usr"))
        );
        // The wrapper directory entry itself is skipped.
        assert_eq!(strip_tarball_prefix(Path::new("msys64")), None);
        // Entries without the prefix land at the root unchanged.
        assert_eq!(
            strip_tarball_prefix(Path::new("usr/bin/extra.exe")),
            Some(PathBuf::from("usr/bin/extra.exe"))
        );
    }

    #[test]
    fn nsswitch_home_line_is_rewritten_and_others_kept() {
        let content = "# nsswitch.conf\n\
                       db_home: cygwin desc\n\
                       db_shell: cygwin\n";
        let rewritten = rewrite_nsswitch_home(content);
        assert!(rewritten.contains("db_home: env windows"));
        assert!(rewritten.contains("db_shell: cygwin"));
        assert!(!rewritten.contains("db_home: cygwin"));
        // No db_home line at all: content passes through unchanged. (The
        // lines/join round trip normalizes a trailing newline away, which is
        // harmless for a config file.)
        assert_eq!(
            rewrite_nsswitch_home("db_shell: cygwin\n"),
            "db_shell: cygwin"
        );
    }

    #[test]
    fn sanitize_still_rejects_escapes_inside_tarball_prefix() {
        // `..`/absolute segments are rejected before prefix stripping.
        for path in [
            "msys64/../escape",
            "msys64/usr/../../escape",
            "msys64\\..\\escape",
        ] {
            assert!(
                ensure_safe_entry_path(Path::new(path)).is_err(),
                "{path} must be rejected"
            );
        }
    }

    #[test]
    fn stripped_entry_matches_vendored_bash_location() {
        // The path a wrapped `bash.exe` entry unpacks to must be exactly the
        // path `vendored_bash_at` looks up after provisioning. On Windows
        // `usr/bin/bash.exe` (tar entry style) and `usr\bin\bash.exe` (joined
        // style) are the same file; on Linux they differ as `Path`s, so the
        // lookup side is built with the same string `vendored_bash_at` uses.
        let home = temp_home("strip-matches-vendored");
        let stripped = strip_tarball_prefix(Path::new("msys64/usr/bin/bash.exe"))
            .expect("wrapped bash entry must map into the runtime");
        assert_eq!(stripped, PathBuf::from("usr/bin/bash.exe"));
        let bash = provision_dir_for(&home).join(r"usr\bin\bash.exe");
        std::fs::create_dir_all(bash.parent().expect("bash parent")).expect("create msys64");
        std::fs::write(&bash, "").expect("write fake bash.exe");
        assert!(vendored_bash_at(&home).is_none(), "no marker yet");
        std::fs::write(marker_path_for(&home), format!("{BASE_VERSION}\n")).expect("write marker");
        assert_eq!(vendored_bash_at(&home), Some(bash));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn sha256_known_answer_and_mismatch() {
        // sha256("abc") test vector.
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let error = verify_tarball(b"definitely not the tarball")
            .expect_err("mismatched tarball must be rejected");
        match error {
            ProvisionError::Checksum { expected, actual } => {
                assert_eq!(expected, BASE_SHA256);
                assert_ne!(actual, BASE_SHA256);
            }
            other => panic!("expected Checksum error, got {other}"),
        }
    }

    #[test]
    fn download_falls_back_across_mirrors() {
        let urls = tarball_urls();
        let mut fetcher = FakeFetcher::new();
        fetcher.failing(&urls[0]).failing(&urls[1]);
        // Third mirror serves bytes with the correct sha256: empty input
        // hashes to the well-known e3b0... value, so serve a precomputed
        // constant-verifying payload instead by mocking the checksum path.
        fetcher.serving(&urls[2], Vec::new());
        fetcher.failing(&urls[3]);

        // The empty byte string does not match the pinned tarball sha256, so
        // a served-but-corrupt payload must surface as a checksum error.
        let error = download_tarball(&fetcher).expect_err("corrupt payload must fail");
        assert!(matches!(error, ProvisionError::Checksum { .. }));
        assert_eq!(
            fetcher.attempted(),
            vec![urls[0].clone(), urls[1].clone(), urls[2].clone()]
        );

        let mut always_fail = FakeFetcher::new();
        for url in &urls {
            always_fail.failing(url);
        }
        let error = download_tarball(&always_fail).expect_err("all mirrors failing must fail");
        assert!(matches!(error, ProvisionError::Download(_)));
        assert_eq!(always_fail.attempted().len(), 4);
    }

    #[test]
    fn vendored_bash_requires_ready_marker_and_existing_binary() {
        let home = temp_home("vendored-bash");
        assert!(vendored_bash_at(&home).is_none());

        std::fs::write(marker_path_for(&home), format!("{BASE_VERSION}\n")).expect("write marker");
        // Marker ready but bash.exe missing: still not usable.
        assert!(vendored_bash_at(&home).is_none());

        let bash = provision_dir_for(&home).join(r"usr\bin\bash.exe");
        std::fs::create_dir_all(bash.parent().expect("bash parent")).expect("create usr/bin");
        std::fs::write(&bash, "").expect("write fake bash.exe");
        assert_eq!(vendored_bash_at(&home), Some(bash));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn vendored_path_value_prepends_bin_dir() {
        let bin = Path::new(r"C:\Users\u\AppData\Local\waitagent\msys64\usr\bin");
        assert_eq!(
            vendored_path_value(bin, Some("C:\\Windows\\system32")),
            r"C:\Users\u\AppData\Local\waitagent\msys64\usr\bin;C:\Windows\system32"
        );
        assert_eq!(
            vendored_path_value(bin, None),
            r"C:\Users\u\AppData\Local\waitagent\msys64\usr\bin"
        );
        assert_eq!(
            vendored_path_value(bin, Some("")),
            r"C:\Users\u\AppData\Local\waitagent\msys64\usr\bin"
        );
    }
}
