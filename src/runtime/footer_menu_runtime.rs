use crate::application::session_service::SessionService;
use crate::cli::FooterMenuCommand;
use crate::domain::session_catalog::ManagedSessionRecord;
use crate::infra::tmux::{EmbeddedTmuxBackend, TmuxError, TmuxSocketName};
use crate::lifecycle::LifecycleError;
use std::io;
use std::path::PathBuf;

const FOOTER_MENU_TITLE: &str = "WaitAgent Sessions";
const MAX_SESSION_SHORTCUTS: usize = 9;

pub struct FooterMenuRuntime {
    backend: EmbeddedTmuxBackend,
    session_service: SessionService<EmbeddedTmuxBackend>,
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
            session_service: SessionService::new(backend.clone()),
            backend,
            current_executable,
        })
    }

    pub fn run(&self, command: FooterMenuCommand) -> Result<(), LifecycleError> {
        let sessions = self
            .session_service
            .list_sessions()
            .map_err(footer_menu_error)?;
        let args = build_footer_menu_args(&command, &self.current_executable, &sessions);
        self.backend
            .run_socket_command(&TmuxSocketName::new(command.socket_name), &args)
            .map(|_| ())
            .map_err(footer_menu_error)
    }
}

fn build_footer_menu_args(
    command: &FooterMenuCommand,
    executable: &std::path::Path,
    sessions: &[ManagedSessionRecord],
) -> Vec<String> {
    let active = active_session(command, sessions);
    let mut args = vec![
        "display-menu".to_string(),
        "-c".to_string(),
        command.client_tty.clone(),
        "-t".to_string(),
        command.pane_id.clone(),
        "-x".to_string(),
        "P".to_string(),
        "-y".to_string(),
        "P".to_string(),
        "-T".to_string(),
        FOOTER_MENU_TITLE.to_string(),
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

    push_action_item(
        &mut args,
        "New Session",
        "c",
        &create_session_command(executable),
    );
    push_separator(&mut args);

    if sessions.is_empty() {
        push_disabled_item(&mut args, "- No Sessions");
        return args;
    }

    push_disabled_item(&mut args, "- Sessions");
    for (index, session) in sessions.iter().take(MAX_SESSION_SHORTCUTS).enumerate() {
        push_action_item(
            &mut args,
            &menu_label(command, session),
            &(index + 1).to_string(),
            &attach_session_command(executable, session),
        );
    }

    args
}

fn active_session<'a>(
    command: &FooterMenuCommand,
    sessions: &'a [ManagedSessionRecord],
) -> Option<&'a ManagedSessionRecord> {
    sessions.iter().find(|session| {
        session.address.server_id() == command.socket_name
            && session.address.session_id() == command.session_name
    })
}

fn menu_label(command: &FooterMenuCommand, session: &ManagedSessionRecord) -> String {
    let marker = if session.address.server_id() == command.socket_name
        && session.address.session_id() == command.session_name
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

fn create_session_command(executable: &std::path::Path) -> String {
    format!(
        "detach-client -E {}",
        shell_escape(&executable.display().to_string())
    )
}

fn attach_session_command(executable: &std::path::Path, session: &ManagedSessionRecord) -> String {
    format!(
        "detach-client -E {}",
        shell_escape(&format!(
            "{} attach {}",
            executable.display(),
            session.address.qualified_target()
        ))
    )
}

fn shell_escape(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn footer_menu_error(error: TmuxError) -> LifecycleError {
    LifecycleError::Io(
        "failed to open waitagent footer menu".to_string(),
        io::Error::new(io::ErrorKind::Other, error.to_string()),
    )
}

#[cfg(test)]
mod tests {
    use super::{build_footer_menu_args, FOOTER_MENU_TITLE};
    use crate::cli::FooterMenuCommand;
    use crate::domain::session_catalog::{
        ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState,
    };
    use std::path::{Path, PathBuf};

    #[test]
    fn footer_menu_builds_display_menu_with_new_session_and_attach_items() {
        let args = build_footer_menu_args(
            &FooterMenuCommand {
                socket_name: "wa-1".to_string(),
                session_name: "1".to_string(),
                pane_id: "%2".to_string(),
                client_tty: "/dev/pts/7".to_string(),
            },
            Path::new("/tmp/waitagent"),
            &[ManagedSessionRecord {
                address: ManagedSessionAddress::local_tmux("wa-1", "1"),
                workspace_dir: Some(PathBuf::from("/tmp/demo")),
                workspace_key: None,
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
        assert!(args
            .iter()
            .any(|value| value == "- Current: codex@local [I]"));
        assert!(args.iter().any(|value| value == "- Cwd: /tmp/demo"));
        assert!(args.iter().any(|value| value == "- Sessions"));
        assert!(args
            .iter()
            .any(|value| value == "* codex@local [I] cwd: /tmp/demo"));
        assert!(args
            .iter()
            .any(|value| value.contains("detach-client -E '/tmp/waitagent attach wa-1:1'")));
    }

    #[test]
    fn footer_menu_shows_empty_state_when_no_sessions_exist() {
        let args = build_footer_menu_args(
            &FooterMenuCommand {
                socket_name: "wa-1".to_string(),
                session_name: "1".to_string(),
                pane_id: "%2".to_string(),
                client_tty: "/dev/pts/7".to_string(),
            },
            Path::new("/tmp/waitagent"),
            &[],
        );

        assert!(args.iter().any(|value| value == "New Session"));
        assert!(args.iter().any(|value| value == "- No Sessions"));
    }
}
