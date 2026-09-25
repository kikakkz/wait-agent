//! Remote shell family detection for SSH targets (Windows-as-target support,
//! task T1 in `docs/windows-ssh-target-design.md`).
//!
//! Every remote command the control host generates is a one-line script
//! executed over SSH exec, so the command generators need to know the target's
//! shell family up front. Detection runs a single `uname -s` exec: exit code 0
//! means a POSIX target, while a missing/non-zero exit means a Windows target
//! running native OpenSSH. An exit-0 answer of `MSYS_NT-…`/`MINGW*_NT-…`/
//! `CYGWIN_NT-…` is still classified as Windows: it means a Windows machine
//! whose `PATH` reaches Git for Windows / Cygwin `uname` (native sshd with a
//! Git install is the common case), not a POSIX OS. The result is cached in
//! the remote host profile so later connects skip the extra exec.

use crate::host::ssh::remote_host_history_store::{RemoteHostAuthProfile, RemoteHostProfile};
use crate::host::ssh::remote_host_secret_store::{
    DefaultRemoteHostSecretStore, KeyringRemoteHostSecretStore, RemoteHostSecretStore,
    RemoteHostSecretValue,
};
use crate::host::ssh::remote_ssh_executor::{
    RemoteSshExecutor, RemoteSshTarget, RusshRemoteSshExecutor,
};
use std::fmt;
use std::str::FromStr;

/// One-shot probe command sent to a remote SSH target to classify its shell.
/// Present on every POSIX target (Linux, macOS, WSL, *BSD). On a Windows
/// target running native OpenSSH it is either missing (exit non-zero) or
/// answered by Git for Windows / Cygwin `uname` (exit 0 with an
/// `MSYS_NT-…`-style kernel name); both shapes are classified by
/// [`SshRemoteShellDetector`].
pub const REMOTE_SHELL_PROBE_COMMAND: &str = "uname -s";

/// Shell family of a remote SSH target.
///
/// Detected once per host by [`SshRemoteShellDetector`] and cached in
/// `RemoteHostProfile.remote_shell` so subsequent connects skip detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RemoteShellKind {
    /// POSIX-family shell (`sh`/`bash`): Linux, macOS, WSL, *BSD. All remote
    /// command generators currently emit POSIX scripts only.
    #[default]
    Posix,
    /// Windows target reached through native OpenSSH (Win32-OpenSSH). A
    /// Windows machine with Git for Windows / Cygwin `uname` on `PATH`
    /// (`MSYS_NT-…`, `MINGW*_NT-…`, `CYGWIN_NT-…` answers) is classified
    /// here too; an actual MSYS/Cygwin *sshd* is a known unsupported
    /// configuration (`docs/windows-ssh-target-design.md` §1).
    Windows,
}

impl RemoteShellKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Posix => "posix",
            Self::Windows => "windows",
        }
    }
}

impl FromStr for RemoteShellKind {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "posix" => Ok(Self::Posix),
            "windows" => Ok(Self::Windows),
            other => Err(format!("unknown remote shell kind `{other}`")),
        }
    }
}

/// `uname -s` answers that identify a Windows machine with Git for Windows /
/// Cygwin on `PATH` (native sshd delivers the command to `cmd.exe`, which
/// resolves `uname` from `C:\Program Files\Git\usr\bin` when the installer
/// added it to the system `PATH`). A real POSIX kernel name never contains
/// these tokens.
fn stdout_is_windows_uname(stdout: &[u8]) -> bool {
    let text = String::from_utf8_lossy(stdout);
    let Some(first_line) = text.lines().next() else {
        return false;
    };
    let upper = first_line.trim().to_ascii_uppercase();
    upper.contains("MSYS") || upper.contains("MINGW") || upper.contains("CYGWIN")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteShellDetectError {
    message: String,
}

impl RemoteShellDetectError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for RemoteShellDetectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RemoteShellDetectError {}

/// Detects the shell family of a remote SSH target.
///
/// Object-safe seam used by the connect runtime so tests can substitute a
/// recording detector.
pub trait RemoteShellDetector {
    fn detect_remote_shell(&self, profile: &RemoteHostProfile) -> Result<RemoteShellKind, String>;
}

/// Probes a remote SSH target's shell family with a single `uname -s` exec.
///
/// Generic over the secret store and SSH executor exactly like
/// `SshRemotePortProbe`, so tests can record the exec calls instead of
/// opening a real SSH session.
#[derive(Debug, Clone)]
pub struct SshRemoteShellDetector<S = DefaultRemoteHostSecretStore, E = RusshRemoteSshExecutor> {
    secret_store: S,
    ssh_executor: E,
}

impl SshRemoteShellDetector<KeyringRemoteHostSecretStore, RusshRemoteSshExecutor> {
    pub fn new() -> Self {
        Self {
            secret_store: KeyringRemoteHostSecretStore,
            ssh_executor: RusshRemoteSshExecutor,
        }
    }
}

// TODO(cleanup): transitional remote code, kept for Phase 8 wiring.
#[allow(dead_code)]
impl<S, E> SshRemoteShellDetector<S, E> {
    pub fn with_secret_store_and_executor(secret_store: S, ssh_executor: E) -> Self {
        Self {
            secret_store,
            ssh_executor,
        }
    }
}

impl<S, E> SshRemoteShellDetector<S, E>
where
    S: RemoteHostSecretStore,
    S::Error: ToString,
    E: RemoteSshExecutor,
    E::Error: ToString,
{
    /// Runs the probe and classifies the target.
    ///
    /// Exit code 0 means POSIX, except when the `uname` answer is an
    /// MSYS/Mingw/Cygwin kernel name (`MSYS_NT-…`, `MINGW64_NT-…`,
    /// `CYGWIN_NT-…`): that is a Windows machine with Git for Windows /
    /// Cygwin on `PATH`, not a POSIX OS. Any executed-but-non-zero exit
    /// (command not found, shell failure) means Windows. Errors from `exec`
    /// itself — SSH connect, authentication, channel setup — describe a
    /// failed SSH session, not a Windows shell, and propagate unchanged
    /// instead of being misclassified as Windows.
    pub fn detect_remote_shell(
        &self,
        profile: &RemoteHostProfile,
    ) -> Result<RemoteShellKind, RemoteShellDetectError> {
        let ssh_password = self.ssh_password(profile)?;
        let target = RemoteSshTarget::from_profile(
            profile.host.clone(),
            profile.ssh_port(),
            profile.ssh_user.clone(),
            &profile.auth,
            ssh_password,
        )
        .map_err(|error| RemoteShellDetectError::new(error.to_string()))?;
        let output = self
            .ssh_executor
            .exec(&target, REMOTE_SHELL_PROBE_COMMAND, None)
            .map_err(|error| RemoteShellDetectError::new(error.to_string()))?;
        Ok(
            if output.status == 0 && !stdout_is_windows_uname(&output.stdout) {
                RemoteShellKind::Posix
            } else {
                RemoteShellKind::Windows
            },
        )
    }
}

impl Default for SshRemoteShellDetector<KeyringRemoteHostSecretStore, RusshRemoteSshExecutor> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S, E> RemoteShellDetector for SshRemoteShellDetector<S, E>
where
    S: RemoteHostSecretStore,
    S::Error: ToString,
    E: RemoteSshExecutor,
    E::Error: ToString,
{
    fn detect_remote_shell(&self, profile: &RemoteHostProfile) -> Result<RemoteShellKind, String> {
        SshRemoteShellDetector::detect_remote_shell(self, profile)
            .map_err(|error| error.to_string())
    }
}

impl<S, E> SshRemoteShellDetector<S, E>
where
    S: RemoteHostSecretStore,
    S::Error: ToString,
{
    fn ssh_password(
        &self,
        profile: &RemoteHostProfile,
    ) -> Result<Option<RemoteHostSecretValue>, RemoteShellDetectError> {
        let RemoteHostAuthProfile::Password { password_secret_id } = &profile.auth else {
            return Ok(None);
        };
        let Some(secret_id) = password_secret_id else {
            return Err(RemoteShellDetectError::new(
                "password auth requires a saved SSH password secret id for remote shell detection",
            ));
        };
        self.secret_store
            .get_secret(secret_id)
            .map_err(|error| RemoteShellDetectError::new(error.to_string()))?
            .ok_or_else(|| {
                RemoteShellDetectError::new(format!(
                    "SSH password secret `{}` was not found for remote shell detection",
                    secret_id.as_str()
                ))
            })
            .map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::ssh::remote_host_history_store::RemotePortPreference;
    use crate::host::ssh::remote_host_secret_store::{
        MemoryRemoteHostSecretStore, RemoteHostSecretId, RemoteHostSecretValue,
    };
    use crate::host::ssh::remote_ssh_executor::RemoteSshOutput;
    use std::cell::RefCell;
    use std::rc::Rc;

    type SshCallLog = Vec<(RemoteSshTarget, String, Option<String>)>;

    #[derive(Clone)]
    struct RecordingSshExecutor {
        calls: Rc<RefCell<SshCallLog>>,
        result: Result<RemoteSshOutput, String>,
    }

    impl RemoteSshExecutor for RecordingSshExecutor {
        type Error = String;

        fn exec(
            &self,
            target: &RemoteSshTarget,
            command: &str,
            stdin: Option<&str>,
        ) -> Result<RemoteSshOutput, Self::Error> {
            self.calls.borrow_mut().push((
                target.clone(),
                command.to_string(),
                stdin.map(str::to_string),
            ));
            self.result.clone()
        }
    }

    fn detector_with_output(
        output: RemoteSshOutput,
    ) -> (
        SshRemoteShellDetector<MemoryRemoteHostSecretStore, RecordingSshExecutor>,
        Rc<RefCell<SshCallLog>>,
    ) {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let detector = SshRemoteShellDetector::with_secret_store_and_executor(
            MemoryRemoteHostSecretStore::default(),
            RecordingSshExecutor {
                calls: calls.clone(),
                result: Ok(output),
            },
        );
        (detector, calls)
    }

    fn key_auth_profile() -> RemoteHostProfile {
        RemoteHostProfile {
            name: "130".to_string(),
            host: "10.1.29.130".to_string(),
            ssh_user: "kk".to_string(),
            auth: crate::host::ssh::remote_host_history_store::RemoteHostAuthProfile::Key {
                key_path: std::path::PathBuf::from("/home/kk/.ssh/id_ed25519"),
            },
            sudo_password_secret_id: None,
            preferred_remote_port: RemotePortPreference::Auto,
            ssh_port: None,
            last_remote_port: None,
            last_endpoint: None,
            last_connected_at: None,
            use_install_proxy: true,
            tls_pin_sha256: None,
            host_kind: crate::host::ssh::remote_host_history_store::RemoteHostKind::Lan,
            remote_shell: None,
        }
    }

    #[test]
    fn remote_shell_detection_classifies_exit_zero_as_posix() {
        let (detector, calls) = detector_with_output(RemoteSshOutput {
            status: 0,
            stdout: b"Linux\n".to_vec(),
            stderr: Vec::new(),
        });

        let kind = detector.detect_remote_shell(&key_auth_profile()).unwrap();

        assert_eq!(kind, RemoteShellKind::Posix);
        let calls = calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0.host, "10.1.29.130");
        assert_eq!(calls[0].0.user, "kk");
        assert_eq!(calls[0].1, REMOTE_SHELL_PROBE_COMMAND);
        assert_eq!(calls[0].2, None);
    }

    #[test]
    fn remote_shell_detection_classifies_msys_uname_answer_as_windows() {
        // Native Win32-OpenSSH with Git for Windows on PATH: cmd.exe resolves
        // `uname` from Git's usr\bin, which answers exit 0 with an MSYS kernel
        // name. The target is still a Windows machine and needs the
        // PowerShell pipeline.
        for stdout in [
            b"MSYS_NT-10.0-19045\n".to_vec(),
            b"MINGW64_NT-10.0-22631\n".to_vec(),
            b"CYGWIN_NT-10.0-19045\n".to_vec(),
        ] {
            let (detector, _calls) = detector_with_output(RemoteSshOutput {
                status: 0,
                stdout,
                stderr: Vec::new(),
            });

            let kind = detector.detect_remote_shell(&key_auth_profile()).unwrap();

            assert_eq!(kind, RemoteShellKind::Windows);
        }
    }

    #[test]
    fn remote_shell_detection_classifies_command_not_found_as_windows() {
        let (detector, calls) = detector_with_output(RemoteSshOutput {
            status: 127,
            stdout: Vec::new(),
            stderr: b"uname : The term 'uname' is not recognized".to_vec(),
        });

        let kind = detector.detect_remote_shell(&key_auth_profile()).unwrap();

        assert_eq!(kind, RemoteShellKind::Windows);
        assert_eq!(calls.borrow().len(), 1);
    }

    #[test]
    fn remote_shell_detection_classifies_any_non_zero_exit_as_windows() {
        let (detector, _calls) = detector_with_output(RemoteSshOutput {
            status: 1,
            stdout: Vec::new(),
            stderr: Vec::new(),
        });

        let kind = detector.detect_remote_shell(&key_auth_profile()).unwrap();

        assert_eq!(kind, RemoteShellKind::Windows);
    }

    #[test]
    fn remote_shell_detection_propagates_ssh_session_errors() {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let detector = SshRemoteShellDetector::with_secret_store_and_executor(
            MemoryRemoteHostSecretStore::default(),
            RecordingSshExecutor {
                calls: calls.clone(),
                result: Err("SSH authentication failed: wrong password".to_string()),
            },
        );

        let error = detector
            .detect_remote_shell(&key_auth_profile())
            .expect_err("SSH session errors must propagate instead of classifying as Windows");

        assert!(error.to_string().contains("SSH authentication failed"));
        assert_eq!(calls.borrow().len(), 1);
    }

    #[test]
    fn remote_shell_detection_requires_saved_password_secret_for_password_auth() {
        let (detector, calls) = detector_with_output(RemoteSshOutput {
            status: 0,
            stdout: Vec::new(),
            stderr: Vec::new(),
        });
        let mut profile = key_auth_profile();
        profile.auth = RemoteHostAuthProfile::Password {
            password_secret_id: None,
        };

        let error = detector
            .detect_remote_shell(&profile)
            .expect_err("password auth without secret id should fail before spawning ssh");

        assert!(error.to_string().contains("saved SSH password secret id"));
        assert!(calls.borrow().is_empty());
    }

    #[test]
    fn remote_shell_detection_loads_password_from_secret_store() {
        let ssh_id = RemoteHostSecretId::new("waitagent.remote-host.130.ssh-password").unwrap();
        let store = MemoryRemoteHostSecretStore::default();
        store
            .put_secret(&ssh_id, RemoteHostSecretValue::new("ssh-secret"))
            .unwrap();
        let calls = Rc::new(RefCell::new(Vec::new()));
        let detector = SshRemoteShellDetector::with_secret_store_and_executor(
            store,
            RecordingSshExecutor {
                calls: calls.clone(),
                result: Ok(RemoteSshOutput {
                    status: 0,
                    stdout: b"Linux\n".to_vec(),
                    stderr: Vec::new(),
                }),
            },
        );
        let mut profile = key_auth_profile();
        profile.auth = RemoteHostAuthProfile::Password {
            password_secret_id: Some(ssh_id),
        };

        let kind = detector.detect_remote_shell(&profile).unwrap();

        assert_eq!(kind, RemoteShellKind::Posix);
        assert_eq!(calls.borrow().len(), 1);
    }

    #[test]
    fn remote_shell_kind_parses_serialized_form() {
        assert_eq!(
            "posix".parse::<RemoteShellKind>().unwrap(),
            RemoteShellKind::Posix
        );
        assert_eq!(
            "windows".parse::<RemoteShellKind>().unwrap(),
            RemoteShellKind::Windows
        );
        assert!("plan9".parse::<RemoteShellKind>().is_err());
        assert_eq!(RemoteShellKind::Posix.as_str(), "posix");
        assert_eq!(RemoteShellKind::Windows.as_str(), "windows");
    }
}
