use super::*;
fn text(term: &Terminal) -> Vec<String> {
    term.snapshot()
        .lines
        .iter()
        .map(|line| {
            line.cells
                .iter()
                .map(|c| c.text.as_str())
                .collect::<String>()
        })
        .collect()
}

#[test]
fn carriage_return_and_ansi_overwrite_cells() {
    let mut t = Terminal::new(10, 3, 100);
    t.feed(b"abc\r\x1b[31mX\x1b[0m");
    assert_eq!(&text(&t)[0][..3], "Xbc");
    let s = t.snapshot();
    assert_ne!(
        s.lines[0].cells[0].foreground,
        s.lines[0].cells[1].foreground
    );
    assert_eq!(s.lines[0].number, Some(1));
}
#[test]
fn chunk_boundaries_preserve_utf8_and_control_sequences() {
    let input = "a中e\u{301}🙂\x1b[31m红\x1b[0m\r\nlast".as_bytes();
    let mut reference = Terminal::new(9, 4, 30);
    reference.feed(input);
    for split in 0..=input.len() {
        let mut t = Terminal::new(9, 4, 30);
        t.feed(&input[..split]);
        t.feed(&input[split..]);
        assert_eq!(text(&t), text(&reference), "split={split}");
        assert_eq!(
            t.snapshot().lines[0].cells,
            reference.snapshot().lines[0].cells
        );
    }
}
#[test]
fn wide_character_has_exactly_one_payload_and_two_cells() {
    let mut t = Terminal::new(8, 2, 20);
    t.feed("中e\u{301}".as_bytes());
    let s = t.snapshot();
    assert_eq!(
        (s.lines[0].cells[0].width, s.lines[0].cells[1].width),
        (2, 0)
    );
    assert_eq!(s.lines[0].cells[1].text, "");
    assert_eq!(s.lines[0].cells[2].text, "e\u{301}");
}
#[test]
fn alternate_screen_preserves_main_and_hides_line_numbers() {
    let mut t = Terminal::new(10, 3, 10);
    t.feed(b"main\x1b[?1049halt");
    assert!(t.snapshot().alternate_screen);
    assert_eq!(t.snapshot().lines[0].number, None);
    t.feed(b"\x1b[?1049l");
    assert!(!t.snapshot().alternate_screen);
    assert!(text(&t)[0].starts_with("main"));
}
#[test]
fn queries_reply_but_clipboard_queries_do_not() {
    let mut t = Terminal::new(10, 3, 10);
    assert_eq!(t.feed(b"abc\x1b[6n"), vec![b"\x1b[1;4R".to_vec()]);
    assert!(t.feed(b"\x1b]52;c;?\x07\x1b]52;c;aGVsbG8=\x07").is_empty());
    assert!(t.feed(b"\x1b[c").iter().any(|r| r.starts_with(b"\x1b[?")));
}
#[test]
fn history_is_bounded_by_lines_and_bytes_and_scroll_does_not_follow() {
    let bytes = row_charge(10) * 4;
    let mut t = Terminal::with_history_budget(10, 3, 100, bytes);
    for i in 0..20 {
        t.feed(format!("{i}\r\n").as_bytes());
    }
    assert_eq!(t.snapshot().history_limit, 4);
    assert!(t.snapshot().history_size <= 4);
    t.scroll(2);
    let first = text(&t)[0].clone();
    t.feed(b"next\r\n");
    assert_eq!(text(&t)[0], first);
    t.scroll_to_bottom();
    assert_eq!(t.snapshot().display_offset, 0);
}

#[test]
fn absolute_history_scroll_does_not_accumulate_drag_requests() {
    let mut terminal = Terminal::new(10, 3, 20);
    for index in 0..15 {
        terminal.feed(format!("{index}\r\n").as_bytes());
    }
    terminal.scroll_to(4);
    assert_eq!(terminal.snapshot().display_offset, 4);
    terminal.scroll_to(4);
    assert_eq!(terminal.snapshot().display_offset, 4);
    terminal.scroll_to(2);
    assert_eq!(terminal.snapshot().display_offset, 2);
    terminal.scroll_to(usize::MAX);
    let snapshot = terminal.snapshot();
    assert_eq!(snapshot.display_offset, snapshot.history_size);

    terminal.scroll_to(4);
    let first = text(&terminal)[0].clone();
    terminal.feed(b"new\r\n");
    assert_eq!(text(&terminal)[0], first);
    terminal.scroll_to(0);
    assert_eq!(terminal.snapshot().display_offset, 0);

    terminal.feed(b"\x1b[?1049h");
    terminal.scroll_to(usize::MAX);
    assert_eq!(terminal.snapshot().display_offset, 0);
}

#[test]
fn resize_reflows_and_recalculates_budget() {
    let mut t = Terminal::with_history_budget(4, 3, 100, row_charge(4) * 10);
    t.feed(b"abcdef");
    assert!(t.snapshot().lines[0].wrapped);
    t.resize(8, 3);
    assert!(text(&t)[0].starts_with("abcdef"));
    assert!(t.snapshot().history_limit < 10);
}
#[test]
fn keyboard_and_paste_follow_modes() {
    let mut t = Terminal::new(10, 3, 10);
    assert_eq!(t.encode_key("up", false, false, false), b"\x1b[A");
    assert_eq!(t.encode_key("c", true, false, false), vec![3]);
    assert!(t.encode_key("a", false, false, false).is_empty());
    t.feed(b"\x1b[?1h\x1b[?2004h");
    assert_eq!(t.encode_key("up", false, false, false), b"\x1bOA");
    assert_eq!(
        t.encode_paste("a\r\nb\x1b[201~"),
        b"\x1b[200~a\nb[201~\x1b[201~"
    );
}

#[test]
fn modified_navigation_and_focus_reporting_follow_terminal_modes() {
    let mut terminal = Terminal::new(10, 3, 10);
    assert_eq!(terminal.encode_key("left", true, true, false), b"\x1b[1;7D");
    assert_eq!(terminal.encode_key("f2", true, true, true), b"\x1b[1;8Q");
    assert!(terminal.encode_focus(true).is_empty());
    assert!(terminal.encode_focus(false).is_empty());
    terminal.feed(b"\x1b[?1004h");
    assert_eq!(terminal.encode_focus(true), b"\x1b[I");
    assert_eq!(terminal.encode_focus(false), b"\x1b[O");
    terminal.feed(b"\x1b[?1004l");
    assert!(terminal.encode_focus(false).is_empty());
}

#[test]
fn alternate_scroll_follows_live_modes_without_injecting_keys_into_the_shell() {
    let mut terminal = Terminal::new(80, 24, 10);
    assert!(terminal.modes().alternate_scroll);
    assert!(terminal.encode_alternate_scroll(3).is_empty());
    terminal.feed(b"\x1b[?1049h");
    assert_eq!(terminal.encode_alternate_scroll(2), b"\x1b[A\x1b[A");
    assert_eq!(terminal.encode_alternate_scroll(-1), b"\x1b[B");
    terminal.feed(b"\x1b[?1h");
    assert_eq!(terminal.encode_alternate_scroll(1), b"\x1bOA");
    assert_eq!(terminal.encode_alternate_scroll(-2), b"\x1bOB\x1bOB");
    assert_eq!(terminal.encode_alternate_scroll(i32::MIN).len(), 300);
    terminal.feed(b"\x1b[?1007l");
    assert!(!terminal.snapshot().modes.alternate_scroll);
    assert!(terminal.encode_alternate_scroll(1).is_empty());
    terminal.feed(b"\x1b[?1007h\x1b[?1000h");
    assert!(terminal.encode_alternate_scroll(1).is_empty());
    terminal.feed(b"\x1b[?1000l\x1b[?1049l");
    assert!(terminal.encode_alternate_scroll(1).is_empty());
}

#[test]
fn cursor_snapshot_preserves_application_shapes_and_wide_character_extent() {
    let mut terminal = Terminal::new(10, 3, 10);
    assert_eq!(terminal.snapshot().cursor.shape, CursorShape::Block);
    for (sequence, shape) in [
        (b"\x1b[4 q".as_slice(), CursorShape::Underline),
        (b"\x1b[6 q".as_slice(), CursorShape::Beam),
        (b"\x1b[2 q".as_slice(), CursorShape::Block),
    ] {
        terminal.feed(sequence);
        assert_eq!(terminal.snapshot().cursor.shape, shape);
        assert!(terminal.snapshot().cursor.visible);
    }
    terminal.feed("中\x1b[1;2H".as_bytes());
    let cursor = terminal.snapshot().cursor;
    assert_eq!((cursor.row, cursor.column, cursor.width), (0, 0, 2));
    terminal.feed(b"\x1b[?25l");
    assert_eq!(terminal.snapshot().cursor.shape, CursorShape::Hidden);
    assert!(!terminal.snapshot().cursor.visible);
    terminal.feed(b"\x1b[?25h\x1b[1;3H");
    assert_eq!(terminal.snapshot().cursor.width, 1);
}

#[test]
fn paste_normalizes_line_endings_and_filters_controls_in_one_payload() {
    let mut terminal = Terminal::new(10, 3, 10);
    let text = "中\r\nä\r\rb\n\t\x00\x07\x1b\u{009b}z";
    assert_eq!(terminal.encode_paste(text), "中\rä\r\rb\r\tz".as_bytes());
    terminal.feed(b"\x1b[?2004h");
    assert_eq!(
        terminal.encode_paste(text),
        "\x1b[200~中\nä\n\nb\n\tz\x1b[201~".as_bytes()
    );
}

#[test]
fn malformed_utf8_does_not_break_later_protocol() {
    let mut t = Terminal::new(10, 3, 10);
    t.feed(b"\xff\xfe\x1b[2;1HX");
    assert_eq!(t.snapshot().lines[1].cells[0].text, "X");
}
#[test]
fn malicious_metadata_and_combining_marks_are_bounded() {
    let mut t = Terminal::new(10, 3, 10);
    let osc = format!("\x1b]2;{}\x07OK", "a".repeat(20_000));
    t.feed(osc.as_bytes());
    assert!(t.snapshot().title.len() <= MAX_STRING_BYTES);
    assert!(text(&t)[0].starts_with("OK"));
    t.feed(format!("e{}", "\u{301}".repeat(10_000)).as_bytes());
    assert!(t.snapshot().lines[0].cells[2].text.chars().count() <= MAX_COMBINING + 1);
}
#[test]
fn sgr_mouse_and_shift_local_override() {
    let mut t = Terminal::new(80, 24, 10);
    t.feed(b"\x1b[?1002h\x1b[?1006h");
    let mut e = MouseEvent {
        button: 0,
        column: 9,
        row: 4,
        pressed: true,
        motion: false,
        shift: false,
        alt: false,
        ctrl: false,
    };
    assert_eq!(t.encode_mouse(e), b"\x1b[<0;10;5M");
    e.pressed = false;
    assert_eq!(t.encode_mouse(e), b"\x1b[<0;10;5m");
    e.shift = true;
    assert!(t.encode_mouse(e).is_empty());
}

#[test]
fn horizontal_wheel_uses_distinct_sgr_and_legacy_mouse_codes() {
    let mut terminal = Terminal::new(80, 24, 10);
    terminal.feed(b"\x1b[?1000h\x1b[?1006h");
    for button in 64..=67 {
        let event = MouseEvent {
            button,
            column: 9,
            row: 4,
            pressed: true,
            motion: false,
            shift: false,
            alt: false,
            ctrl: false,
        };
        assert_eq!(
            terminal.encode_mouse(event),
            format!("\x1b[<{button};10;5M").as_bytes()
        );
        terminal.feed(b"\x1b[?1006l");
        assert_eq!(
            terminal.encode_mouse(event),
            [27, b'[', b'M', button + 32, 42, 37]
        );
        terminal.feed(b"\x1b[?1006h");
        assert!(
            terminal
                .encode_mouse(MouseEvent {
                    shift: true,
                    ..event
                })
                .is_empty()
        );
    }
}

#[test]
fn utf8_mouse_supports_wide_terminals_and_wheel_reports_are_press_only() {
    let mut terminal = Terminal::new(500, 200, 10);
    terminal.feed(b"\x1b[?1000h\x1b[?1005h");
    let event = MouseEvent {
        button: 0,
        column: 250,
        row: 120,
        pressed: true,
        motion: false,
        shift: false,
        alt: false,
        ctrl: false,
    };
    assert!(terminal.modes().utf8_mouse);
    assert_eq!(terminal.encode_mouse(event), b"\x1b[M \xc4\x9b\xc2\x99");
    terminal.feed(b"\x1b[?1006h");
    assert_eq!(terminal.encode_mouse(event), b"\x1b[<0;251;121M");
    terminal.feed(b"\x1b[?1006l");
    assert!(terminal.encode_mouse(event).is_empty());
    for mode in [b"\x1b[?1005h".as_slice(), b"\x1b[?1006h".as_slice()] {
        terminal.feed(mode);
        for button in 64..=67 {
            assert!(
                terminal
                    .encode_mouse(MouseEvent {
                        button,
                        pressed: false,
                        ..event
                    })
                    .is_empty()
            );
            assert!(
                terminal
                    .encode_mouse(MouseEvent {
                        button,
                        motion: true,
                        ..event
                    })
                    .is_empty()
            );
        }
    }
}

#[test]
fn scroll_region_does_not_move_header_or_footer() {
    let mut t = Terminal::new(8, 4, 100);
    t.feed(b"header\r\na\r\nb\r\nfooter\x1b[2;3r\x1b[3;1H\nnew");
    let lines = text(&t);
    assert!(lines[0].starts_with("header"));
    assert!(lines[3].starts_with("footer"));
    assert_eq!(t.snapshot().history_size, 0);
}

#[test]
fn inverse_and_hidden_styles_are_preserved_for_renderer() {
    let mut t = Terminal::new(8, 2, 10);
    t.feed(b"\x1b[31;44;7mX\x1b[0;8mY");
    let s = t.snapshot();
    assert_eq!(s.lines[0].cells[0].foreground, palette(4));
    assert_eq!(s.lines[0].cells[0].background, palette(1));
    assert!(s.lines[0].cells[1].hidden);
}

#[test]
fn synchronized_update_without_end_does_not_stall_terminal() {
    let mut t = Terminal::new(8, 2, 10);
    t.feed(b"\x1b[?2026hhello");
    assert!(text(&t)[0].starts_with("hello"));
}

#[test]
fn shrinking_width_does_not_undercharge_retained_allocations() {
    let mut t = Terminal::new(80, 4, 50_000);
    for _ in 0..100 {
        t.feed(b"hello\r\n");
    }
    let limit = t.snapshot().history_limit;
    t.feed(b"\x1b[?1049h");
    t.resize(20, 4);
    t.feed(b"\x1b[?1049l");
    assert_eq!(t.snapshot().history_limit, limit);
    assert!(t.snapshot().history_size <= limit);
}

fn finish_search(t: &mut Terminal) -> SearchStatus {
    for _ in 0..10_000 {
        let status = t.search_step(8);
        if status.complete || status.paused {
            return status;
        }
    }
    panic!("search failed to finish bounded waterline");
}

#[test]
fn selection_copies_soft_wrap_wide_characters_and_explicit_spaces() {
    let mut t = Terminal::new(4, 3, 100);
    t.feed(b"abcdef");
    t.begin_selection(0, 0, SelectionKind::Character);
    t.update_selection(1, 1);
    assert_eq!(t.selected_text().as_deref(), Some("abcdef"));
    t.clear_selection();
    t.feed(b"\x1b[H\x1b[2J");
    t.feed("中  ".as_bytes());
    t.begin_selection(1, 0, SelectionKind::Character);
    assert_eq!(t.selected_text().as_deref(), Some("中"));
    t.begin_selection(2, 0, SelectionKind::Character);
    t.update_selection(3, 0);
    assert_eq!(t.selected_text().as_deref(), Some("  "));
}

#[test]
fn reverse_character_selection_includes_both_endpoints_and_reverses_again() {
    let mut terminal = Terminal::new(8, 3, 10);
    terminal.feed(b"abcdefgh");
    terminal.begin_selection(4, 0, SelectionKind::Character);
    terminal.update_selection(1, 0);
    assert_eq!(terminal.selected_text().as_deref(), Some("bcde"));
    terminal.update_selection(6, 0);
    assert_eq!(terminal.selected_text().as_deref(), Some("efg"));

    let mut terminal = Terminal::new(4, 3, 10);
    terminal.feed(b"abcdefgh");
    terminal.begin_selection(1, 1, SelectionKind::Character);
    terminal.update_selection(2, 0);
    assert_eq!(terminal.selected_text().as_deref(), Some("cdef"));

    let mut terminal = Terminal::new(8, 3, 10);
    terminal.feed("a中bc".as_bytes());
    terminal.begin_selection(4, 0, SelectionKind::Character);
    terminal.update_selection(2, 0);
    assert_eq!(terminal.selected_text().as_deref(), Some("中bc"));
}

#[test]
fn select_all_includes_retained_history_and_stays_inside_the_active_screen() {
    let mut terminal = Terminal::new(4, 2, 10);
    terminal.feed(b"one\r\ntwo\r\nlast");
    terminal.select_all();
    assert!(terminal.snapshot().has_selection);
    assert_eq!(
        terminal.selected_text().as_deref(),
        Some("one \ntwo \nlast")
    );
    terminal.feed(b"\x1b[?1049h\x1b[HALT");
    terminal.select_all();
    assert_eq!(terminal.selected_text().as_deref(), Some("ALT \n    "));
    terminal.feed(b"\x1b[?1049l");
    terminal.select_all();
    assert_eq!(
        terminal.selected_text().as_deref(),
        Some("one \ntwo \nlast")
    );
    terminal.clear_selection();
    assert!(!terminal.snapshot().has_selection);
}
#[test]
fn selection_copies_tab_spans_as_visual_spaces() {
    let mut t = Terminal::new(16, 3, 100);
    t.feed(b"a\tb");
    t.begin_selection(0, 0, SelectionKind::Character);
    t.update_selection(8, 0);
    assert_eq!(t.selected_text().as_deref(), Some("a       b"));

    // Starting at the tab marker or inside its span copies only selected cells.
    t.begin_selection(1, 0, SelectionKind::Character);
    t.update_selection(3, 0);
    assert_eq!(t.selected_text().as_deref(), Some("   "));
    t.begin_selection(5, 0, SelectionKind::Character);
    t.update_selection(8, 0);
    assert_eq!(t.selected_text().as_deref(), Some("   b"));

    // A custom stop at column five must not be mistaken for the default eighth.
    t.feed(b"\x1b[2;1H\x1b[3g\x1b[5G\x1bH\rX\tY");
    t.begin_selection(0, 1, SelectionKind::Character);
    t.update_selection(4, 1);
    assert_eq!(t.selected_text().as_deref(), Some("X   Y"));
}
#[test]
fn semantic_and_visual_line_selection_are_distinct() {
    let mut t = Terminal::new(8, 3, 100);
    t.feed(b"foo bar more");
    t.begin_selection(5, 0, SelectionKind::Word);
    assert_eq!(t.selected_text().as_deref(), Some("bar"));
    t.begin_selection(0, 0, SelectionKind::Line);
    assert_eq!(t.selected_text().as_deref(), Some("foo bar "));
}
#[test]
fn selection_survives_append_but_not_overwrite_or_eviction() {
    let mut t = Terminal::new(10, 3, 10);
    t.feed(b"keep\r\n");
    t.begin_selection(0, 0, SelectionKind::Word);
    t.feed(b"next\r\nmore\r\n");
    assert_eq!(t.selected_text().as_deref(), Some("keep"));
    t.feed(b"\x1b[3J");
    assert!(t.selected_text().is_none());
    let mut t = Terminal::new(10, 3, 10);
    t.feed(b"keep");
    t.begin_selection(0, 0, SelectionKind::Word);
    t.feed(b"\rX");
    assert!(t.selected_text().is_none());
    assert!(t.snapshot().selection_invalidated);
}

#[test]
fn selection_revalidates_live_rows_after_they_scroll_into_history() {
    for overwrite in [false, true] {
        let mut terminal = Terminal::new(6, 2, 10);
        terminal.feed(b"old\r\nkeep\r\nlive");
        terminal.select_all();
        let live_row = terminal.selection_anchor.as_ref().unwrap().stamps[1].id;
        terminal.feed(if overwrite {
            b"\x1b[HX\x1b[2;1H\r\n"
        } else {
            b"\x1b[2;1H\r\n"
        });
        assert!(terminal.point_for(live_row, 0).unwrap().line.0 < 0);
        let snapshot = terminal.snapshot();
        assert_eq!(snapshot.selection_invalidated, overwrite);
        assert_eq!(snapshot.has_selection, !overwrite);
        if overwrite {
            assert!(terminal.selected_text().is_none());
        } else {
            assert_eq!(
                terminal.selected_text().as_deref(),
                Some("old   \nkeep  \nlive  ")
            );
            assert!(snapshot.lines[0].cells.iter().all(|cell| cell.selected));
            assert!(snapshot.lines[1].cells.iter().all(|cell| !cell.selected));
            terminal.feed(b"\x1b[3J");
            assert!(terminal.snapshot().selection_invalidated);
        }
    }
}

#[test]
fn search_crosses_soft_wrap_and_respects_hard_breaks() {
    let mut t = Terminal::new(4, 4, 100);
    t.feed(b"abcdef\r\nxy");
    t.start_search("cde".into(), true);
    assert_eq!(finish_search(&mut t).matches, 1);
    assert!(t.search_next(false));
    t.start_search("fxy".into(), true);
    assert_eq!(finish_search(&mut t).matches, 0);
    t.start_search("CDE".into(), false);
    assert_eq!(finish_search(&mut t).matches, 1);
}

#[test]
fn search_starts_in_the_viewport_and_distinguishes_the_current_match() {
    let mut terminal = Terminal::new(12, 3, 100);
    for row in 0..8 {
        if row != 0 {
            terminal.feed(b"\r\n");
        }
        terminal.feed(format!("needle {row}").as_bytes());
    }
    terminal.scroll_to(3);
    assert!(text(&terminal)[0].starts_with("needle 2"));
    for (backwards, expected, row) in [(false, 2, 0), (true, 4, 2)] {
        terminal.start_search("needle".into(), true);
        assert_eq!(finish_search(&mut terminal).matches, 8);
        assert!(terminal.search_next(backwards));
        assert_eq!(terminal.search_status().current, Some(expected));
        let snapshot = terminal.snapshot();
        assert_eq!(snapshot.display_offset, 3);
        for (index, line) in snapshot.lines.iter().enumerate() {
            let expected = if index == row {
                SearchHighlight::Current
            } else {
                SearchHighlight::Match
            };
            assert!(
                line.cells[..6]
                    .iter()
                    .all(|cell| cell.search_highlight == Some(expected))
            );
            assert!(
                line.cells[6..]
                    .iter()
                    .all(|cell| cell.search_highlight.is_none())
            );
        }
    }
    terminal.cancel_search();
    assert!(terminal.snapshot().lines.iter().all(|line| {
        line.cells
            .iter()
            .all(|cell| cell.search_highlight.is_none())
    }));
}

#[test]
fn search_navigation_wraps_when_no_match_is_visible() {
    let mut terminal = Terminal::new(12, 3, 100);
    terminal.feed(b"needle old\r\n1\r\n2\r\n3\r\n4\r\n5\r\n6\r\nneedle new");
    for (backwards, expected) in [(false, 1), (true, 0)] {
        terminal.scroll_to(3);
        terminal.start_search("needle".into(), true);
        assert_eq!(finish_search(&mut terminal).matches, 2);
        assert!(terminal.search_next(backwards));
        assert_eq!(terminal.search_status().current, Some(expected));
        assert!(terminal.search_next(backwards));
        assert_eq!(terminal.search_status().current, Some(1 - expected));
    }
}

#[test]
fn search_navigation_waits_for_the_scan_to_reach_the_viewport() {
    for (backwards, expected) in [(false, 147), (true, 149)] {
        let mut terminal = Terminal::new(12, 3, 200);
        for row in 0..150 {
            if row != 0 {
                terminal.feed(b"\r\n");
            }
            terminal.feed(format!("needle {row}").as_bytes());
        }
        terminal.start_search("needle".into(), true);
        assert_eq!(terminal.search_step(1).matches, 1);
        assert!(!terminal.search_next(backwards));
        assert_eq!(terminal.snapshot().display_offset, 0);
        assert_eq!(terminal.search_status().current, None);
        let status = finish_search(&mut terminal);
        assert_eq!(status.current, Some(expected));
        assert_eq!(terminal.snapshot().display_offset, 0);
    }
}

#[test]
fn search_keeps_results_while_output_continues_and_rejects_stale_matches() {
    let mut t = Terminal::new(20, 3, 100);
    t.feed(b"needle\r\n");
    t.start_search("needle".into(), true);
    assert_eq!(finish_search(&mut t).matches, 1);
    for _ in 0..10 {
        t.feed(b"more\r\n");
        t.search_step(1);
        assert!(t.search_next(false));
    }
    t.feed(b"\x1b[3J");
    assert!(!t.search_next(false));
    let mut t = Terminal::new(20, 3, 100);
    t.feed(b"needle");
    t.start_search("needle".into(), true);
    finish_search(&mut t);
    t.feed(b"\rxxxxxx");
    assert!(!t.search_next(false));
    assert_eq!(finish_search(&mut t).matches, 0);
}

#[test]
fn incremental_search_replaces_results_and_preserves_buffer_order() {
    let mut terminal = Terminal::new(8, 4, 10);
    terminal.feed(b"needle\r\nneedle\r\nneedle\r\nother");
    terminal.start_search("needle".into(), true);
    assert_eq!(terminal.search_step(1).matches, 1);
    assert!(terminal.search_next(false));
    assert_eq!(terminal.search_step(1).current, Some(0));

    terminal.feed(b"\x1b[Hxxxxxx\x1b[4;1Hneedle");
    let snapshot = terminal.snapshot();
    assert!(snapshot.lines[0].cells[0].search_highlight.is_none());
    assert!(snapshot.lines[1].cells[0].search_highlight.is_some());
    let progress = terminal.search_step(1);
    assert_eq!(progress.matches, 2);
    assert_eq!(progress.current, None);
    assert_eq!(finish_search(&mut terminal).matches, 3);
    for expected in 0..3 {
        assert!(terminal.search_next(false));
        assert_eq!(terminal.search_status().current, Some(expected));
    }

    // Reusing the removed match's location must not be blocked by a stale key.
    terminal.feed(b"\x1b[Hneedle\x1b[4;1Hxxxxxx");
    assert_eq!(finish_search(&mut terminal).matches, 3);
    let results = terminal.search_results();
    let rows: Vec<_> = results
        .iter()
        .map(|item| terminal.point_for(item.rows[0].id, 0).unwrap().line.0)
        .collect();
    assert_eq!(rows, [0, 1, 2]);
    let snapshot = terminal.snapshot();
    assert!(snapshot.lines[0].cells[0].search_highlight.is_some());
    assert!(snapshot.lines[3].cells[0].search_highlight.is_none());
}

#[test]
fn search_rejects_a_soft_wrapped_match_changed_between_slices() {
    let mut terminal = Terminal::new(4, 3, 10);
    terminal.feed(b"abcdefgh");
    terminal.start_search("cde".into(), true);
    assert_eq!(terminal.search_step(1).matches, 0);
    terminal.feed(b"\x1b[1;3HX");
    assert_eq!(terminal.search_step(1).matches, 0);
    assert_eq!(finish_search(&mut terminal).matches, 0);
    assert!(terminal.snapshot().lines.iter().all(|line| {
        line.cells
            .iter()
            .all(|cell| cell.search_highlight.is_none())
    }));
    terminal.feed(b"\x1b[1;3Hc");
    assert_eq!(finish_search(&mut terminal).matches, 1);
    assert!(
        terminal.snapshot().lines[0].cells[2]
            .search_highlight
            .is_some()
    );
}

#[test]
fn replacement_query_cancels_old_work_and_unicode_maps_to_cells() {
    let mut t = Terminal::new(8, 4, 100);
    t.feed("中Ä\r\nold\r\nnew".as_bytes());
    let old = t.start_search("old".into(), true);
    t.search_step(1);
    let new = t.start_search("中ä".into(), false);
    assert_ne!(old, new);
    assert_eq!(finish_search(&mut t).matches, 1);
    let snapshot = t.snapshot();
    assert!(snapshot.lines[0].cells[0].search_highlight.is_some());
    assert!(snapshot.lines[0].cells[1].search_highlight.is_some());
    assert!(snapshot.lines[0].cells[2].search_highlight.is_some());
    t.cancel_search();
    assert_eq!(t.search_status().matches, 0);
}

#[test]
fn search_case_folding_matches_unicode_equivalents_consistently() {
    for (text, query) in [
        ("ΟΣ", "ΟΣ"),
        ("ος", "οσ"),
        ("οσ", "ος"),
        ("S", "ſ"),
        ("K", "k"),
    ] {
        let mut terminal = Terminal::new(8, 2, 10);
        terminal.feed(text.as_bytes());
        terminal.start_search(query.into(), false);
        assert_eq!(
            finish_search(&mut terminal).matches,
            1,
            "{text:?} / {query:?}"
        );
        terminal.start_search(query.into(), true);
        assert_eq!(
            finish_search(&mut terminal).matches,
            usize::from(text == query)
        );
    }
}

#[test]
fn search_case_folding_preserves_wide_combining_and_wrapped_cell_ranges() {
    let mut terminal = Terminal::new(4, 3, 10);
    terminal.feed("中Οςe\u{301}!".as_bytes());
    terminal.start_search("中οσE\u{301}".into(), false);
    assert_eq!(finish_search(&mut terminal).matches, 1);
    let matches = terminal.search_results();
    assert_eq!(matches[0].start_column, 0);
    assert_eq!(matches[0].end_column, 0);
    assert_eq!(matches[0].rows.len(), 2);
    let snapshot = terminal.snapshot();
    assert!(
        snapshot.lines[0]
            .cells
            .iter()
            .all(|cell| cell.search_highlight.is_some())
    );
    assert!(snapshot.lines[1].cells[0].search_highlight.is_some());
    assert!(snapshot.lines[1].cells[1].search_highlight.is_none());
}

#[test]
fn search_keeps_regex_metacharacters_literal_in_both_case_modes() {
    let mut terminal = Terminal::new(32, 2, 10);
    terminal.feed(br"[A-Z]+\path (a.b)");
    for case_sensitive in [false, true] {
        for query in [r"[A-Z]+\path", "(a.b)"] {
            terminal.start_search(query.into(), case_sensitive);
            assert_eq!(finish_search(&mut terminal).matches, 1);
        }
        terminal.start_search(".*".into(), case_sensitive);
        assert_eq!(finish_search(&mut terminal).matches, 0);
    }
}

#[test]
fn search_accepts_unicode_queries_at_the_byte_limit_without_expanding_grid_text() {
    let query = "ſ".repeat(2048);
    let mut terminal = Terminal::new(500, 10, 10);
    terminal.feed(query.as_bytes());
    for case_sensitive in [false, true] {
        terminal.start_search(query.clone(), case_sensitive);
        let status = finish_search(&mut terminal);
        assert!(!status.truncated);
        assert_eq!(status.matches, 1);
    }
    terminal.start_search(format!("{query}s"), false);
    let status = finish_search(&mut terminal);
    assert!(status.complete && status.truncated);
    assert_eq!(status.matches, 0);
}
#[test]
fn search_waterline_completes_despite_continuous_appends() {
    let mut t = Terminal::new(20, 4, 500);
    for _ in 0..200 {
        t.feed(b"old\r\n");
    }
    t.start_search("old".into(), true);
    let mut completed = false;
    for _ in 0..100 {
        t.feed(b"new\r\n");
        if t.search_step(4).complete {
            completed = true;
            break;
        }
    }
    assert!(completed);
}
#[test]
fn semantic_highlight_respects_ansi_and_overwrites() {
    let mut t = Terminal::new(40, 3, 100);
    t.feed(b"ERROR \x1b[31mERROR\x1b[0m WARN");
    let snapshot = t.snapshot();
    assert_eq!(snapshot.lines[0].cells[0].foreground, t.theme.error);
    assert_eq!(snapshot.lines[0].cells[6].foreground, palette(1));
    assert_eq!(snapshot.lines[0].cells[12].foreground, t.theme.warn);
    t.feed(b"\rOK   ");
    assert_eq!(
        t.snapshot().lines[0].cells[0].foreground,
        t.theme.foreground
    );
    t.set_highlight(false);
    assert_eq!(
        t.snapshot().lines[0].cells[12].foreground,
        t.theme.foreground
    );
}
#[test]
fn highlight_matches_soft_wrap_and_theme_changes_clear_cache() {
    let mut t = Terminal::new(4, 4, 100);
    t.feed(b"ERROR");
    assert_eq!(t.snapshot().lines[0].cells[0].foreground, t.theme.error);
    assert_eq!(t.snapshot().lines[1].cells[0].foreground, t.theme.error);
    let mut theme = t.theme.clone();
    theme.error = 0x123456;
    theme.foreground = 0xabcdef;
    t.set_theme(theme);
    assert_eq!(t.snapshot().lines[1].cells[0].foreground, 0x123456);
    t.feed(b"\x1b[?1049h\x1b[HERROR");
    let alternate = t.snapshot();
    assert!(alternate.alternate_screen);
    assert_eq!(alternate.lines[0].cells[0].text, "E");
    assert_eq!(alternate.lines[0].cells[0].foreground, 0x123456);
    assert_eq!(alternate.lines[1].cells[0].foreground, 0x123456);
}
#[test]
fn frozen_session_retains_main_history_and_final_alternate_without_protocol_execution() {
    let mut t = Terminal::new(12, 3, 100);
    t.feed(b"main\x1b[?1049halt");
    t.freeze();
    assert!(t.snapshot().is_read_only);
    assert!(!t.snapshot().alternate_screen);
    assert!(text(&t)[0].starts_with("main"));
    assert!(t.final_alt_snapshot().unwrap().alternate_screen);
    let before = text(&t);
    assert!(t.feed(b"overwrite\x1b[6n").is_empty());
    assert_eq!(text(&t), before);
    t.start_search("main".into(), true);
    assert_eq!(finish_search(&mut t).matches, 1);
}
#[test]
fn shrinking_budget_invalidates_evicted_anchors_and_results() {
    let mut t = Terminal::new(20, 3, 100);
    t.feed(b"needle\r\n1\r\n2\r\n3\r\n");
    t.scroll(100);
    t.begin_selection(0, 0, SelectionKind::Word);
    t.start_search("needle".into(), true);
    assert_eq!(finish_search(&mut t).matches, 1);
    t.set_history_budget(0);
    assert_eq!(t.snapshot().history_size, 0);
    assert!(t.selected_text().is_none());
    assert!(!t.search_next(false));
}

#[test]
fn unrelated_append_on_same_row_preserves_selection_and_match() {
    let mut t = Terminal::new(40, 3, 100);
    t.feed(b"needle ");
    t.begin_selection(0, 0, SelectionKind::Word);
    t.start_search("needle".into(), true);
    finish_search(&mut t);
    t.feed(b"unrelated suffix");
    assert_eq!(t.selected_text().as_deref(), Some("needle"));
    assert!(t.search_next(false));
    assert_eq!(finish_search(&mut t).matches, 1);
    assert!(t.snapshot().lines[0].cells[0].search_highlight.is_some());
}

#[test]
fn visual_line_selection_survives_mouse_release_and_drag() {
    let mut t = Terminal::new(8, 3, 100);
    t.feed(b"foo bar\r\nnext");
    t.begin_selection(4, 0, SelectionKind::Line);
    t.update_selection(4, 0);
    assert_eq!(t.selected_text().as_deref(), Some("foo bar "));
    t.update_selection(2, 1);
    assert_eq!(t.selected_text().as_deref(), Some("foo bar \nnext    "));
}

#[test]
fn idle_and_cancelled_searches_have_no_pending_work() {
    let mut terminal = Terminal::new(80, 24, 100);
    assert!(terminal.search_status().complete);
    terminal.start_search("".into(), false);
    assert!(terminal.search_status().complete);
    terminal.start_search("query".into(), false);
    assert!(!terminal.search_status().complete);
    terminal.cancel_search();
    assert!(terminal.search_status().complete);
    assert!(terminal.search_step(1).complete);
}

#[test]
fn paused_viewport_keeps_its_row_and_reports_eviction_until_navigation() {
    let mut t = Terminal::new(8, 2, 3);
    t.feed(b"0\r\n1\r\n2\r\n3");
    t.scroll(2);
    assert!(text(&t)[0].starts_with('0'));
    t.feed(b"\r\n4");
    assert!(text(&t)[0].starts_with('0'));
    assert!(!t.snapshot().viewport_evicted);
    t.feed(b"\r\n5");
    assert!(text(&t)[0].starts_with('1'));
    assert!(t.snapshot().viewport_evicted);
    t.scroll(-1);
    assert!(!t.snapshot().viewport_evicted);
    t.set_history_limits(0, DEFAULT_HISTORY_BYTES);
    assert!(t.snapshot().viewport_evicted);
    assert_eq!(t.snapshot().history_size, 0);
    t.scroll_to_bottom();
    assert!(!t.snapshot().viewport_evicted);
}

#[test]
fn primary_viewport_eviction_during_alternate_screen_is_reported_on_return() {
    let mut t = Terminal::new(8, 2, 10);
    t.feed(b"0\r\n1\r\n2\r\n3\r\n4");
    t.scroll(20);
    t.feed(b"\x1b[?1049h\x1b[Halt");
    t.set_history_limits(1, DEFAULT_HISTORY_BYTES);
    assert!(text(&t)[0].starts_with("alt"));
    assert!(!t.snapshot().viewport_evicted);
    t.feed(b"\x1b[?1049l");
    assert!(t.snapshot().viewport_evicted);
    assert!(text(&t)[0].starts_with('2'));
}

#[test]
fn search_navigation_tracks_the_match_after_an_earlier_result_disappears() {
    let mut t = Terminal::new(8, 4, 10);
    t.feed(b"x\r\nx\r\nx");
    t.start_search("x".into(), true);
    assert_eq!(finish_search(&mut t).matches, 3);
    assert!(t.search_next(false));
    assert!(t.search_next(false));
    t.feed(b"\x1b[H.");
    assert!(t.search_next(false));
    assert_eq!(t.search_status().current, Some(1));
    let result = &t.search_results()[1];
    assert_eq!(t.main_tracker.positions[&result.rows[0].id], 2);
}

#[test]
fn newly_discovered_search_results_are_ordered_without_changing_current_match() {
    let mut t = Terminal::new(8, 4, 10);
    t.feed(b".\r\nx\r\nx");
    t.start_search("x".into(), true);
    finish_search(&mut t);
    assert!(t.search_next(false));
    t.feed(b"\x1b[Hx");
    assert!(
        !t.search_status().complete,
        "new output must wake the search actor"
    );
    assert_eq!(finish_search(&mut t).matches, 3);
    assert_eq!(t.search_status().current, Some(1));
    let positions: Vec<_> = t
        .search_results()
        .iter()
        .map(|m| t.main_tracker.positions[&m.rows[0].id])
        .collect();
    assert_eq!(positions, vec![0, 1, 2]);
}

#[test]
fn oversized_search_query_is_rejected_without_searching_its_prefix() {
    let mut t = Terminal::new(8, 2, 10);
    t.feed(b"a");
    t.start_search("a".repeat(10_000), true);
    let status = t.search_step(1);
    assert!(status.complete && status.truncated);
    assert!(status.query.len() <= 4096);
    assert!(t.search.as_ref().unwrap().matcher.is_none());
    assert_eq!(status.matches, 0);
}

#[test]
fn mouse_release_does_not_erase_selection_invalidation_feedback() {
    let mut t = Terminal::new(8, 2, 10);
    t.feed(b"keep");
    t.begin_selection(0, 0, SelectionKind::Word);
    t.feed(b"\rX");
    t.update_selection(3, 0);
    assert!(t.snapshot().selection_invalidated);
    assert!(t.selected_text().is_none());
}

#[test]
fn repeated_combining_marks_are_bounded_even_when_rep_finishes_in_ascii() {
    let mut t = Terminal::new(8, 2, 10);
    t.feed("e\u{301}\x1b[99b".as_bytes());
    assert_eq!(
        t.snapshot().lines[0].cells[0].text.chars().count(),
        MAX_COMBINING + 1
    );
}

#[test]
fn string_gate_follows_escape_interrupts_and_embedded_controls() {
    let mut t = Terminal::new(8, 2, 10);
    t.feed(b"\x1b]2;title\x1b[H");
    t.feed("a".repeat(MAX_STRING_BYTES + 20).as_bytes());
    t.feed(b"\rOK");
    assert!(text(&t)[1].starts_with("OK"));
    let mut t = Terminal::new(8, 2, 10);
    let input = format!("\x1b\x01]2;{}\x07OK", "a".repeat(MAX_STRING_BYTES + 20));
    t.feed(input.as_bytes());
    assert!(t.snapshot().title.len() <= MAX_STRING_BYTES);
    assert!(text(&t)[0].starts_with("OK"));
    t.feed(b"\x1b]\x01\x028;id=test;https://example.test\x07x");
    assert!(
        t.term.grid()[Point::new(Line(0), Column(2))]
            .hyperlink()
            .is_none()
    );
}

#[test]
fn dim_semantic_text_keeps_ansi_dim_intensity() {
    let mut t = Terminal::new(10, 2, 10);
    t.feed(b"\x1b[2mERROR");
    assert_eq!(
        t.snapshot().lines[0].cells[0].foreground,
        dim(Theme::default().error)
    );
}

#[test]
fn alternate_budget_reduction_preserves_screens_and_drop_releases_cached_extras() {
    let mut t = Terminal::new(8, 2, 30);
    let row = format!("e{}\r\n", "\u{301}".repeat(MAX_COMBINING));
    for _ in 0..12 {
        t.feed(row.as_bytes());
    }
    let removed: Vec<_> = (t.term.grid().topmost_line().0..0)
        .map(|line| {
            Arc::downgrade(
                t.term.grid()[Point::new(Line(line), Column(0))]
                    .extra
                    .as_ref()
                    .unwrap(),
            )
        })
        .collect();
    assert!(!removed.is_empty());
    let main_cursor = t.term.grid().cursor.point;
    t.feed(b"\x1b[?1049h\x1b[1;2H\x1b[31mALT\x1b[?1h");
    let alternate = t.snapshot();
    // Shrinking history must preserve the active alternate screen and saved
    // main cursor. Upstream is allowed to cache the removed cell allocations.
    t.set_history_limits(0, DEFAULT_HISTORY_BYTES);
    let after = t.snapshot();
    assert!(after.alternate_screen && after.modes.application_cursor);
    assert_eq!(after.cursor.row, alternate.cursor.row);
    assert_eq!(after.cursor.column, alternate.cursor.column);
    assert_eq!(after.lines[0].cells, alternate.lines[0].cells);
    t.feed(b"\x1b[?1049l");
    assert_eq!(t.term.grid().cursor.point, main_cursor);
    assert_eq!(t.snapshot().history_size, 0);
    assert_eq!(t.snapshot().history_limit, 0);
    t.feed(row.as_bytes());
    assert_eq!(t.snapshot().history_size, 0);
    drop(t);
    assert!(removed.iter().all(|extra| extra.upgrade().is_none()));
}

#[test]
fn soft_wrap_appended_after_a_match_preserves_selection_and_search_identity() {
    let mut t = Terminal::new(4, 3, 10);
    t.feed(b"abcd");
    t.begin_selection(3, 0, SelectionKind::Character);
    t.start_search("d".into(), true);
    assert_eq!(finish_search(&mut t).matches, 1);
    t.feed(b"e");
    assert_eq!(t.selected_text().as_deref(), Some("d"));
    assert!(t.search_next(false));
    assert!(t.snapshot().lines[0].cells[3].search_highlight.is_some());
}

#[test]
fn height_resize_preserves_history_selection_search_and_viewport_identity() {
    let mut t = Terminal::new(12, 3, 30);
    t.feed(b"keep\r\none\r\ntwo\r\nthree\r\nfour");
    t.scroll(2);
    t.begin_selection(1, 0, SelectionKind::Word);
    let row_id = t.selection_anchor.as_ref().unwrap().start.0;
    let query_id = t.start_search("keep".into(), true);
    assert_eq!(finish_search(&mut t).matches, 1);
    let history_limit = t.snapshot().history_limit;
    for rows in [4, 3] {
        t.resize(12, rows);
        assert_eq!(t.selected_text().as_deref(), Some("keep"));
        assert_eq!(t.selection_anchor.as_ref().unwrap().start.0, row_id);
        assert_eq!(t.search_status().query_id, query_id);
        assert_eq!(t.snapshot().history_limit, history_limit);
        assert!(!t.snapshot().selection_invalidated);
        assert!(!t.snapshot().viewport_evicted);
        assert!(text(&t)[0].starts_with("keep"));
        assert!(t.search_next(false));
    }
}

#[test]
fn height_resize_keeps_alternate_selection_and_paused_primary_search() {
    let mut t = Terminal::new(8, 3, 20);
    t.feed(b"needle\r\none\r\ntwo\r\nthree");
    let query_id = t.start_search("needle".into(), true);
    assert_eq!(finish_search(&mut t).matches, 1);
    t.feed(b"\x1b[?1049h\x1b[HALT");
    t.begin_selection(1, 0, SelectionKind::Word);
    t.resize(8, 4);
    assert_eq!(t.selected_text().as_deref(), Some("ALT"));
    assert!(t.search_status().paused);
    assert_eq!(t.search_status().query_id, query_id);
    t.resize(8, 3);
    assert_eq!(t.selected_text().as_deref(), Some("ALT"));
    t.feed(b"\x1b[?1049l");
    assert!(!t.search_status().paused);
    assert_eq!(t.search_status().query_id, query_id);
    assert_eq!(finish_search(&mut t).matches, 1);
    assert!(t.search_next(false));
}

#[test]
fn height_resize_invalidates_selection_and_matches_for_removed_rows() {
    let mut t = Terminal::new(8, 4, 0);
    t.feed(b"top\r\none\r\ntwo\r\nbottom\x1b[H");
    t.begin_selection(1, 3, SelectionKind::Word);
    let query_id = t.start_search("bottom".into(), true);
    assert_eq!(finish_search(&mut t).matches, 1);
    t.resize(8, 3);
    assert!(t.selected_text().is_none());
    assert!(t.snapshot().selection_invalidated);
    assert_eq!(t.search_status().query_id, query_id);
    assert!(!t.search_next(false));
}

#[test]
fn width_resize_still_invalidates_selection_and_restarts_soft_wrap_search() {
    let mut t = Terminal::new(4, 3, 20);
    t.feed(b"abcdef");
    t.begin_selection(2, 0, SelectionKind::Character);
    t.update_selection(0, 1);
    assert_eq!(t.selected_text().as_deref(), Some("cde"));
    let query_id = t.start_search("cde".into(), true);
    assert_eq!(finish_search(&mut t).matches, 1);
    t.resize(6, 3);
    assert!(t.selected_text().is_none());
    assert!(t.snapshot().selection_invalidated);
    assert_ne!(t.search_status().query_id, query_id);
    assert_eq!(finish_search(&mut t).matches, 1);
    assert!(text(&t)[0].starts_with("abcdef"));
    t.start_search("x".repeat(5000), true);
    t.resize(7, 3);
    assert!(t.search_status().truncated);
    assert!(t.search.as_ref().unwrap().matcher.is_none());
}
