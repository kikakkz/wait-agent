use crate::infra::tmux_types::TmuxSocketName;
use std::fmt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TmuxCommandOutput {
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TmuxCommandFailure {
    pub(crate) command_summary: String,
    pub(crate) exit_code: Option<i32>,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TmuxCommandRunner {
    tmux_binary_path: PathBuf,
}

impl TmuxCommandRunner {
    pub(crate) fn new(tmux_binary_path: impl Into<PathBuf>) -> Self {
        Self {
            tmux_binary_path: tmux_binary_path.into(),
        }
    }

    pub(crate) fn run(
        &self,
        socket_name: &TmuxSocketName,
        args: &[String],
    ) -> Result<TmuxCommandOutput, TmuxError> {
        let command_summary = self.command_summary(socket_name, args);
        let output = self
            .base_socket_command(socket_name)
            .args(args)
            .output()
            .map_err(|error| {
                TmuxError::new(format!(
                    "failed to spawn vendored tmux command `{command_summary}`: {error}"
                ))
            })?;
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

        if !output.status.success() {
            return Err(TmuxError::command_failure(TmuxCommandFailure {
                command_summary,
                exit_code: output.status.code(),
                stdout,
                stderr,
            }));
        }

        Ok(TmuxCommandOutput { stdout, stderr })
    }

    pub(crate) fn run_interactive(
        &self,
        socket_name: &TmuxSocketName,
        args: &[String],
    ) -> Result<(), TmuxError> {
        let command_summary = self.command_summary(socket_name, args);
        let status = self
            .base_socket_command(socket_name)
            .args(args)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .map_err(|error| {
                TmuxError::new(format!(
                    "failed to spawn vendored tmux command `{command_summary}`: {error}"
                ))
            })?;

        if status.success() {
            return Ok(());
        }

        Err(TmuxError::new(format!(
            "vendored tmux command failed with exit code {}: `{command_summary}`",
            status
                .code()
                .map(|value| value.to_string())
                .unwrap_or_else(|| "signal".to_string())
        )))
    }

    pub(crate) fn run_from_current_client(&self, args: &[String]) -> Result<(), TmuxError> {
        let command_summary = self.command_summary_without_socket(args);
        let status = Command::new(&self.tmux_binary_path)
            .args(args)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .map_err(|error| {
                TmuxError::new(format!(
                    "failed to spawn vendored tmux command `{command_summary}`: {error}"
                ))
            })?;

        if status.success() {
            return Ok(());
        }

        Err(TmuxError::new(format!(
            "vendored tmux command failed with exit code {}: `{command_summary}`",
            status
                .code()
                .map(|value| value.to_string())
                .unwrap_or_else(|| "signal".to_string())
        )))
    }

    pub(crate) fn capture_from_current_client(
        &self,
        args: &[String],
    ) -> Result<TmuxCommandOutput, TmuxError> {
        let command_summary = self.command_summary_without_socket(args);
        let output = Command::new(&self.tmux_binary_path)
            .args(args)
            .output()
            .map_err(|error| {
                TmuxError::new(format!(
                    "failed to spawn vendored tmux command `{command_summary}`: {error}"
                ))
            })?;
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

        if !output.status.success() {
            return Err(TmuxError::command_failure(TmuxCommandFailure {
                command_summary,
                exit_code: output.status.code(),
                stdout,
                stderr,
            }));
        }

        Ok(TmuxCommandOutput { stdout, stderr })
    }

    fn base_socket_command(&self, socket_name: &TmuxSocketName) -> Command {
        let mut command = Command::new(&self.tmux_binary_path);
        command
            .arg("-f")
            .arg("/dev/null")
            .arg("-L")
            .arg(socket_name.as_str());
        command
    }

    fn command_summary(&self, socket_name: &TmuxSocketName, args: &[String]) -> String {
        format!(
            "{} -f /dev/null -L {} {}",
            self.tmux_binary_path.display(),
            socket_name.as_str(),
            args.join(" ")
        )
    }

    fn command_summary_without_socket(&self, args: &[String]) -> String {
        format!("{} {}", self.tmux_binary_path.display(), args.join(" "))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxError {
    message: String,
    command_failure: Option<TmuxCommandFailure>,
}

impl TmuxError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            command_failure: None,
        }
    }

    pub(crate) fn command_failure(failure: TmuxCommandFailure) -> Self {
        let exit_code = failure
            .exit_code
            .map(|value| value.to_string())
            .unwrap_or_else(|| "signal".to_string());
        let stderr_suffix = if failure.stderr.is_empty() {
            String::new()
        } else {
            format!(": {}", failure.stderr)
        };
        Self {
            message: format!(
                "vendored tmux command failed with exit code {exit_code}: `{}`{stderr_suffix}",
                failure.command_summary
            ),
            command_failure: Some(failure),
        }
    }

    pub(crate) fn is_command_failure(&self) -> bool {
        self.command_failure.is_some()
    }
}

impl fmt::Display for TmuxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for TmuxError {}

impl From<&'static str> for TmuxError {
    fn from(value: &'static str) -> Self {
        Self::new(value)
    }
}

pub(crate) fn validate_percent(value: u8, label: &str) -> Result<(), TmuxError> {
    if value == 0 || value > 100 {
        return Err(TmuxError::new(format!(
            "{label} must be between 1 and 100, got {value}"
        )));
    }
    Ok(())
}

pub(crate) fn parse_tmux_identifier(value: &str, label: &str) -> Result<String, TmuxError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(TmuxError::new(format!(
            "vendored tmux did not print a {label}"
        )));
    }
    if trimmed.contains(char::is_whitespace) {
        return Err(TmuxError::new(format!(
            "vendored tmux printed an invalid {label}: `{trimmed}`"
        )));
    }
    Ok(trimmed.to_string())
}

pub(crate) fn parse_tmux_id(value: &str, prefix: char, label: &str) -> Result<String, TmuxError> {
    let identifier = parse_tmux_identifier(value, label)?;
    if !identifier.starts_with(prefix) {
        return Err(TmuxError::new(format!(
            "vendored tmux printed an unexpected {label}: `{identifier}`"
        )));
    }
    Ok(identifier)
}

pub(crate) fn tmux_socket_dir() -> PathBuf {
    let base = std::env::var_os("TMUX_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join(format!("tmux-{}", effective_uid()))
}

fn effective_uid() -> u32 {
    unsafe { geteuid() }
}

extern "C" {
    fn geteuid() -> u32;
}
