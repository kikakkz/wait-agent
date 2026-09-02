use super::types::{MouseEncoding, MouseReportingMode};
use super::*;

#[test]
fn engine_tracks_plain_text_and_cursor_state() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 2,
        cols: 6,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"hello");
    let snapshot = engine.snapshot();

    assert_eq!(snapshot.lines[0], "hello ");
    assert_eq!(snapshot.cursor_row, 0);
    assert_eq!(snapshot.cursor_col, 5);
    assert!(snapshot.cursor_visible);
    assert!(!snapshot.alternate_screen);
}

#[test]
fn engine_snapshot_preserves_ansi_sgr_styling() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 2,
        cols: 16,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"\x1b[38;5;196mred\x1b[0m plain");
    let snapshot = engine.snapshot();

    assert_eq!(snapshot.lines[0], "red plain       ");
    assert!(
        snapshot.styled_lines[0].starts_with("\x1b[0;38;5;196mred\x1b[0m plain"),
        "styled line should preserve the foreground color: {:?}",
        snapshot.styled_lines[0]
    );
    assert_eq!(snapshot.active_style_ansi, "\x1b[0m");
}

#[test]
fn engine_snapshot_preserves_active_sgr_for_future_output() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 2,
        cols: 16,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"\x1b[38;5;196mred");
    let snapshot = engine.snapshot();

    assert_eq!(snapshot.lines[0], "red             ");
    assert!(
        snapshot.styled_lines[0].starts_with("\x1b[0;38;5;196mred"),
        "styled line should preserve the foreground color: {:?}",
        snapshot.styled_lines[0]
    );
    assert_eq!(snapshot.active_style_ansi, "\x1b[0;38;5;196m");
}

#[test]
fn engine_preserves_split_utf8_sequences() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 2,
        cols: 6,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(&[0xE4, 0xBD]);
    engine.feed(&[0xA0, b'a']);
    let snapshot = engine.snapshot();

    assert_eq!(snapshot.lines[0], "你a   ");
    assert_eq!(snapshot.cursor_col, 3);
}

#[test]
fn engine_tracks_wide_character_cursor_width() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 2,
        cols: 6,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed("你好".as_bytes());
    let snapshot = engine.snapshot();

    assert_eq!(snapshot.lines[0], "你好  ");
    assert_eq!(snapshot.cursor_col, 4);
}

#[test]
fn engine_handles_carriage_return_and_cursor_positioning() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 2,
        cols: 6,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"hello\rHE");
    engine.feed(b"\x1b[2;3H!");
    let snapshot = engine.snapshot();

    assert_eq!(snapshot.lines[0], "HEllo ");
    assert_eq!(snapshot.lines[1], "  !   ");
}

#[test]
fn engine_handles_clear_line_and_clear_screen() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 2,
        cols: 6,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"hello\x1b[2K");
    let snapshot = engine.snapshot();
    assert_eq!(snapshot.lines[0], "      ");

    engine.feed(b"\x1b[2J");
    let cleared = engine.snapshot();
    assert_eq!(
        cleared.lines,
        vec!["      ".to_string(), "      ".to_string()]
    );
    assert_eq!(cleared.cursor_row, 0);
    assert_eq!(cleared.cursor_col, 0);
}

#[test]
fn engine_handles_delete_character_csi() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 2,
        cols: 8,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"abcdef");
    engine.feed(b"\r\x1b[3C\x1b[1P");
    let snapshot = engine.snapshot();

    assert_eq!(snapshot.lines[0], "abcef   ");
    assert_eq!(snapshot.cursor_row, 0);
    assert_eq!(snapshot.cursor_col, 3);
}

#[test]
fn engine_replays_real_bash_reverse_search_backspace_sequence() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 2,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(
        b"\r(reverse-i-search)`': \x1b[K\x08\x08\x081': echo abcdef\x1b[3m1\x1b[23m2345\x08\x08\x08\x08\x08\x08\x08\x08\x08\x08\x08\x08\x08\x08\x08\x08\x08\x08\x08\
2': echo abcdef\x1b[3m12\x1b[23m345\x08\x08\x08\x08\x08\x08\x08\x08\x08\x08\x08\x08\x08\x08\x08\x08\x08\x08\x08\
3': echo abcdef\x1b[3m123\x1b[23m45\x08\x08\x08\x08\x08\x08\x08\x08\x08\x08\x08\x08\x08\x08\x08\x08\x08\x08\x08\x08\
\x1b[1P': echo abcdef\x1b[3m12\x1b[23m345\x08\x08\x08\x08\x08",
    );
    let snapshot = engine.snapshot();

    assert!(snapshot.lines[0].starts_with("(reverse-i-search)`12': echo abcdef12345"));
}

#[test]
fn engine_handles_split_csi_sequences_across_feed_calls() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 2,
        cols: 16,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"echo abc");
    engine.feed(b"\x08\x1b[");
    engine.feed(b"K");
    let snapshot = engine.snapshot();

    assert_eq!(snapshot.lines[0], "echo ab         ");
    assert_eq!(snapshot.cursor_col, 7);
}

#[test]
fn engine_does_not_scroll_immediately_after_filling_last_cell() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 3,
        cols: 4,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"\x1b[3;1HABCD");
    let snapshot = engine.snapshot();

    assert_eq!(snapshot.lines[0], "    ");
    assert_eq!(snapshot.lines[1], "    ");
    assert_eq!(snapshot.lines[2], "ABCD");
    assert_eq!(snapshot.cursor_row, 2);
    assert_eq!(snapshot.cursor_col, 3);

    engine.feed(b"Z");
    let wrapped = engine.snapshot();

    assert_eq!(wrapped.lines[0], "    ");
    assert_eq!(wrapped.lines[1], "ABCD");
    assert_eq!(wrapped.lines[2], "Z   ");
    assert_eq!(wrapped.cursor_row, 2);
    assert_eq!(wrapped.cursor_col, 1);
}

#[test]
fn engine_handles_save_and_restore_cursor_sequences() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 3,
        cols: 16,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"hello\x1b7\x1b[2;1Hrow2\x1b8 world");
    let snapshot = engine.snapshot();

    assert_eq!(snapshot.lines[0], "hello world     ");
    assert_eq!(snapshot.lines[1], "row2            ");
    assert_eq!(snapshot.cursor_row, 0);
    assert_eq!(snapshot.cursor_col, 11);
}

#[test]
fn engine_handles_csi_save_and_restore_cursor_sequences() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 3,
        cols: 16,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"abc\x1b[s\x1b[2;1Hxyz\x1b[u123");
    let snapshot = engine.snapshot();

    assert_eq!(snapshot.lines[0], "abc123          ");
    assert_eq!(snapshot.lines[1], "xyz             ");
    assert_eq!(snapshot.cursor_row, 0);
    assert_eq!(snapshot.cursor_col, 6);
}

#[test]
fn engine_restores_line_tails_after_csi_cursor_save_and_restore() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 5,
        cols: 24,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed("› explain co\x1b[s\x1b[2;1Hhelper\x1b[u".as_bytes());
    engine.feed("debase\x1b[3;1H不确定\x1b[s\x1b[4;1Hhelper\x1b[uXXXXX".as_bytes());
    let snapshot = engine.snapshot();

    assert_eq!(snapshot.lines[0], "› explain codebase      ");
    assert_eq!(snapshot.lines[2], "不确定XXXXX             ");
}

#[test]
fn engine_ignores_unknown_single_char_escape_sequences() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 2,
        cols: 8,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"a\x1b=b");
    let snapshot = engine.snapshot();

    assert_eq!(snapshot.lines[0], "ab      ");
    assert_eq!(snapshot.cursor_col, 2);
}

#[test]
fn engine_preserves_codex_placeholder_tail_across_reverse_index_scroll() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 20,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(
        b"\x1b[6;22H\x1b[0m\x1b[49m\x1b[K\
\x1b[7;2H\x1b[0m\x1b[49m\x1b[K\
\x1b[8;48H\x1b[0m\x1b[49m\x1b[K\
\x1b[6;1H\x1b[1m\xe2\x80\xba\
\x1b[6;3H\x1b[22m\x1b[2m\x1b[2mImplement {fe",
    );
    engine.feed(
        b"ature}\
\x1b[8;1H  gpt-5.4 high \xc2\xb7 /tmp\
\x1b[39m\x1b[49m\x1b[0m\x1b[?25h\
\x1b[6;3H\x1b[?2026l\x1b[?2026h\
\x1b[4;20r\x1b[4;1H\
\x1bM\x1bM\x1bM\x1bM\x1bM\x1bM\x1bM\x1bM\x1bM\
\x1b[r\x1b[1;12r\x1b[3;1H",
    );
    let snapshot = engine.snapshot();

    assert!(
        snapshot
            .lines
            .iter()
            .any(|line| line.contains("› Implement {feature}")),
        "expected full placeholder in snapshot, got: {:?}",
        snapshot.lines
    );
}

#[test]
fn engine_preserves_codex_placeholder_across_arbitrary_chunking() {
    let bytes = b"\x1b[6;22H\x1b[0m\x1b[49m\x1b[K\
\x1b[7;2H\x1b[0m\x1b[49m\x1b[K\
\x1b[8;48H\x1b[0m\x1b[49m\x1b[K\
\x1b[6;1H\x1b[1m\xe2\x80\xba\
\x1b[6;3H\x1b[22m\x1b[2m\x1b[2mImplement {feature}\
\x1b[8;1H  gpt-5.4 high \xc2\xb7 /tmp\
\x1b[39m\x1b[49m\x1b[0m\x1b[?25h\
\x1b[6;3H\x1b[?2026l\x1b[?2026h\
\x1b[4;20r\x1b[4;1H\
\x1bM\x1bM\x1bM\x1bM\x1bM\x1bM\x1bM\x1bM\x1bM\
\x1b[r\x1b[1;12r\x1b[3;1H";

    for chunk_size in 1..=bytes.len() {
        let mut engine = TerminalEngine::new(TerminalSize {
            rows: 20,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        });

        for chunk in bytes.chunks(chunk_size) {
            engine.feed(chunk);
        }

        let snapshot = engine.snapshot();
        assert!(
            snapshot
                .lines
                .iter()
                .any(|line| line.contains("› Implement {feature}")),
            "expected full placeholder with chunk size {chunk_size}, got: {:?}",
            snapshot.lines
        );
    }
}

#[test]
fn engine_preserves_codex_placeholder_across_three_chunk_splits_with_snapshots() {
    let bytes = b"\x1b[6;22H\x1b[0m\x1b[49m\x1b[K\
\x1b[7;2H\x1b[0m\x1b[49m\x1b[K\
\x1b[8;48H\x1b[0m\x1b[49m\x1b[K\
\x1b[6;1H\x1b[1m\xe2\x80\xba\
\x1b[6;3H\x1b[22m\x1b[2m\x1b[2mImplement {feature}\
\x1b[8;1H  gpt-5.4 high \xc2\xb7 /tmp\
\x1b[39m\x1b[49m\x1b[0m\x1b[?25h\
\x1b[6;3H\x1b[?2026l\x1b[?2026h\
\x1b[4;20r\x1b[4;1H\
\x1bM\x1bM\x1bM\x1bM\x1bM\x1bM\x1bM\x1bM\x1bM\
\x1b[r\x1b[1;12r\x1b[3;1H";

    for first_split in 1..bytes.len() {
        for second_split in first_split + 1..bytes.len() {
            let mut engine = TerminalEngine::new(TerminalSize {
                rows: 20,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            });

            engine.feed(&bytes[..first_split]);
            let _ = engine.state();
            engine.feed(&bytes[first_split..second_split]);
            let _ = engine.state();
            engine.feed(&bytes[second_split..]);
            let snapshot = engine.snapshot();

            assert!(
                snapshot
                    .lines
                    .iter()
                    .any(|line| line.contains("› Implement {feature}")),
                "expected full placeholder with splits {first_split}/{second_split}, got: {:?}",
                snapshot.lines
            );
        }
    }
}

#[test]
fn engine_replays_codex_update_menu_down_redraw_from_live_capture() {
    let bootstrap_screen = concat!(
        "\n",
        "  ✨\u{200a}Update available! \x1b[2m0.125.0 -> 0.128.0\x1b[0m      \n",
        "\n",
        "  \x1b[2mRelease notes: \x1b[4mhttps://github.com/openai/code\n",
        "\n",
        "\x1b[0m› 1. Update now (runs `npm install -g          \n",
        "     @openai/codex`)   \n",
        "  2. Skip  \n",
        "  3. Skip until next version                  \n",
        "\n",
        "  \x1b[2mPress enter to continue\x1b[0m                    \n",
        "\n\n\n\n\n\n\n\n\n\n",
    );
    let redraw = b"\x1b[?2026h\x1b[1;2H\x1b[0m\x1b[m\x1b[K\x1b[2;42H\x1b[0m\x1b[m\x1b[K\x1b[3;2H\x1b[0m\x1b[m\x1b[K\x1b[5;2H\x1b[0m\x1b[m\x1b[K\x1b[6;38H\x1b[0m\x1b[m\x1b[K\x1b[7;21H\x1b[0m\x1b[m\x1b[K\x1b[8;10H\x1b[0m\x1b[m\x1b[K\x1b[9;29H\x1b[0m\x1b[m\x1b[K\x1b[10;2H\x1b[0m\x1b[m\x1b[K\x1b[11;26H\x1b[0m\x1b[m\x1b[K\x1b[12;2H\x1b[0m\x1b[m\x1b[K\x1b[13;2H\x1b[0m\x1b[m\x1b[K\x1b[14;2H\x1b[0m\x1b[m\x1b[K\x1b[15;2H\x1b[0m\x1b[m\x1b[K\x1b[16;2H\x1b[0m\x1b[m\x1b[K\x1b[17;2H\x1b[0m\x1b[m\x1b[K\x1b[18;2H\x1b[0m\x1b[m\x1b[K\x1b[19;2H\x1b[0m\x1b[m\x1b[K\x1b[20;2H\x1b[0m\x1b[m\x1b[K\x1b[21;2H\x1b[0m\x1b[m\x1b[K\x1b[6;1H  1. Update now (runs `npm install -g\x1b[7;6H@openai/codex`)\x1b[8;1H\x1b[;m\xe2\x80\xba 2. Skip\x1b[m\x1b[m\x1b[0m\x1b[?25l\x1b[?2026l";
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 21,
        cols: 47,
        pixel_width: 0,
        pixel_height: 0,
    });
    let mut bootstrap = String::from("\x1b[2J\x1b[H");
    for (index, line) in bootstrap_screen.lines().enumerate() {
        bootstrap.push_str(&format!("\x1b[{};1H{}", index + 1, line));
    }
    bootstrap.push_str("\x1b[11;26H");

    engine.feed(bootstrap.as_bytes());
    engine.feed(redraw);
    let snapshot = engine.snapshot();

    assert_eq!(
        snapshot.lines[0],
        "                                               "
    );
    assert!(
        snapshot.lines[1].starts_with("  ✨ Update available! 0.125.0 -> 0.128.0"),
        "unexpected line 2: {:?}",
        snapshot.lines[1]
    );
    assert_eq!(
        snapshot.lines[5],
        "  1. Update now (runs `npm install -g          "
    );
    assert_eq!(
        snapshot.lines[6],
        "     @openai/codex`)                           "
    );
    assert_eq!(
        snapshot.lines[7],
        "› 2. Skip                                      "
    );
}

#[test]
fn engine_buffers_synchronized_update_until_closed() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 5,
        cols: 20,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"initial");
    let before = engine.snapshot();
    assert_eq!(before.lines[0].trim_end(), "initial");

    // Start a synchronized update and perform a partial redraw.
    engine.feed(b"\x1b[?2026h\x1b[2K\x1b[Hupdated");
    let during = engine.snapshot();
    assert_eq!(
        during.lines[0].trim_end(),
        "initial",
        "snapshot should not change while synchronized update is open"
    );

    engine.feed(b"\x1b[?2026l");
    let after = engine.snapshot();
    assert_eq!(after.lines[0].trim_end(), "updated");
}

#[test]
fn engine_handles_split_synchronized_update() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 5,
        cols: 20,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"base");

    engine.feed(b"\x1b[?2026h\x1b[2K");
    assert_eq!(engine.snapshot().lines[0].trim_end(), "base");

    engine.feed(b"\x1b[Hredraw");
    assert_eq!(engine.snapshot().lines[0].trim_end(), "base");

    engine.feed(b"\x1b[?2026l");
    assert_eq!(engine.snapshot().lines[0].trim_end(), "redraw");
}

#[test]
fn engine_preserves_replies_across_synchronized_update() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 5,
        cols: 20,
        pixel_width: 0,
        pixel_height: 0,
    });

    let replies = engine.feed_and_collect_replies(b"\x1b[?2026h\x1b[6n\x1b[?2026l");
    assert!(
        replies.starts_with(b"\x1b["),
        "cursor-position reply should be generated after sync closes: {replies:?}"
    );
}

#[test]
fn engine_applies_content_before_synchronized_update() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 5,
        cols: 20,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"before\x1b[?2026h\x1b[2K\x1b[Hafter\x1b[?2026l");
    let snapshot = engine.snapshot();
    assert_eq!(snapshot.lines[0].trim_end(), "after");
}

#[test]
fn engine_handles_scroll_region_and_scroll_up() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 5,
        cols: 8,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"row1\r\nrow2\r\nrow3\r\nrow4\r\nrow5\x1b[1;3r\x1b[2S\x1b[r");
    let snapshot = engine.snapshot();

    assert_eq!(snapshot.lines[0], "row3    ");
    assert_eq!(snapshot.lines[1], "        ");
    assert_eq!(snapshot.lines[2], "        ");
    assert_eq!(snapshot.lines[3], "row4    ");
    assert_eq!(snapshot.lines[4], "row5    ");
}

#[test]
fn engine_line_feed_respects_scroll_region() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 4,
        cols: 8,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"top\r\nmid\r\nbot");
    engine.feed(b"\x1b[2;3r\x1b[3;1H!\n");
    let snapshot = engine.snapshot();

    assert_eq!(snapshot.lines[0], "top     ");
    assert_eq!(snapshot.lines[1], "!ot     ");
    assert_eq!(snapshot.lines[2], "        ");
}

#[test]
fn engine_reverse_index_respects_scroll_region() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 4,
        cols: 8,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"row1\r\nrow2\r\nrow3\r\nrow4");
    engine.feed(b"\x1b[2;4r\x1b[2;1H\x1bM");
    let snapshot = engine.snapshot();

    assert_eq!(snapshot.lines[0], "row1    ");
    assert_eq!(snapshot.lines[1], "        ");
    assert_eq!(snapshot.lines[2], "row2    ");
    assert_eq!(snapshot.lines[3], "row3    ");
    assert_eq!(snapshot.cursor_row, 1);
    assert_eq!(snapshot.cursor_col, 0);
}

#[test]
fn engine_ignores_bell_without_advancing_cursor() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 2,
        cols: 8,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"abc\x07\x07");
    let snapshot = engine.snapshot();

    assert_eq!(snapshot.lines[0], "abc     ");
    assert_eq!(snapshot.cursor_col, 3);
}

#[test]
fn engine_replies_to_terminal_capability_queries() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 2,
        cols: 20,
        pixel_width: 0,
        pixel_height: 0,
    });

    let replies = engine.feed_and_collect_replies(b"\x1b[6n\x1b[c\x1b[?u\x1b]10;?\x1b\\");

    let reply_text = String::from_utf8_lossy(&replies);
    assert!(reply_text.contains("\x1b[1;1R"));
    assert!(reply_text.contains("\x1b[?61;1;21;22c"));
    assert!(!reply_text.contains("\x1b[?0u"));
    assert!(reply_text.contains("\x1b]10;rgb:ffff/ffff/ffff\x1b\\"));
}

#[test]
fn engine_tracks_application_cursor_mode() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 2,
        cols: 20,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"\x1b[?1h");
    assert!(engine.application_cursor_keys());

    engine.feed(b"\x1b[?1l");
    assert!(!engine.application_cursor_keys());
}

#[test]
fn engine_tracks_cursor_visibility() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 2,
        cols: 20,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"\x1b[?25l");
    assert!(!engine.snapshot().cursor_visible);

    engine.feed(b"\x1b[?25h");
    assert!(engine.snapshot().cursor_visible);
}

#[test]
fn engine_tracks_scrollback_when_screen_overflows() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 2,
        cols: 5,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"one\r\ntwo\r\nthree");
    let snapshot = engine.snapshot();

    assert_eq!(snapshot.styled_scrollback, vec!["one  ".to_string()]);
    assert_eq!(snapshot.lines[0], "two  ");
    assert_eq!(snapshot.lines[1], "three");
}

#[test]
fn engine_tracks_styled_scrollback_when_screen_overflows() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 2,
        cols: 5,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"\x1b[31mred\r\nplain\r\nnext");
    let snapshot = engine.snapshot();

    assert!(
        snapshot.styled_scrollback[0].starts_with("\x1b[0;38;5;1mred"),
        "expected styled scrollback to retain foreground color, got {:?}",
        snapshot.styled_scrollback
    );
}

#[test]
fn engine_preserves_normal_and_alternate_screens() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 2,
        cols: 6,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"main");
    engine.feed(b"\x1b[?1049h");
    engine.feed(b"alt");
    let alternate = engine.snapshot();
    assert!(alternate.alternate_screen);
    assert_eq!(alternate.lines[0], "alt   ");

    engine.feed(b"\x1b[?1049l");
    let normal = engine.snapshot();
    assert!(!normal.alternate_screen);
    assert_eq!(normal.lines[0], "main  ");
}

#[test]
fn engine_ignores_osc_window_title_sequences() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 2,
        cols: 20,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"\x1b]0;k@k: /tmp\x07prompt$ ");
    let snapshot = engine.snapshot();

    assert_eq!(snapshot.lines[0], "prompt$             ");
    assert_eq!(snapshot.window_title.as_deref(), Some("k@k: /tmp"));
}

#[test]
fn engine_ignores_osc_sequences_terminated_by_st() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 2,
        cols: 20,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"\x1b]0;session title\x1b\\ready");
    let snapshot = engine.snapshot();

    assert_eq!(snapshot.lines[0], "ready               ");
    assert_eq!(snapshot.window_title.as_deref(), Some("session title"));
}

#[test]
fn engine_handles_split_osc_window_title_sequences() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 2,
        cols: 20,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"\x1b]0;k@k: /tm");
    engine.feed(b"p\x07prompt$ ");
    let snapshot = engine.snapshot();

    assert_eq!(snapshot.lines[0], "prompt$             ");
    assert_eq!(snapshot.window_title.as_deref(), Some("k@k: /tmp"));
}

#[test]
fn engine_resize_preserves_visible_prefix() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 2,
        cols: 6,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"hello\r\nworld");
    engine.resize(TerminalSize {
        rows: 3,
        cols: 4,
        pixel_width: 0,
        pixel_height: 0,
    });
    let snapshot = engine.snapshot();

    assert_eq!(snapshot.lines[0], "hell");
    assert_eq!(snapshot.lines[1], "worl");
    assert_eq!(snapshot.size.cols, 4);
    assert_eq!(snapshot.size.rows, 3);
}

#[test]
fn engine_resize_resets_scroll_region_to_full_screen() {
    // Regression: growing from the default 24 rows to 50 must reset the
    // DECSTBM scrolling region, otherwise output stays confined to the old
    // 24-row region and only the top half of the main pane is used.
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.resize(TerminalSize {
        rows: 50,
        cols: 176,
        pixel_width: 0,
        pixel_height: 0,
    });
    let snapshot = engine.snapshot();

    assert_eq!(snapshot.scroll_top, 0);
    assert_eq!(snapshot.scroll_bottom, 49);
    assert_eq!(snapshot.size.rows, 50);
}

#[test]
fn engine_translates_dec_graphics_charset_line_drawing() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 2,
        cols: 8,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"\x1b(0lqk\x1b(Bx");
    let snapshot = engine.snapshot();

    assert_eq!(snapshot.lines[0], "┌─┐x    ");
    assert_eq!(snapshot.cursor_col, 4);
}

#[test]
fn engine_translates_full_dec_special_graphics_map() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 2,
        cols: 32,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"\x1b(0jklmnqtuvwx");
    let snapshot = engine.snapshot();
    assert_eq!(snapshot.lines[0].trim_end(), "┘┐┌└┼─├┤┴┬│");

    engine.feed(b"\rafgoprsyz{|}~_`");
    let snapshot = engine.snapshot();
    assert_eq!(snapshot.lines[0].trim_end(), "▒°±⎺⎻⎼⎽≤≥π≠£·\u{a0}◆");
}

#[test]
fn engine_dec_graphics_control_pictures_render_as_spaces() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 2,
        cols: 32,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"\x1b(0bcdehi");
    let snapshot = engine.snapshot();

    assert_eq!(snapshot.lines[0], " ".repeat(32));
}

#[test]
fn engine_shifts_charsets_with_si_and_so() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 2,
        cols: 8,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"\x1b)0q\x0eq\x0fq");
    let snapshot = engine.snapshot();

    assert_eq!(snapshot.lines[0], "q─q     ");
}

#[test]
fn engine_save_restore_cursor_preserves_charset() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 2,
        cols: 8,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"\x1b(0\x1b7\x1b(B\x1b8q");
    let snapshot = engine.snapshot();

    assert_eq!(snapshot.lines[0], "─       ");
}

#[test]
fn engine_insert_lines_preserves_content_outside_scroll_region() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 5,
        cols: 8,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"row1\r\nrow2\r\nrow3\r\nrow4\r\nrow5");
    engine.feed(b"\x1b[2;4r\x1b[2;1H\x1b[L");
    let snapshot = engine.snapshot();

    assert_eq!(snapshot.lines[0], "row1    ");
    assert_eq!(snapshot.lines[1], "        ");
    assert_eq!(snapshot.lines[2], "row2    ");
    assert_eq!(snapshot.lines[3], "row3    ");
    assert_eq!(snapshot.lines[4], "row5    ");
    assert_eq!(snapshot.cursor_row, 1);
    assert_eq!(snapshot.cursor_col, 0);
}

#[test]
fn engine_delete_lines_pulls_up_rows_inside_scroll_region() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 5,
        cols: 8,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"row1\r\nrow2\r\nrow3\r\nrow4\r\nrow5");
    engine.feed(b"\x1b[2;4r\x1b[2;1H\x1b[2M");
    let snapshot = engine.snapshot();

    assert_eq!(snapshot.lines[0], "row1    ");
    assert_eq!(snapshot.lines[1], "row4    ");
    assert_eq!(snapshot.lines[2], "        ");
    assert_eq!(snapshot.lines[3], "        ");
    assert_eq!(snapshot.lines[4], "row5    ");
}

#[test]
fn engine_handles_insert_characters() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 2,
        cols: 8,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"abcdef\r\x1b[2C\x1b[2@");
    let snapshot = engine.snapshot();

    assert_eq!(snapshot.lines[0], "ab  cdef");
    assert_eq!(snapshot.cursor_col, 2);
}

#[test]
fn engine_handles_erase_characters() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 2,
        cols: 8,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"abcdef\r\x1b[2C\x1b[2X");
    let snapshot = engine.snapshot();

    assert_eq!(snapshot.lines[0], "ab  ef  ");
    assert_eq!(snapshot.cursor_col, 2);
}

#[test]
fn engine_repeats_last_printed_character() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 2,
        cols: 8,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"x\x1b[5b");
    let snapshot = engine.snapshot();

    assert_eq!(snapshot.lines[0], "xxxxxx  ");
    assert_eq!(snapshot.cursor_col, 6);
}

#[test]
fn engine_repeats_wide_character_respecting_width() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 2,
        cols: 8,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed("你\x1b[2b".as_bytes());
    let snapshot = engine.snapshot();

    assert_eq!(snapshot.lines[0], "你你你  ");
    assert_eq!(snapshot.cursor_col, 6);
}

#[test]
fn engine_handles_cha_and_hpa_cursor_column() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 2,
        cols: 8,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"abcdef\r\x1b[3G!");
    let snapshot = engine.snapshot();
    assert_eq!(snapshot.lines[0], "ab!def  ");
    assert_eq!(snapshot.cursor_col, 3);

    engine.feed(b"\x1b[6`@");
    let snapshot = engine.snapshot();
    assert_eq!(snapshot.lines[0], "ab!de@  ");
    assert_eq!(snapshot.cursor_col, 6);
}

#[test]
fn engine_handles_vpa_cnl_cpl_cursor_positioning() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 4,
        cols: 8,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"a\r\nb");
    engine.feed(b"\x1b[1d");
    let snapshot = engine.snapshot();
    assert_eq!(snapshot.cursor_row, 0);
    assert_eq!(snapshot.cursor_col, 1);

    engine.feed(b"\x1b[2E");
    let snapshot = engine.snapshot();
    assert_eq!(snapshot.cursor_row, 2);
    assert_eq!(snapshot.cursor_col, 0);

    engine.feed(b"\x1b[1F");
    let snapshot = engine.snapshot();
    assert_eq!(snapshot.cursor_row, 1);
    assert_eq!(snapshot.cursor_col, 0);
}

#[test]
fn engine_sgr_colon_underline_does_not_set_italic() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 2,
        cols: 8,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"\x1b[4:3mX");
    let snapshot = engine.snapshot();

    assert_eq!(snapshot.active_style_ansi, "\x1b[0;4m");
    assert!(
        snapshot.styled_lines[0].starts_with("\x1b[0;4mX"),
        "styled line should use underline only: {:?}",
        snapshot.styled_lines[0]
    );
}

#[test]
fn engine_sgr_colon_underline_variants() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 2,
        cols: 8,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"\x1b[4:1m");
    assert_eq!(engine.snapshot().active_style_ansi, "\x1b[0;4m");

    engine.feed(b"\x1b[4:0m");
    assert_eq!(engine.snapshot().active_style_ansi, "\x1b[0m");
}

#[test]
fn engine_sgr_colon_truecolor_and_indexed_colors() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 2,
        cols: 8,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"\x1b[38:2:10:20:30m");
    assert_eq!(engine.snapshot().active_style_ansi, "\x1b[0;38;2;10;20;30m");

    engine.feed(b"\x1b[0m\x1b[48:2:0:10:20:30m");
    assert_eq!(engine.snapshot().active_style_ansi, "\x1b[0;48;2;10;20;30m");

    engine.feed(b"\x1b[0m\x1b[38:5:196m");
    assert_eq!(engine.snapshot().active_style_ansi, "\x1b[0;38;5;196m");
}

#[test]
fn engine_sgr_semicolon_truecolor_still_works() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 2,
        cols: 8,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"\x1b[38;2;10;20;30m");
    assert_eq!(engine.snapshot().active_style_ansi, "\x1b[0;38;2;10;20;30m");
}

#[test]
fn engine_sgr_dim_blink_strikethrough_and_resets() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 2,
        cols: 8,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"\x1b[2;5;9m");
    assert_eq!(engine.snapshot().active_style_ansi, "\x1b[0;2;5;9m");

    engine.feed(b"\x1b[1m\x1b[22m");
    assert_eq!(engine.snapshot().active_style_ansi, "\x1b[0;5;9m");

    engine.feed(b"\x1b[25m\x1b[29m");
    assert_eq!(engine.snapshot().active_style_ansi, "\x1b[0m");
}

#[test]
fn engine_sgr_ignores_underline_color_operands() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 2,
        cols: 8,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"\x1b[58:5:2mX");
    assert_eq!(engine.snapshot().active_style_ansi, "\x1b[0m");

    engine.feed(b"\x1b[58;2;10;20;30;31mX");
    assert_eq!(engine.snapshot().active_style_ansi, "\x1b[0;38;5;1m");
}

#[test]
fn engine_autowrap_off_overwrites_last_column() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 2,
        cols: 3,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"\x1b[?7labcde");
    let snapshot = engine.snapshot();
    assert_eq!(snapshot.lines[0], "abe");
    assert_eq!(snapshot.lines[1], "   ");
    assert_eq!(snapshot.cursor_row, 0);
    assert_eq!(snapshot.cursor_col, 2);

    engine.feed(b"\x1b[?7hf");
    let snapshot = engine.snapshot();
    assert_eq!(snapshot.lines[0], "abf");

    engine.feed(b"g");
    let snapshot = engine.snapshot();
    assert_eq!(snapshot.lines[0], "abf");
    assert_eq!(snapshot.lines[1], "g  ");
    assert_eq!(snapshot.cursor_row, 1);
    assert_eq!(snapshot.cursor_col, 1);
}

#[test]
fn engine_origin_mode_addresses_relative_to_scroll_region() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 5,
        cols: 8,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"\x1b[2;4r\x1b[?6h\x1b[1;1H");
    let snapshot = engine.snapshot();
    assert_eq!(snapshot.cursor_row, 1);
    assert_eq!(snapshot.cursor_col, 0);

    engine.feed(b"\x1b[9;9H");
    let snapshot = engine.snapshot();
    assert_eq!(snapshot.cursor_row, 3);
    assert_eq!(snapshot.cursor_col, 7);

    engine.feed(b"\x1b[?6l\x1b[1;1H");
    let snapshot = engine.snapshot();
    assert_eq!(snapshot.cursor_row, 0);
    assert_eq!(snapshot.cursor_col, 0);
}

#[test]
fn engine_decstbm_homes_cursor_origin_aware() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 5,
        cols: 8,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"\x1b[3;4H\x1b[2;4r");
    let snapshot = engine.snapshot();
    assert_eq!(snapshot.cursor_row, 0);
    assert_eq!(snapshot.cursor_col, 0);

    let mut origin_engine = TerminalEngine::new(TerminalSize {
        rows: 5,
        cols: 8,
        pixel_width: 0,
        pixel_height: 0,
    });
    origin_engine.feed(b"\x1b[?6h\x1b[2;4r");
    let snapshot = origin_engine.snapshot();
    assert_eq!(snapshot.cursor_row, 1);
    assert_eq!(snapshot.cursor_col, 0);
}

#[test]
fn engine_insert_mode_shifts_cells_right() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 2,
        cols: 8,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"abc\r\x1b[4hXY");
    let snapshot = engine.snapshot();
    assert_eq!(snapshot.lines[0], "XYabc   ");
    assert_eq!(snapshot.cursor_col, 2);

    engine.feed(b"\x1b[4lZ");
    let snapshot = engine.snapshot();
    assert_eq!(snapshot.lines[0], "XYZbc   ");
    assert_eq!(snapshot.cursor_col, 3);
}

#[test]
fn engine_ris_resets_terminal_state() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 3,
        cols: 8,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"one\r\ntwo\r\nthree\r\nfour");
    engine.feed(b"\x1b[1m\x1b(0\x1b[?7l\x1b[?1049h\x1b[2;3H\x1b7");
    engine.feed(b"\x1bc");
    let snapshot = engine.snapshot();

    assert!(!snapshot.alternate_screen);
    assert_eq!(
        snapshot.lines,
        vec![
            "        ".to_string(),
            "        ".to_string(),
            "        ".to_string()
        ]
    );
    assert_eq!(snapshot.cursor_row, 0);
    assert_eq!(snapshot.cursor_col, 0);
    assert_eq!(snapshot.active_style_ansi, "\x1b[0m");
    assert_eq!(snapshot.scroll_top, 0);
    assert_eq!(snapshot.scroll_bottom, 2);
    assert_eq!(snapshot.styled_scrollback, vec!["one     ".to_string()]);

    engine.feed(b"q\x1b8Z");
    let snapshot = engine.snapshot();
    assert_eq!(snapshot.lines[0], "qZ      ");
}

#[test]
fn engine_bounds_scrollback_to_ten_thousand_lines() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 2,
        cols: 8,
        pixel_width: 0,
        pixel_height: 0,
    });

    let mut input = String::new();
    for n in 0..10_002 {
        if n > 0 {
            input.push_str("\r\n");
        }
        input.push_str(&format!("L{n}"));
    }
    input.push_str("\r\n");
    engine.feed(input.as_bytes());
    let snapshot = engine.snapshot();

    assert_eq!(snapshot.styled_scrollback.len(), 10_000);
    assert_eq!(snapshot.styled_scrollback[0], format!("{:<8}", "L1"));
    assert_eq!(
        snapshot.styled_scrollback[9_999],
        format!("{:<8}", "L10000")
    );
}

#[test]
fn engine_scrolls_down_in_region_with_csi_t() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 3,
        cols: 4,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"a\r\nb\r\nc");
    engine.feed(b"\x1b[T");
    let snapshot = engine.snapshot();
    assert_eq!(snapshot.lines[0], "    ");
    assert_eq!(snapshot.lines[1], "a   ");
    assert_eq!(snapshot.lines[2], "b   ");

    engine.feed(b"\x1b[2T");
    let snapshot = engine.snapshot();
    assert_eq!(snapshot.lines[0], "    ");
    assert_eq!(snapshot.lines[1], "    ");
    assert_eq!(snapshot.lines[2], "    ");
}

#[test]
fn engine_ignores_csi_gt_t_title_mode_queries() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 3,
        cols: 4,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"a\r\nb\r\nc");
    engine.feed(b"\x1b[>1T");
    let snapshot = engine.snapshot();

    assert_eq!(snapshot.lines[0], "a   ");
    assert_eq!(snapshot.lines[1], "b   ");
    assert_eq!(snapshot.lines[2], "c   ");
}

#[test]
fn engine_alt_screen_1049_saves_and_restores_cursor() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 2,
        cols: 6,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"main\x1b[?1049halt\x1b[?1049l");
    let snapshot = engine.snapshot();
    assert!(!snapshot.alternate_screen);
    assert_eq!(snapshot.lines[0], "main  ");
    assert_eq!(snapshot.cursor_row, 0);
    assert_eq!(snapshot.cursor_col, 4);

    engine.feed(b"Z");
    let snapshot = engine.snapshot();
    assert_eq!(snapshot.lines[0], "mainZ ");
}

#[test]
fn engine_alt_screen_1047_clears_alternate_on_exit() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 2,
        cols: 6,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"\x1b[?1047halt\x1b[?1047l\x1b[?47h");
    let snapshot = engine.snapshot();

    assert!(snapshot.alternate_screen);
    assert_eq!(snapshot.lines[0], "      ");
}

#[test]
fn engine_alt_screen_47_keeps_content_across_switches() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 2,
        cols: 6,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"\x1b[?47hx\x1b[?47l\x1b[?47h");
    let snapshot = engine.snapshot();

    assert!(snapshot.alternate_screen);
    assert_eq!(snapshot.lines[0], "x     ");
}

#[test]
fn engine_alt_screen_1048_switches_and_restores_cursor() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 2,
        cols: 6,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"ab\x1b[?1048hcd\x1b[?1048l");
    let snapshot = engine.snapshot();

    assert!(!snapshot.alternate_screen);
    assert_eq!(snapshot.lines[0], "ab    ");
    assert_eq!(snapshot.cursor_row, 0);
    assert_eq!(snapshot.cursor_col, 2);
}

#[test]
fn engine_snapshot_visible_omits_scrollback() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 2,
        cols: 5,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"one\r\ntwo\r\nthree");
    let visible = engine.snapshot_visible();

    assert_eq!(visible.lines[0], "two  ");
    assert_eq!(visible.lines[1], "three");
    assert!(visible.styled_scrollback.is_empty());

    let full = engine.snapshot();
    assert_eq!(full.styled_scrollback, vec!["one  ".to_string()]);
}

#[test]
fn engine_tracks_bracketed_paste_and_mouse_modes() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 2,
        cols: 8,
        pixel_width: 0,
        pixel_height: 0,
    });

    assert!(!engine.bracketed_paste());
    assert_eq!(engine.mouse_reporting(), MouseReportingMode::None);
    assert_eq!(engine.mouse_encoding(), MouseEncoding::X10);

    engine.feed(b"\x1b[?2004h\x1b[?1006h\x1b[?1000h");
    assert!(engine.bracketed_paste());
    assert_eq!(engine.mouse_reporting(), MouseReportingMode::Click);
    assert_eq!(engine.mouse_encoding(), MouseEncoding::Sgr);

    engine.feed(b"\x1b[?1002h");
    assert_eq!(engine.mouse_reporting(), MouseReportingMode::Drag);

    engine.feed(b"\x1b[?1003h\x1b[?1015h");
    assert_eq!(engine.mouse_reporting(), MouseReportingMode::AnyMotion);
    assert_eq!(engine.mouse_encoding(), MouseEncoding::Utf8);

    engine.feed(b"\x1b[?1003l\x1b[?1015l\x1b[?2004l");
    assert!(!engine.bracketed_paste());
    assert_eq!(engine.mouse_reporting(), MouseReportingMode::None);
    assert_eq!(engine.mouse_encoding(), MouseEncoding::X10);
}

#[test]
fn engine_ris_resets_bracketed_paste_and_mouse_modes() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 2,
        cols: 8,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"\x1b[?2004h\x1b[?1006h\x1b[?1000h\x1bc");
    assert!(!engine.bracketed_paste());
    assert_eq!(engine.mouse_reporting(), MouseReportingMode::None);
    assert_eq!(engine.mouse_encoding(), MouseEncoding::X10);
}

#[test]
fn engine_collects_osc52_and_swallows_other_osc() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 2,
        cols: 8,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"\x1b]52;c;aGVsbG8=\x07\x1b]0;title\x07\x1b]52;p;d29ybGQ=\x1b\\");
    let collected = engine.drain_osc52();

    assert_eq!(
        collected,
        vec!["52;c;aGVsbG8=".to_string(), "52;p;d29ybGQ=".to_string()]
    );
    assert!(engine.drain_osc52().is_empty());
    assert_eq!(engine.snapshot().window_title.as_deref(), Some("title"));
}

#[test]
fn engine_drains_scrollback_lines_incrementally() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 2,
        cols: 5,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"one\r\ntwo\r\nthree");
    assert_eq!(engine.drain_scrollback_lines(), vec!["one  ".to_string()]);
    assert!(engine.drain_scrollback_lines().is_empty());

    engine.feed(b"\r\nfour");
    assert_eq!(engine.drain_scrollback_lines(), vec!["two  ".to_string()]);
}

#[test]
fn engine_does_not_bridge_alternate_buffer_scrollback() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 2,
        cols: 5,
        pixel_width: 0,
        pixel_height: 0,
    });

    engine.feed(b"one\r\ntwo\r\nthree");
    assert_eq!(engine.drain_scrollback_lines(), vec!["one  ".to_string()]);

    // While the alternate screen is active, scrolling stays in the alternate
    // buffer and must not add to the normal-buffer scrollback bridge.
    engine.feed(b"\x1b[?47hthree\r\nfour\r\nfive");
    assert!(engine.drain_scrollback_lines().is_empty());

    // Returning to the normal screen resumes draining the queued normal
    // scrollback, but still skips anything produced in the alternate buffer.
    engine.feed(b"\x1b[?47l\r\nsix");
    assert_eq!(engine.drain_scrollback_lines(), vec!["two  ".to_string()]);
}

#[test]
fn engine_scrollback_drain_handles_trim() {
    let mut engine = TerminalEngine::new(TerminalSize {
        rows: 1,
        cols: 10,
        pixel_width: 0,
        pixel_height: 0,
    });

    // Push enough lines to exceed MAX_SCROLLBACK_LINES. With a 1-row screen,
    // each completed line adds one scrollback line. Use a 10-column width so
    // "line10004" (9 chars) does not wrap and create extra scrollback rows.
    for i in 0..10_005 {
        engine.feed(format!("line{i:04}\r\n").as_bytes());
    }

    let drained = engine.drain_scrollback_lines();
    assert_eq!(drained.len(), 10_000);
    // The oldest five lines were trimmed, so the first drained line is line0005.
    assert_eq!(drained[0], "line0005  ");
    assert_eq!(drained[9_999], "line10004 ");
}

#[test]
fn engine_resize_same_size_preserves_scroll_region() {
    let size = TerminalSize {
        rows: 10,
        cols: 20,
        pixel_width: 0,
        pixel_height: 0,
    };
    let mut engine = TerminalEngine::new(size);

    // DECSTBM: set scroll region to rows 2-8 (1-indexed).
    engine.feed(b"\x1b[2;8r");
    let before = engine.snapshot();
    assert_eq!(before.scroll_top, 1);
    assert_eq!(before.scroll_bottom, 7);

    engine.resize(size);
    let after = engine.snapshot();
    assert_eq!(
        after.scroll_top, 1,
        "same-size resize should preserve scroll_top"
    );
    assert_eq!(
        after.scroll_bottom, 7,
        "same-size resize should preserve scroll_bottom"
    );
}
