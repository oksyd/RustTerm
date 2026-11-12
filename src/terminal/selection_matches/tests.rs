use super::*;
use crate::terminal::{SearchHighlight, Snapshot};

fn marked(snapshot: &Snapshot) -> Vec<(usize, usize)> {
    snapshot
        .lines
        .iter()
        .enumerate()
        .flat_map(|(row, line)| {
            line.cells
                .iter()
                .enumerate()
                .filter_map(move |(column, cell)| cell.selection_match.then_some((row, column)))
        })
        .collect()
}

#[test]
fn double_click_matches_whole_tokens_case_sensitively_and_preserves_text_colors() {
    let mut terminal = Terminal::new(64, 3, 100);
    terminal.feed(b"error error_count myerror ERROR \x1b[31merror\x1b[0m");
    let before = terminal.snapshot();
    terminal.begin_selection(2, 0, SelectionKind::Word);
    assert_eq!(terminal.selected_text().as_deref(), Some("error"));
    let after = terminal.snapshot();
    assert_eq!(
        marked(&after),
        (0..5)
            .chain(32..37)
            .map(|column| (0, column))
            .collect::<Vec<_>>()
    );
    for (before, after) in before.lines[0].cells.iter().zip(&after.lines[0].cells) {
        assert_eq!(
            (&before.text, before.foreground, before.background),
            (&after.text, after.foreground, after.background)
        );
    }
    assert!(after.lines[0].cells[..5].iter().all(|c| c.selected));
    assert!(!after.lines[0].cells[32].selected);
}

#[test]
fn dragging_a_short_literal_matches_substrings_and_overlap_without_regex_interpretation() {
    let mut terminal = Terminal::new(32, 3, 100);
    terminal.feed(b"aba ababa\r\na.b axb a.b");
    terminal.begin_selection(0, 0, SelectionKind::Character);
    terminal.update_selection(2, 0);
    assert_eq!(
        marked(&terminal.snapshot()),
        (0..3).chain(4..9).map(|col| (0, col)).collect::<Vec<_>>()
    );
    terminal.begin_selection(0, 1, SelectionKind::Character);
    terminal.update_selection(2, 1);
    assert_eq!(
        marked(&terminal.snapshot()),
        (0..3).chain(8..11).map(|col| (1, col)).collect::<Vec<_>>()
    );
}

#[test]
fn single_click_blank_line_selection_and_large_selections_do_not_flash_matches() {
    let mut terminal = Terminal::new(80, 8, 100);
    terminal.feed(b"aaa aaa\r\n    \r\nline\r\nline");
    for (column, row, kind) in [
        (0, 0, SelectionKind::Character),
        (2, 1, SelectionKind::Word),
        (0, 2, SelectionKind::Line),
    ] {
        terminal.begin_selection(column, row, kind);
        assert!(marked(&terminal.snapshot()).is_empty());
    }
    terminal.begin_selection(0, 2, SelectionKind::Character);
    terminal.update_selection(3, 3);
    assert!(marked(&terminal.snapshot()).is_empty());
    terminal.select_all();
    assert!(terminal.selection_query().is_none());
    assert!(terminal.selection_visible_marks().is_none());
    terminal.begin_selection(0, 0, SelectionKind::Word);
    assert_eq!(marked(&terminal.snapshot()).len(), 6);
}

#[test]
fn clearing_or_overwriting_selection_clears_matches_but_appended_output_refreshes_them() {
    let mut terminal = Terminal::new(40, 4, 100);
    terminal.feed(b"request request\r\n");
    terminal.begin_selection(2, 0, SelectionKind::Word);
    assert_eq!(marked(&terminal.snapshot()).len(), 14);
    terminal.feed(b"request");
    assert_eq!(marked(&terminal.snapshot()).len(), 21);
    terminal.feed(b"\x1b[1;1Hchanged");
    assert!(terminal.snapshot().selection_invalidated);
    assert!(marked(&terminal.snapshot()).is_empty());
    terminal.begin_selection(9, 0, SelectionKind::Word);
    assert_eq!(marked(&terminal.snapshot()).len(), 14);
    terminal.clear_selection();
    assert!(marked(&terminal.snapshot()).is_empty());
}

#[test]
fn search_state_and_selection_matches_remain_independent() {
    let mut terminal = Terminal::new(30, 3, 100);
    terminal.feed(b"word word other");
    terminal.start_search("word".into(), true);
    while !terminal.search_step(16).complete {}
    let search = terminal.search_status();
    terminal.begin_selection(1, 0, SelectionKind::Word);
    let snapshot = terminal.snapshot();
    assert_eq!(snapshot.search, search);
    assert_eq!(marked(&snapshot).len(), 8);
    assert!(snapshot.lines[0].cells[5].search_highlight.is_some());
    terminal.set_selection_highlight(false);
    let disabled = terminal.snapshot();
    assert!(marked(&disabled).is_empty());
    assert!(disabled.has_selection);
    assert_eq!(disabled.search, search);
    assert!(disabled.lines[0].cells.iter().any(|c| matches!(
        c.search_highlight,
        Some(SearchHighlight::Match | SearchHighlight::Current)
    )));
    terminal.set_selection_highlight(true);
    terminal.set_highlight(false);
    assert_eq!(marked(&terminal.snapshot()).len(), 8);
    terminal.cancel_search();
    assert_eq!(marked(&terminal.snapshot()).len(), 8);
}

#[test]
fn unicode_combining_sequences_and_wide_spacers_match_as_complete_cells() {
    let mut terminal = Terminal::new(40, 3, 100);
    terminal.feed("错误 错误 错误码 e\u{301} e e\u{301}".as_bytes());
    terminal.begin_selection(1, 0, SelectionKind::Word);
    assert_eq!(terminal.selected_text().as_deref(), Some("错误"));
    assert_eq!(
        marked(&terminal.snapshot()),
        (0..4).chain(5..9).map(|col| (0, col)).collect::<Vec<_>>()
    );
    terminal.begin_selection(17, 0, SelectionKind::Word);
    assert_eq!(terminal.selected_text().as_deref(), Some("e\u{301}"));
    assert_eq!(marked(&terminal.snapshot()), [(0, 17), (0, 21)]);
    terminal.begin_selection(19, 0, SelectionKind::Word);
    assert_eq!(marked(&terminal.snapshot()), [(0, 19)]);
}

#[test]
fn soft_wrapped_words_match_across_rows_but_hard_line_breaks_do_not() {
    let mut terminal = Terminal::new(6, 6, 100);
    terminal.feed(b" abcdef abcdef\r\nabc\r\ndef");
    terminal.begin_selection(3, 0, SelectionKind::Word);
    assert_eq!(terminal.selected_text().as_deref(), Some("abcdef"));
    assert_eq!(
        marked(&terminal.snapshot()),
        [
            (0, 1),
            (0, 2),
            (0, 3),
            (0, 4),
            (0, 5),
            (1, 0),
            (1, 2),
            (1, 3),
            (1, 4),
            (1, 5),
            (2, 0),
            (2, 1)
        ]
    );
}

#[test]
fn leading_wide_spacer_does_not_break_matching_or_token_boundaries() {
    let mut terminal = Terminal::new(5, 5, 100);
    terminal.feed("abc 错误 错误 ".as_bytes());
    terminal.begin_selection(1, 1, SelectionKind::Word);
    assert_eq!(terminal.selected_text().as_deref(), Some("错误"));
    let snapshot = terminal.snapshot();
    assert!(
        snapshot.lines[1].cells[..4]
            .iter()
            .all(|cell| cell.selection_match)
    );
    assert!(
        snapshot.lines[2].cells[..4]
            .iter()
            .all(|cell| cell.selection_match)
    );
}

#[test]
fn matches_partially_outside_the_viewport_have_complete_word_context() {
    let mut terminal = Terminal::new(6, 2, 100);
    terminal.feed(b" abcdef abcdef ");
    // The first occurrence begins in history; its trailing f remains visible.
    terminal.begin_selection(3, 0, SelectionKind::Word);
    assert_eq!(terminal.selected_text().as_deref(), Some("abcdef"));
    assert_eq!(
        marked(&terminal.snapshot()),
        [(0, 0), (0, 2), (0, 3), (0, 4), (0, 5), (1, 0), (1, 1)]
    );
    terminal.scroll(1);
    assert_eq!(
        marked(&terminal.snapshot()),
        [
            (0, 1),
            (0, 2),
            (0, 3),
            (0, 4),
            (0, 5),
            (1, 0),
            (1, 2),
            (1, 3),
            (1, 4),
            (1, 5)
        ]
    );
}

#[test]
fn height_changes_preserve_matches_and_column_reflow_clears_invalidated_selection() {
    let mut terminal = Terminal::new(20, 3, 100);
    terminal.feed(b"word word");
    terminal.begin_selection(1, 0, SelectionKind::Word);
    terminal.resize(20, 6);
    assert_eq!(marked(&terminal.snapshot()).len(), 8);
    terminal.resize(10, 6);
    assert!(terminal.snapshot().selection_invalidated);
    assert!(marked(&terminal.snapshot()).is_empty());
    terminal.begin_selection(1, 0, SelectionKind::Word);
    assert_eq!(marked(&terminal.snapshot()).len(), 8);
}

#[test]
fn concealed_output_is_not_used_as_query_or_match_and_screen_switch_drops_marks() {
    let mut terminal = Terminal::new(40, 3, 100);
    terminal.feed(b"secret \x1b[8msecret\x1b[0m secret");
    terminal.begin_selection(1, 0, SelectionKind::Word);
    assert_eq!(
        marked(&terminal.snapshot()),
        (0..6).chain(14..20).map(|col| (0, col)).collect::<Vec<_>>()
    );
    terminal.begin_selection(8, 0, SelectionKind::Word);
    assert!(marked(&terminal.snapshot()).is_empty());
    terminal.feed(b"\x1b[?1049h\x1b[?1000h\x1b[Hword word");
    assert!(terminal.snapshot().mouse_reporting);
    assert!(marked(&terminal.snapshot()).is_empty());
    // An explicit Shift selection remains usable in mouse-reporting applications.
    terminal.begin_selection(1, 0, SelectionKind::Word);
    assert_eq!(marked(&terminal.snapshot()).len(), 8);
    assert!(terminal.snapshot().mouse_reporting);
    terminal.feed(b"\x1b[?1049l");
    assert!(marked(&terminal.snapshot()).is_empty());
}

#[test]
fn unchanged_view_reuses_cache_and_offscreen_selection_only_matches_visible_output() {
    let mut terminal = Terminal::new(20, 3, 100);
    terminal.feed(b"word\r\nother\r\nword\r\nother\r\nword");
    terminal.scroll(2);
    terminal.begin_selection(1, 0, SelectionKind::Word);
    let first = terminal.selection_visible_marks().unwrap();
    assert!(Arc::ptr_eq(
        &first,
        &terminal.selection_visible_marks().unwrap()
    ));
    terminal.scroll(-2);
    assert_eq!(terminal.selected_text().as_deref(), Some("word"));
    assert_eq!(marked(&terminal.snapshot()).len(), 8);
    let next = terminal.selection_visible_marks().unwrap();
    assert!(!Arc::ptr_eq(&first, &next));
    assert_eq!(next.len(), 60);
}

#[test]
fn overlong_words_and_combining_sequences_are_bounded_before_matching() {
    let mut terminal = Terminal::new(300, 4, 100);
    terminal.feed("a".repeat(MAX_QUERY_CELLS + 1).as_bytes());
    terminal.begin_selection(0, 0, SelectionKind::Word);
    assert!(terminal.selection_visible_marks().is_none());
    terminal.clear_selection();
    terminal.feed(b"\r\n");
    terminal.feed(format!("{} ", format!("a{}", "\u{301}".repeat(32)).repeat(20)).as_bytes());
    terminal.begin_selection(0, 1, SelectionKind::Word);
    assert!(terminal.selection_visible_marks().is_none());
}
