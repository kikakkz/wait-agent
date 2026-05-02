use crate::domain::session_catalog::{ManagedSessionRecord, SessionAvailability, SessionTransport};
use crate::infra::remote_protocol::{
    ApplyResizePayload, ControlPlaneDestination, ControlPlanePayload, MirrorBootstrapChunkPayload,
    MirrorBootstrapCompletePayload, NodeBoundControlPlaneMessage, OpenMirrorRequestPayload,
    OpenTargetOkPayload, ProtocolEnvelope, RemoteConsoleDescriptor, ResizeAuthorityChangedPayload,
    RoutedControlPlaneMessage, TargetInputPayload, TargetOutputPayload, REMOTE_PROTOCOL_VERSION,
    SERVER_SENDER_ID,
};
use std::collections::{BTreeSet, HashMap};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Default)]
pub struct RemoteControlPlaneService {
    next_message_id: u64,
    next_attachment_id: u64,
    session_states: HashMap<String, RemoteSessionState>,
}

impl RemoteControlPlaneService {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn open_target(
        &mut self,
        target: &ManagedSessionRecord,
        console: RemoteConsoleDescriptor,
        cols: usize,
        rows: usize,
    ) -> Result<Vec<RoutedControlPlaneMessage>, RemoteControlPlaneError> {
        validate_remote_target(target)?;
        let session_id = target.address.session_id().to_string();
        let target_id = target.address.id().as_str().to_string();
        let attachment_id = {
            let state = self
                .session_states
                .entry(session_id.clone())
                .or_insert_with(RemoteSessionState::new);
            let existing_index = state.attachments.iter().position(|attachment| {
                attachment.console.console_id == console.console_id
                    && attachment.console.console_host_id == console.console_host_id
            });
            if let Some(index) = existing_index {
                let existing = state.attachments.remove(index);
                existing.attachment_id
            } else {
                self.next_attachment_id += 1;
                format!("attach-{}", self.next_attachment_id)
            }
        };
        let (resize_epoch, authority_console_id, authority_host_id) = {
            let state = self
                .session_states
                .get_mut(&session_id)
                .ok_or(RemoteControlPlaneError::MissingAuthorityState)?;
            if !state.mirror_open {
                state.mirror_open = true;
            }
            state.last_open_ordinal += 1;
            state.pty_resize_epoch += 1;
            state.attachments.push(RemoteAttachment {
                attachment_id: attachment_id.clone(),
                console: console.clone(),
                open_ordinal: state.last_open_ordinal,
                viewport_size: (cols, rows),
            });
            state.pty_resize_authority_attachment_id = Some(attachment_id.clone());

            let authority = state
                .current_pty_resize_authority()
                .ok_or(RemoteControlPlaneError::MissingAuthorityState)?;
            (
                state.pty_resize_epoch,
                authority.console.console_id.clone(),
                authority.console.console_host_id.clone(),
            )
        };
        let open_ok = self.server_message(
            Some(session_id.clone()),
            Some(target_id.clone()),
            Some(attachment_id.clone()),
            Some(console.console_id.clone()),
            ControlPlanePayload::OpenTargetOk(OpenTargetOkPayload {
                session_id: session_id.clone(),
                target_id: target_id.clone(),
                attachment_id: attachment_id.clone(),
                console_id: console.console_id.clone(),
                resize_epoch,
                resize_authority_console_id: authority_console_id.clone(),
                resize_authority_host_id: authority_host_id.clone(),
                availability: availability_label(target.availability),
                initial_snapshot: None,
            }),
        );
        let authority_changed = self.server_message(
            Some(session_id.clone()),
            Some(target_id.clone()),
            None,
            None,
            ControlPlanePayload::ResizeAuthorityChanged(ResizeAuthorityChangedPayload {
                session_id: session_id.clone(),
                target_id: target_id.clone(),
                resize_epoch,
                resize_authority_console_id: authority_console_id.clone(),
                resize_authority_host_id: authority_host_id.clone(),
                cols: None,
                rows: None,
            }),
        );

        let mut messages = vec![
            RoutedControlPlaneMessage {
                destination: ControlPlaneDestination::ObserverNode(console.console_host_id.clone()),
                envelope: open_ok,
            },
            RoutedControlPlaneMessage {
                destination: ControlPlaneDestination::AllOpenedObservers {
                    session_id: session_id.clone(),
                },
                envelope: authority_changed,
            },
        ];
        if self
            .session_states
            .get(&session_id)
            .map(|state| state.mirror_open && state.attachments.len() == 1)
            .unwrap_or(false)
        {
            messages.push(RoutedControlPlaneMessage {
                destination: ControlPlaneDestination::AuthorityNode(
                    target.address.authority_id().to_string(),
                ),
                envelope: self.server_message(
                    Some(session_id.clone()),
                    Some(target_id.clone()),
                    None,
                    Some(console.console_id.clone()),
                    ControlPlanePayload::OpenMirrorRequest(OpenMirrorRequestPayload {
                        session_id,
                        target_id,
                        console_id: console.console_id,
                        cols,
                        rows,
                    }),
                ),
            });
        }
        Ok(messages)
    }

    pub fn route_console_input(
        &mut self,
        target: &ManagedSessionRecord,
        attachment_id: &str,
        console_seq: u64,
        bytes_base64: impl Into<String>,
    ) -> Result<RoutedControlPlaneMessage, RemoteControlPlaneError> {
        validate_remote_target(target)?;
        let session_id = target.address.session_id().to_string();
        let target_id = target.address.id().as_str().to_string();
        if console_seq == 0 {
            return Err(RemoteControlPlaneError::InvalidConsoleSequence);
        }

        let (input_seq, console_id, console_host_id) = {
            let state = self
                .session_states
                .get_mut(&session_id)
                .ok_or(RemoteControlPlaneError::TargetNotOpened(target_id.clone()))?;
            let attachment = state
                .attachments
                .iter()
                .find(|attachment| attachment.attachment_id == attachment_id)
                .ok_or_else(|| {
                    RemoteControlPlaneError::AttachmentNotOpen(attachment_id.to_string())
                })?;
            state.input_seq += 1;
            (
                state.input_seq,
                attachment.console.console_id.clone(),
                attachment.console.console_host_id.clone(),
            )
        };
        Ok(RoutedControlPlaneMessage {
            destination: ControlPlaneDestination::AuthorityNode(
                target.address.authority_id().to_string(),
            ),
            envelope: self.server_message(
                Some(session_id.clone()),
                Some(target_id.clone()),
                Some(attachment_id.to_string()),
                Some(console_id.clone()),
                ControlPlanePayload::TargetInput(TargetInputPayload {
                    attachment_id: attachment_id.to_string(),
                    session_id,
                    target_id,
                    console_id,
                    console_host_id,
                    input_seq,
                    bytes_base64: bytes_base64.into(),
                }),
            ),
        })
    }

    pub fn route_pty_resize_request(
        &mut self,
        target: &ManagedSessionRecord,
        attachment_id: &str,
        cols: usize,
        rows: usize,
    ) -> Result<RoutedControlPlaneMessage, RemoteControlPlaneError> {
        validate_remote_target(target)?;
        let session_id = target.address.session_id().to_string();
        let target_id = target.address.id().as_str().to_string();
        let (resize_epoch, authority_console_id) = {
            let state = self
                .session_states
                .get_mut(&session_id)
                .ok_or(RemoteControlPlaneError::TargetNotOpened(target_id.clone()))?;
            let authority_attachment_id = state
                .pty_resize_authority_attachment_id
                .as_deref()
                .ok_or(RemoteControlPlaneError::MissingAuthorityState)?;
            if authority_attachment_id != attachment_id {
                return Err(RemoteControlPlaneError::PtyResizeDenied {
                    attachment_id: attachment_id.to_string(),
                });
            }

            let authority_console_id = state
                .current_pty_resize_authority()
                .ok_or(RemoteControlPlaneError::MissingAuthorityState)?
                .console
                .console_id
                .clone();
            state.last_pty_size = Some((cols, rows));
            (state.pty_resize_epoch, authority_console_id)
        };
        Ok(RoutedControlPlaneMessage {
            destination: ControlPlaneDestination::AuthorityNode(
                target.address.authority_id().to_string(),
            ),
            envelope: self.server_message(
                Some(session_id.clone()),
                Some(target_id.clone()),
                Some(attachment_id.to_string()),
                Some(authority_console_id.clone()),
                ControlPlanePayload::ApplyResize(ApplyResizePayload {
                    session_id,
                    target_id,
                    resize_epoch,
                    resize_authority_console_id: authority_console_id,
                    cols,
                    rows,
                }),
            ),
        })
    }

    pub fn route_target_output(
        &mut self,
        target: &ManagedSessionRecord,
        output_seq: u64,
        stream: &'static str,
        bytes_base64: impl Into<String>,
    ) -> Result<RoutedControlPlaneMessage, RemoteControlPlaneError> {
        validate_remote_target(target)?;
        let session_id = target.address.session_id().to_string();
        let target_id = target.address.id().as_str().to_string();
        if output_seq == 0 {
            return Err(RemoteControlPlaneError::InvalidOutputSequence);
        }
        if !self.session_states.contains_key(&session_id) {
            return Err(RemoteControlPlaneError::TargetNotOpened(target_id));
        }

        Ok(RoutedControlPlaneMessage {
            destination: ControlPlaneDestination::AllOpenedObservers {
                session_id: session_id.clone(),
            },
            envelope: self.server_message(
                Some(session_id.clone()),
                Some(target.address.id().as_str().to_string()),
                None,
                None,
                ControlPlanePayload::TargetOutput(TargetOutputPayload {
                    session_id,
                    target_id: target.address.id().as_str().to_string(),
                    output_seq,
                    stream,
                    bytes_base64: bytes_base64.into(),
                }),
            ),
        })
    }

    pub fn route_mirror_bootstrap_chunk(
        &mut self,
        target: &ManagedSessionRecord,
        chunk_seq: u64,
        stream: &'static str,
        bytes_base64: impl Into<String>,
    ) -> Result<RoutedControlPlaneMessage, RemoteControlPlaneError> {
        validate_remote_target(target)?;
        let session_id = target.address.session_id().to_string();
        let target_id = target.address.id().as_str().to_string();
        if !self.session_states.contains_key(&session_id) {
            return Err(RemoteControlPlaneError::TargetNotOpened(target_id));
        }

        Ok(RoutedControlPlaneMessage {
            destination: ControlPlaneDestination::AllOpenedObservers {
                session_id: session_id.clone(),
            },
            envelope: self.server_message(
                Some(session_id.clone()),
                Some(target.address.id().as_str().to_string()),
                None,
                None,
                ControlPlanePayload::MirrorBootstrapChunk(MirrorBootstrapChunkPayload {
                    session_id,
                    target_id: target.address.id().as_str().to_string(),
                    chunk_seq,
                    stream,
                    bytes_base64: bytes_base64.into(),
                }),
            ),
        })
    }

    pub fn route_mirror_bootstrap_complete(
        &mut self,
        target: &ManagedSessionRecord,
        last_chunk_seq: u64,
    ) -> Result<RoutedControlPlaneMessage, RemoteControlPlaneError> {
        validate_remote_target(target)?;
        let session_id = target.address.session_id().to_string();
        let target_id = target.address.id().as_str().to_string();
        if !self.session_states.contains_key(&session_id) {
            return Err(RemoteControlPlaneError::TargetNotOpened(target_id));
        }

        Ok(RoutedControlPlaneMessage {
            destination: ControlPlaneDestination::AllOpenedObservers {
                session_id: session_id.clone(),
            },
            envelope: self.server_message(
                Some(session_id.clone()),
                Some(target.address.id().as_str().to_string()),
                None,
                None,
                ControlPlanePayload::MirrorBootstrapComplete(MirrorBootstrapCompletePayload {
                    session_id,
                    target_id: target.address.id().as_str().to_string(),
                    last_chunk_seq,
                }),
            ),
        })
    }

    pub fn close_target(
        &mut self,
        target: &ManagedSessionRecord,
        attachment_id: &str,
    ) -> Result<Vec<RoutedControlPlaneMessage>, RemoteControlPlaneError> {
        validate_remote_target(target)?;
        let session_id = target.address.session_id().to_string();
        let target_id = target.address.id().as_str().to_string();
        let close_outcome = {
            let state = self
                .session_states
                .get_mut(&session_id)
                .ok_or(RemoteControlPlaneError::TargetNotOpened(target_id.clone()))?;
            let removed_index = state
                .attachments
                .iter()
                .position(|attachment| attachment.attachment_id == attachment_id)
                .ok_or_else(|| {
                    RemoteControlPlaneError::AttachmentNotOpen(attachment_id.to_string())
                })?;
            let removed = state.attachments.remove(removed_index);
            let removed_was_authority =
                state.pty_resize_authority_attachment_id.as_deref() == Some(attachment_id);

            if !removed_was_authority {
                CloseOutcome::ClosedWithoutAuthorityChange { removed }
            } else if state.attachments.is_empty() {
                CloseOutcome::ClosedLastAttachment { removed }
            } else {
                let next_authority = state
                    .attachments
                    .iter()
                    .max_by_key(|attachment| attachment.open_ordinal)
                    .cloned()
                    .ok_or(RemoteControlPlaneError::MissingAuthorityState)?;
                state.pty_resize_authority_attachment_id =
                    Some(next_authority.attachment_id.clone());
                state.pty_resize_epoch += 1;
                CloseOutcome::PromotedNewAuthority {
                    removed,
                    next_authority,
                    resize_epoch: state.pty_resize_epoch,
                    pty_size: state.last_pty_size,
                }
            }
        };

        match close_outcome {
            CloseOutcome::ClosedWithoutAuthorityChange { .. } => Ok(Vec::new()),
            CloseOutcome::ClosedLastAttachment { .. } => {
                self.session_states.remove(&session_id);
                Ok(vec![RoutedControlPlaneMessage {
                    destination: ControlPlaneDestination::AuthorityNode(
                        target.address.authority_id().to_string(),
                    ),
                    envelope: self.server_message(
                        Some(session_id),
                        Some(target_id),
                        None,
                        None,
                        ControlPlanePayload::CloseMirrorRequest(
                            crate::infra::remote_protocol::CloseMirrorRequestPayload {
                                session_id: target.address.session_id().to_string(),
                                target_id: target.address.id().as_str().to_string(),
                            },
                        ),
                    ),
                }])
            }
            CloseOutcome::PromotedNewAuthority {
                removed,
                next_authority,
                resize_epoch,
                pty_size,
            } => {
                let authority_changed = self.server_message(
                    Some(session_id.clone()),
                    Some(target_id.clone()),
                    None,
                    None,
                    ControlPlanePayload::ResizeAuthorityChanged(ResizeAuthorityChangedPayload {
                        session_id: session_id.clone(),
                        target_id: target_id.clone(),
                        resize_epoch,
                        resize_authority_console_id: next_authority.console.console_id.clone(),
                        resize_authority_host_id: next_authority.console.console_host_id.clone(),
                        cols: pty_size.map(|(cols, _)| cols),
                        rows: pty_size.map(|(_, rows)| rows),
                    }),
                );

                Ok(vec![
                    RoutedControlPlaneMessage {
                        destination: ControlPlaneDestination::AllOpenedObservers {
                            session_id: session_id.clone(),
                        },
                        envelope: authority_changed,
                    },
                    RoutedControlPlaneMessage {
                        destination: ControlPlaneDestination::ObserverNode(
                            removed.console.console_host_id,
                        ),
                        envelope: self.server_message(
                            Some(session_id),
                            Some(target_id),
                            Some(removed.attachment_id),
                            Some(removed.console.console_id),
                            ControlPlanePayload::Error(
                                crate::infra::remote_protocol::ErrorPayload {
                                    code: "attachment_closed",
                                    message: "attachment closed".to_string(),
                                    details: None,
                                },
                            ),
                        ),
                    },
                ])
            }
        }
    }

    pub fn resolve_node_deliveries(
        &self,
        messages: &[RoutedControlPlaneMessage],
    ) -> Result<Vec<NodeBoundControlPlaneMessage>, RemoteControlPlaneError> {
        let mut deliveries = Vec::new();
        for message in messages {
            match &message.destination {
                ControlPlaneDestination::ObserverNode(node_id)
                | ControlPlaneDestination::AuthorityNode(node_id) => {
                    deliveries.push(NodeBoundControlPlaneMessage {
                        node_id: node_id.clone(),
                        envelope: message.envelope.clone(),
                    });
                }
                ControlPlaneDestination::AllOpenedObservers { session_id } => {
                    let state = self.session_states.get(session_id).ok_or_else(|| {
                        RemoteControlPlaneError::TargetNotOpened(session_id.clone())
                    })?;
                    let node_ids = state
                        .attachments
                        .iter()
                        .map(|attachment| attachment.console.console_host_id.clone())
                        .collect::<BTreeSet<_>>();
                    deliveries.extend(node_ids.into_iter().map(|node_id| {
                        NodeBoundControlPlaneMessage {
                            node_id,
                            envelope: message.envelope.clone(),
                        }
                    }));
                }
            }
        }
        Ok(deliveries)
    }

    fn server_message(
        &mut self,
        session_id: Option<String>,
        target_id: Option<String>,
        attachment_id: Option<String>,
        console_id: Option<String>,
        payload: ControlPlanePayload,
    ) -> ProtocolEnvelope<ControlPlanePayload> {
        self.next_message_id += 1;
        ProtocolEnvelope {
            protocol_version: REMOTE_PROTOCOL_VERSION.to_string(),
            message_id: format!("server-msg-{}", self.next_message_id),
            message_type: payload.message_type(),
            timestamp: now_rfc3339_like(),
            sender_id: SERVER_SENDER_ID.to_string(),
            correlation_id: None,
            session_id,
            target_id,
            attachment_id,
            console_id,
            payload,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteControlPlaneError {
    NotRemoteTarget,
    TargetUnavailable,
    TargetNotOpened(String),
    AttachmentNotOpen(String),
    PtyResizeDenied { attachment_id: String },
    InvalidConsoleSequence,
    InvalidOutputSequence,
    MissingAuthorityState,
}

impl std::fmt::Display for RemoteControlPlaneError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotRemoteTarget => write!(f, "target is not a remote peer target"),
            Self::TargetUnavailable => write!(f, "target is not currently available"),
            Self::TargetNotOpened(target_id) => {
                write!(f, "target `{target_id}` has not been opened in any console")
            }
            Self::AttachmentNotOpen(attachment_id) => {
                write!(f, "attachment `{attachment_id}` is not open")
            }
            Self::PtyResizeDenied { attachment_id } => {
                write!(
                    f,
                    "attachment `{attachment_id}` does not hold PTY resize authority"
                )
            }
            Self::InvalidConsoleSequence => write!(f, "console sequence must be positive"),
            Self::InvalidOutputSequence => write!(f, "output sequence must be positive"),
            Self::MissingAuthorityState => {
                write!(f, "remote target routing state has no authority")
            }
        }
    }
}

impl std::error::Error for RemoteControlPlaneError {}

#[derive(Debug, Clone)]
struct RemoteSessionState {
    mirror_open: bool,
    input_seq: u64,
    pty_resize_epoch: u64,
    last_open_ordinal: u64,
    last_pty_size: Option<(usize, usize)>,
    pty_resize_authority_attachment_id: Option<String>,
    attachments: Vec<RemoteAttachment>,
}

impl RemoteSessionState {
    fn new() -> Self {
        Self {
            mirror_open: false,
            input_seq: 0,
            pty_resize_epoch: 0,
            last_open_ordinal: 0,
            last_pty_size: None,
            pty_resize_authority_attachment_id: None,
            attachments: Vec::new(),
        }
    }

    fn current_pty_resize_authority(&self) -> Option<&RemoteAttachment> {
        let authority_attachment_id = self.pty_resize_authority_attachment_id.as_deref()?;
        self.attachments
            .iter()
            .find(|attachment| attachment.attachment_id == authority_attachment_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RemoteAttachment {
    attachment_id: String,
    console: RemoteConsoleDescriptor,
    open_ordinal: u64,
    viewport_size: (usize, usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CloseOutcome {
    ClosedWithoutAuthorityChange {
        removed: RemoteAttachment,
    },
    ClosedLastAttachment {
        removed: RemoteAttachment,
    },
    PromotedNewAuthority {
        removed: RemoteAttachment,
        next_authority: RemoteAttachment,
        resize_epoch: u64,
        pty_size: Option<(usize, usize)>,
    },
}

fn validate_remote_target(target: &ManagedSessionRecord) -> Result<(), RemoteControlPlaneError> {
    match target.address.transport() {
        SessionTransport::RemotePeer => {}
        SessionTransport::LocalTmux => return Err(RemoteControlPlaneError::NotRemoteTarget),
    }
    if target.availability != SessionAvailability::Online {
        return Err(RemoteControlPlaneError::TargetUnavailable);
    }
    Ok(())
}

fn availability_label(availability: SessionAvailability) -> &'static str {
    match availability {
        SessionAvailability::Online => "online",
        SessionAvailability::Offline => "offline",
        SessionAvailability::Exited => "exited",
        SessionAvailability::Unknown => "unknown",
    }
}

fn now_rfc3339_like() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("{millis}Z")
}

#[cfg(test)]
mod tests {
    use super::RemoteControlPlaneService;
    use crate::domain::session_catalog::{
        ConsoleLocation, ManagedSessionAddress, ManagedSessionRecord, ManagedSessionTaskState,
        SessionAvailability,
    };
    use crate::infra::remote_protocol::{
        ControlPlaneDestination, ControlPlanePayload, RemoteConsoleDescriptor,
    };

    #[test]
    fn open_target_assigns_attachment_and_pty_resize_authority_without_apply_resize() {
        let mut service = RemoteControlPlaneService::new();
        let target = remote_target("peer-a", "shell-1");

        let messages = service
            .open_target(
                &target,
                console("console-a", "observer-a", ConsoleLocation::LocalWorkspace),
                120,
                40,
            )
            .expect("open should succeed");

        assert_eq!(messages.len(), 3);
        assert!(matches!(
            messages[0].destination,
            ControlPlaneDestination::ObserverNode(ref node) if node == "observer-a"
        ));
        match &messages[0].envelope.payload {
            ControlPlanePayload::OpenTargetOk(payload) => {
                assert_eq!(payload.attachment_id, "attach-1");
                assert_eq!(payload.resize_epoch, 1);
                assert_eq!(payload.resize_authority_console_id, "console-a");
            }
            other => panic!("unexpected payload: {other:?}"),
        }
        assert!(matches!(
            messages[1].destination,
            ControlPlaneDestination::AllOpenedObservers { .. }
        ));
        match &messages[1].envelope.payload {
            ControlPlanePayload::ResizeAuthorityChanged(payload) => {
                assert_eq!(payload.cols, None);
                assert_eq!(payload.rows, None);
            }
            other => panic!("unexpected payload: {other:?}"),
        }
        assert!(matches!(
            messages[2].destination,
            ControlPlaneDestination::AuthorityNode(ref node) if node == "peer-a"
        ));
        match &messages[2].envelope.payload {
            ControlPlanePayload::OpenMirrorRequest(payload) => {
                assert_eq!(payload.session_id, "shell-1");
                assert_eq!(payload.target_id, "remote-peer:peer-a:shell-1");
                assert_eq!(payload.console_id, "console-a");
                assert_eq!(payload.cols, 120);
                assert_eq!(payload.rows, 40);
            }
            other => panic!("unexpected payload: {other:?}"),
        }
    }

    #[test]
    fn console_input_is_serialized_by_server_sequence() {
        let mut service = RemoteControlPlaneService::new();
        let target = remote_target("peer-a", "shell-1");

        let first_open = service
            .open_target(
                &target,
                console("console-a", "observer-a", ConsoleLocation::LocalWorkspace),
                100,
                30,
            )
            .expect("first open should succeed");
        let second_open = service
            .open_target(
                &target,
                console("console-b", "observer-b", ConsoleLocation::ServerConsole),
                140,
                50,
            )
            .expect("second open should succeed");
        let first_attachment = match &first_open[0].envelope.payload {
            ControlPlanePayload::OpenTargetOk(payload) => payload.attachment_id.clone(),
            other => panic!("unexpected payload: {other:?}"),
        };
        let second_attachment = match &second_open[0].envelope.payload {
            ControlPlanePayload::OpenTargetOk(payload) => payload.attachment_id.clone(),
            other => panic!("unexpected payload: {other:?}"),
        };

        let first_input = service
            .route_console_input(&target, &first_attachment, 1, "YQ==")
            .expect("first input should route");
        let second_input = service
            .route_console_input(&target, &second_attachment, 9, "Yg==")
            .expect("second input should route");

        match &first_input.envelope.payload {
            ControlPlanePayload::TargetInput(payload) => assert_eq!(payload.input_seq, 1),
            other => panic!("unexpected payload: {other:?}"),
        }
        match &second_input.envelope.payload {
            ControlPlanePayload::TargetInput(payload) => assert_eq!(payload.input_seq, 2),
            other => panic!("unexpected payload: {other:?}"),
        }
    }

    #[test]
    fn non_authority_pty_resize_is_rejected() {
        let mut service = RemoteControlPlaneService::new();
        let target = remote_target("peer-a", "shell-1");

        let first_open = service
            .open_target(
                &target,
                console("console-a", "observer-a", ConsoleLocation::LocalWorkspace),
                100,
                30,
            )
            .expect("first open should succeed");
        let second_open = service
            .open_target(
                &target,
                console("console-b", "observer-b", ConsoleLocation::ServerConsole),
                140,
                50,
            )
            .expect("second open should succeed");
        let first_attachment = match &first_open[0].envelope.payload {
            ControlPlanePayload::OpenTargetOk(payload) => payload.attachment_id.clone(),
            other => panic!("unexpected payload: {other:?}"),
        };
        let second_attachment = match &second_open[0].envelope.payload {
            ControlPlanePayload::OpenTargetOk(payload) => payload.attachment_id.clone(),
            other => panic!("unexpected payload: {other:?}"),
        };

        let error = service
            .route_pty_resize_request(&target, &first_attachment, 80, 24)
            .expect_err("older attachment should no longer hold authority");
        assert_eq!(
            error.to_string(),
            format!("attachment `{first_attachment}` does not hold PTY resize authority")
        );

        let routed = service
            .route_pty_resize_request(&target, &second_attachment, 160, 60)
            .expect("authority resize should route");
        match routed.envelope.payload {
            ControlPlanePayload::ApplyResize(payload) => {
                assert_eq!(payload.resize_epoch, 2);
                assert_eq!(payload.cols, 160);
                assert_eq!(payload.rows, 60);
            }
            other => panic!("unexpected payload: {other:?}"),
        }
    }

    #[test]
    fn closing_authority_promotes_most_recent_remaining_attachment() {
        let mut service = RemoteControlPlaneService::new();
        let target = remote_target("peer-a", "shell-1");

        let first_open = service
            .open_target(
                &target,
                console("console-a", "observer-a", ConsoleLocation::LocalWorkspace),
                100,
                30,
            )
            .expect("first open should succeed");
        let second_open = service
            .open_target(
                &target,
                console("console-b", "observer-b", ConsoleLocation::ServerConsole),
                140,
                50,
            )
            .expect("second open should succeed");
        let second_attachment = match &second_open[0].envelope.payload {
            ControlPlanePayload::OpenTargetOk(payload) => payload.attachment_id.clone(),
            other => panic!("unexpected payload: {other:?}"),
        };
        let close_messages = service
            .close_target(&target, &second_attachment)
            .expect("closing the authority attachment should succeed");

        assert_eq!(close_messages.len(), 2);
        match &close_messages[0].envelope.payload {
            ControlPlanePayload::ResizeAuthorityChanged(payload) => {
                assert_eq!(payload.resize_epoch, 3);
                assert_eq!(payload.resize_authority_console_id, "console-a");
                assert_eq!(payload.cols, None);
                assert_eq!(payload.rows, None);
            }
            other => panic!("unexpected payload: {other:?}"),
        }
        match &first_open[0].envelope.payload {
            ControlPlanePayload::OpenTargetOk(payload) => {
                assert_eq!(payload.attachment_id, "attach-1")
            }
            other => panic!("unexpected payload: {other:?}"),
        }
    }

    #[test]
    fn expands_all_opened_observers_into_concrete_node_deliveries() {
        let mut service = RemoteControlPlaneService::new();
        let target = remote_target("peer-a", "shell-1");

        service
            .open_target(
                &target,
                console("console-a", "observer-a", ConsoleLocation::LocalWorkspace),
                100,
                30,
            )
            .expect("first open should succeed");
        let routed = service
            .open_target(
                &target,
                console("console-b", "observer-b", ConsoleLocation::ServerConsole),
                140,
                50,
            )
            .expect("second open should succeed");

        let deliveries = service
            .resolve_node_deliveries(&routed)
            .expect("deliveries should resolve");

        assert_eq!(deliveries.len(), 3);
        assert_eq!(deliveries[0].node_id, "observer-b");
        assert_eq!(deliveries[1].node_id, "observer-a");
        assert_eq!(deliveries[2].node_id, "observer-b");
        assert!(matches!(
            deliveries[1].envelope.payload,
            ControlPlanePayload::ResizeAuthorityChanged(_)
        ));
        assert!(matches!(
            deliveries[2].envelope.payload,
            ControlPlanePayload::ResizeAuthorityChanged(_)
        ));
    }

    #[test]
    fn second_open_reuses_existing_mirror_route_without_duplicate_open_request() {
        let mut service = RemoteControlPlaneService::new();
        let target = remote_target("peer-a", "shell-1");

        service
            .open_target(
                &target,
                console("console-a", "observer-a", ConsoleLocation::LocalWorkspace),
                100,
                30,
            )
            .expect("first open should succeed");
        let second_open = service
            .open_target(
                &target,
                console("console-b", "observer-b", ConsoleLocation::ServerConsole),
                140,
                50,
            )
            .expect("second open should succeed");

        assert_eq!(second_open.len(), 2);
        assert!(!second_open.iter().any(|message| matches!(
            message.envelope.payload,
            ControlPlanePayload::OpenMirrorRequest(_)
        )));
    }

    #[test]
    fn closing_last_attachment_emits_close_mirror_request() {
        let mut service = RemoteControlPlaneService::new();
        let target = remote_target("peer-a", "shell-1");

        let first_open = service
            .open_target(
                &target,
                console("console-a", "observer-a", ConsoleLocation::LocalWorkspace),
                100,
                30,
            )
            .expect("first open should succeed");
        let attachment = match &first_open[0].envelope.payload {
            ControlPlanePayload::OpenTargetOk(payload) => payload.attachment_id.clone(),
            other => panic!("unexpected payload: {other:?}"),
        };

        let close_messages = service
            .close_target(&target, &attachment)
            .expect("closing the last attachment should succeed");

        assert_eq!(close_messages.len(), 1);
        assert!(matches!(
            close_messages[0].destination,
            ControlPlaneDestination::AuthorityNode(ref node) if node == "peer-a"
        ));
        match &close_messages[0].envelope.payload {
            ControlPlanePayload::CloseMirrorRequest(payload) => {
                assert_eq!(payload.session_id, "shell-1");
                assert_eq!(payload.target_id, "remote-peer:peer-a:shell-1");
            }
            other => panic!("unexpected payload: {other:?}"),
        }
    }

    #[test]
    fn target_output_is_fanned_out_to_all_opened_observers() {
        let mut service = RemoteControlPlaneService::new();
        let target = remote_target("peer-a", "shell-1");

        service
            .open_target(
                &target,
                console("console-a", "observer-a", ConsoleLocation::LocalWorkspace),
                100,
                30,
            )
            .expect("first open should succeed");
        service
            .open_target(
                &target,
                console("console-b", "observer-b", ConsoleLocation::ServerConsole),
                140,
                50,
            )
            .expect("second open should succeed");

        let routed = service
            .route_target_output(&target, 7, "pty", "YQ==")
            .expect("output should route");
        let deliveries = service
            .resolve_node_deliveries(&[routed])
            .expect("deliveries should resolve");

        assert_eq!(deliveries.len(), 2);
        assert_eq!(deliveries[0].node_id, "observer-a");
        assert_eq!(deliveries[1].node_id, "observer-b");
        match &deliveries[0].envelope.payload {
            ControlPlanePayload::TargetOutput(payload) => {
                assert_eq!(payload.output_seq, 7);
                assert_eq!(payload.stream, "pty");
            }
            other => panic!("unexpected payload: {other:?}"),
        }
    }

    fn console(
        console_id: &str,
        host_id: &str,
        location: ConsoleLocation,
    ) -> RemoteConsoleDescriptor {
        RemoteConsoleDescriptor {
            console_id: console_id.to_string(),
            console_host_id: host_id.to_string(),
            location,
        }
    }

    fn remote_target(authority_id: &str, session_id: &str) -> ManagedSessionRecord {
        ManagedSessionRecord {
            address: ManagedSessionAddress::remote_peer(authority_id, session_id),
            selector: None,
            availability: SessionAvailability::Online,
            workspace_dir: None,
            workspace_key: None,
            session_role: None,
            opened_by: Vec::new(),
            attached_clients: 0,
            window_count: 1,
            command_name: Some("bash".to_string()),
            current_path: None,
            task_state: ManagedSessionTaskState::Running,
        }
    }
}
