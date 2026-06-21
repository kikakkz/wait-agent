use crate::infra::tmux_types::TmuxSocketName;
use std::env;
use std::fmt;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

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
    pub(crate) diagnostics: String,
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
                diagnostics: self.failure_diagnostics(socket_name, args),
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
                diagnostics: String::new(),
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

    fn failure_diagnostics(&self, socket_name: &TmuxSocketName, args: &[String]) -> String {
        if !args.iter().any(|arg| arg == "new-session") {
            return String::new();
        }

        let mut lines = Vec::new();
        lines.push("waitagent tmux bootstrap diagnostics:".to_string());
        lines.push(format!("  tmux_binary={}", self.tmux_binary_path.display()));
        lines.push(format!(
            "  tmux_binary_exists={}",
            self.tmux_binary_path.exists()
        ));
        if let Ok(metadata) = fs::metadata(&self.tmux_binary_path) {
            lines.push(format!("  tmux_binary_len={}", metadata.len()));
        }
        lines.push(format!("  socket_name={}", socket_name.as_str()));
        lines.push(format!("  socket_dir={}", tmux_socket_dir().display()));
        for key in [
            "HOME",
            "SHELL",
            "TERM",
            "TMUX",
            "TMUX_TMPDIR",
            "XDG_RUNTIME_DIR",
            "WSL_DISTRO_NAME",
            "WSL_INTEROP",
        ] {
            lines.push(format!(
                "  env.{key}={}",
                env::var(key).unwrap_or_else(|_| "<unset>".to_string())
            ));
        }
        lines.push(format!("  current_dir={}", display_current_dir()));

        let version = Command::new(&self.tmux_binary_path).arg("-V").output();
        lines.push(format!("  tmux_version={}", render_command_probe(version)));

        let debug_dir = env::temp_dir().join(format!(
            "waitagent-tmux-debug-{}-{}",
            std::process::id(),
            monotonic_millis()
        ));
        match fs::create_dir_all(&debug_dir) {
            Ok(()) => {
                lines.push(format!("  verbose_debug_dir={}", debug_dir.display()));
                let diagnostic_socket =
                    format!("{}-diag-{}", socket_name.as_str(), std::process::id());
                let output = Command::new(&self.tmux_binary_path)
                    .arg("-f")
                    .arg("/dev/null")
                    .arg("-L")
                    .arg(&diagnostic_socket)
                    .arg("-vv")
                    .args(args)
                    .current_dir(&debug_dir)
                    .output();
                lines.push(format!("  verbose_replay={}", render_command_probe(output)));
                let _ = Command::new(&self.tmux_binary_path)
                    .arg("-f")
                    .arg("/dev/null")
                    .arg("-L")
                    .arg(&diagnostic_socket)
                    .arg("kill-server")
                    .output();
                append_tmux_verbose_logs(&mut lines, &debug_dir);
                append_tmux_strace(
                    &mut lines,
                    &debug_dir,
                    &self.tmux_binary_path,
                    socket_name,
                    args,
                );
            }
            Err(error) => {
                lines.push(format!(
                    "  verbose_debug_dir_error=failed to create {}: {error}",
                    debug_dir.display()
                ));
            }
        }

        lines.join("\n")
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

fn display_current_dir() -> String {
    env::current_dir()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|error| format!("<error: {error}>"))
}

fn monotonic_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

fn render_command_probe(output: std::io::Result<std::process::Output>) -> String {
    match output {
        Ok(output) => {
            let code = output
                .status
                .code()
                .map(|value| value.to_string())
                .unwrap_or_else(|| "signal".to_string());
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            format!(
                "exit={code}, stdout={}, stderr={}",
                render_probe_text(&stdout),
                render_probe_text(&stderr)
            )
        }
        Err(error) => format!("spawn_error={error}"),
    }
}

fn render_probe_text(value: &str) -> String {
    if value.is_empty() {
        return "<empty>".to_string();
    }
    value.replace('\n', "\\n")
}

fn append_tmux_verbose_logs(lines: &mut Vec<String>, debug_dir: &std::path::Path) {
    let Ok(entries) = fs::read_dir(debug_dir) else {
        lines.push("  verbose_logs=<read_dir failed>".to_string());
        return;
    };
    let mut paths = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.starts_with("tmux-client-") || name.starts_with("tmux-server-"))
                .unwrap_or(false)
        })
        .collect::<Vec<_>>();
    paths.sort();

    if paths.is_empty() {
        lines.push("  verbose_logs=<none>".to_string());
        return;
    }

    for path in paths {
        lines.push(format!("  verbose_log={}:", path.display()));
        match fs::read_to_string(&path) {
            Ok(contents) => {
                for line in tail_lines(&contents, 120) {
                    lines.push(format!("    {line}"));
                }
            }
            Err(error) => lines.push(format!("    <failed to read: {error}>")),
        }
    }
}

fn append_tmux_strace(
    lines: &mut Vec<String>,
    debug_dir: &std::path::Path,
    tmux_binary_path: &std::path::Path,
    socket_name: &TmuxSocketName,
    args: &[String],
) {
    let strace_probe = Command::new("strace").arg("-V").output();
    if strace_probe.is_err() {
        lines.push("  strace=<unavailable>".to_string());
        return;
    }

    let diagnostic_socket = format!("{}-strace-{}", socket_name.as_str(), std::process::id());
    let output_prefix = debug_dir.join("strace-tmux");
    let output = Command::new("strace")
        .arg("-ff")
        .arg("-tt")
        .arg("-s")
        .arg("256")
        .arg("-o")
        .arg(&output_prefix)
        .arg(tmux_binary_path)
        .arg("-f")
        .arg("/dev/null")
        .arg("-L")
        .arg(&diagnostic_socket)
        .args(args)
        .current_dir(debug_dir)
        .output();
    lines.push(format!("  strace_replay={}", render_command_probe(output)));

    let _ = Command::new(tmux_binary_path)
        .arg("-f")
        .arg("/dev/null")
        .arg("-L")
        .arg(&diagnostic_socket)
        .arg("kill-server")
        .output();
    append_strace_logs(lines, debug_dir);
}

fn append_strace_logs(lines: &mut Vec<String>, debug_dir: &std::path::Path) {
    let Ok(entries) = fs::read_dir(debug_dir) else {
        lines.push("  strace_logs=<read_dir failed>".to_string());
        return;
    };
    let mut paths = entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.starts_with("strace-tmux."))
                .unwrap_or(false)
        })
        .collect::<Vec<_>>();
    paths.sort();

    if paths.is_empty() {
        lines.push("  strace_logs=<none>".to_string());
        return;
    }

    for path in paths.iter().rev().take(8).rev() {
        lines.push(format!("  strace_log={}:", path.display()));
        match fs::read_to_string(path) {
            Ok(contents) => {
                for line in tail_lines(&contents, 80) {
                    lines.push(format!("    {line}"));
                }
            }
            Err(error) => lines.push(format!("    <failed to read: {error}>")),
        }
    }
}

fn tail_lines(contents: &str, max_lines: usize) -> Vec<&str> {
    let lines = contents.lines().collect::<Vec<_>>();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..].to_vec()
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
        let stdout_suffix = if failure.stdout.is_empty() {
            String::new()
        } else {
            format!("\nstdout:\n{}", failure.stdout)
        };
        let diagnostics_suffix = if failure.diagnostics.is_empty() {
            String::new()
        } else {
            format!("\n{}", failure.diagnostics)
        };
        Self {
            message: format!(
                "vendored tmux command failed with exit code {exit_code}: `{}`{stderr_suffix}{stdout_suffix}{diagnostics_suffix}",
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
