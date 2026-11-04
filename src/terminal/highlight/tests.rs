use crate::terminal::{Cell, Snapshot, Terminal, Theme};

fn screen(text: &str) -> (Terminal, Snapshot) {
    let mut terminal = Terminal::new(240, 3, 16);
    terminal.feed(text.as_bytes());
    let snapshot = terminal.snapshot();
    (terminal, snapshot)
}

fn assert_token(snapshot: &Snapshot, row: usize, token: &str, color: u32) {
    let cells = &snapshot.lines[row].cells;
    let text: String = cells.iter().map(|cell| cell.text.as_str()).collect();
    let start = text
        .find(token)
        .unwrap_or_else(|| panic!("missing {token:?} in {text:?}"));
    let end = start + token.len();
    let mut offset = 0;
    for cell in cells {
        let next = offset + cell.text.len();
        if offset < end && start < next {
            assert_eq!(cell.foreground, color, "{token:?}: cell {:?}", cell.text);
        }
        offset = next;
    }
}

fn visible_cell(snapshot: &Snapshot, offset: usize) -> &Cell {
    &snapshot.lines[offset / usize::from(snapshot.columns)].cells
        [offset % usize::from(snapshot.columns)]
}

#[test]
fn default_rules_color_shell_values_addresses_and_keep_log_palette() {
    let theme = Theme::default();
    let (_, snapshot) = screen(
        "-v --verbose --no-cache --output=value 0x002A DEAD1234 true false not found \
         192.0.2.12 aa:bb:cc:dd:ee:ff user@example.com TRACE DEBUG INFO WARN ERROR FATAL",
    );
    for token in ["-v", "--verbose", "--no-cache", "--output"] {
        assert_token(&snapshot, 0, token, theme.debug);
    }
    assert_token(&snapshot, 0, "=value", theme.foreground);
    for token in ["0x002A", "DEAD1234"] {
        assert_token(&snapshot, 0, token, theme.warn);
    }
    assert_token(&snapshot, 0, "true", theme.info);
    assert_token(&snapshot, 0, "false", theme.error);
    assert_token(&snapshot, 0, "not found", theme.error);
    for token in ["192.0.2.12", "aa:bb:cc:dd:ee:ff"] {
        assert_token(&snapshot, 0, token, theme.address);
    }
    assert_token(&snapshot, 0, "user@example.com", theme.url);
    for (token, color) in [
        ("TRACE", theme.trace),
        ("DEBUG", theme.debug),
        ("INFO", theme.info),
        ("WARN", theme.warn),
        ("ERROR", theme.error),
        ("FATAL", theme.fatal),
    ] {
        assert_token(&snapshot, 0, token, color);
    }
}

#[test]
fn token_boundaries_reject_embedded_words_and_malformed_addresses() {
    let (_, snapshot) = screen(
        "abc--flag x0x0042 0xGGGG untrue falsehood 中文true \
         999.1.2.3 192.0.2.1.2 aa:bb:cc:dd:ee:gg user@@example.com",
    );
    for cell in &snapshot.lines[0].cells {
        assert_eq!(
            cell.foreground,
            Theme::default().foreground,
            "{:?}",
            cell.text
        );
    }
}

#[test]
fn six_nested_pairs_share_depth_colors_and_unmatched_delimiters_stay_plain() {
    let theme = Theme::default();
    let (_, snapshot) = screen("([{([{x}])}])\r\n( [ { x\r\n) ] }");
    for (position, color) in [
        (0, theme.warn),
        (1, theme.address),
        (2, theme.info),
        (3, theme.url),
        (4, theme.debug),
        (5, theme.error),
        (7, theme.error),
        (8, theme.debug),
        (9, theme.url),
        (10, theme.info),
        (11, theme.address),
        (12, theme.warn),
    ] {
        assert_eq!(snapshot.lines[0].cells[position].foreground, color);
    }
    assert_eq!(snapshot.lines[0].cells[6].foreground, theme.foreground);
    for row in &snapshot.lines[1..] {
        assert!(
            row.cells
                .iter()
                .all(|cell| cell.foreground == theme.foreground)
        );
    }
}

#[test]
fn quoted_delimiters_and_complete_urls_paths_are_not_recolored_as_brackets() {
    let theme = Theme::default();
    let (_, snapshot) = screen(
        "'(quoted[words])' \"{also quoted}\" \
         https://example.com/a(b)?enabled=true /tmp/[draft] user+tag@example.com",
    );
    assert_token(&snapshot, 0, "'(quoted[words])'", theme.foreground);
    assert_token(&snapshot, 0, "\"{also quoted}\"", theme.foreground);
    assert_token(
        &snapshot,
        0,
        "https://example.com/a(b)?enabled=true",
        theme.url,
    );
    assert_token(&snapshot, 0, "/tmp/[draft]", theme.path);
    assert_token(&snapshot, 0, "user+tag@example.com", theme.url);
}

#[test]
fn semantic_tokens_and_matching_pairs_follow_soft_wrap_and_column_reflow() {
    let theme = Theme::default();
    let mut terminal = Terminal::new(8, 8, 16);
    terminal.feed(b"([abcdefghij]) --verbose");
    for columns in [8, 5, 12] {
        terminal.scroll_to_bottom();
        terminal.resize(columns, 8);
        if columns == 5 {
            // Alacritty retains blank rows below the cursor when shrinking columns,
            // moving the start of this logical line into scrollback.
            let live = terminal.snapshot();
            assert!(live.history_size > 0);
            assert_eq!(live.display_offset, 0);
            assert_token(&live, 0, "]", theme.address);
            assert_token(&live, 0, ")", theme.warn);
        }
        terminal.scroll(16);
        let snapshot = terminal.snapshot();
        let visible: String = snapshot
            .lines
            .iter()
            .flat_map(|line| &line.cells)
            .map(|cell| cell.text.as_str())
            .collect();
        assert!(
            visible.starts_with("([abcdefghij]) --verbose"),
            "width {columns}: {visible:?}"
        );
        assert!(snapshot.lines[0].wrapped);
        for (offset, text, color) in [
            (0, "(", theme.warn),
            (1, "[", theme.address),
            (12, "]", theme.address),
            (13, ")", theme.warn),
        ] {
            let cell = visible_cell(&snapshot, offset);
            assert_eq!(cell.text, text);
            assert_eq!(cell.foreground, color, "width {columns}, offset {offset}");
        }
        for offset in 15..24 {
            assert_eq!(visible_cell(&snapshot, offset).foreground, theme.debug);
        }
    }
}

#[test]
fn real_line_breaks_do_not_join_brackets_or_partial_keywords() {
    let theme = Theme::default();
    let mut terminal = Terminal::new(16, 4, 16);
    terminal.feed(b"(tru\r\ne)\r\n[fals\r\ne]");
    let snapshot = terminal.snapshot();
    for row in &snapshot.lines {
        assert!(
            row.cells
                .iter()
                .all(|cell| cell.foreground == theme.foreground)
        );
    }
}

#[test]
fn overwritten_text_theme_changes_and_switches_do_not_reuse_stale_colors() {
    let mut terminal = Terminal::new(40, 3, 16);
    terminal.feed(b"(word) --verbose true");
    assert_token(&terminal.snapshot(), 0, "(", Theme::default().warn);
    terminal.feed(b"\r\x1b[2Kordinary prose");
    let snapshot = terminal.snapshot();
    assert_token(&snapshot, 0, "ordinary prose", Theme::default().foreground);
    terminal.feed(b"\r\x1b[2K(word) --verbose true");
    let theme = Theme {
        foreground: 0x223344,
        warn: 0xaabbcc,
        debug: 0x998877,
        info: 0x456789,
        ..Theme::default()
    };
    terminal.set_theme(theme.clone());
    let snapshot = terminal.snapshot();
    assert_token(&snapshot, 0, "(", theme.warn);
    assert_token(&snapshot, 0, "--verbose", theme.debug);
    assert_token(&snapshot, 0, "true", theme.info);
    terminal.set_highlight(false);
    assert_token(
        &terminal.snapshot(),
        0,
        "(word) --verbose true",
        theme.foreground,
    );
    terminal.set_highlight(true);
    assert_token(&terminal.snapshot(), 0, "--verbose", theme.debug);
}

#[test]
fn ansi_foreground_dim_inverse_and_hidden_cells_keep_their_presentation() {
    for alternate in [false, true] {
        let mut terminal = Terminal::new(40, 3, 16);
        if alternate {
            terminal.feed(b"\x1b[?1049h\x1b[H");
        }
        let theme = Theme {
            warn: 0x90c060,
            ..Theme::default()
        };
        terminal.set_theme(theme.clone());
        terminal.feed(
            b"\x1b[38;2;18;52;86m(a)\x1b[0m \
              \x1b[2m(b)\x1b[0m \x1b[7m(c)\x1b[0m \x1b[8m(d)\x1b[0m",
        );
        let snapshot = terminal.snapshot();
        assert_token(&snapshot, 0, "(a)", 0x123456);
        assert_eq!(snapshot.lines[0].cells[4].foreground, 0x608040);
        assert_eq!(snapshot.lines[0].cells[6].foreground, 0x608040);
        assert_token(&snapshot, 0, "(c)", theme.background);
        assert_token(&snapshot, 0, "(d)", theme.foreground);
        assert!(
            snapshot.lines[0].cells[12..15]
                .iter()
                .all(|cell| cell.hidden)
        );
        terminal.feed(b"\x1b[2J\x1b[H\x1b[8mER\x1b[28mROR\r\n\x1b[8m(\x1b[28m(v)");
        let snapshot = terminal.snapshot();
        assert_token(&snapshot, 0, "ROR", theme.foreground);
        assert_eq!(snapshot.lines[1].cells[1].text, "(");
        assert_eq!(snapshot.lines[1].cells[1].foreground, theme.warn);
        assert_eq!(snapshot.lines[1].cells[3].text, ")");
        assert_eq!(snapshot.lines[1].cells[3].foreground, theme.warn);
    }
}

#[test]
fn alternate_screen_highlights_pager_pages_and_restores_main_screen() {
    let theme = Theme::default();
    let mut terminal = Terminal::new(40, 3, 16);
    terminal.feed(b"(word) --verbose true");
    assert_token(&terminal.snapshot(), 0, "(", theme.warn);
    terminal.feed(b"\x1b[?1049h\x1b[HINFO startup\r\nWARN retry\r\nERROR failed");
    let alternate = terminal.snapshot();
    assert!(alternate.alternate_screen);
    assert!(alternate.lines.iter().all(|line| line.number.is_none()));
    assert_token(&alternate, 0, "INFO", theme.info);
    assert_token(&alternate, 1, "WARN", theme.warn);
    assert_token(&alternate, 2, "ERROR", theme.error);

    terminal.feed(b"\x1b[H\x1b[2Jordinary output\r\nINFO next page");
    let next_page = terminal.snapshot();
    assert_token(&next_page, 0, "ordinary output", theme.foreground);
    assert_token(&next_page, 1, "INFO", theme.info);
    assert!(
        next_page.lines[2]
            .cells
            .iter()
            .all(|cell| { cell.text == " " && cell.foreground == theme.foreground })
    );

    terminal.feed(b"\x1b[H\x1b[2KWARN changed row");
    assert_token(&terminal.snapshot(), 0, "WARN", theme.warn);
    terminal.set_highlight(false);
    let plain = terminal.snapshot();
    assert_token(&plain, 0, "WARN", theme.foreground);
    assert_token(&plain, 1, "INFO", theme.foreground);
    terminal.set_highlight(true);
    assert_token(&terminal.snapshot(), 0, "WARN", theme.warn);

    terminal.feed(b"\x1b[?1049l");
    let restored = terminal.snapshot();
    assert!(!restored.alternate_screen);
    assert_token(&restored, 0, "(", theme.warn);
    assert_token(&restored, 0, "--verbose", theme.debug);
    assert_token(&restored, 0, "true", theme.info);
}

#[test]
fn alternate_screen_search_and_ansi_styles_override_cached_log_colors() {
    let theme = Theme::default();
    let mut terminal = Terminal::new(40, 3, 16);
    terminal.feed(b"\x1b[?1049h\x1b[HINFO");
    assert_token(&terminal.snapshot(), 0, "INFO", theme.info);

    terminal.feed(b"\x1b[H\x1b[7mINFO\x1b[0m");
    let selected = terminal.snapshot();
    assert_token(&selected, 0, "INFO", theme.background);
    assert!(
        selected.lines[0].cells[..4]
            .iter()
            .all(|cell| { !cell.default_foreground && cell.background == theme.foreground })
    );

    terminal.feed(b"\x1b[H\x1b[38;2;18;52;86mINFO\x1b[0m");
    assert_token(&terminal.snapshot(), 0, "INFO", 0x123456);
    terminal.set_highlight(false);
    assert_token(&terminal.snapshot(), 0, "INFO", 0x123456);
    terminal.set_highlight(true);

    terminal.feed(b"\x1b[HINFO");
    assert_token(&terminal.snapshot(), 0, "INFO", theme.info);
}

#[test]
fn wide_and_combining_cells_do_not_shift_decoration_boundaries() {
    let theme = Theme::default();
    let (_, snapshot) = screen("界 e\u{301} ([word]) true");
    assert_eq!(snapshot.lines[0].cells[0].text, "界");
    assert_eq!(snapshot.lines[0].cells[0].width, 2);
    assert_eq!(snapshot.lines[0].cells[1].width, 0);
    assert_eq!(snapshot.lines[0].cells[3].text, "e\u{301}");
    assert_token(&snapshot, 0, "界 e\u{301}", theme.foreground);
    assert_token(&snapshot, 0, "(", theme.warn);
    assert_token(&snapshot, 0, "[", theme.address);
    assert_token(&snapshot, 0, "]", theme.address);
    assert_token(&snapshot, 0, ")", theme.warn);
    assert_token(&snapshot, 0, "true", theme.info);
}

#[test]
fn docker_status_colors_health_without_treating_uptime_as_health() {
    let theme = Theme::default();
    let (_, snapshot) = screen(
        "api    Up 2 hours (healthy)\r\n\
         worker Up 3 minutes (unhealthy)\r\n\
         db     Up 6 seconds (health: starting)",
    );
    assert_token(&snapshot, 0, "healthy", theme.info);
    assert_token(&snapshot, 1, "unhealthy", theme.error);
    assert_token(&snapshot, 2, "health: starting", theme.warn);
    for row in 0..3 {
        assert_token(&snapshot, row, "Up", theme.foreground);
    }
    assert_token(&snapshot, 0, "2 hours", theme.foreground);
    assert_token(&snapshot, 1, "3 minutes", theme.foreground);
    assert_token(&snapshot, 2, "6 seconds", theme.foreground);
}

#[test]
fn docker_health_keeps_ansi_colors_and_recomputes_after_carriage_return() {
    let theme = Theme::default();
    let mut terminal = Terminal::new(80, 2, 16);
    terminal.feed(b"\x1b[38;2;18;52;86mhealthy unhealthy health: starting\x1b[0m");
    assert_token(
        &terminal.snapshot(),
        0,
        "healthy unhealthy health: starting",
        0x123456,
    );
    terminal.feed(b"\r\x1b[2KUp 1 second starting not healthy unhealthyish");
    let snapshot = terminal.snapshot();
    assert_token(&snapshot, 0, "Up 1 second starting", theme.foreground);
    assert_token(&snapshot, 0, "not healthy", theme.error);
    assert_token(&snapshot, 0, "unhealthyish", theme.foreground);
}
