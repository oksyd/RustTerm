//! Selection decoration is independent of search, copying and the remote PTY.
//! Scan the viewport plus bounded wrap context, never the entire scrollback.
use super::{SelectionKind, Terminal};
use alacritty_terminal::{
    grid::Dimensions,
    index::{Column, Line, Point},
    term::cell::{Cell, Flags},
};
use std::{collections::VecDeque, sync::Arc};

const MAX_QUERY_CELLS: usize = 256;
const MAX_QUERY_BYTES: usize = 1024;

#[derive(Default)]
pub(super) struct MatchCache {
    version: Option<u64>,
    marks: Option<Arc<[bool]>>,
}

// Keep combining characters attached to their base cell: selecting `e` must
// not match half of an `é`, and wide glyphs must mark both display columns.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Glyph<'a> {
    base: char,
    combining: &'a [char],
}
impl<'a> Glyph<'a> {
    fn from_cell(cell: &'a Cell) -> Self {
        Self {
            base: if cell.c == '\t' { ' ' } else { cell.c },
            combining: cell.zerowidth().unwrap_or_default(),
        }
    }
    fn bytes(self) -> usize {
        self.base.len_utf8() + self.combining.iter().map(|c| c.len_utf8()).sum::<usize>()
    }
}

fn spacer(cell: &Cell) -> bool {
    cell.flags
        .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
}
fn point(index: i32, columns: i32) -> Point {
    Point::new(
        Line(index.div_euclid(columns)),
        Column(index.rem_euclid(columns) as usize),
    )
}

impl Terminal {
    pub fn set_selection_highlight(&mut self, enabled: bool) {
        if self.selection_highlight_enabled == enabled {
            return;
        }
        self.selection_highlight_enabled = enabled;
        *self
            .selection_match_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = MatchCache::default();
        self.version = self.version.wrapping_add(1);
    }

    pub(super) fn selection_visible_marks(&self) -> Option<Arc<[bool]>> {
        if !self.selection_highlight_enabled {
            return None;
        }
        let mut cache = self
            .selection_match_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if cache.version != Some(self.version) {
            cache.marks = self
                .selection_query()
                .map(|query| self.match_visible(&query).into());
            cache.version = Some(self.version);
        }
        cache.marks.clone()
    }

    fn selection_query(&self) -> Option<Vec<Glyph<'_>>> {
        if self.selection_kind == Some(SelectionKind::Line) {
            return None;
        }
        let (first, last) = self.selection_points()?;
        let columns = i32::from(self.size.columns);
        let start = first.line.0 * columns + first.column.0 as i32;
        let end = last.line.0 * columns + last.column.0 as i32;
        // Check the extent before validating anchors or reading selection text;
        // Select All must not introduce a history-sized scan/allocation here.
        if end < start || end - start >= (MAX_QUERY_CELLS * 2) as i32 || !self.selection_valid(0) {
            return None;
        }
        let grid = self.term.grid();
        let mut query = Vec::new();
        let mut bytes = 0;
        for index in start..=end {
            let at = point(index, columns);
            if index != start
                && at.column.0 == 0
                && !grid[point(index - 1, columns)]
                    .flags
                    .contains(Flags::WRAPLINE)
            {
                return None;
            }
            let cell = &grid[at];
            if cell.flags.contains(Flags::HIDDEN) {
                return None;
            }
            if spacer(cell) {
                continue;
            }
            let glyph = Glyph::from_cell(cell);
            bytes += glyph.bytes();
            if bytes > MAX_QUERY_BYTES || query.len() >= MAX_QUERY_CELLS {
                return None;
            }
            query.push(glyph);
        }
        if query.is_empty() || query.iter().all(|glyph| glyph.base.is_whitespace()) {
            return None;
        }
        // A single mouse press starts a one-cell character selection; it should
        // focus the terminal without flashing every occurrence of that letter.
        if self.selection_kind == Some(SelectionKind::Character) && query.len() == 1 {
            return None;
        }
        Some(query)
    }

    fn match_visible(&self, query: &[Glyph<'_>]) -> Vec<bool> {
        let columns = i32::from(self.size.columns);
        let grid = self.term.grid();
        let view_start = -(grid.display_offset() as i32) * columns;
        let view_end = view_start + i32::from(self.size.rows) * columns;
        // Include enough context for a match crossing either viewport edge,
        // even when each selected glyph occupies two columns.
        let context = (query.len() * 2 + 2) as i32;
        let start = (view_start - context).max(grid.topmost_line().0 * columns);
        let end = (view_end + context).min((grid.bottommost_line().0 + 1) * columns);
        let mut marks = vec![false; (view_end - view_start) as usize];
        let mut marked_until = view_start;
        let mut recent = VecDeque::with_capacity(query.len());
        // Linear-time literal matching, including overlapping occurrences.
        let mut prefix = vec![0; query.len()];
        let mut matched = 0;
        for index in 1..query.len() {
            while matched > 0 && query[index] != query[matched] {
                matched = prefix[matched - 1];
            }
            if query[index] == query[matched] {
                matched += 1;
            }
            prefix[index] = matched;
        }
        matched = 0;
        for index in start..end {
            let at = point(index, columns);
            if at.column.0 == 0
                && at.line > grid.topmost_line()
                && !grid[point(index - 1, columns)]
                    .flags
                    .contains(Flags::WRAPLINE)
            {
                matched = 0;
                recent.clear();
            }
            let cell = &grid[at];
            if cell.flags.contains(Flags::HIDDEN) {
                matched = 0;
                recent.clear();
                continue;
            }
            if spacer(cell) {
                continue;
            }
            if recent.len() == query.len() {
                recent.pop_front();
            }
            recent.push_back(at);
            let glyph = Glyph::from_cell(cell);
            while matched > 0 && glyph != query[matched] {
                matched = prefix[matched - 1];
            }
            if glyph == query[matched] {
                matched += 1;
            }
            if matched != query.len() {
                continue;
            }
            let first = *recent
                .front()
                .expect("a nonempty match has a starting cell");
            let width = if cell.flags.contains(Flags::WIDE_CHAR) {
                2
            } else {
                1
            };
            let last = Point::new(at.line, Column(at.column.0 + width - 1));
            if self.selection_kind != Some(SelectionKind::Word)
                || self.word_boundaries(first, last, query)
            {
                let first = (first.line.0 * columns + first.column.0 as i32).max(marked_until);
                let last = (last.line.0 * columns + last.column.0 as i32 + 1).min(view_end);
                if first < last {
                    marks[(first - view_start) as usize..(last - view_start) as usize].fill(true);
                    marked_until = last;
                }
            }
            matched = prefix[matched - 1];
        }
        marks
    }

    fn word_boundaries(&self, first: Point, last: Point, query: &[Glyph<'_>]) -> bool {
        let delimiter = |c| self.term.semantic_escape_chars().contains(c);
        (delimiter(query[0].base)
            || self
                .neighbor(first, false)
                .is_none_or(|glyph| delimiter(glyph.base)))
            && (delimiter(query[query.len() - 1].base)
                || self
                    .neighbor(last, true)
                    .is_none_or(|glyph| delimiter(glyph.base)))
    }

    fn neighbor(&self, mut at: Point, right: bool) -> Option<Glyph<'_>> {
        let grid = self.term.grid();
        let last_column = self.size.columns as usize - 1;
        // A wide glyph can have a trailing spacer plus a leading spacer at a
        // soft wrap. No unbounded semantic search through history is needed.
        for _ in 0..3 {
            if right {
                if at.column.0 == last_column {
                    if at.line == grid.bottommost_line()
                        || !grid[at].flags.contains(Flags::WRAPLINE)
                    {
                        return None;
                    }
                    at = Point::new(Line(at.line.0 + 1), Column(0));
                } else {
                    at.column.0 += 1;
                }
            } else if at.column.0 == 0 {
                if at.line == grid.topmost_line() {
                    return None;
                }
                at = Point::new(Line(at.line.0 - 1), Column(last_column));
                if !grid[at].flags.contains(Flags::WRAPLINE) {
                    return None;
                }
            } else {
                at.column.0 -= 1;
            }
            let cell = &grid[at];
            if cell.flags.contains(Flags::HIDDEN) {
                return None;
            }
            if !spacer(cell) {
                return Some(Glyph::from_cell(cell));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests;
