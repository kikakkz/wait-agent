//! Geometry coordination primitives for the remote authority target host.
//!
//! Implements the accepted contract from
//! `docs/remote-geometry-coordination-design.md`:
//!
//! - the mirrored pane and the viewer pane always share one geometry `T`
//!   (per-dimension min over viewer capacities)
//! - chrome panes (sidebar/footer, identified by title) keep fixed size and
//!   position; slack introduced by coordination is absorbed by blank padding
//!   panes, never by stretching chrome
//! - rebalanced layouts are applied atomically via a single `select-layout`
//!   call with a layout string computed from the current `#{window_layout}`

use crate::application::layout_service::{
    FOOTER_HEIGHT_CELLS, FOOTER_PANE_TITLE, SIDEBAR_PANE_TITLE, SIDEBAR_WIDTH_CELLS,
};

pub const PADDING_PANE_TITLE: &str = "waitagent-padding";

/// One pane's identity and live geometry as reported by tmux.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneInfo {
    pub id: u32,
    pub title: String,
    pub w: u32,
    pub h: u32,
}

/// tmux window layout tree (offsets are recomputed when dumping).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutCell {
    Pane {
        w: u32,
        h: u32,
        id: u32,
    },
    /// left-right split: children sit side by side, `h` equals the node `h`
    HSplit {
        w: u32,
        h: u32,
        children: Vec<LayoutCell>,
    },
    /// top-bottom split: children are stacked, `w` equals the node `w`
    VSplit {
        w: u32,
        h: u32,
        children: Vec<LayoutCell>,
    },
}

impl LayoutCell {
    fn width(&self) -> u32 {
        match self {
            Self::Pane { w, .. } | Self::HSplit { w, .. } | Self::VSplit { w, .. } => *w,
        }
    }

    fn height(&self) -> u32 {
        match self {
            Self::Pane { h, .. } | Self::HSplit { h, .. } | Self::VSplit { h, .. } => *h,
        }
    }

    fn leaf_ids(&self, out: &mut Vec<u32>) {
        match self {
            Self::Pane { id, .. } => out.push(*id),
            Self::HSplit { children, .. } | Self::VSplit { children, .. } => {
                for child in children {
                    child.leaf_ids(out);
                }
            }
        }
    }
}

/// Placeholder pane id used for padding slots before real ids are assigned.
pub const PADDING_SLOT: u32 = u32::MAX;

impl LayoutCell {
    fn substitute_padding_slots(&mut self, ids: &[u32], cursor: &mut usize) {
        match self {
            Self::Pane { id, .. } if *id == PADDING_SLOT => {
                if *cursor < ids.len() {
                    *id = ids[*cursor];
                    *cursor += 1;
                }
            }
            Self::Pane { .. } => {}
            Self::HSplit { children, .. } | Self::VSplit { children, .. } => {
                for child in children.iter_mut() {
                    child.substitute_padding_slots(ids, cursor);
                }
            }
        }
    }
}

fn parse_u32(input: &[u8], pos: &mut usize) -> Result<u32, String> {
    let start = *pos;
    while *pos < input.len() && input[*pos].is_ascii_digit() {
        *pos += 1;
    }
    if start == *pos {
        return Err("expected number in layout string".to_string());
    }
    std::str::from_utf8(&input[start..*pos])
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .ok_or_else(|| "invalid number in layout string".to_string())
}

fn expect(input: &[u8], pos: &mut usize, byte: u8) -> Result<(), String> {
    if *pos >= input.len() || input[*pos] != byte {
        return Err(format!(
            "expected `{}` at position {pos} in layout string",
            byte as char
        ));
    }
    *pos += 1;
    Ok(())
}

fn parse_cell(input: &[u8], pos: &mut usize) -> Result<LayoutCell, String> {
    let w = parse_u32(input, pos)?;
    expect(input, pos, b'x')?;
    let h = parse_u32(input, pos)?;
    expect(input, pos, b',')?;
    let _x = parse_u32(input, pos)?;
    expect(input, pos, b',')?;
    let _y = parse_u32(input, pos)?;
    if *pos >= input.len() {
        return Err("layout string truncated after cell geometry".to_string());
    }
    match input[*pos] {
        b',' => {
            *pos += 1;
            let id = parse_u32(input, pos)?;
            Ok(LayoutCell::Pane { w, h, id })
        }
        b'{' | b'[' => {
            let horizontal = input[*pos] == b'{';
            let closing = if horizontal { b'}' } else { b']' };
            *pos += 1;
            let mut children = Vec::new();
            loop {
                children.push(parse_cell(input, pos)?);
                if *pos >= input.len() {
                    return Err("layout string truncated inside split".to_string());
                }
                if input[*pos] == closing {
                    *pos += 1;
                    break;
                }
                expect(input, pos, b',')?;
            }
            if children.is_empty() {
                return Err("split without children in layout string".to_string());
            }
            if horizontal {
                Ok(LayoutCell::HSplit { w, h, children })
            } else {
                Ok(LayoutCell::VSplit { w, h, children })
            }
        }
        other => Err(format!(
            "unexpected byte `{}` in layout string",
            other as char
        )),
    }
}

/// Parse a tmux `#{window_layout}` value (with or without the checksum
/// prefix) into a layout tree.
pub fn parse_layout(input: &str) -> Result<LayoutCell, String> {
    let body = match input.split_once(',') {
        Some((checksum, body)) if checksum.len() == 4 && body.contains('x') => body,
        _ => input,
    };
    let bytes = body.as_bytes();
    let mut pos = 0;
    let cell = parse_cell(bytes, &mut pos)?;
    if pos != bytes.len() {
        return Err(format!("trailing bytes at position {pos} in layout string"));
    }
    Ok(cell)
}

fn dump_cell(cell: &LayoutCell, x: u32, y: u32, out: &mut String) {
    match cell {
        LayoutCell::Pane { w, h, id } => {
            out.push_str(&format!("{w}x{h},{x},{y},{id}"));
        }
        LayoutCell::HSplit { w, h, children } => {
            out.push_str(&format!("{w}x{h},{x},{y}{{"));
            let mut cx = x;
            for (index, child) in children.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                dump_cell(child, cx, y, out);
                cx = cx.saturating_add(child.width()).saturating_add(1);
            }
            out.push('}');
        }
        LayoutCell::VSplit { w, h, children } => {
            out.push_str(&format!("{w}x{h},{x},{y}["));
            let mut cy = y;
            for (index, child) in children.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                dump_cell(child, x, cy, out);
                cy = cy.saturating_add(child.height()).saturating_add(1);
            }
            out.push(']');
        }
    }
}

/// tmux layout checksum algorithm (see layout-custom.c in vendored tmux).
pub fn layout_checksum(body: &str) -> u16 {
    let mut csum: u16 = 0;
    for byte in body.bytes() {
        csum = (csum >> 1).wrapping_add((csum & 1) << 15);
        csum = csum.wrapping_add(byte as u16);
    }
    csum
}

/// Serialize a layout tree back into a checksummed `select-layout` string.
pub fn dump_layout_with_checksum(cell: &LayoutCell) -> String {
    let mut body = String::new();
    dump_cell(cell, 0, 0, &mut body);
    format!("{:04x},{body}", layout_checksum(&body))
}

/// Negotiated action for one coordination round.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoordinationAction {
    /// Nothing changed; only report the current geometry.
    NoOp,
    /// Local user detached: kill stale padding panes, resize the window to
    /// `window`, then resize the target pane to `pane`.
    ResizeWindowAndPane {
        window: (u32, u32),
        pane: (u32, u32),
        kill_panes: Vec<u32>,
    },
    /// Local user attached: apply a fully computed layout atomically.
    ApplyLayout {
        layout: LayoutCell,
        window_target_size: (u32, u32),
        /// existing padding pane ids to place into padding slots, in order
        reuse_padding: Vec<u32>,
        /// padding pane ids that are no longer needed
        kill_panes: Vec<u32>,
    },
    /// Exotic layout (user splits inside the target window): best-effort
    /// plain resize-pane without touching other panes.
    ResizePaneOnly { pane: (u32, u32) },
}

fn per_dimension_min(desired: (u32, u32), capacity: Option<(u32, u32)>) -> (u32, u32) {
    match capacity {
        Some((cw, ch)) => (desired.0.min(cw).max(2), desired.1.min(ch).max(2)),
        None => desired,
    }
}

fn chrome_titles(title: &str) -> Option<&'static str> {
    if title == SIDEBAR_PANE_TITLE {
        Some(SIDEBAR_PANE_TITLE)
    } else if title == FOOTER_PANE_TITLE {
        Some(FOOTER_PANE_TITLE)
    } else {
        None
    }
}

/// Compute the action for one coordination round.
///
/// - `window_size`: current size of the target pane's window
/// - `panes`: live panes of that window (ids, titles, sizes)
/// - `target_pane_id`: pane being mirrored
/// - `desired`: viewer-desired geometry for the target pane
/// - `client_sizes`: sizes of tmux clients attached to the target session;
///   empty means the local user is detached (unbounded capacity)
pub fn plan_coordination(
    layout: &LayoutCell,
    window_size: (u32, u32),
    panes: &[PaneInfo],
    target_pane_id: u32,
    desired: (u32, u32),
    client_sizes: &[(u32, u32)],
) -> CoordinationAction {
    let sidebar = panes.iter().find(|p| p.title == SIDEBAR_PANE_TITLE);
    let footer = panes.iter().find(|p| p.title == FOOTER_PANE_TITLE);
    let padding_ids: Vec<u32> = panes
        .iter()
        .filter(|p| p.title == PADDING_PANE_TITLE)
        .map(|p| p.id)
        .collect();
    let target = panes.iter().find(|p| p.id == target_pane_id);

    // Chrome overhead next to the target pane: the sidebar eats its intent
    // width plus a border, the footer eats its intent height plus a border.
    // WaitAgent chrome sizes come from the layout intent, never from the
    // live layout: after an external reflow the live sizes can be distorted,
    // and ratifying distorted values collapses the chrome over rounds.
    let chrome_w = sidebar.map(|_| SIDEBAR_WIDTH_CELLS as u32 + 1).unwrap_or(0);
    let chrome_h = footer.map(|_| FOOTER_HEIGHT_CELLS as u32 + 1).unwrap_or(0);

    let capacity = if client_sizes.is_empty() {
        None
    } else {
        Some(
            client_sizes
                .iter()
                .fold((u32::MAX, u32::MAX), |acc, &(cw, ch)| {
                    (
                        acc.0.min(cw.saturating_sub(chrome_w)),
                        acc.1.min(ch.saturating_sub(chrome_h)),
                    )
                }),
        )
    };
    let t = per_dimension_min(desired, capacity);

    let stale_padding = padding_ids.clone();
    if client_sizes.is_empty() {
        let current = target.map(|p| (p.w, p.h));
        let window_target = (t.0 + chrome_w, t.1 + chrome_h);
        if current == Some(t) && stale_padding.is_empty() && window_target == window_size {
            return CoordinationAction::NoOp;
        }
        return CoordinationAction::ResizeWindowAndPane {
            window: window_target,
            pane: t,
            kill_panes: stale_padding,
        };
    }

    // Attached: the window follows the smallest attached client so the local
    // user always sees the complete chrome layout (network-wide smallest
    // semantics; window-size may be manual per window, so we resize
    // explicitly rather than relying on tmux to snap).
    let window_target = client_sizes
        .iter()
        .fold((u32::MAX, u32::MAX), |acc, &(cw, ch)| {
            (acc.0.min(cw), acc.1.min(ch))
        });
    // Only the standard waitagent chrome shape is restructured; anything
    // else (user splits) falls back to a plain resize-pane.
    let mut leaf_ids = Vec::new();
    layout.leaf_ids(&mut leaf_ids);
    let known: std::collections::BTreeSet<u32> = [target_pane_id]
        .into_iter()
        .chain(sidebar.map(|p| p.id))
        .chain(footer.map(|p| p.id))
        .chain(padding_ids.iter().copied())
        .collect();
    let exotic = leaf_ids.iter().any(|id| !known.contains(id))
        || target.is_none()
        || chrome_titles(SIDEBAR_PANE_TITLE).is_none();
    if exotic {
        return CoordinationAction::ResizePaneOnly { pane: t };
    }

    // Standard shape: root VSplit[ HSplit{ target, [hpad], sidebar? }, footer? ]
    let row_h = window_target.1.saturating_sub(chrome_h);
    let sidebar_w = sidebar.map(|_| SIDEBAR_WIDTH_CELLS as u32).unwrap_or(0);
    let borders = 1 + sidebar.map(|_| 1).unwrap_or(0);
    let hpad_w = window_target
        .0
        .saturating_sub(t.0)
        .saturating_sub(sidebar_w)
        .saturating_sub(borders);
    let vpad_h = row_h.saturating_sub(t.1).saturating_sub(1);

    let need_hpad = hpad_w >= 1;
    let need_vpad = vpad_h >= 1;
    let needed = need_hpad as usize + need_vpad as usize;

    if !need_hpad && !need_vpad && window_size == window_target {
        // T fills the available area: the plain chrome layout with exact
        // sizes. If the live layout already matches (sizes and no padding),
        // there is nothing to do.
        let sizes_ok = target.map(|p| (p.w, p.h)) == Some(t)
            && padding_ids.is_empty()
            && sidebar.map(|p| (p.w, p.h)) == sidebar.map(|_| (SIDEBAR_WIDTH_CELLS as u32, row_h))
            && footer.map(|p| (p.w, p.h))
                == footer.map(|_| (window_target.0, FOOTER_HEIGHT_CELLS as u32));
        if sizes_ok {
            return CoordinationAction::NoOp;
        }
    }

    let mut reuse: Vec<u32> = padding_ids.clone();
    let target_branch = if need_vpad {
        reuse.truncate(needed.min(reuse.len()));
        LayoutCell::VSplit {
            w: t.0,
            h: row_h,
            children: vec![
                LayoutCell::Pane {
                    w: t.0,
                    h: t.1,
                    id: target_pane_id,
                },
                LayoutCell::Pane {
                    w: t.0,
                    h: vpad_h,
                    id: PADDING_SLOT,
                },
            ],
        }
    } else {
        LayoutCell::Pane {
            w: t.0,
            h: row_h,
            id: target_pane_id,
        }
    };
    let mut row_children = vec![target_branch];
    if need_hpad {
        row_children.push(LayoutCell::Pane {
            w: hpad_w,
            h: row_h,
            id: PADDING_SLOT,
        });
    }
    if let Some(sidebar) = sidebar {
        row_children.push(LayoutCell::Pane {
            w: SIDEBAR_WIDTH_CELLS as u32,
            h: row_h,
            id: sidebar.id,
        });
    }
    let row = LayoutCell::HSplit {
        w: window_target.0,
        h: row_h,
        children: row_children,
    };
    let root = if let Some(footer) = footer {
        LayoutCell::VSplit {
            w: window_target.0,
            h: window_target.1,
            children: vec![
                row,
                LayoutCell::Pane {
                    w: window_target.0,
                    h: FOOTER_HEIGHT_CELLS as u32,
                    id: footer.id,
                },
            ],
        }
    } else {
        row
    };

    let used = needed.min(reuse.len());
    let kill_panes = padding_ids[used.min(padding_ids.len())..].to_vec();
    CoordinationAction::ApplyLayout {
        layout: root,
        window_target_size: window_target,
        reuse_padding: reuse,
        kill_panes,
    }
}

/// Assign real pane ids to padding slots of a planned layout tree.
pub fn assign_padding_ids(layout: &mut LayoutCell, ids: &[u32]) {
    let mut cursor = 0;
    layout.substitute_padding_slots(ids, &mut cursor);
}

/// Size of the layout root cell.
pub fn root_size(cell: &LayoutCell) -> (u32, u32) {
    (cell.width(), cell.height())
}

/// Count unassigned padding slots in a planned layout tree.
pub fn count_padding_slots(cell: &LayoutCell) -> usize {
    let mut count = 0;
    let mut ids = Vec::new();
    cell.leaf_ids(&mut ids);
    for id in ids {
        if id == PADDING_SLOT {
            count += 1;
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHROME_LAYOUT: &str = "b6f3,80x24,0,0[80x23,0,0{47x22,0,0,3,32x22,48,0,1},80x1,0,23,2]";

    fn pane(id: u32, title: &str, w: u32, h: u32) -> PaneInfo {
        PaneInfo {
            id,
            title: title.to_string(),
            w,
            h,
        }
    }

    #[test]
    fn parse_round_trips_chrome_layout() {
        let cell = parse_layout(CHROME_LAYOUT).expect("layout should parse");
        let dumped = dump_layout_with_checksum(&cell);
        let reparsed = parse_layout(&dumped).expect("dumped layout should parse");
        assert_eq!(cell, reparsed);
    }

    #[test]
    fn checksum_matches_tmux_algorithm() {
        // Independently computed reference: checksum of the body of
        // CHROME_LAYOUT, evaluated with the algorithm from layout-custom.c.
        let body = "80x24,0,0[80x23,0,0{47x22,0,0,3,32x22,48,0,1},80x1,0,23,2]";
        let mut csum: u16 = 0;
        for byte in body.bytes() {
            csum = (csum >> 1).wrapping_add((csum & 1) << 15);
            csum = csum.wrapping_add(byte as u16);
        }
        assert_eq!(layout_checksum(body), csum);
        assert_eq!(layout_checksum(""), 0);
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(parse_layout("not a layout").is_err());
        assert!(parse_layout("80x24,0,0{47x22,0,0,3").is_err());
    }

    #[test]
    fn detached_resizes_window_and_pane() {
        let layout = parse_layout(CHROME_LAYOUT).expect("layout should parse");
        let panes = vec![
            pane(3, "bash", 47, 22),
            pane(1, SIDEBAR_PANE_TITLE, 32, 22),
            pane(2, FOOTER_PANE_TITLE, 80, 1),
        ];
        let action = plan_coordination(&layout, (80, 24), &panes, 3, (176, 48), &[]);
        assert_eq!(
            action,
            CoordinationAction::ResizeWindowAndPane {
                window: (209, 50),
                pane: (176, 48),
                kill_panes: vec![],
            }
        );
    }

    #[test]
    fn attached_smaller_capacity_is_noop_when_layout_already_matches() {
        let layout = parse_layout(CHROME_LAYOUT).expect("layout should parse");
        let panes = vec![
            pane(3, "bash", 47, 22),
            pane(1, SIDEBAR_PANE_TITLE, 32, 22),
            pane(2, FOOTER_PANE_TITLE, 80, 1),
        ];
        // kk attached at 80x24: capacity 47x22 < desired; T == current pane.
        let action = plan_coordination(&layout, (80, 24), &panes, 3, (176, 48), &[(80, 24)]);
        assert_eq!(action, CoordinationAction::NoOp);
    }

    #[test]
    fn attached_larger_terminal_rebalances_with_padding() {
        let layout = parse_layout(CHROME_LAYOUT).expect("layout should parse");
        let panes = vec![
            pane(3, "bash", 47, 22),
            pane(1, SIDEBAR_PANE_TITLE, 32, 22),
            pane(2, FOOTER_PANE_TITLE, 80, 1),
        ];
        // kk attached at 237x60: capacity 204x58; desired 176x48 -> T=176x48.
        let action = plan_coordination(&layout, (237, 60), &panes, 3, (176, 48), &[(237, 60)]);
        let CoordinationAction::ApplyLayout {
            layout,
            reuse_padding,
            kill_panes,
            ..
        } = action
        else {
            panic!("expected ApplyLayout, got {action:?}");
        };
        assert!(reuse_padding.is_empty());
        assert!(kill_panes.is_empty());
        // Window keeps kk's size; serialize and re-verify geometry.
        let mut layout = layout;
        assign_padding_ids(&mut layout, &[90, 91]);
        let dumped = dump_layout_with_checksum(&layout);
        let parsed = parse_layout(&dumped).expect("planned layout should parse");
        let mut ids = Vec::new();
        parsed.leaf_ids(&mut ids);
        assert!(ids.contains(&3));
        assert!(ids.contains(&1));
        assert!(ids.contains(&2));
        // Sidebar pinned at the right edge: window 237 wide, sidebar 32 ->
        // its x offset must be 237-32=205.  Verify via the dumped string.
        assert!(dumped.contains("32x58,205,0,1"), "dumped: {dumped}");
        // Footer pinned at the bottom row.
        assert!(dumped.contains("237x1,0,59,2"), "dumped: {dumped}");
        // Target pane is exactly T.
        assert!(dumped.contains("176x48,0,0,3"), "dumped: {dumped}");
        // Vertical padding below the target: row_h=58, 58-48-1=9 tall.
        assert!(dumped.contains("176x9,0,49,90"), "dumped: {dumped}");
        // Horizontal padding: 237-176-32-2=27 wide.
        assert!(dumped.contains("27x58,177,0,91"), "dumped: {dumped}");
    }

    #[test]
    fn attached_window_follows_smallest_client() {
        // window-size is manual: the window stayed at 200x49 after a detached
        // coordination round; kk then attaches at 80x24. The coordinator must
        // resize the window down to his size and lay out the standard chrome.
        let layout = parse_layout(CHROME_LAYOUT).expect("layout should parse");
        let panes = vec![
            pane(3, "bash", 47, 22),
            pane(1, SIDEBAR_PANE_TITLE, 32, 22),
            pane(2, FOOTER_PANE_TITLE, 80, 1),
        ];
        let action = plan_coordination(&layout, (200, 49), &panes, 3, (167, 47), &[(80, 24)]);
        let CoordinationAction::ApplyLayout {
            layout,
            window_target_size,
            reuse_padding,
            kill_panes,
        } = action
        else {
            panic!("expected ApplyLayout, got {action:?}");
        };
        assert_eq!(window_target_size, (80, 24));
        assert!(reuse_padding.is_empty());
        assert!(kill_panes.is_empty());
        let mut layout = layout;
        assign_padding_ids(&mut layout, &[]);
        let dumped = dump_layout_with_checksum(&layout);
        assert!(dumped.contains("47x22,0,0,3"), "dumped: {dumped}");
        assert!(dumped.contains("32x22,48,0,1"), "dumped: {dumped}");
        assert!(dumped.contains("80x1,0,23,2"), "dumped: {dumped}");
    }

    #[test]
    fn exotic_layout_falls_back_to_resize_pane_only() {
        let exotic = "aaaa,200x50,0,0{100x50,0,0{50x50,0,0,3,50x50,51,0,9},99x50,101,0,1}";
        let layout = parse_layout(exotic).expect("layout should parse");
        let panes = vec![
            pane(3, "bash", 50, 50),
            pane(9, "bash", 50, 50),
            pane(1, SIDEBAR_PANE_TITLE, 32, 22),
        ];
        let action = plan_coordination(&layout, (200, 50), &panes, 3, (176, 48), &[(237, 60)]);
        assert_eq!(
            action,
            CoordinationAction::ResizePaneOnly { pane: (176, 48) }
        );
    }

    #[test]
    fn capacity_uses_per_dimension_minimum() {
        let layout = parse_layout(CHROME_LAYOUT).expect("layout should parse");
        let panes = vec![
            pane(3, "bash", 47, 22),
            pane(1, SIDEBAR_PANE_TITLE, 32, 22),
            pane(2, FOOTER_PANE_TITLE, 80, 1),
        ];
        // Wide but short client: 237x30 -> capacity 204x28, T = 176x28.
        let action = plan_coordination(&layout, (237, 30), &panes, 3, (176, 48), &[(237, 30)]);
        let CoordinationAction::ApplyLayout { layout, .. } = action else {
            panic!("expected ApplyLayout, got {action:?}");
        };
        let mut layout = layout;
        assign_padding_ids(&mut layout, &[90]);
        let dumped = dump_layout_with_checksum(&layout);
        assert!(dumped.contains("176x28,0,0,3"), "dumped: {dumped}");
        // No horizontal padding needed: 237-176-32-2=27 -> there IS hpad.
        assert!(dumped.contains("27x28,177,0,90"), "dumped: {dumped}");
    }
}
