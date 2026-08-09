//! Paste dispatcher for clipboard content.
//!
//! The dispatcher takes classified clipboard content, resolves it against the
//! active session in the current snapshot, and produces either an immediate
//! paste action or a background job for file I/O.

use crate::domain::agent_detector::accepts_at_reference;
use crate::ratatui_node::clipboard_classifier::ClipboardContent;
use crate::ratatui_node::clipboard_platform::{format_file_reference, PlatformContext};
use crate::ratatui_node::snapshot::{RatatuiSnapshot, SessionView};
use std::path::PathBuf;

/// Action produced by the paste dispatcher.
///
/// Immediate actions can be sent to the server directly; file jobs must be
/// executed on a background worker so the TUI event loop is not blocked.
#[derive(Debug)]
pub enum PasteAction {
    /// Send plain text via the `PASTE_TEXT` command.
    SendText { target_id: String, text: String },
    /// Run a file-related job on the background worker.
    RunJob(PasteJob),
    /// Show an error to the user.
    Error(String),
    /// Nothing to do.
    Nothing,
}

/// Background job for clipboard operations that may block or use significant CPU.
#[derive(Debug)]
pub enum PasteJob {
    /// Read a local file and forward it to a remote session as `PASTE_FILE`.
    ReadRemoteFile {
        target_id: String,
        path: PathBuf,
        filename_hint: String,
    },
    /// Cache a binary clipboard payload locally and inject the resulting path.
    CacheLocalBinary {
        target_id: String,
        filename_hint: String,
        bytes: Vec<u8>,
        supports_at: bool,
    },
    /// Encode a binary clipboard payload as base64 and send it as `PASTE_FILE`.
    EncodeRemoteBinary {
        target_id: String,
        filename_hint: String,
        bytes: Vec<u8>,
    },
}

/// Result returned by the background worker after completing a paste job.
#[derive(Debug)]
pub enum PasteJobResult {
    PasteText {
        target_id: String,
        text: String,
    },
    PasteFile {
        target_id: String,
        filename_hint: String,
        base64: String,
    },
    Error(String),
}

/// Context used by the dispatcher to resolve sessions and platform paths.
pub struct PasteContext<'a> {
    pub platform: PlatformContext,
    pub snapshot: &'a RatatuiSnapshot,
}

impl PasteContext<'_> {
    fn active_session(&self) -> Option<&SessionView> {
        let target_id = self.snapshot.active_target.as_deref()?;
        self.snapshot.sessions.iter().find(|s| s.id == target_id)
    }

    fn supports_at(&self, session: &SessionView) -> bool {
        session
            .agent_command_name
            .as_deref()
            .or(Some(session.command_name.as_str()))
            .map(accepts_at_reference)
            .unwrap_or(false)
    }

    fn is_local(&self, session: &SessionView) -> bool {
        session.transport == "local"
    }
}

/// Dispatch classified clipboard content to the appropriate paste handler.
pub fn dispatch_paste(content: ClipboardContent, ctx: &PasteContext<'_>) -> PasteAction {
    dispatch_paste_inner(content, ctx)
}

fn dispatch_paste_inner(content: ClipboardContent, ctx: &PasteContext<'_>) -> PasteAction {
    let Some(session) = ctx.active_session() else {
        return PasteAction::Nothing;
    };
    let target_id = session.id.clone();
    let is_local = ctx.is_local(session);
    let supports_at = ctx.supports_at(session);

    match content {
        ClipboardContent::PlainText(text) => {
            // If the text can be parsed as existing file paths, treat it as a
            // file paste; otherwise send it as plain text.
            if let Some(paths) = ctx.platform.parse_file_paths_from_text(&text) {
                let existing: Vec<PathBuf> = paths.into_iter().filter(|p| p.exists()).collect();
                if !existing.is_empty() {
                    return dispatch_file_paths(ctx, target_id, is_local, supports_at, &existing);
                }
            }
            PasteAction::SendText { target_id, text }
        }
        ClipboardContent::FileUris(uris) => {
            let mut paths = Vec::with_capacity(uris.len());
            for uri in uris {
                match ctx.platform.resolve_file_uri(&uri) {
                    Some(path) => paths.push(path),
                    None => {
                        return PasteAction::Error(format!("invalid file URI: {uri}"));
                    }
                }
            }
            dispatch_file_paths(ctx, target_id, is_local, supports_at, &paths)
        }
        ClipboardContent::BinaryFile {
            filename_hint,
            bytes,
        } => {
            if is_local {
                PasteAction::RunJob(PasteJob::CacheLocalBinary {
                    target_id,
                    filename_hint,
                    bytes,
                    supports_at,
                })
            } else {
                PasteAction::RunJob(PasteJob::EncodeRemoteBinary {
                    target_id,
                    filename_hint,
                    bytes,
                })
            }
        }
    }
}

fn dispatch_file_paths(
    ctx: &PasteContext<'_>,
    target_id: String,
    is_local: bool,
    supports_at: bool,
    paths: &[PathBuf],
) -> PasteAction {
    if is_local {
        let path_string = paths
            .iter()
            .map(|p| format_file_reference(&ctx.platform.path_for_input(p), supports_at))
            .collect::<Vec<_>>()
            .join(" ");
        return PasteAction::SendText {
            target_id,
            text: path_string,
        };
    }

    if paths.len() > 1 {
        return PasteAction::Error(
            "pasting multiple remote files at once is not yet supported".to_string(),
        );
    }

    let Some(path) = paths.first() else {
        return PasteAction::Nothing;
    };

    let filename_hint = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("paste")
        .to_string();
    PasteAction::RunJob(PasteJob::ReadRemoteFile {
        target_id,
        path: path.clone(),
        filename_hint,
    })
}

/// Execute a paste job on a background worker.
///
/// This function is intended to be called from the clipboard worker thread.
/// It performs blocking file I/O and CPU-heavy base64 encoding, then returns
/// a result that the main event loop can send to the server.
pub fn run_paste_job(ctx: &PlatformContext, job: PasteJob) -> PasteJobResult {
    run_paste_job_inner(ctx, job)
}

fn run_paste_job_inner(ctx: &PlatformContext, job: PasteJob) -> PasteJobResult {
    match job {
        PasteJob::ReadRemoteFile {
            target_id,
            path,
            filename_hint,
        } => match ctx.read_file(&path) {
            Ok(bytes) => PasteJobResult::PasteFile {
                target_id,
                filename_hint,
                base64: base64_encode(&bytes),
            },
            Err(error) => {
                PasteJobResult::Error(format!("failed to read file {}: {error}", path.display()))
            }
        },
        PasteJob::CacheLocalBinary {
            target_id,
            filename_hint,
            bytes,
            supports_at,
        } => match ctx.write_temp_file(&filename_hint, &bytes) {
            Ok(path) => {
                let path_ref = format_file_reference(&ctx.path_for_input(&path), supports_at);
                PasteJobResult::PasteText {
                    target_id,
                    text: path_ref,
                }
            }
            Err(error) => PasteJobResult::Error(format!("failed to cache pasted file: {error}")),
        },
        PasteJob::EncodeRemoteBinary {
            target_id,
            filename_hint,
            bytes,
        } => PasteJobResult::PasteFile {
            target_id,
            filename_hint,
            base64: base64_encode(&bytes),
        },
    }
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::{engine::general_purpose, Engine as _};
    general_purpose::STANDARD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ratatui_node::snapshot::SessionView;

    fn snapshot_with_session(transport: &str, agent_command_name: Option<&str>) -> RatatuiSnapshot {
        RatatuiSnapshot {
            active_target: Some("target#1".to_string()),
            sessions: vec![SessionView {
                id: "target#1".to_string(),
                transport: transport.to_string(),
                command_name: "bash".to_string(),
                agent_command_name: agent_command_name.map(String::from),
                authority_node_id: "node#1".to_string(),
                display_authority_id: "node".to_string(),
                session_id: "1".to_string(),
                task_state: "running".to_string(),
                availability: "available".to_string(),
                attached_clients: 1,
                current_path: None,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn plain_text_sent_directly() {
        let snapshot = snapshot_with_session("local", None);
        let ctx = PasteContext {
            platform: PlatformContext::Linux,
            snapshot: &snapshot,
        };
        let action = dispatch_paste(ClipboardContent::PlainText("hello".to_string()), &ctx);
        match action {
            PasteAction::SendText { target_id, text } => {
                assert_eq!(target_id, "target#1");
                assert_eq!(text, "hello");
            }
            other => panic!("expected SendText, got {other:?}"),
        }
    }

    #[test]
    fn agent_session_gets_at_reference() {
        let temp_file = std::env::temp_dir().join("waitagent-test-at-reference.txt");
        let _ = std::fs::write(&temp_file, b"test");
        let path = temp_file.to_string_lossy().to_string();

        let snapshot = snapshot_with_session("local", Some("kimi"));
        let ctx = PasteContext {
            platform: PlatformContext::Linux,
            snapshot: &snapshot,
        };
        let action = dispatch_paste(ClipboardContent::PlainText(path.clone()), &ctx);
        let _ = std::fs::remove_file(&temp_file);

        match action {
            PasteAction::SendText { text, .. } => {
                assert_eq!(text, format!("@{path}"));
            }
            other => panic!("expected SendText, got {other:?}"),
        }
    }

    #[test]
    fn bash_session_gets_raw_path() {
        let snapshot = snapshot_with_session("local", None);
        let ctx = PasteContext {
            platform: PlatformContext::Linux,
            snapshot: &snapshot,
        };
        let action = dispatch_paste(
            ClipboardContent::PlainText("/tmp/waitagent/file.txt".to_string()),
            &ctx,
        );
        match action {
            PasteAction::SendText { text, .. } => {
                assert_eq!(text, "/tmp/waitagent/file.txt");
            }
            other => panic!("expected SendText, got {other:?}"),
        }
    }

    #[test]
    fn remote_binary_enqueues_encode_job() {
        let snapshot = snapshot_with_session("remote", Some("kimi"));
        let ctx = PasteContext {
            platform: PlatformContext::Linux,
            snapshot: &snapshot,
        };
        let action = dispatch_paste(
            ClipboardContent::BinaryFile {
                filename_hint: "shot.png".to_string(),
                bytes: b"png".to_vec(),
            },
            &ctx,
        );
        match action {
            PasteAction::RunJob(PasteJob::EncodeRemoteBinary { .. }) => {}
            other => panic!("expected EncodeRemoteBinary job, got {other:?}"),
        }
    }
}
