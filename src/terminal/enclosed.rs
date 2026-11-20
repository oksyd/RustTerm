use super::{Terminal, delimiters};
use alacritty_terminal::{
    grid::Dimensions,
    index::{Column, Line, Point},
    term::cell::{Flags, LineLength},
};

const MAX_CELLS: usize = 16 * 1024;
const MAX_BYTES: usize = 64 * 1024;
const MAX_ROWS: i32 = 256;

pub(super) enum Enclosure {
    Empty,
    Content(Point, Point),
}

impl Terminal {
    pub(super) fn enclosed_range(&self, mut clicked: Point) -> Option<Enclosure> {
        let grid = self.term.grid();
        // Map to the glyph itself. expand_wide(Left) instead includes the
        // previous row's padding when a wide glyph wrapped at the edge.
        if grid[clicked]
            .flags
            .contains(Flags::LEADING_WIDE_CHAR_SPACER)
        {
            if clicked.line == grid.bottommost_line() {
                return None;
            }
            clicked = Point::new(clicked.line + 1, Column(0));
        } else if grid[clicked].flags.contains(Flags::WIDE_CHAR_SPACER) {
            clicked.column -= 1;
        }
        if grid[clicked].flags.contains(Flags::HIDDEN) {
            return None;
        }
        let columns = usize::from(self.size.columns);
        let wraps = |line| {
            grid[Point::new(line, Column(columns - 1))]
                .flags
                .contains(Flags::WRAPLINE)
        };
        let mut first = clicked.line;
        while first > grid.topmost_line() && wraps(first - 1) {
            first -= 1;
            if clicked.line.0 - first.0 >= MAX_ROWS {
                return None;
            }
        }
        let mut last = clicked.line;
        while last < grid.bottommost_line() && wraps(last) {
            last += 1;
            if last.0 - first.0 >= MAX_ROWS {
                return None;
            }
        }
        if (last.0 - first.0 + 1) as usize * columns > MAX_CELLS {
            return None;
        }
        let mut text = String::new();
        let mut cells = Vec::new();
        let mut hit = None;
        for row in first.0..=last.0 {
            let line = Line(row);
            for column in 0..grid[line].line_length().0 {
                let point = Point::new(line, Column(column));
                let cell = &grid[point];
                if cell
                    .flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    continue;
                }
                if point == clicked {
                    hit = Some(text.len());
                }
                cells.push((text.len(), point));
                if cell.flags.contains(Flags::HIDDEN) {
                    text.push(' ');
                } else {
                    text.push(cell.c);
                    text.extend(cell.zerowidth().unwrap_or_default());
                }
                if text.len() > MAX_BYTES {
                    return None;
                }
            }
        }
        let hit = hit?;
        let pair = delimiters::pairs(&text, std::iter::empty())
            .into_iter()
            .filter(|pair| pair.open <= hit && hit <= pair.close)
            .min_by_key(|pair| pair.close - pair.open)?;
        let mut content = cells
            .iter()
            .filter(|(byte, _)| pair.open < *byte && *byte < pair.close);
        let Some(&(_, start)) = content.next() else {
            return Some(Enclosure::Empty);
        };
        let end = content.next_back().map_or(start, |&(_, point)| point);
        Some(Enclosure::Content(start, end))
    }
}

#[cfg(test)]
mod tests {
    use crate::terminal::{SelectionKind, Terminal};

    #[test]
    fn enclosing_selection_chooses_the_nearest_pair_without_delimiters() {
        let text = "prefix (alpha [beta gamma]) suffix";
        for (target, expected) in [
            ("(", "alpha [beta gamma]"),
            (")", "alpha [beta gamma]"),
            ("alpha", "alpha [beta gamma]"),
            ("beta", "beta gamma"),
            ("]", "beta gamma"),
        ] {
            let mut terminal = Terminal::new(80, 3, 16);
            terminal.feed(text.as_bytes());
            let column = text.find(target).unwrap() as u16;
            terminal.begin_selection(column, 0, SelectionKind::Enclosed);
            terminal.update_selection(column, 0);
            assert_eq!(
                terminal.selected_text().as_deref(),
                Some(expected),
                "{target}"
            );
        }
    }

    #[test]
    fn enclosing_selection_understands_quotes_escapes_and_apostrophes() {
        for (text, target, expected) in [
            (
                r#"value="a \"quoted\" tail""#,
                "quoted",
                r#"a \"quoted\" tail"#,
            ),
            ("don't change 'alpha beta'", "alpha", "alpha beta"),
            (r#"value='it\'s fine'"#, "fine", r#"it\'s fine"#),
            (r#"value="a [b] (c)""#, "b", "a [b] (c)"),
            ("value=\x60alpha beta\x60", "beta", "alpha beta"),
        ] {
            let mut terminal = Terminal::new(80, 3, 16);
            terminal.feed(text.as_bytes());
            terminal.begin_selection(
                text.find(target).unwrap() as u16,
                0,
                SelectionKind::Enclosed,
            );
            assert_eq!(
                terminal.selected_text().as_deref(),
                Some(expected),
                "{text}"
            );
        }
    }

    #[test]
    fn enclosing_selection_handles_wrapped_wide_and_combining_text() {
        for text in [
            "prefix (中文 e\u{301} value)",
            "prefix(中文 e\u{301} value)",
        ] {
            for half in [0, 1] {
                let mut terminal = Terminal::new(8, 8, 16);
                terminal.feed(text.as_bytes());
                let snapshot = terminal.snapshot();
                let (row, column) = snapshot
                    .lines
                    .iter()
                    .enumerate()
                    .find_map(|(row, line)| {
                        line.cells
                            .iter()
                            .position(|cell| cell.text == "中")
                            .map(|col| (row, col))
                    })
                    .unwrap();
                // Both halves work, including a wide glyph wrapped from the last column.
                terminal.begin_selection(column as u16 + half, row as u16, SelectionKind::Enclosed);
                terminal.update_selection(column as u16 + half, row as u16);
                assert_eq!(
                    terminal.selected_text().as_deref(),
                    Some("中文 e\u{301} value"),
                    "{text}, {half}"
                );
            }
        }
    }

    #[test]
    fn enclosing_selection_falls_back_to_words_and_clears_empty_pairs() {
        for text in [
            "unmatched (alpha beta",
            "plain alpha beta",
            "(first\r\nalpha)",
        ] {
            let mut terminal = Terminal::new(80, 3, 16);
            terminal.feed(text.as_bytes());
            let (row, col) = if text.contains('\n') {
                (1, 0)
            } else {
                (0, text.find("alpha").unwrap())
            };
            terminal.begin_selection(col as u16, row, SelectionKind::Enclosed);
            assert_eq!(terminal.selected_text().as_deref(), Some("alpha"));
        }
        for text in ["()", "[]", "{}", "\"\"", "''", "(\u{301})"] {
            let mut terminal = Terminal::new(80, 3, 16);
            terminal.feed(text.as_bytes());
            terminal.begin_selection(0, 0, SelectionKind::Enclosed);
            assert!(terminal.selected_text().is_none(), "{text}");
        }
    }

    #[test]
    fn enclosing_selection_ignores_mismatches_without_changing_normal_selection() {
        for (text, column, expected) in [
            ("([alpha beta)]", 3, "alpha"),
            ("(broken [alpha beta]", 10, "alpha beta"),
            ("\\(alpha beta\\)", 3, "alpha"),
        ] {
            let mut terminal = Terminal::new(40, 3, 16);
            terminal.feed(text.as_bytes());
            terminal.begin_selection(column, 0, SelectionKind::Enclosed);
            assert_eq!(
                terminal.selected_text().as_deref(),
                Some(expected),
                "{text}"
            );
        }
        let mut terminal = Terminal::new(40, 3, 16);
        terminal.feed(b"prefix (alpha beta) suffix");
        terminal.begin_selection(9, 0, SelectionKind::Word);
        assert_eq!(terminal.selected_text().as_deref(), Some("alpha"));
        terminal.begin_selection(7, 0, SelectionKind::Word);
        assert_eq!(terminal.selected_text().as_deref(), Some("(alpha beta)"));
        terminal.begin_selection(9, 0, SelectionKind::Line);
        assert_eq!(
            terminal.selected_text().as_deref(),
            Some("prefix (alpha beta) suffix")
        );
    }

    #[test]
    fn enclosing_selection_extends_and_reverses_without_losing_its_initial_content() {
        let mut terminal = Terminal::new(40, 3, 16);
        terminal.feed(b"before (alpha beta) after");
        terminal.begin_selection(7, 0, SelectionKind::Enclosed);
        for (column, expected) in [
            (7, "alpha beta"),
            (24, "alpha beta) after"),
            (0, "before (alpha beta"),
            (12, "alpha beta"),
        ] {
            terminal.update_selection(column, 0);
            assert_eq!(terminal.selected_text().as_deref(), Some(expected));
        }
        terminal.feed(b"\r\nnext\r\nlast\r\nend");
        terminal.scroll(1);
        terminal.update_selection(24, 0);
        assert_eq!(
            terminal.selected_text().as_deref(),
            Some("alpha beta) after")
        );
        terminal.feed(b"\x1b[3J");
        assert!(terminal.selected_text().is_none());
        assert!(terminal.snapshot().selection_invalidated);
    }

    #[test]
    fn enclosing_selection_copies_exact_spaces_and_rejects_changed_output() {
        for text in ["(   )", "[alpha   ]", "{  alpha beta  }"] {
            let mut terminal = Terminal::new(40, 3, 16);
            terminal.feed(text.as_bytes());
            terminal.begin_selection(0, 0, SelectionKind::Enclosed);
            assert_eq!(
                terminal.selected_text().as_deref(),
                Some(&text[1..text.len() - 1])
            );
            terminal.feed(b"\r\x1b[CX");
            terminal.update_selection(0, 0);
            assert!(terminal.selected_text().is_none());
            assert!(terminal.snapshot().selection_invalidated);
        }
    }

    #[test]
    fn enclosing_selection_handles_alternate_screen_and_escaped_delimiters() {
        let mut terminal = Terminal::new(60, 3, 16);
        terminal.feed(b"main\x1b[?1049h\x1b[H{value: (one \\) two), text: \"three four\"}");
        terminal.begin_selection(10, 0, SelectionKind::Enclosed);
        assert_eq!(terminal.selected_text().as_deref(), Some("one \\) two"));
        terminal.begin_selection(29, 0, SelectionKind::Enclosed);
        assert_eq!(terminal.selected_text().as_deref(), Some("three four"));
        terminal.feed(b"\x1b[?1049l");
        assert!(terminal.selected_text().is_none());
    }

    #[test]
    fn enclosing_selection_bounds_work_for_long_logical_lines() {
        let mut terminal = Terminal::new(80, 20, 512);
        terminal.feed(format!("(alpha {} beta)", "x".repeat(super::MAX_CELLS)).as_bytes());
        terminal.scroll(1000);
        terminal.begin_selection(2, 0, SelectionKind::Enclosed);
        assert_eq!(terminal.selected_text().as_deref(), Some("alpha"));

        let mut terminal = Terminal::new(4, 20, 512);
        terminal.feed(format!("({} foo)", "x".repeat(4 * super::MAX_ROWS as usize)).as_bytes());
        let snapshot = terminal.snapshot();
        let (row, column) = snapshot
            .lines
            .iter()
            .enumerate()
            .find_map(|(row, line)| {
                line.cells
                    .iter()
                    .position(|cell| cell.text == "f")
                    .map(|column| (row, column))
            })
            .unwrap();
        terminal.begin_selection(column as u16, row as u16, SelectionKind::Enclosed);
        assert_eq!(terminal.selected_text().as_deref(), Some("foo"));
    }
}
