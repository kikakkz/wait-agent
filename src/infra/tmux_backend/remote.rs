use super::{
    exact_session_target, EmbeddedTmuxBackend, TmuxError, WAITAGENT_PANE_PIPE_OWNER_OPTION,
};
use crate::infra::tmux::{TmuxPaneId, TmuxSocketName};

const WAITAGENT_SIDEBAR_PANE_TITLE: &str = "waitagent-sidebar";
const WAITAGENT_FOOTER_PANE_TITLE: &str = "waitagent-footer";
const WAITAGENT_MAIN_PANE_OPTION: &str = "@waitagent_main_pane_id";
const WAITAGENT_PANE_TARGET_SESSION_OPTION: &str = "@waitagent_target_session_name";

impl EmbeddedTmuxBackend {
    pub(crate) fn target_presentation_pane_on_socket(
        &self,
        socket_name: &str,
        target_session_name: &str,
    ) -> Result<TmuxPaneId, TmuxError> {
        self.configured_target_presentation_pane_on_socket(socket_name, target_session_name)?
            .ok_or_else(|| {
                TmuxError::new(format!(
                    "target session `{target_session_name}` on socket `{socket_name}` has no authoritative presentation pane"
                ))
            })
    }

    fn configured_target_presentation_pane_on_socket(
        &self,
        socket_name: &str,
        target_session_name: &str,
    ) -> Result<Option<TmuxPaneId>, TmuxError> {
        if let Some(pane) =
            self.configured_session_presentation_pane_on_socket(socket_name, target_session_name)?
        {
            return Ok(Some(pane));
        }
        self.configured_pane_backed_presentation_pane_on_socket(socket_name, target_session_name)
    }

    fn configured_session_presentation_pane_on_socket(
        &self,
        socket_name: &str,
        target_session_name: &str,
    ) -> Result<Option<TmuxPaneId>, TmuxError> {
        let socket = TmuxSocketName::new(socket_name);
        let output = match self.run_on_socket(
            &socket,
            &[
                "show-options".to_string(),
                "-qv".to_string(),
                "-t".to_string(),
                exact_session_target(target_session_name),
                WAITAGENT_MAIN_PANE_OPTION.to_string(),
            ],
        ) {
            Ok(output) => output,
            Err(error) if error.is_command_failure() => return Ok(None),
            Err(error) => return Err(error),
        };
        let pane = output.stdout.trim();
        if pane.is_empty() {
            return Ok(None);
        }
        if !self.presentation_pane_is_live_on_socket(socket_name, pane)? {
            return Err(TmuxError::new(format!(
                "authoritative presentation pane `{pane}` for target session `{target_session_name}` on socket `{socket_name}` is not live"
            )));
        }
        let pane_target_session = self
            .pane_target_session_name_on_socket(socket_name, pane)?
            .ok_or_else(|| {
                TmuxError::new(format!(
                    "authoritative presentation pane `{pane}` on socket `{socket_name}` is missing `{WAITAGENT_PANE_TARGET_SESSION_OPTION}`"
                ))
            })?;
        if pane_target_session != target_session_name {
            return Err(TmuxError::new(format!(
                "authoritative presentation pane `{pane}` on socket `{socket_name}` belongs to target session `{pane_target_session}`, expected `{target_session_name}`"
            )));
        }
        Ok(Some(TmuxPaneId::new(pane)))
    }

    fn configured_pane_backed_presentation_pane_on_socket(
        &self,
        socket_name: &str,
        target_session_name: &str,
    ) -> Result<Option<TmuxPaneId>, TmuxError> {
        let socket = TmuxSocketName::new(socket_name);
        self.target_content_pane_for_session_instance_id(&socket, target_session_name)
    }

    fn pane_target_session_name_on_socket(
        &self,
        socket_name: &str,
        pane: &str,
    ) -> Result<Option<String>, TmuxError> {
        let socket = TmuxSocketName::new(socket_name);
        let output = match self.run_on_socket(
            &socket,
            &[
                "show-options".to_string(),
                "-pqv".to_string(),
                "-t".to_string(),
                pane.to_string(),
                WAITAGENT_PANE_TARGET_SESSION_OPTION.to_string(),
            ],
        ) {
            Ok(output) => output,
            Err(error) if error.is_command_failure() => return Ok(None),
            Err(error) => return Err(error),
        };
        let value = output.stdout.trim();
        if value.is_empty() {
            Ok(None)
        } else {
            Ok(Some(value.to_string()))
        }
    }

    fn presentation_pane_is_live_on_socket(
        &self,
        socket_name: &str,
        pane: &str,
    ) -> Result<bool, TmuxError> {
        let socket = TmuxSocketName::new(socket_name);
        let output = match self.run_on_socket(
            &socket,
            &[
                "display-message".to_string(),
                "-p".to_string(),
                "-t".to_string(),
                pane.to_string(),
                "#{pane_dead}	#{pane_title}".to_string(),
            ],
        ) {
            Ok(output) => output,
            Err(error) if error.is_command_failure() => return Ok(false),
            Err(error) => return Err(error),
        };
        let mut parts = output.stdout.trim_end().split('\t');
        let pane_dead = parts.next().unwrap_or_default();
        let pane_title = parts.next().unwrap_or_default();
        Ok(pane_dead == "0"
            && pane_title != WAITAGENT_SIDEBAR_PANE_TITLE
            && pane_title != WAITAGENT_FOOTER_PANE_TITLE)
    }

    pub(crate) fn target_main_pane_on_socket(
        &self,
        socket_name: &str,
        target_session_name: &str,
    ) -> Result<TmuxPaneId, TmuxError> {
        let panes =
            self.list_panes_on_target(&TmuxSocketName::new(socket_name), target_session_name)?;
        panes.into_iter()
            .find(|pane| {
                !pane.is_dead
                    && pane.title != WAITAGENT_SIDEBAR_PANE_TITLE
                    && pane.title != WAITAGENT_FOOTER_PANE_TITLE
            })
            .map(|pane| pane.pane_id)
            .ok_or_else(|| {
                TmuxError::new(format!(
                    "target session `{target_session_name}` on socket `{socket_name}` has no live main pane"
                ))
            })
    }

    pub(crate) fn pane_session_name_on_socket(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
    ) -> Result<String, TmuxError> {
        let args = vec![
            "display-message".to_string(),
            "-p".to_string(),
            "-t".to_string(),
            pane.as_str().to_string(),
            "#{session_name}".to_string(),
        ];
        let output = self.run_on_socket(&TmuxSocketName::new(socket_name), &args)?;
        Ok(output.stdout.trim().to_string())
    }

    #[allow(dead_code)]
    pub(crate) fn pane_tty_path_on_socket(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
    ) -> Result<String, TmuxError> {
        let args = vec![
            "display-message".to_string(),
            "-p".to_string(),
            "-t".to_string(),
            pane.as_str().to_string(),
            "#{pane_tty}".to_string(),
        ];
        let output = self.run_on_socket(&TmuxSocketName::new(socket_name), &args)?;
        Ok(output.stdout.trim().to_string())
    }

    pub(crate) fn resize_pane_on_socket(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
        cols: usize,
        rows: usize,
    ) -> Result<(), TmuxError> {
        let socket = TmuxSocketName::new(socket_name);
        let window = self.run_on_socket(&socket, &pane_window_id_args(pane))?;
        let window = window.stdout.trim();

        let pane_count = if window.is_empty() {
            1
        } else {
            let panes = self.run_on_socket(
                &socket,
                &[
                    "list-panes".to_string(),
                    "-t".to_string(),
                    window.to_string(),
                    "-F".to_string(),
                    "#{pane_id}".to_string(),
                ],
            )?;
            panes.stdout.lines().filter(|l| !l.is_empty()).count()
        };

        if pane_count <= 1 {
            // Dedicated target-host or hidden mirror window: resize both the
            // pane and its window so the mirror matches the remote console.
            self.run_on_socket(&socket, &resize_pane_args(pane, cols, rows))?;
            if !window.is_empty() {
                self.run_on_socket(&socket, &resize_window_args(window, cols, rows))?;
            }
        } else {
            // Workspace chrome window: do not resize here. Resizing the pane or
            // window would shrink the entire UI to match a remote viewer's
            // console size. The pane keeps the local main-slot geometry, and
            // remote-main-slot re-syncs its local size to the authority later.
        }
        Ok(())
    }

    pub(crate) fn clear_pane_pipe_on_socket_if_owner(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
        expected_owner: &str,
    ) -> Result<bool, TmuxError> {
        let socket = TmuxSocketName::new(socket_name);
        let owner =
            self.show_pane_option_on_socket(&socket, pane, WAITAGENT_PANE_PIPE_OWNER_OPTION)?;
        if owner.as_deref() != Some(expected_owner) {
            return Ok(false);
        }
        self.run_on_socket(&socket, &clear_pane_pipe_args(pane))?;
        self.unset_pane_option_on_socket(&socket, pane, WAITAGENT_PANE_PIPE_OWNER_OPTION)?;
        Ok(true)
    }

    pub(crate) fn pane_pipe_is_live_on_socket_for_owner(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
        expected_owner: &str,
    ) -> Result<bool, TmuxError> {
        let socket = TmuxSocketName::new(socket_name);
        let owner =
            self.show_pane_option_on_socket(&socket, pane, WAITAGENT_PANE_PIPE_OWNER_OPTION)?;
        if owner.as_deref() != Some(expected_owner) {
            return Ok(false);
        }
        let output = self.run_on_socket(
            &socket,
            &[
                "display-message".to_string(),
                "-p".to_string(),
                "-t".to_string(),
                pane.as_str().to_string(),
                "#{pane_pipe}".to_string(),
            ],
        )?;
        Ok(output.stdout.trim() == "1")
    }

    pub(crate) fn set_pane_pipe_on_socket_owned(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
        owner: &str,
        command: &str,
    ) -> Result<(), TmuxError> {
        let socket = TmuxSocketName::new(socket_name);
        self.run_on_socket(&socket, &clear_pane_pipe_args(pane))?;
        self.set_pane_option_on_socket(&socket, pane, WAITAGENT_PANE_PIPE_OWNER_OPTION, owner)?;
        self.run_on_socket(&socket, &set_pane_pipe_args(pane, command))?;
        Ok(())
    }

    pub(crate) fn set_pane_hook_on_socket(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
        hook_name: &str,
        command: &str,
    ) -> Result<(), TmuxError> {
        self.run_on_socket(
            &TmuxSocketName::new(socket_name),
            &set_pane_hook_args(pane, hook_name, command),
        )?;
        Ok(())
    }

    pub(crate) fn list_client_sizes_on_socket(
        &self,
        socket_name: &str,
        session_name: &str,
    ) -> Result<Vec<(u32, u32)>, TmuxError> {
        let args = vec![
            "list-clients".to_string(),
            "-t".to_string(),
            session_name.to_string(),
            "-F".to_string(),
            "#{client_width} #{client_height}".to_string(),
        ];
        let output = self.run_on_socket(&TmuxSocketName::new(socket_name), &args)?;
        Ok(output
            .stdout
            .lines()
            .filter_map(|line| {
                let mut parts = line.split(' ');
                // Control-mode clients report an empty client_height; tmux
                // falls back to 80x24 for size-less clients, mirror that.
                let w = parts
                    .next()
                    .and_then(|v| v.parse::<u32>().ok())
                    .unwrap_or(80);
                let h = parts
                    .next()
                    .and_then(|v| v.parse::<u32>().ok())
                    .unwrap_or(24);
                Some((w, h))
            })
            .collect())
    }

    pub(crate) fn window_layout_on_socket(
        &self,
        socket_name: &str,
        window_target: &str,
    ) -> Result<String, TmuxError> {
        let args = vec![
            "display-message".to_string(),
            "-p".to_string(),
            "-t".to_string(),
            window_target.to_string(),
            "#{window_layout}".to_string(),
        ];
        let output = self.run_on_socket(&TmuxSocketName::new(socket_name), &args)?;
        Ok(output.stdout.trim().to_string())
    }

    pub(crate) fn select_layout_on_socket(
        &self,
        socket_name: &str,
        window_target: &str,
        layout: &str,
    ) -> Result<(), TmuxError> {
        let args = vec![
            "select-layout".to_string(),
            "-t".to_string(),
            window_target.to_string(),
            layout.to_string(),
        ];
        self.run_on_socket(&TmuxSocketName::new(socket_name), &args)?;
        Ok(())
    }

    pub(crate) fn list_panes_detailed_on_socket(
        &self,
        socket_name: &str,
        window_target: &str,
    ) -> Result<Vec<(u32, String, u32, u32)>, TmuxError> {
        let args = vec![
            "list-panes".to_string(),
            "-t".to_string(),
            window_target.to_string(),
            "-F".to_string(),
            "#{pane_id}\t#{pane_title}\t#{pane_width}\t#{pane_height}".to_string(),
        ];
        let output = self.run_on_socket(&TmuxSocketName::new(socket_name), &args)?;
        Ok(output
            .stdout
            .lines()
            .filter_map(|line| {
                let mut parts = line.split('\t');
                let id = parts.next()?.trim_start_matches('%').parse::<u32>().ok()?;
                let title = parts.next().unwrap_or("").to_string();
                let w = parts.next()?.parse::<u32>().ok()?;
                let h = parts.next()?.parse::<u32>().ok()?;
                Some((id, title, w, h))
            })
            .collect())
    }

    pub(crate) fn split_padding_pane_on_socket(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
        title: &str,
    ) -> Result<TmuxPaneId, TmuxError> {
        let args = vec![
            "split-window".to_string(),
            "-d".to_string(),
            "-P".to_string(),
            "-F".to_string(),
            "#{pane_id}".to_string(),
            "-t".to_string(),
            pane.as_str().to_string(),
            "sleep 86400".to_string(),
        ];
        let output = self.run_on_socket(&TmuxSocketName::new(socket_name), &args)?;
        let new_pane = TmuxPaneId::new(output.stdout.trim().to_string());
        self.run_on_socket(
            &TmuxSocketName::new(socket_name),
            &[
                "select-pane".to_string(),
                "-t".to_string(),
                new_pane.as_str().to_string(),
                "-T".to_string(),
                title.to_string(),
            ],
        )?;
        Ok(new_pane)
    }

    pub(crate) fn kill_pane_on_socket(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
    ) -> Result<(), TmuxError> {
        self.run_on_socket(
            &TmuxSocketName::new(socket_name),
            &[
                "kill-pane".to_string(),
                "-t".to_string(),
                pane.as_str().to_string(),
            ],
        )?;
        Ok(())
    }

    pub(crate) fn window_has_waitagent_chrome_on_socket(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
    ) -> Result<bool, TmuxError> {
        let window_output = self.run_on_socket(
            &TmuxSocketName::new(socket_name),
            &pane_window_id_args(pane),
        )?;
        let window_id = window_output.stdout.trim().to_string();
        let panes = self.list_panes_detailed_on_socket(socket_name, &window_id)?;
        Ok(panes.iter().any(|(_, title, _, _)| {
            title == WAITAGENT_SIDEBAR_PANE_TITLE || title == WAITAGENT_FOOTER_PANE_TITLE
        }))
    }

    pub(crate) fn window_size_option_on_socket(
        &self,
        socket_name: &str,
        window_target: &str,
    ) -> Result<Option<String>, TmuxError> {
        let args = vec![
            "show-window-options".to_string(),
            "-v".to_string(),
            "-t".to_string(),
            window_target.to_string(),
            "window-size".to_string(),
        ];
        let output = self.run_on_socket(&TmuxSocketName::new(socket_name), &args)?;
        let value = output.stdout.trim().to_string();
        Ok((!value.is_empty()).then_some(value))
    }

    pub(crate) fn set_window_size_option_on_socket(
        &self,
        socket_name: &str,
        window_target: &str,
        value: Option<&str>,
    ) -> Result<(), TmuxError> {
        let mut args = vec!["set-window-option".to_string()];
        if value.is_none() {
            args.push("-u".to_string());
        }
        args.push("-t".to_string());
        args.push(window_target.to_string());
        args.push("window-size".to_string());
        if let Some(value) = value {
            args.push(value.to_string());
        }
        self.run_on_socket(&TmuxSocketName::new(socket_name), &args)?;
        Ok(())
    }

    pub(crate) fn set_session_hook_on_socket(
        &self,
        socket_name: &str,
        session_name: &str,
        hook_name: &str,
        command: &str,
    ) -> Result<(), TmuxError> {
        // Append (-a) so the coordination hook coexists with other hooks on
        // the same session (for example the layout reconcile client-resized
        // hook registered by the local layout service).
        self.run_on_socket(
            &TmuxSocketName::new(socket_name),
            &[
                "set-hook".to_string(),
                "-a".to_string(),
                "-t".to_string(),
                session_name.to_string(),
                hook_name.to_string(),
                command.to_string(),
            ],
        )?;
        Ok(())
    }

    /// Run one geometry coordination round for a mirrored pane and return the
    /// applied (read-back) geometry.  Implements the coordination contract
    /// from docs/remote-geometry-coordination-design.md: negotiated
    /// per-dimension minimum, chrome pinned, slack absorbed by blank padding
    /// panes, attached layouts applied atomically via select-layout.
    pub(crate) fn coordinate_geometry_on_socket(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
        cols: usize,
        rows: usize,
    ) -> Result<(usize, usize), TmuxError> {
        use crate::runtime::remote_authority::geometry_coordinator as geo;

        let socket = TmuxSocketName::new(socket_name);
        let window_output = self.run_on_socket(&socket, &pane_window_id_args(pane))?;
        let window_id = window_output.stdout.trim().to_string();
        // tmux resize-window flips the window's window-size option to manual
        // (cmd-resize-window.c); remember the previous value so it can be
        // restored after coordination instead of leaving the window stuck in
        // manual mode and unable to follow client resizes.
        let window_size_option = self.window_size_option_on_socket(socket_name, &window_id)?;
        let session = self.pane_session_name_on_socket(socket_name, pane)?;
        let clients = self.list_client_sizes_on_socket(socket_name, &session)?;
        let layout_string = self.window_layout_on_socket(socket_name, &window_id)?;
        let tree = geo::parse_layout(&layout_string)
            .map_err(|error| TmuxError::new(format!("parse window layout: {error}")))?;
        let panes: Vec<geo::PaneInfo> = self
            .list_panes_detailed_on_socket(socket_name, &window_id)?
            .into_iter()
            .map(|(id, title, w, h)| geo::PaneInfo { id, title, w, h })
            .collect();
        let target_id = pane
            .as_str()
            .trim_start_matches('%')
            .parse::<u32>()
            .map_err(|_| TmuxError::new(format!("unexpected pane id `{}`", pane.as_str())))?;
        let action = geo::plan_coordination(
            &tree,
            geo::root_size(&tree),
            &panes,
            target_id,
            (cols as u32, rows as u32),
            &clients,
        );
        match action {
            geo::CoordinationAction::NoOp => {}
            geo::CoordinationAction::ResizeWindowAndPane {
                window,
                pane: target,
                kill_panes,
            } => {
                for id in kill_panes {
                    let _ =
                        self.kill_pane_on_socket(socket_name, &TmuxPaneId::new(format!("%{id}")));
                }
                self.run_on_socket(
                    &socket,
                    &resize_window_args(&window_id, window.0 as usize, window.1 as usize),
                )?;
                self.run_on_socket(
                    &socket,
                    &resize_pane_args(pane, target.0 as usize, target.1 as usize),
                )?;
            }
            geo::CoordinationAction::ApplyLayout {
                mut layout,
                window_target_size,
                reuse_padding,
                kill_panes,
            } => {
                let needed = geo::count_padding_slots(&layout);
                let mut ids = reuse_padding;
                while ids.len() < needed {
                    let created = self.split_padding_pane_on_socket(
                        socket_name,
                        pane,
                        geo::PADDING_PANE_TITLE,
                    )?;
                    let created_id = created
                        .as_str()
                        .trim_start_matches('%')
                        .parse::<u32>()
                        .map_err(|_| {
                            TmuxError::new(format!("unexpected pane id `{}`", created.as_str()))
                        })?;
                    ids.push(created_id);
                }
                geo::assign_padding_ids(&mut layout, &ids);
                // Kill unused padding panes before applying the layout:
                // select-layout requires the string to reference exactly the
                // panes the window has, so stale pads must go first.
                for id in &kill_panes {
                    let _ =
                        self.kill_pane_on_socket(socket_name, &TmuxPaneId::new(format!("%{id}")));
                }
                if window_target_size != geo::root_size(&tree) {
                    self.run_on_socket(
                        &socket,
                        &resize_window_args(
                            &window_id,
                            window_target_size.0 as usize,
                            window_target_size.1 as usize,
                        ),
                    )?;
                }
                let planned = geo::dump_layout_with_checksum(&layout);
                self.select_layout_on_socket(socket_name, &window_id, &planned)?;
            }
            geo::CoordinationAction::ResizePaneOnly { pane: target } => {
                self.run_on_socket(
                    &socket,
                    &resize_pane_args(pane, target.0 as usize, target.1 as usize),
                )?;
            }
        }
        // Restore the window-size option that resize-window flipped to
        // manual, so the window keeps following client resizes afterwards.
        self.set_window_size_option_on_socket(
            socket_name,
            &window_id,
            window_size_option.as_deref(),
        )?;
        self.pane_dimensions_on_socket(socket_name, pane.as_str())
    }

    /// Coordinate geometry while keeping the window in `window-size manual`
    /// mode.  Used on the authority side: the coordinator must stay the only
    /// resizer of the target window, otherwise tmux's automatic snap/reflow
    /// on client attach would distort the live pane sizes that the planner
    /// reads (progressively collapsing the layout over successive rounds).
    pub(crate) fn coordinate_geometry_manual_on_socket(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
        cols: usize,
        rows: usize,
    ) -> Result<(usize, usize), TmuxError> {
        let applied = self.coordinate_geometry_on_socket(socket_name, pane, cols, rows)?;
        let window_output = self.run_on_socket(
            &TmuxSocketName::new(socket_name),
            &pane_window_id_args(pane),
        )?;
        let window_id = window_output.stdout.trim().to_string();
        self.set_window_size_option_on_socket(socket_name, &window_id, Some("manual"))?;
        Ok(applied)
    }

    pub(crate) fn unset_pane_hook_on_socket(
        &self,
        socket_name: &str,
        pane: &TmuxPaneId,
        hook_name: &str,
    ) -> Result<(), TmuxError> {
        self.run_on_socket(
            &TmuxSocketName::new(socket_name),
            &unset_pane_hook_args(pane, hook_name),
        )?;
        Ok(())
    }
}

fn resize_pane_args(pane: &TmuxPaneId, cols: usize, rows: usize) -> Vec<String> {
    vec![
        "resize-pane".to_string(),
        "-t".to_string(),
        pane.as_str().to_string(),
        "-x".to_string(),
        cols.to_string(),
        "-y".to_string(),
        rows.to_string(),
    ]
}

fn pane_window_id_args(pane: &TmuxPaneId) -> Vec<String> {
    vec![
        "display-message".to_string(),
        "-p".to_string(),
        "-t".to_string(),
        pane.as_str().to_string(),
        "#{window_id}".to_string(),
    ]
}

fn resize_window_args(window_id: &str, cols: usize, rows: usize) -> Vec<String> {
    vec![
        "resize-window".to_string(),
        "-t".to_string(),
        window_id.to_string(),
        "-x".to_string(),
        cols.to_string(),
        "-y".to_string(),
        rows.to_string(),
    ]
}

fn clear_pane_pipe_args(pane: &TmuxPaneId) -> Vec<String> {
    vec![
        "pipe-pane".to_string(),
        "-t".to_string(),
        pane.as_str().to_string(),
    ]
}

fn set_pane_pipe_args(pane: &TmuxPaneId, command: &str) -> Vec<String> {
    vec![
        "pipe-pane".to_string(),
        "-I".to_string(),
        "-O".to_string(),
        "-t".to_string(),
        pane.as_str().to_string(),
        command.to_string(),
    ]
}

fn set_pane_hook_args(pane: &TmuxPaneId, hook_name: &str, command: &str) -> Vec<String> {
    let target = pane.as_str();
    let session_target = target.split(':').next().unwrap_or(target);
    vec![
        "set-hook".to_string(),
        "-p".to_string(),
        "-t".to_string(),
        session_target.to_string(),
        hook_name.to_string(),
        command.to_string(),
    ]
}

fn unset_pane_hook_args(pane: &TmuxPaneId, hook_name: &str) -> Vec<String> {
    let target = pane.as_str();
    let session_target = target.split(':').next().unwrap_or(target);
    vec![
        "set-hook".to_string(),
        "-u".to_string(),
        "-p".to_string(),
        "-t".to_string(),
        session_target.to_string(),
        hook_name.to_string(),
    ]
}

#[allow(dead_code)]
fn set_session_environment_args(session_name: &str, key: &str, value: &str) -> Vec<String> {
    vec![
        "set-environment".to_string(),
        "-t".to_string(),
        session_name.to_string(),
        key.to_string(),
        value.to_string(),
    ]
}

#[allow(dead_code)]
fn unset_session_environment_args(session_name: &str, key: &str) -> Vec<String> {
    vec![
        "set-environment".to_string(),
        "-u".to_string(),
        "-t".to_string(),
        session_name.to_string(),
        key.to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::{
        clear_pane_pipe_args, pane_window_id_args, resize_pane_args, resize_window_args,
        set_pane_hook_args, set_pane_pipe_args, set_session_environment_args, unset_pane_hook_args,
        unset_session_environment_args,
    };
    use crate::infra::tmux::TmuxPaneId;

    #[test]
    fn remote_tmux_args_use_native_send_resize_and_pipe_primitives() {
        assert_eq!(
            resize_pane_args(&TmuxPaneId::new("%4"), 120, 40),
            vec!["resize-pane", "-t", "%4", "-x", "120", "-y", "40"]
        );
        assert_eq!(
            pane_window_id_args(&TmuxPaneId::new("%4")),
            vec!["display-message", "-p", "-t", "%4", "#{window_id}"]
        );
        assert_eq!(
            resize_window_args("@7", 120, 40),
            vec!["resize-window", "-t", "@7", "-x", "120", "-y", "40"]
        );
        assert_eq!(
            clear_pane_pipe_args(&TmuxPaneId::new("%4")),
            vec!["pipe-pane", "-t", "%4"]
        );
        assert_eq!(
            set_pane_pipe_args(&TmuxPaneId::new("%4"), "echo bridge"),
            vec!["pipe-pane", "-I", "-O", "-t", "%4", "echo bridge"]
        );
        assert_eq!(
            set_pane_hook_args(
                &TmuxPaneId::new("shell-1:0.0"),
                "pane-died",
                "run-shell true"
            ),
            vec![
                "set-hook",
                "-p",
                "-t",
                "shell-1",
                "pane-died",
                "run-shell true"
            ]
        );
        assert_eq!(
            unset_pane_hook_args(&TmuxPaneId::new("shell-1:0.0"), "pane-died"),
            vec!["set-hook", "-u", "-p", "-t", "shell-1", "pane-died"]
        );
        assert_eq!(
            set_session_environment_args("shell-1", "WAITAGENT_X", "value"),
            vec!["set-environment", "-t", "shell-1", "WAITAGENT_X", "value"]
        );
        assert_eq!(
            unset_session_environment_args("shell-1", "WAITAGENT_X"),
            vec!["set-environment", "-u", "-t", "shell-1", "WAITAGENT_X"]
        );
    }
}
