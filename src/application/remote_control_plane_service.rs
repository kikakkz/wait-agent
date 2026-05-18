use crate::domain::session_catalog::{ManagedSessionRecord, SessionAvailability, SessionTransport};
use crate::infra::remote_protocol::{
    ApplyResizePayload, BootstrapMode, ControlPlaneDestination, ControlPlanePayload,
    MirrorBootstrapChunkPayload, MirrorBootstrapCompletePayload, NodeBoundControlPlaneMessage,
    OpenMirrorRequestPayload, OpenTargetOkPayload, ProtocolEnvelope, RawPtyInputPayload,
    RawPtyOutputPayload, RemoteConsoleDescriptor, ResizeAuthorityChangedPayload,
    RoutedControlPlaneMessage, TargetOutputPayload, REMOTE_PROTOCOL_VERSION, SERVER_SENDER_ID,
};
use std::collections::{BTreeSet, HashMap};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_OUTPUT_LOG_ENTRIES: usize = 2000;

#[derive(Debug, Clone)]
pub(super) struct OutputLogEntry {
    pub(super) output_seq: u64,
    pub(super) stream: &'static str,
    pub(super) output_bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TerminalFlags {
    pub(super) alternate_screen_active: bool,
    pub(super) application_cursor_keys: bool,
    pub(super) cursor_visible: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MirrorRouteState {
    /// Mirror not yet requested for this session.
    None,
    /// OpenMirrorRequest has been sent; awaiting response.
    Pending,
    /// OpenMirrorAccepted received; mirror is active on the authority side.
    Active,
    /// OpenMirrorRejected received; mirror is not available.
    Rejected(String),
}

impl MirrorRouteState {
    #[allow(dead_code)]
    fn is_active(&self) -> bool {
        matches!(self, Self::Active)
    }

    fn should_send_mirror_request(&self) -> bool {
        matches!(self, Self::None)
    }

    fn is_pending(&self) -> bool {
        matches!(self, Self::Pending)
    }
}

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

    pub fn record_mirror_accepted(&mut self, session_id: &str) {
        if let Some(state) = self.session_states.get_mut(session_id) {
            if state.mirror_route == MirrorRouteState::Pending {
                state.mirror_route = MirrorRouteState::Active;
            }
        }
    }

    pub fn record_mirror_rejected(&mut self, session_id: &str, reason: String) {
        if let Some(state) = self.session_states.get_mut(session_id) {
            state.mirror_route = MirrorRouteState::Rejected(reason);
        }
    }

    pub fn handle_authority_disconnect(&mut self, authority_node_id: &str) {
        let mut sessions_to_remove = Vec::new();
        for (session_id, state) in &self.session_states {
            if !matches!(state.mirror_route, MirrorRouteState::None)
                && state.authority_node_id.as_deref() == Some(authority_node_id)
            {
                sessions_to_remove.push(session_id.clone());
            }
        }
        for session_id in sessions_to_remove {
            if let Some(state) = self.session_states.get_mut(&session_id) {
                state.mirror_route = MirrorRouteState::None;
                state.authority_node_id = None;
            }
        }
    }

    #[cfg(test)]
    pub fn open_target(
        &mut self,
        target: &ManagedSessionRecord,
        console: RemoteConsoleDescriptor,
        cols: usize,
        rows: usize,
    ) -> Result<Vec<RoutedControlPlaneMessage>, RemoteControlPlaneError> {
        self.open_target_with_raw_pty_mode(target, console, cols, rows, false)
    }

    pub fn open_target_with_raw_pty_mode(
        &mut self,
        target: &ManagedSessionRecord,
        console: RemoteConsoleDescriptor,
        cols: usize,
        rows: usize,
        raw_pty_passthrough: bool,
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
            .map(|state| {
                state.mirror_route.should_send_mirror_request() && state.attachments.len() == 1
            })
            .unwrap_or(false)
        {
            let has_log = self
                .session_states
                .get(&session_id)
                .map(|s| !s.output_log.is_empty())
                .unwrap_or(false);
            let bootstrap_mode = if has_log {
                BootstrapMode::VisibleOnly
            } else {
                BootstrapMode::Full
            };
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
                        session_id: session_id.clone(),
                        target_id: target_id.clone(),
                        console_id: console.console_id.clone(),
                        cols,
                        rows,
                        raw_pty_passthrough,
                        bootstrap_mode,
                    }),
                ),
            });
            if let Some(state) = self.session_states.get_mut(&session_id) {
                state.mirror_route = MirrorRouteState::Pending;
                state.authority_node_id = Some(target.address.authority_id().to_string());
            }
        }

        // Append output_log replay for the opening observer so content
        // appears immediately without waiting for the authority bootstrap.
        let has_history = self
            .session_states
            .get(&session_id)
            .map(|s| !s.output_log.is_empty())
            .unwrap_or(false);
        if has_history {
            let console_host_id = console.console_host_id.clone();
            messages.extend(self.build_output_replay(&session_id, &console_host_id));
        }

        Ok(messages)
    }

    pub fn route_raw_pty_input(
        &mut self,
        target: &ManagedSessionRecord,
        attachment_id: &str,
        console_seq: u64,
        input_bytes: Vec<u8>,
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
                ControlPlanePayload::RawPtyInput(RawPtyInputPayload {
                    attachment_id: attachment_id.to_string(),
                    session_id,
                    target_id,
                    console_id,
                    console_host_id,
                    input_seq,
                    input_bytes,
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
        output_bytes: Vec<u8>,
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

        if let Some(state) = self.session_states.get_mut(&session_id) {
            state.push_output_log_entry(OutputLogEntry {
                output_seq,
                stream,
                output_bytes: output_bytes.clone(),
            });
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
                    output_bytes,
                }),
            ),
        })
    }

    pub fn route_raw_pty_output(
        &mut self,
        target: &ManagedSessionRecord,
        output_seq: u64,
        output_bytes: Vec<u8>,
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
                ControlPlanePayload::RawPtyOutput(RawPtyOutputPayload {
                    session_id,
                    target_id: target.address.id().as_str().to_string(),
                    output_seq,
                    output_bytes,
                }),
            ),
        })
    }

    pub fn route_mirror_bootstrap_chunk(
        &mut self,
        target: &ManagedSessionRecord,
        chunk_seq: u64,
        stream: &'static str,
        output_bytes: Vec<u8>,
    ) -> Result<RoutedControlPlaneMessage, RemoteControlPlaneError> {
        validate_remote_target(target)?;
        let session_id = target.address.session_id().to_string();
        let target_id = target.address.id().as_str().to_string();
        if !self.session_states.contains_key(&session_id) {
            return Err(RemoteControlPlaneError::TargetNotOpened(target_id));
        }

        // Bootstrap chunk on an existing session replaces stale output_log
        if let Some(state) = self.session_states.get_mut(&session_id) {
            if !state.output_log.is_empty() {
                // This is a reconnect/reopen bootstrap that replaces prior history.
                // Clear the old output_log so the new snapshot becomes the base.
                state.output_log.clear();
                state.output_seq = 0;
            }
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
                    output_bytes,
                }),
            ),
        })
    }

    pub fn route_mirror_bootstrap_complete(
        &mut self,
        target: &ManagedSessionRecord,
        last_chunk_seq: u64,
        alternate_screen_active: bool,
        application_cursor_keys: bool,
        cursor_visible: bool,
    ) -> Result<RoutedControlPlaneMessage, RemoteControlPlaneError> {
        validate_remote_target(target)?;
        let session_id = target.address.session_id().to_string();
        let target_id = target.address.id().as_str().to_string();
        if !self.session_states.contains_key(&session_id) {
            return Err(RemoteControlPlaneError::TargetNotOpened(target_id));
        }

        if let Some(state) = self.session_states.get_mut(&session_id) {
            state.terminal_flags = Some(TerminalFlags {
                alternate_screen_active,
                application_cursor_keys,
                cursor_visible,
            });
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
                    alternate_screen_active,
                    application_cursor_keys,
                    cursor_visible,
                }),
            ),
        })
    }

    /// Build playback messages from stored output_log for a new observer.
    /// Returns TargetOutput entries followed by a synthetic MirrorBootstrapComplete
    /// so the observer's terminal engine reaches the stored terminal state.
    /// Build repeated messages from stored output_log for a new observer.
    /// Returns TargetOutput entries followed by a synthetic MirrorBootstrapComplete
    /// so the observer's terminal engine reaches the stored terminal state.
    pub fn build_output_replay(
        &self,
        session_id: &str,
        console_host_id: &str,
    ) -> Vec<RoutedControlPlaneMessage> {
        let state = match self.session_states.get(session_id) {
            Some(state) => state,
            None => return Vec::new(),
        };
        let mut messages = Vec::new();
        for entry in &state.output_log {
            messages.push(RoutedControlPlaneMessage {
                destination: ControlPlaneDestination::ObserverNode(console_host_id.to_string()),
                envelope: ProtocolEnvelope {
                    protocol_version: REMOTE_PROTOCOL_VERSION.to_string(),
                    message_id: String::new(),
                    message_type: "target_output",
                    timestamp: String::new(),
                    sender_id: SERVER_SENDER_ID.to_string(),
                    correlation_id: None,
                    session_id: Some(session_id.to_string()),
                    target_id: None,
                    attachment_id: None,
                    console_id: None,
                    payload: ControlPlanePayload::TargetOutput(TargetOutputPayload {
                        session_id: session_id.to_string(),
                        target_id: String::new(),
                        output_seq: entry.output_seq,
                        stream: entry.stream,
                        output_bytes: entry.output_bytes.clone(),
                    }),
                },
            });
        }
        if let Some(flags) = &state.terminal_flags {
            messages.push(RoutedControlPlaneMessage {
                destination: ControlPlaneDestination::ObserverNode(console_host_id.to_string()),
                envelope: ProtocolEnvelope {
                    protocol_version: REMOTE_PROTOCOL_VERSION.to_string(),
                    message_id: String::new(),
                    message_type: "mirror_bootstrap_complete",
                    timestamp: String::new(),
                    sender_id: SERVER_SENDER_ID.to_string(),
                    correlation_id: None,
                    session_id: Some(session_id.to_string()),
                    target_id: None,
                    attachment_id: None,
                    console_id: None,
                    payload: ControlPlanePayload::MirrorBootstrapComplete(
                        MirrorBootstrapCompletePayload {
                            session_id: session_id.to_string(),
                            target_id: String::new(),
                            last_chunk_seq: 1,
                            alternate_screen_active: flags.alternate_screen_active,
                            application_cursor_keys: flags.application_cursor_keys,
                            cursor_visible: flags.cursor_visible,
                        },
                    ),
                },
            });
        }
        messages
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
            } else if state.attachments.is_empty() && state.output_log.is_empty() {
                // No remaining output history either — safe to destroy state.
                CloseOutcome::ClosedLastAttachment { removed }
            } else if state.attachments.is_empty() {
                // No live observers, but output_log exists — keep state so
                // the next reopen can replay from history without needing a
                // full authority tmux capture.
                state.mirror_route = MirrorRouteState::None;
                state.authority_node_id = None;
                CloseOutcome::ClosedWithoutAuthorityChange { removed }
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

    /// Returns true if the session has a pending mirror request
    /// (OpenMirrorRequest was sent but not yet accepted/rejected).
    pub fn is_mirror_pending(&self, session_id: &str) -> bool {
        self.session_states
            .get(session_id)
            .map(|s| s.mirror_route.is_pending())
            .unwrap_or(false)
    }

    /// Resets mirror_route from Pending back to None so the next
    /// activation will retry OpenMirrorRequest.
    pub fn clear_mirror_pending(&mut self, session_id: &str) {
        if let Some(state) = self.session_states.get_mut(session_id) {
            if state.mirror_route.is_pending() {
                state.mirror_route = MirrorRouteState::None;
            }
        }
    }

    /// Returns true if the session's mirror_route is None
    /// (not yet requested, or was cleared). Used to detect
    /// whether OpenMirrorRequest needs to be (re-)sent when
    /// the authority transport becomes available.
    pub fn is_mirror_needed(&self, session_id: &str) -> bool {
        self.session_states
            .get(session_id)
            .map(|s| s.mirror_route.should_send_mirror_request())
            .unwrap_or(false)
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
    mirror_route: MirrorRouteState,
    authority_node_id: Option<String>,
    input_seq: u64,
    pty_resize_epoch: u64,
    last_open_ordinal: u64,
    last_pty_size: Option<(usize, usize)>,
    pty_resize_authority_attachment_id: Option<String>,
    attachments: Vec<RemoteAttachment>,
    output_log: Vec<OutputLogEntry>,
    output_seq: u64,
    terminal_flags: Option<TerminalFlags>,
}

impl RemoteSessionState {
    fn new() -> Self {
        Self {
            mirror_route: MirrorRouteState::None,
            authority_node_id: None,
            input_seq: 0,
            pty_resize_epoch: 0,
            last_open_ordinal: 0,
            last_pty_size: None,
            pty_resize_authority_attachment_id: None,
            attachments: Vec::new(),
            output_log: Vec::new(),
            output_seq: 0,
            terminal_flags: None,
        }
    }

    fn push_output_log_entry(&mut self, entry: OutputLogEntry) {
        self.output_seq = entry.output_seq;
        self.output_log.push(entry);
        while self.output_log.len() > MAX_OUTPUT_LOG_ENTRIES {
            self.output_log.remove(0);
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
mod remote_control_plane_service_test;
