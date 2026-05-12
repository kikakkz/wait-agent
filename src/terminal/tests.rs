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

    assert_eq!(snapshot.scrollback, vec!["one  ".to_string()]);
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

    assert_eq!(snapshot.scrollback, vec!["red  ".to_string()]);
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
