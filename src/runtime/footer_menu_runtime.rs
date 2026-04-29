use crate::application::target_registry_service::{
    DefaultTargetCatalogGateway, TargetRegistryService,
};
use crate::cli::FooterMenuCommand;
use crate::domain::session_catalog::ManagedSessionRecord;
use crate::domain::workspace::WorkspaceInstanceId;
use crate::infra::tmux::{
    EmbeddedTmuxBackend, TmuxError, TmuxSessionName, TmuxSocketName, TmuxWorkspaceHandle,
};
use crate::lifecycle::LifecycleError;
use std::io;
use std::path::PathBuf;

const FOOTER_MENU_TITLE: &str = "WaitAgent Menu";
const MAX_SESSION_SHORTCUTS: usize = 9;
const WAITAGENT_ACTIVE_TARGET_OPTION: &str = "@waitagent_active_target";

pub struct FooterMenuRuntime {
    backend: EmbeddedTmuxBackend,
    target_registry: TargetRegistryService<DefaultTargetCatalogGateway>,
    current_executable: PathBuf,
}

impl FooterMenuRuntime {
    pub fn from_build_env() -> Result<Self, LifecycleError> {
        let backend = EmbeddedTmuxBackend::from_build_env().map_err(footer_menu_error)?;
        let current_executable = std::env::current_exe().map_err(|error| {
            LifecycleError::Io(
                "failed to locate current waitagent executable".to_string(),
                error,
            )
        })?;

        Ok(Self {
            target_registry: TargetRegistryService::new(
                DefaultTargetCatalogGateway::from_build_env().map_err(footer_menu_error)?,
            ),
            backend,
            current_executable,
        })
    }

    pub fn run(&self, command: FooterMenuCommand) -> Result<(), LifecycleError> {
        let active_target = self
            .backend
            .show_session_option(
                &workspace_handle(&command.socket_name, &command.session_name),
                WAITAGENT_ACTIVE_TARGET_OPTION,
            )
            .map_err(footer_menu_error)?;
        let visible_sessions = self
            .target_registry
            .visible_targets_in_workspace(
                &command.socket_name,
                &command.session_name,
                active_target.as_deref(),
            )
            .map_err(footer_menu_error)?;
        let args = build_footer_menu_args(
            &command,
            &self.current_executable,
            active_target.as_deref(),
            &visible_sessions,
        );
        self.backend
            .run_socket_command(&TmuxSocketName::new(command.socket_name), &args)
            .map(|_| ())
            .map_err(footer_menu_error)
    }
}

fn build_footer_menu_args(
    command: &FooterMenuCommand,
    executable: &std::path::Path,
    active_target: Option<&str>,
    sessions: &[ManagedSessionRecord],
) -> Vec<String> {
    let active = active_session(command, active_target, sessions);
    let mut args = vec![
        "display-menu".to_string(),
        "-c".to_string(),
        command.client_tty.clone(),
        "-x".to_string(),
        "P".to_string(),
        "-y".to_string(),
        "P".to_string(),
        "-T".to_string(),
        FOOTER_MENU_TITLE.to_string(),
        "--".to_string(),
    ];

    if let Some(session) = active {
        push_disabled_item(
            &mut args,
            &format!(
                "- Current: {} [{}]",
                session.display_label(),
                session.task_state.short_label()
            ),
        );
        push_disabled_item(
            &mut args,
            &format!("- Cwd: {}", current_path_label(session)),
        );
        push_separator(&mut args);
    }

    push_disabled_item(&mut args, "- Commands");
    push_action_item(
        &mut args,
        "New Session",
        "c",
        &create_session_command(executable, command),
    );
    if active.is_some() {
        push_action_item(
            &mut args,
            "Close Current Session",
            "x",
            &close_session_command(executable, command),
        );
    }
    push_action_item(&mut args, "Quit Client", "q", "detach-client");
    push_separator(&mut args);

    if sessions.is_empty() {
        push_disabled_item(&mut args, "- No Sessions");
        return args;
    }

    push_disabled_item(&mut args, "- Sessions");
    for (index, session) in sessions.iter().take(MAX_SESSION_SHORTCUTS).enumerate() {
        push_action_item(
            &mut args,
            &menu_label_with_active_target(command, active_target, session),
            &(index + 1).to_string(),
            &activate_target_command(executable, command, session),
        );
    }

    args
}

fn active_session<'a>(
    command: &FooterMenuCommand,
    active_target: Option<&str>,
    sessions: &'a [ManagedSessionRecord],
) -> Option<&'a ManagedSessionRecord> {
    active_target
        .and_then(|target| {
            sessions
                .iter()
                .find(|session| session.address.qualified_target() == target)
        })
        .or_else(|| {
            sessions.iter().find(|session| {
                session.address.server_id() == command.socket_name
                    && session.address.session_id() == command.session_name
            })
        })
}

fn menu_label_with_active_target(
    command: &FooterMenuCommand,
    active_target: Option<&str>,
    session: &ManagedSessionRecord,
) -> String {
    let qualified_target = session.address.qualified_target();
    let marker = if active_target == Some(qualified_target.as_str())
        || (active_target.is_none()
            && session.address.server_id() == command.socket_name
            && session.address.session_id() == command.session_name)
    {
        "*"
    } else {
        " "
    };
    format!(
        "{marker} {} [{}] cwd: {}",
        session.display_label(),
        session.task_state.short_label(),
        current_path_label(session)
    )
}

fn current_path_label(session: &ManagedSessionRecord) -> String {
    session
        .current_path
        .as_ref()
        .or(session.workspace_dir.as_ref())
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "-".to_string())
}

fn push_separator(args: &mut Vec<String>) {
    args.push(String::new());
}

fn push_disabled_item(args: &mut Vec<String>, label: &str) {
    args.push(label.to_string());
    args.push(String::new());
    args.push(String::new());
}

fn push_action_item(args: &mut Vec<String>, label: &str, key: &str, command: &str) {
    args.push(label.to_string());
    args.push(key.to_string());
    args.push(command.to_string());
}

fn create_session_command(executable: &std::path::Path, command: &FooterMenuCommand) -> String {
    let shell_command = [
        shell_escape(&executable.display().to_string()),
        shell_escape("__new-target"),
        shell_escape("--current-socket-name"),
        shell_escape(&command.socket_name),
        shell_escape("--current-session-name"),
        shell_escape(&command.session_name),
    ]
    .join(" ");

    format!("run-shell -b {}", tmux_quote_argument(&shell_command))
}

fn activate_target_command(
    executable: &std::path::Path,
    command: &FooterMenuCommand,
    session: &ManagedSessionRecord,
) -> String {
    let shell_command = [
        shell_escape(&executable.display().to_string()),
        shell_escape("__activate-target"),
        shell_escape("--current-socket-name"),
        shell_escape(&command.socket_name),
        shell_escape("--current-session-name"),
        shell_escape(&command.session_name),
        shell_escape("--target"),
        shell_escape(&session.address.qualified_target()),
    ]
    .join(" ");

    format!("run-shell -b {}", tmux_quote_argument(&shell_command))
}

fn close_session_command(executable: &std::path::Path, command: &FooterMenuCommand) -> String {
    format!(
        "detach-client -E {}",
        shell_escape(&format!(
            "{} __close-session --socket-name {} --session-name {}",
            executable.display(),
            command.socket_name,
            command.session_name
        ))
    )
}

fn shell_escape(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn tmux_quote_argument(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn footer_menu_error(error: TmuxError) -> LifecycleError {
    LifecycleError::Io(
        "failed to open waitagent footer menu".to_string(),
        io::Error::new(io::ErrorKind::Other, error.to_string()),
    )
}

fn workspace_handle(socket_name: &str, session_name: &str) -> TmuxWorkspaceHandle {
    TmuxWorkspaceHandle {
        workspace_id: WorkspaceInstanceId::new(session_name.to_string()),
        socket_name: TmuxSocketName::new(socket_name.to_string()),
        session_name: TmuxSessionName::new(session_name.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::{build_footer_menu_args, FOOTER_MENU_TITLE};
    use crate::cli::FooterMenuCommand;
    use crate::domain::session_catalog::{
        ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState,
    };
    use crate::domain::workspace::WorkspaceSessionRole;
    use std::path::{Path, PathBuf};

    #[test]
    fn footer_menu_builds_display_menu_with_new_session_and_attach_items() {
        let args = build_footer_menu_args(
            &FooterMenuCommand {
                socket_name: "wa-1".to_string(),
                session_name: "1".to_string(),
                client_tty: "/dev/pts/7".to_string(),
            },
            Path::new("/tmp/waitagent"),
            Some("wa-1:1"),
            &[ManagedSessionRecord {
                address: ManagedSessionAddress::local_tmux("wa-1", "1"),
                selector: Some("wa-1:1".to_string()),
                availability: crate::domain::session_catalog::SessionAvailability::Online,
                workspace_dir: Some(PathBuf::from("/tmp/demo")),
                workspace_key: None,
                session_role: Some(WorkspaceSessionRole::WorkspaceChrome),
                opened_by: Vec::new(),
                attached_clients: 1,
                window_count: 1,
                command_name: Some("codex".to_string()),
                current_path: Some(PathBuf::from("/tmp/demo")),
                task_state: ManagedSessionTaskState::Input,
            }],
        );

        assert_eq!(args[0], "display-menu");
        assert!(args.contains(&"New Session".to_string()));
        assert!(args.contains(&FOOTER_MENU_TITLE.to_string()));
        assert!(args.contains(&"--".to_string()));
        assert!(args
            .iter()
            .any(|value| value == "- Current: codex@local [I]"));
        assert!(args.iter().any(|value| value == "- Cwd: /tmp/demo"));
        assert!(args.iter().any(|value| value == "- Commands"));
        assert!(args.iter().any(|value| value == "- Sessions"));
        assert!(args.iter().any(|value| value == "Close Current Session"));
        assert!(args.iter().any(|value| value == "Quit Client"));
        assert!(args
            .iter()
            .any(|value| value == "* codex@local [I] cwd: /tmp/demo"));
        assert!(args.iter().any(|value| {
            value.contains("run-shell -b ")
                && value.contains("'__activate-target'")
                && value.contains("'--current-socket-name'")
                && value.contains("'wa-1'")
                && value.contains("'--current-session-name'")
                && value.contains("'1'")
                && value.contains("'--target'")
                && value.contains("'wa-1:1'")
        }));
        assert!(args.iter().any(|value| {
            value.contains(
                "detach-client -E '/tmp/waitagent __close-session --socket-name wa-1 --session-name 1'",
            )
        }));
    }

    #[test]
    fn footer_menu_shows_empty_state_when_no_sessions_exist() {
        let args = build_footer_menu_args(
            &FooterMenuCommand {
                socket_name: "wa-1".to_string(),
                session_name: "1".to_string(),
                client_tty: "/dev/pts/7".to_string(),
            },
            Path::new("/tmp/waitagent"),
            None,
            &[],
        );

        assert!(args.iter().any(|value| value == "New Session"));
        assert!(args.iter().any(|value| value == "- No Sessions"));
    }
}
