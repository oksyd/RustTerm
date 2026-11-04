//! Receipt times are side metadata; terminal cells remain authoritative.
use alacritty_terminal::{
    grid::{Dimensions, Grid},
    index::{Column, Line, Point},
    term::cell::{Cell, Flags, LineLength},
    vte::{Params, Parser, Perform},
};
use chrono::Timelike;
use std::{
    collections::HashMap,
    hash::{DefaultHasher, Hash, Hasher},
    ops::Range,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineTimestamp(u32);

impl LineTimestamp {
    pub(super) fn now() -> Self {
        Self(chrono::Local::now().num_seconds_from_midnight())
    }
    #[cfg(test)]
    pub(super) fn at(seconds: u32) -> Self {
        Self(seconds)
    }
}

impl std::fmt::Display for LineTimestamp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:02}:{:02}:{:02}",
            self.0 / 3600,
            self.0 / 60 % 60,
            self.0 % 60
        )
    }
}

#[derive(Default)]
pub(super) struct Observation {
    pub printed: bool,
    pub linefeed: bool,
    pub cleanup: Option<Range<usize>>,
    pub bulk: bool,
    pub repeat: bool,
    pub previous_time: Option<LineTimestamp>,
    pub enter_alternate: bool,
    pub reset: bool,
    edit: Option<(char, usize)>,
    scroll_region: Option<(usize, Option<usize>)>,
    reset_region: bool,
    reverse_index: bool,
}

impl Perform for Observation {
    fn print(&mut self, _: char) {
        self.printed = true;
    }
    fn execute(&mut self, byte: u8) {
        self.printed = byte == b'\t';
        self.linefeed = matches!(byte, b'\n' | 0x0b | 0x0c);
    }
    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        if ignore {
            return;
        }
        if intermediates == b"?" && action == 'h' {
            self.enter_alternate = params
                .iter()
                .any(|p| matches!(p.first(), Some(47 | 1047 | 1049)));
        }
        self.reset_region = intermediates == b"?"
            && matches!(action, 'h' | 'l')
            && params.iter().any(|p| p.first() == Some(&3));
        if intermediates.is_empty() {
            let mut params = params.iter().map(|p| usize::from(p[0]));
            let first = params.next().unwrap_or(0);
            if action == 'r' {
                self.scroll_region = Some((first.max(1), params.next().filter(|p| *p != 0)));
            }
            if matches!(action, 'J' | 'K' | 'X' | 'P' | 'L' | 'M' | 'S' | 'T') {
                self.edit = Some((action, first));
            }
            self.bulk = action == 'b';
            self.repeat = action == 'b';
        }
    }
    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        if ignore {
            return;
        }
        self.reset = intermediates.is_empty() && byte == b'c';
        self.linefeed = intermediates.is_empty() && matches!(byte, b'D' | b'E');
        self.reverse_index = intermediates.is_empty() && byte == b'M';
        self.bulk = intermediates == b"#" && byte == b'8';
    }
}

#[derive(Default)]
pub(super) struct LineTimes {
    parser: Parser,
    times: HashMap<usize, LineTimestamp>,
    primary: Option<LogicalLines>,
    primary_resized: bool,
    scroll_region: Option<Range<usize>>,
}

struct LogicalLines {
    lines: Vec<LogicalTime>,
    cursor: usize,
}

struct LogicalTime {
    fingerprint: u64,
    has_content: bool,
    time: Option<LineTimestamp>,
    rows: Vec<usize>,
}

pub(super) fn address(grid: &Grid<Cell>, line: Line) -> usize {
    &grid[Point::new(line, Column(0))] as *const _ as usize
}

impl LineTimes {
    pub fn observe(&mut self, byte: u8, grid: &Grid<Cell>, alternate: bool) -> Observation {
        let mut event = Observation::default();
        self.parser.advance(&mut event, &[byte]);
        let rows = grid.screen_lines();
        let cursor = grid.cursor.point.line.0 as usize;
        if event.reset || event.reset_region {
            self.scroll_region = None;
        }
        if let Some((top, bottom)) = event.scroll_region {
            let bottom = bottom.unwrap_or(rows);
            if top < bottom {
                self.scroll_region = Some((top - 1).min(rows)..bottom.min(rows));
            }
        }
        let region = self.scroll_region.clone().unwrap_or(0..rows);
        event.cleanup = if event.reset_region {
            Some(0..rows)
        } else if event.linefeed && cursor + 1 == region.end {
            Some(region.end - 1..region.end)
        } else if event.reverse_index && cursor == region.start {
            Some(region.start..(region.start + 1).min(region.end))
        } else {
            event.edit.and_then(|(action, param)| {
                let count = param.max(1).min(region.end - region.start);
                match action {
                    'J' => match param {
                        0 => Some(cursor..rows),
                        1 if cursor > 1 => Some(0..cursor + 1),
                        1 => Some(cursor..cursor + 1),
                        2 => Some(0..rows),
                        _ => None,
                    },
                    'K' if param <= 2 && !(param == 0 && grid.cursor.input_needs_wrap) => {
                        Some(cursor..cursor + 1)
                    }
                    'X' | 'P' => Some(cursor..cursor + 1),
                    'L' if region.contains(&cursor) => {
                        Some(cursor..(cursor + count).min(region.end))
                    }
                    'M' if region.contains(&cursor) => {
                        Some((region.end - count).max(cursor)..region.end)
                    }
                    'S' => Some(region.end - count..region.end),
                    'T' => Some(region.start..region.start + count),
                    _ => None,
                }
            })
        };
        if event.repeat {
            event.previous_time = self.get(grid, grid.cursor.point.line);
        }
        if event.enter_alternate && !alternate {
            self.primary = Some(self.logical_lines(grid));
            self.primary_resized = false;
        }
        event
    }

    pub fn retire(&mut self, address: usize) {
        self.times.remove(&address);
    }

    pub fn written(&mut self, grid: &Grid<Cell>, now: LineTimestamp) {
        let line = grid.cursor.point.line;
        let previous = Line(line.0 - 1);
        let inherited = (previous >= grid.topmost_line()
            && grid[previous][Column(grid.columns() - 1)]
                .flags
                .contains(Flags::WRAPLINE))
        .then(|| self.get(grid, previous))
        .flatten();
        self.times
            .entry(address(grid, line))
            .or_insert(inherited.unwrap_or(now));
    }

    pub fn cleanup_screen(&mut self, grid: &Grid<Cell>, rows: Range<usize>) {
        for row in rows {
            let line = Line(row as i32);
            if grid[line].line_length().0 == 0 {
                self.retire(address(grid, line));
            }
        }
    }

    pub fn stamp_bulk(&mut self, grid: &Grid<Cell>, event: &Observation, now: LineTimestamp) {
        if !event.repeat {
            for row in 0..grid.screen_lines() {
                self.times.insert(address(grid, Line(row as i32)), now);
            }
            return;
        }
        let mut line = grid.cursor.point.line;
        if grid[line].line_length().0 == 0 {
            return;
        }
        let time = event.previous_time.unwrap_or(now);
        loop {
            self.times.insert(address(grid, line), time);
            let previous = Line(line.0 - 1);
            if previous < grid.topmost_line()
                || !grid[previous][Column(grid.columns() - 1)]
                    .flags
                    .contains(Flags::WRAPLINE)
            {
                break;
            }
            line = previous;
        }
    }

    pub fn retain(&mut self, tracker: &super::reading::Tracker) {
        self.times
            .retain(|address, _| tracker.contains_address(*address));
        if self.times.capacity() > tracker.positions.len().saturating_mul(2) {
            self.times.shrink_to_fit();
        }
    }

    pub fn get(&self, grid: &Grid<Cell>, line: Line) -> Option<LineTimestamp> {
        self.times.get(&address(grid, line)).copied()
    }

    pub fn before_resize(&mut self, grid: &Grid<Cell>, alternate: bool) {
        if alternate {
            self.primary_resized = true;
        } else {
            self.primary = Some(self.logical_lines(grid));
        }
    }

    pub fn after_resize(&mut self, grid: &Grid<Cell>, alternate: bool) {
        self.scroll_region = None;
        if !alternate && let Some(previous) = self.primary.take() {
            self.restore(grid, previous);
        }
    }

    pub fn primary_restored(&mut self, grid: &Grid<Cell>) {
        if let Some(previous) = self.primary.take()
            && self.primary_resized
        {
            self.restore(grid, previous);
        }
        self.primary_resized = false;
    }

    pub fn reset(&mut self) {
        self.times.clear();
        self.primary = None;
        self.primary_resized = false;
        self.scroll_region = None;
    }

    fn logical_lines(&self, grid: &Grid<Cell>) -> LogicalLines {
        let mut lines = Vec::new();
        let mut cursor = 0;
        let mut hash = DefaultHasher::new();
        let mut has_content = false;
        let mut time = None;
        let mut rows = Vec::new();
        for line in grid.topmost_line().0..=grid.bottommost_line().0 {
            let line = Line(line);
            if line == grid.cursor.point.line {
                cursor = lines.len();
            }
            rows.push(address(grid, line));
            time = time.or(self.get(grid, line));
            let row = &grid[line];
            for col in 0..row.line_length().0 {
                let cell = &row[Column(col)];
                if cell
                    .flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    continue;
                }
                has_content |=
                    !cell.c.is_whitespace() || !cell.zerowidth().unwrap_or_default().is_empty();
                cell.c.hash(&mut hash);
                for mark in cell.zerowidth().unwrap_or_default() {
                    mark.hash(&mut hash);
                }
            }
            if !row[Column(grid.columns() - 1)]
                .flags
                .contains(Flags::WRAPLINE)
                || line == grid.bottommost_line()
            {
                lines.push(LogicalTime {
                    fingerprint: hash.finish(),
                    has_content,
                    time,
                    rows: std::mem::take(&mut rows),
                });
                hash = DefaultHasher::new();
                has_content = false;
                time = None;
            }
        }
        LogicalLines { lines, cursor }
    }

    fn restore(&mut self, grid: &Grid<Cell>, previous: LogicalLines) {
        let current = self.logical_lines(grid);
        let mut indices: HashMap<u64, Vec<usize>> = HashMap::new();
        for (index, line) in previous.lines.iter().enumerate() {
            if line.has_content {
                indices.entry(line.fingerprint).or_default().push(index);
            }
        }
        let mut matches = vec![None; current.lines.len()];
        // Reflow evicts from the oldest end; match equal repeated lines from the bottom.
        let mut boundary = previous.lines.len();
        for (index, line) in current.lines.iter().enumerate().rev() {
            // A larger viewport adds empty rows. Matching them against earlier
            // blank separators would move the boundary past retained output.
            if !line.has_content {
                continue;
            }
            let matched = indices.get_mut(&line.fingerprint).and_then(|indices| {
                while indices.last().is_some_and(|index| *index >= boundary) {
                    indices.pop();
                }
                indices.pop()
            });
            // Only the first retained logical line may have lost a prefix to eviction.
            let matched = matched.or_else(|| {
                (index == 0)
                    .then(|| {
                        previous.lines[..boundary]
                            .iter()
                            .rposition(|line| line.has_content)
                    })
                    .flatten()
            });
            if let Some(matched) = matched {
                boundary = matched;
                matches[index] = Some(matched);
            }
        }
        // A blank cursor row is an anchor too, including on an all-blank screen.
        // Its position distinguishes emitted newlines from new viewport padding.
        if !current.lines[current.cursor].has_content
            && !previous.lines[previous.cursor].has_content
            && matches[..current.cursor]
                .iter()
                .flatten()
                .all(|index| *index < previous.cursor)
            && matches[current.cursor + 1..]
                .iter()
                .flatten()
                .all(|index| *index > previous.cursor)
        {
            matches[current.cursor] = Some(previous.cursor);
        }
        let anchors: Vec<_> = matches
            .iter()
            .enumerate()
            .filter_map(|(index, matched)| matched.map(|old| (index, old)))
            .collect();
        // Blank separators are matched relative to retained text/cursor anchors,
        // never by an empty fingerprint that could consume unused bottom rows.
        for &(mut index, mut old) in &anchors {
            while index > 0 && old > 0 {
                index -= 1;
                old -= 1;
                if matches[index].is_some()
                    || current.lines[index].has_content
                    || previous.lines[old].has_content
                {
                    break;
                }
                matches[index] = Some(old);
            }
        }
        if let Some(&(index, old)) = anchors.last() {
            for (index, old) in (index + 1..current.lines.len()).zip(old + 1..previous.lines.len())
            {
                if current.lines[index].has_content || previous.lines[old].has_content {
                    break;
                }
                matches[index] = Some(old);
            }
        }
        self.times.clear();
        for (line, matched) in current.lines.into_iter().zip(matches) {
            if let Some(time) = matched.and_then(|index| previous.lines[index].time) {
                for row in line.rows {
                    self.times.insert(row, time);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::Terminal;

    fn retained(term: &Terminal) -> Vec<(String, Option<LineTimestamp>)> {
        let grid = term.term.grid();
        (grid.topmost_line().0..=grid.bottommost_line().0)
            .filter_map(|line| {
                let line = Line(line);
                let row = &grid[line];
                let text: String = (0..row.line_length().0)
                    .map(|col| row[Column(col)].c)
                    .collect();
                (!text.is_empty()).then(|| (text, term.line_times.get(grid, line)))
            })
            .collect()
    }

    fn used_times(term: &Terminal) -> Vec<LineTimestamp> {
        let grid = term.term.grid();
        (grid.topmost_line().0..=grid.bottommost_line().0)
            .filter_map(|row| term.line_times.get(grid, Line(row)))
            .collect()
    }

    #[test]
    fn output_blank_lines_have_metadata_but_unused_bottom_rows_do_not() {
        let mut term = Terminal::new(20, 8, 10);
        assert!(
            term.snapshot()
                .lines
                .iter()
                .all(|line| line.number.is_none())
        );
        term.feed_at(b"first\r\n", LineTimestamp::at(1));
        term.feed_at(b"\r\n \t \r\n", LineTimestamp::at(2));
        term.feed_at(b"last\r\n", LineTimestamp::at(3));
        let snapshot = term.snapshot();
        for (row, seconds) in [1, 2, 2, 3].into_iter().enumerate() {
            assert_eq!(snapshot.lines[row].number, Some(row + 1));
            assert_eq!(
                snapshot.lines[row].received_at,
                Some(LineTimestamp::at(seconds))
            );
        }
        assert!(
            snapshot.lines[1]
                .cells
                .iter()
                .all(|cell| cell.text.trim().is_empty())
        );
        assert!(
            snapshot.lines[2]
                .cells
                .iter()
                .all(|cell| cell.text.trim().is_empty())
        );
        assert!(
            snapshot.lines[4..]
                .iter()
                .all(|line| { line.number.is_none() && line.received_at.is_none() })
        );
        term.select_all();
        let copied = term.selected_text().unwrap();
        assert_eq!(
            copied
                .lines()
                .take(4)
                .map(str::trim_end)
                .collect::<Vec<_>>(),
            vec!["first", "", "", "last"]
        );
        assert!(!copied.contains("00:00:"));
    }

    #[test]
    fn blank_output_survives_scrolling_recycling_and_resize_without_filling_padding() {
        let mut term = Terminal::new(8, 3, 4);
        for seconds in 1..=9 {
            term.feed_at(b"\r\n", LineTimestamp::at(seconds));
        }
        let expected: Vec<_> = (4..=9).map(LineTimestamp::at).collect();
        assert_eq!(used_times(&term), expected);
        for (columns, rows) in [(8, 10), (20, 10), (3, 3), (8, 3)] {
            term.resize(columns, rows);
            assert_eq!(used_times(&term), expected, "size={columns}x{rows}");
            let snapshot = term.snapshot();
            assert!(
                snapshot.lines[..snapshot.cursor.row]
                    .iter()
                    .all(|line| { line.number.is_some() && line.received_at.is_some() })
            );
            assert!(
                snapshot.lines[snapshot.cursor.row..]
                    .iter()
                    .all(|line| { line.number.is_none() && line.received_at.is_none() })
            );
        }
        term.scroll_to(usize::MAX);
        assert_eq!(
            term.snapshot().lines[0].received_at,
            Some(LineTimestamp::at(4))
        );
        assert!(term.line_times.times.len() <= term.term.grid().total_lines());
    }

    #[test]
    fn clearing_one_line_does_not_remove_metadata_from_other_blank_output() {
        let mut term = Terminal::new(12, 6, 10);
        term.feed_at(b"first\r\n\r\nlast", LineTimestamp::at(1));
        term.feed_at(b"\r\x1b[Knew\x1b[J\x1b[3J", LineTimestamp::at(2));
        let snapshot = term.snapshot();
        assert_eq!(snapshot.lines[1].number, Some(2));
        assert_eq!(snapshot.lines[1].received_at, Some(LineTimestamp::at(1)));
        assert_eq!(snapshot.lines[2].received_at, Some(LineTimestamp::at(2)));
        assert!(snapshot.lines[3..].iter().all(|line| line.number.is_none()));
        term.feed_at(b"\x1b[H\x1b[2J", LineTimestamp::at(3));
        assert!(
            term.snapshot()
                .lines
                .iter()
                .all(|line| { line.number.is_none() && line.received_at.is_none() })
        );
    }

    #[test]
    fn blank_output_moves_with_scroll_regions_and_insert_delete_lines() {
        let mut term = Terminal::new(12, 5, 10);
        let first = LineTimestamp::at(1);
        term.feed_at(b"\r\nb\r\n\r\nd\r\nz", first);
        term.feed_at(b"\x1b[2;4r\x1b[4;1H\n", LineTimestamp::at(2));
        let snapshot = term.snapshot();
        assert_eq!(snapshot.lines[0].received_at, Some(first));
        assert_eq!(snapshot.lines[1].received_at, Some(first));
        assert_eq!(snapshot.lines[2].received_at, Some(first));
        assert_eq!(snapshot.lines[3].received_at, None);
        term.feed_at(b"\x1b[2;1H\x1b[L", LineTimestamp::at(3));
        let snapshot = term.snapshot();
        assert_eq!(snapshot.lines[0].received_at, Some(first));
        assert_eq!(snapshot.lines[1].received_at, None);
        assert_eq!(snapshot.lines[2].received_at, Some(first));
        assert_eq!(snapshot.lines[4].received_at, Some(first));
        term.feed_at(b"\x1b[M", LineTimestamp::at(4));
        let snapshot = term.snapshot();
        assert_eq!(snapshot.lines[1].received_at, Some(first));
        assert_eq!(snapshot.lines[3].received_at, None);
    }

    #[test]
    fn whitespace_only_output_keeps_metadata_when_soft_wraps_merge() {
        let mut term = Terminal::new(5, 4, 10);
        let time = LineTimestamp::at(1);
        term.feed_at(b"          ", time);
        assert_eq!(used_times(&term), vec![time, time]);
        term.resize(20, 8);
        assert_eq!(used_times(&term), vec![time]);
        assert_eq!(term.snapshot().lines[0].number, Some(1));
        assert!(
            term.snapshot().lines[1..]
                .iter()
                .all(|line| line.number.is_none())
        );
    }

    #[test]
    fn receipt_time_is_recorded_once_and_survives_overwrite_and_snapshots() {
        let mut term = Terminal::new(20, 3, 10);
        let first = LineTimestamp::at(3723);
        let second = LineTimestamp::at(4000);
        assert_eq!(first.to_string(), "01:02:03");
        assert!(
            term.snapshot()
                .lines
                .iter()
                .all(|line| line.received_at.is_none())
        );
        term.feed_at(b"abc", first);
        term.feed_at(b"def\rX", second);
        assert_eq!(term.snapshot().lines[0].received_at, Some(first));
        term.feed_at(b"\r\nnext", second);
        assert_eq!(term.snapshot().lines[1].received_at, Some(second));
        assert_eq!(term.snapshot().lines[0].received_at, Some(first));
        term.feed_at(b"\x1b[31m\x1b[3;1H", second);
        assert_eq!(term.snapshot().lines[2].received_at, None);
        assert_eq!(term.snapshot().lines[0].received_at, Some(first));
    }

    #[test]
    fn utf8_and_control_chunk_boundaries_keep_the_same_receipt_times() {
        let bytes = "中e\u{301}\x1b[31m红\x1b[0m\r\n\r\nnext".as_bytes();
        let time = LineTimestamp::at(1234);
        let mut reference = Terminal::new(8, 3, 10);
        reference.feed_at(bytes, time);
        for split in 0..=bytes.len() {
            let mut term = Terminal::new(8, 3, 10);
            term.feed_at(&bytes[..split], time);
            term.feed_at(&bytes[split..], time);
            assert_eq!(retained(&term), retained(&reference), "split={split}");
            assert_eq!(used_times(&term), used_times(&reference), "split={split}");
        }
    }

    #[test]
    fn identical_scrollback_lines_keep_their_own_times_after_row_recycling() {
        let mut term = Terminal::new(10, 2, 2);
        for seconds in 1..=6 {
            term.feed_at(b"same\r\n", LineTimestamp::at(seconds));
        }
        assert_eq!(
            retained(&term),
            (4..=6)
                .map(|seconds| ("same".into(), Some(LineTimestamp::at(seconds))))
                .collect::<Vec<_>>()
        );
        term.scroll_to(usize::MAX);
        assert_eq!(
            term.snapshot().lines[0].received_at,
            Some(LineTimestamp::at(4))
        );
        term.scroll_to_bottom();
        assert_eq!(
            term.snapshot().lines[0].received_at,
            Some(LineTimestamp::at(6))
        );
        assert!(term.line_times.times.len() <= term.term.grid().total_lines());
    }

    #[test]
    fn wrapped_output_keeps_time_through_reflow_and_copy_excludes_metadata() {
        let mut term = Terminal::new(5, 3, 10);
        let first = LineTimestamp::at(10);
        let second = LineTimestamp::at(20);
        term.feed_at(b"abcde", first);
        term.feed_at(b"fghij\r\nsecond", second);
        term.resize(20, 4);
        assert_eq!(
            retained(&term),
            vec![
                ("abcdefghij".into(), Some(first)),
                ("second".into(), Some(second))
            ]
        );
        term.select_all();
        let copied = term.selected_text().unwrap();
        assert!(copied.contains("abcdefghij"));
        assert!(!copied.contains("00:00:"));
    }

    #[test]
    fn alternate_screen_resize_and_freeze_preserve_main_times() {
        for freeze in [false, true] {
            let mut term = Terminal::new(5, 3, 10);
            let time = LineTimestamp::at(12);
            term.feed_at(b"abcdefgh\r\n\r\nmain", time);
            term.feed_at(b"\x1b[?1049halt", LineTimestamp::at(99));
            assert!(
                term.snapshot()
                    .lines
                    .iter()
                    .all(|line| line.received_at.is_none())
            );
            term.resize(20, 4);
            if freeze {
                term.freeze();
            } else {
                term.feed_at(b"\x1b[?1049l", LineTimestamp::at(100));
            }
            assert_eq!(
                retained(&term),
                vec![("abcdefgh".into(), Some(time)), ("main".into(), Some(time))]
            );
            assert_eq!(used_times(&term), vec![time; 3]);
        }
    }

    #[test]
    fn cleared_lines_and_partial_scroll_regions_do_not_reuse_old_times() {
        let mut term = Terminal::new(10, 4, 10);
        for (row, time) in [("a\r\n", 1), ("b\r\n", 2), ("c\r\n", 3), ("d", 4)] {
            term.feed_at(row.as_bytes(), LineTimestamp::at(time));
        }
        term.feed_at(b"\x1b[2;3r\x1b[3;1H\nnew", LineTimestamp::at(5));
        assert_eq!(
            retained(&term),
            vec![
                ("a".into(), Some(LineTimestamp::at(1))),
                ("c".into(), Some(LineTimestamp::at(3))),
                ("new".into(), Some(LineTimestamp::at(5))),
                ("d".into(), Some(LineTimestamp::at(4))),
            ]
        );
        term.feed_at(b"\r\x1b[2Kfresh", LineTimestamp::at(6));
        assert_eq!(
            term.snapshot().lines[2].received_at,
            Some(LineTimestamp::at(6))
        );
    }

    #[test]
    fn repeated_character_output_replaces_recycled_row_timestamps() {
        let mut term = Terminal::new(5, 2, 2);
        term.feed_at(b"old\r\nold\r\nold\r\n", LineTimestamp::at(1));
        term.feed_at(b"a\x1b[100b", LineTimestamp::at(99));
        assert!(
            retained(&term)
                .iter()
                .all(|(_, time)| *time == Some(LineTimestamp::at(99)))
        );
    }

    #[test]
    fn narrow_reflow_preserves_time_for_the_partially_evicted_oldest_line() {
        let mut term = Terminal::new(10, 2, 2);
        for time in 1..=6 {
            term.feed_at(b"same\r\n", LineTimestamp::at(time));
        }
        term.resize(2, 2);
        let rows = retained(&term);
        assert!(rows.iter().all(|(_, time)| time.is_some()));
        assert_eq!(rows.last().unwrap().1, Some(LineTimestamp::at(6)));
    }

    #[test]
    fn new_ssh_banner_times_survive_fullscreen_and_windowed_resize_cycles() {
        let mut term = Terminal::new(80, 24, 100);
        term.feed_at(b"Welcome to the server\r\n\r\n", LineTimestamp::at(1));
        term.feed_at(b"Last login: today\r\n\r\n", LineTimestamp::at(2));
        term.feed_at(b"user@server:~$ ", LineTimestamp::at(3));
        let expected = retained(&term);
        for (columns, rows) in [(80, 50), (160, 50), (80, 24), (50, 15), (160, 60), (80, 24)] {
            term.resize(columns, rows);
            assert_eq!(retained(&term), expected, "size={columns}x{rows}");
            let snapshot = term.snapshot();
            for (row, line) in snapshot.lines.iter().enumerate() {
                if row < 5 {
                    assert_eq!(line.number, Some(row + 1));
                    assert_eq!(
                        line.received_at,
                        Some(LineTimestamp::at([1, 1, 2, 2, 3][row]))
                    );
                } else {
                    assert_eq!(
                        line.received_at, None,
                        "new empty rows must not inherit a time"
                    );
                }
            }
        }
    }

    #[test]
    fn resize_does_not_confuse_repeated_output_separated_by_blank_lines() {
        let mut term = Terminal::new(10, 6, 100);
        term.feed_at(b"same\r\n\r\n", LineTimestamp::at(1));
        term.feed_at(b"same\r\n\r\n", LineTimestamp::at(2));
        term.feed_at(b"prompt> ", LineTimestamp::at(3));
        let expected = retained(&term);
        let expected_times = used_times(&term);
        for (columns, rows) in [(30, 30), (10, 6), (80, 50), (10, 6)] {
            term.resize(columns, rows);
            assert_eq!(retained(&term), expected, "size={columns}x{rows}");
            assert_eq!(used_times(&term), expected_times, "size={columns}x{rows}");
        }
    }
}
