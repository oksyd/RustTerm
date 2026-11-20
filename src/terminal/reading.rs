//! Selection anchors and incremental search state. Metadata never replaces the grid.
use super::{SearchHighlight, SearchMarkers, Terminal};
use alacritty_terminal::{
    grid::{Dimensions, Scroll},
    index::Side,
    index::{Column, Direction, Line, Point},
    selection::{Selection, SelectionType},
    term::cell::{Flags, LineLength},
};
use regex::{Regex, RegexBuilder};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

#[cfg(test)]
thread_local! { static MARK_CANDIDATES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

pub const MAX_SEARCH_RESULTS: usize = 10_000;
const MAX_QUERY_BYTES: usize = 4096;
const MAX_MATCHER_BYTES: usize = 1024 * 1024;
const MAX_LOGICAL_BYTES: usize = 64 * 1024;
// Independent caps cover result payloads, scan metadata and the UTF-8 byte map.
// Including geometric allocation growth and a temporary row, these stay below
// a conservative 32 MiB search working-set allowance per terminal.
const MAX_SEARCH_ROWS: usize = 65_536;
const MAX_RESULT_BYTES: usize = 2 * 1024 * 1024;
type SearchKey = (u64, usize, u64, usize);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionKind {
    /// Anchor a character drag; the initial press does not select a cell.
    Character,
    Word,
    Enclosed,
    Line,
    CommandBlock {
        trailing_newline: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionSide {
    Left,
    Right,
}

impl SelectionSide {
    fn side(self) -> Side {
        match self {
            Self::Left => Side::Left,
            Self::Right => Side::Right,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelectionPoint {
    pub column: u16,
    pub row: u16,
    pub side: SelectionSide,
}

impl SelectionPoint {
    pub const fn new(column: u16, row: u16, side: SelectionSide) -> Self {
        Self { column, row, side }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SearchScope {
    #[default]
    Session,
    Command(u64),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchStatus {
    pub scope: SearchScope,
    pub scope_unavailable: bool,
    pub query_id: u64,
    pub query: String,
    pub case_sensitive: bool,
    pub scanned_rows: usize,
    pub total_rows: usize,
    pub complete: bool,
    pub paused: bool,
    pub truncated: bool,
    pub matches: usize,
    pub current: Option<usize>,
    pub current_unavailable: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct RowStamp {
    pub id: u64,
    pub fingerprint: u64,
    pub start_column: usize,
    pub end_column: usize,
    pub check_wrap: bool,
}
#[derive(Clone, Debug)]
pub struct SearchMatch {
    pub query_id: u64,
    pub start_column: usize,
    pub end_column: usize,
    pub(super) rows: Vec<RowStamp>,
}
impl SearchMatch {
    fn key(&self) -> SearchKey {
        (
            self.rows[0].id,
            self.start_column,
            self.rows[self.rows.len() - 1].id,
            self.end_column,
        )
    }
    fn charge(&self) -> usize {
        std::mem::size_of::<Self>() + self.rows.capacity() * std::mem::size_of::<RowStamp>() + 64
    }
}
#[derive(Default)]
pub(super) struct Positions {
    entries: HashMap<u64, usize>,
    origin: usize,
}
impl Positions {
    pub fn get(&self, id: &u64) -> Option<usize> {
        self.entries
            .get(id)
            .map(|value| value.wrapping_sub(self.origin))
    }
    pub fn contains_key(&self, id: &u64) -> bool {
        self.entries.contains_key(id)
    }
    fn insert(&mut self, id: u64, index: usize) {
        self.entries.insert(id, self.origin.wrapping_add(index));
    }
    fn remove(&mut self, id: &u64) {
        self.entries.remove(id);
    }
}

#[derive(Default)]
pub(super) struct Tracker {
    ids: HashMap<usize, u64>,
    order: std::collections::VecDeque<usize>,
    history: usize,
    pub positions: Positions,
}
impl Tracker {
    pub fn retire(&mut self, address: usize) {
        if let Some(id) = self.ids.remove(&address) {
            self.positions.remove(&id);
        }
    }
    // Retained history is immutable between structural changes. Reconcile only
    // the newly historical/live tail; a logical origin makes eviction O(1) per row.
    fn sync(
        &mut self,
        history: usize,
        rows: usize,
        mut address: impl FnMut(usize) -> usize,
        next_id: &mut u64,
    ) -> Vec<usize> {
        let first = address(0);
        let dropped = self.ids.get(&first).and_then(|id| self.positions.get(id));
        let mut removed = Vec::new();
        let stable = if let Some(dropped) = dropped {
            for _ in 0..dropped {
                if let Some(old) = self.order.pop_front() {
                    self.retire(old);
                    removed.push(old);
                }
            }
            self.positions.origin = self.positions.origin.wrapping_add(dropped);
            self.history.saturating_sub(dropped).min(history)
        } else {
            0
        };
        let previous: HashMap<_, _> = self
            .order
            .drain(stable..)
            .enumerate()
            .map(|(index, old)| {
                let id = self.ids.remove(&old);
                if let Some(id) = id {
                    self.positions.remove(&id);
                }
                (old, (id, stable + index + dropped.unwrap_or(0)))
            })
            .collect();
        for index in stable..history + rows {
            let current = if index == 0 { first } else { address(index) };
            let id = previous
                .get(&current)
                .filter(|(_, before)| index <= *before)
                .and_then(|(id, _)| *id)
                .unwrap_or_else(|| {
                    *next_id = next_id.wrapping_add(1);
                    *next_id
                });
            self.ids.insert(current, id);
            self.positions.insert(id, index);
            self.order.push_back(current);
        }
        removed.extend(previous.keys().copied());
        removed.retain(|address| !self.ids.contains_key(address));
        self.history = history;
        self.shrink_excess();
        removed
    }
    pub fn reset(&mut self, addresses: impl Iterator<Item = usize>, next_id: &mut u64) {
        *self = Self::default();
        for address in addresses {
            *next_id = next_id.wrapping_add(1);
            self.ids.insert(address, *next_id);
            self.positions.insert(*next_id, self.order.len());
            self.order.push_back(address);
        }
    }
    fn shrink_excess(&mut self) {
        let limit = self.order.len().saturating_mul(3).max(64);
        if self.ids.capacity() > limit {
            self.ids.shrink_to_fit();
        }
        if self.positions.entries.capacity() > limit {
            self.positions.entries.shrink_to_fit();
        }
        if self.order.capacity() > limit {
            self.order.shrink_to_fit();
        }
    }
    pub fn clear(&mut self) {
        *self = Self::default();
    }
    pub fn id_at(&self, index: usize) -> Option<u64> {
        self.ids.get(self.order.get(index)?).copied()
    }
}

#[cfg(test)]
mod tracker_tests {
    use super::*;

    #[test]
    fn ordinary_output_updates_only_the_changed_tail_of_history() {
        let mut tracker = Tracker::default();
        let mut next = 0;
        tracker.sync(100, 4, |row| row + 1, &mut next);
        let retained = tracker.id_at(50);
        let mut visits = 0;
        tracker.sync(
            101,
            4,
            |row| {
                visits += 1;
                row + 1
            },
            &mut next,
        );
        assert_eq!(tracker.id_at(50), retained);
        assert!(visits <= 6, "append visited {visits} rows");
        tracker.retire(1);
        visits = 0;
        tracker.sync(
            101,
            4,
            |row| {
                visits += 1;
                row + 2
            },
            &mut next,
        );
        assert_eq!(tracker.id_at(49), retained);
        assert!(visits <= 6, "eviction visited {visits} rows");
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SelectionAnchor {
    pub stamps: Vec<RowStamp>,
    pub start: (u64, usize),
    pub end: (u64, usize),
}

pub(super) enum SelectionOrigin {
    Boundary {
        anchor: SelectionAnchor,
        pressed: (u64, usize),
        side: Side,
    },
    Character {
        anchor: SelectionAnchor,
        pressed: (u64, usize),
    },
    Range {
        start: (u64, usize),
        end: (u64, usize),
        pressed: (u64, usize),
    },
}

impl From<&SelectionAnchor> for SelectionOrigin {
    fn from(anchor: &SelectionAnchor) -> Self {
        Self::Range {
            start: anchor.start,
            end: anchor.end,
            pressed: anchor.start,
        }
    }
}
pub(super) struct SearchState {
    pub status: SearchStatus,
    pub results: Vec<SearchMatch>,
    by_row: HashMap<u64, std::ops::Range<usize>>,
    marker_rows: Arc<[usize]>,
    pub watermark: Vec<u64>,
    pub cursor: usize,
    pub pending: String,
    pub map: Vec<(u64, usize, usize)>,
    pub pending_rows: Vec<RowStamp>,
    pub matcher: Option<Regex>,
    pub scanned: HashMap<u64, u64>,
    pub dirty: bool,
    validated_content_version: Option<u64>,
    result_keys: HashSet<SearchKey>,
    result_bytes: usize,
    row_limit_reached: bool,
    pending_navigation: Option<bool>,
}
impl SearchState {
    pub fn new(id: u64, mut query: String, case_sensitive: bool, rows: Vec<u64>) -> Self {
        let oversized = query.len() > MAX_QUERY_BYTES;
        if oversized {
            let mut end = MAX_QUERY_BYTES;
            while !query.is_char_boundary(end) {
                end -= 1;
            }
            // Do not retain an arbitrarily large String capacity or search a
            // silently truncated query. Status keeps only a bounded preview.
            query = query[..end].to_owned();
        }
        let searchable = !oversized && !query.is_empty() && !query.contains(['\n', '\r']);
        // Escape the literal query and let Unicode simple case folding handle
        // equivalences such as Σ/σ/ς. Keep compilation and cache sizes bounded.
        let matcher = searchable
            .then(|| {
                RegexBuilder::new(&regex::escape(&query))
                    .case_insensitive(!case_sensitive)
                    .size_limit(MAX_MATCHER_BYTES)
                    .dfa_size_limit(MAX_MATCHER_BYTES)
                    .build()
                    .ok()
            })
            .flatten();
        let truncated = oversized || (searchable && matcher.is_none());
        let row_limit_reached = rows.len() > MAX_SEARCH_ROWS;
        let mut state = Self {
            status: SearchStatus {
                query_id: id,
                query,
                case_sensitive,
                total_rows: rows.len(),
                truncated,
                ..Default::default()
            },
            results: Vec::new(),
            by_row: HashMap::new(),
            marker_rows: Arc::default(),
            watermark: rows,
            cursor: 0,
            pending: String::new(),
            map: Vec::new(),
            pending_rows: Vec::new(),
            matcher,
            scanned: HashMap::new(),
            dirty: false,
            validated_content_version: None,
            result_keys: HashSet::new(),
            result_bytes: 0,
            row_limit_reached,
            pending_navigation: None,
        };
        state.watermark.truncate(MAX_SEARCH_ROWS);
        state.status.total_rows = state.watermark.len();
        if state.matcher.is_none() {
            state.status.complete = true;
            state.watermark.clear();
        }
        state
    }
    fn index_results(&mut self, positions: &Positions) {
        self.by_row.clear();
        for (index, item) in self.results.iter().enumerate() {
            for stamp in &item.rows {
                self.by_row.entry(stamp.id).or_insert(index..index).end = index + 1;
            }
        }
        self.marker_rows = self.result_rows(positions);
    }

    fn result_rows(&self, positions: &Positions) -> Arc<[usize]> {
        let mut rows = Vec::new();
        for item in &self.results {
            if let Some(row) = positions.get(&item.rows[0].id)
                && rows.last() != Some(&row)
            {
                rows.push(row);
            }
        }
        rows.into()
    }

    fn finish_logical_line(&mut self) {
        if !self.pending.is_empty()
            && let Some(matcher) = &self.matcher
        {
            let rows: HashMap<_, _> = self
                .pending_rows
                .iter()
                .enumerate()
                .map(|(i, row)| (row.id, i))
                .collect();
            for found in matcher.find_iter(&self.pending) {
                let Some(&(start_id, start_column, _)) = self.map.get(found.start()) else {
                    continue;
                };
                let Some(&(end_id, _, end_column)) = self.map.get(found.end() - 1) else {
                    continue;
                };
                let key = (start_id, start_column, end_id, end_column);
                if self.result_keys.contains(&key) {
                    continue;
                }
                let Some(&first) = rows.get(&start_id) else {
                    continue;
                };
                let Some(&last) = rows.get(&end_id) else {
                    continue;
                };
                let charge = std::mem::size_of::<SearchMatch>()
                    + (last - first + 1) * std::mem::size_of::<RowStamp>()
                    + 64;
                if self.results.len() >= MAX_SEARCH_RESULTS
                    || self.result_bytes + charge > MAX_RESULT_BYTES
                {
                    self.status.truncated = true;
                    break;
                }
                let candidate = SearchMatch {
                    query_id: self.status.query_id,
                    start_column,
                    end_column,
                    rows: self.pending_rows[first..=last].to_vec(),
                };
                self.result_bytes += candidate.charge();
                self.result_keys.insert(key);
                self.results.push(candidate);
            }
        }
        self.pending.clear();
        self.map.clear();
        self.pending_rows.clear();
    }
}

impl Terminal {
    pub(super) fn anchor_viewport(&mut self) {
        if self.is_alternate() {
            return;
        }
        let grid = self.term.grid();
        if grid.display_offset() == 0 {
            self.scroll_fraction = 0.;
            self.new_output = false;
        }
        self.viewport_anchor = (grid.display_offset() > 0)
            .then(|| {
                self.main_tracker
                    .id_at(grid.history_size() - grid.display_offset())
            })
            .flatten();
    }
    pub(super) fn restore_viewport(&mut self) {
        if self.is_alternate() {
            return;
        }
        let Some(id) = self.viewport_anchor else {
            self.anchor_viewport();
            return;
        };
        let history = self.term.grid().history_size();
        if let Some(index) = self.main_tracker.positions.get(&id) {
            let offset = history.saturating_sub(index);
            let delta = offset as i64 - self.term.grid().display_offset() as i64;
            self.term.scroll_display(Scroll::Delta(
                delta.clamp(i32::MIN as i64, i32::MAX as i64) as i32
            ));
        } else {
            self.term.scroll_display(Scroll::Top);
            self.viewport_evicted = true;
            self.scroll_fraction = 0.;
            // Keep the earliest content anchored, including when all history was
            // removed. A later append must not silently resume following output.
            self.viewport_anchor = self.main_tracker.id_at(0);
        }
    }
    pub(super) fn tracker(&self) -> &Tracker {
        if self.is_alternate() {
            &self.alt_tracker
        } else {
            &self.main_tracker
        }
    }
    pub(super) fn reconcile_rows(&mut self) {
        let grid = self.term.grid();
        let tracker = if self.is_alternate() {
            &mut self.alt_tracker
        } else {
            &mut self.main_tracker
        };
        let removed = tracker.sync(
            grid.history_size(),
            grid.screen_lines(),
            |index| {
                super::timestamps::address(grid, Line(index as i32 - grid.history_size() as i32))
            },
            &mut self.next_row_id,
        );
        if !self.is_alternate() {
            for address in removed {
                self.line_times.retire(address);
            }
        }
    }

    pub(super) fn reset_rows(&mut self) {
        let alternate = self.is_alternate();
        let grid = self.term.grid();
        let addresses = (grid.topmost_line().0..=grid.bottommost_line().0)
            .map(|line| &grid[Point::new(Line(line), Column(0))] as *const _ as usize);
        if alternate {
            self.main_tracker.clear();
            self.alt_tracker.reset(addresses, &mut self.next_row_id);
        } else {
            self.alt_tracker.clear();
            self.main_tracker.reset(addresses, &mut self.next_row_id);
        }
    }
    pub(super) fn row_stamp(&self, index: usize) -> Option<RowStamp> {
        self.range_stamp(index, 0, self.size.columns as usize - 1, true)
    }
    fn range_stamp(
        &self,
        index: usize,
        start_column: usize,
        end_column: usize,
        check_wrap: bool,
    ) -> Option<RowStamp> {
        let id = self.tracker().id_at(index)?;
        let line = Line(index as i32 - self.term.grid().history_size() as i32);
        if line < self.term.grid().topmost_line() || line > self.term.grid().bottommost_line() {
            return None;
        }
        // A content fingerprint is always checked at use time, never merely a global version.
        let mut hash = 0xcbf29ce484222325u64;
        for col in start_column..=end_column {
            let cell = &self.term.grid()[Point::new(line, Column(col))];
            for n in std::iter::once(cell.c as u32)
                .chain(
                    cell.zerowidth()
                        .unwrap_or_default()
                        .iter()
                        .map(|&c| c as u32),
                )
                .chain(std::iter::once((cell.flags - Flags::WRAPLINE).bits() as u32))
            {
                hash ^= n as u64;
                hash = hash.wrapping_mul(0x100000001b3);
            }
        }
        if check_wrap {
            hash ^= u64::from(
                self.term.grid()[Point::new(line, Column(self.size.columns as usize - 1))]
                    .flags
                    .contains(Flags::WRAPLINE),
            );
            hash = hash.wrapping_mul(0x100000001b3);
        }
        // Erasing an emitted space changes its meaning even when its grid cell
        // remains identical to padding. Appends beyond the stamped range do not.
        hash ^= self
            .selection_columns(line)
            .clamp(start_column, end_column + 1) as u64;
        hash = hash.wrapping_mul(0x100000001b3);
        Some(RowStamp {
            id,
            fingerprint: hash,
            start_column,
            end_column,
            check_wrap,
        })
    }
    pub(super) fn stamp_valid(&self, stamp: RowStamp) -> bool {
        self.tracker().positions.get(&stamp.id).and_then(|i| {
            self.range_stamp(i, stamp.start_column, stamp.end_column, stamp.check_wrap)
        }) == Some(stamp)
    }
    pub(super) fn point_for(&self, id: u64, column: usize) -> Option<Point> {
        let index = self.tracker().positions.get(&id)?;
        Some(Point::new(
            Line(index as i32 - self.term.grid().history_size() as i32),
            Column(column.min(self.size.columns as usize - 1)),
        ))
    }
    fn viewport_point(&self, column: u16, row: u16) -> Point {
        let viewport = self.viewport();
        Point::new(
            viewport
                .lines
                .get(usize::from(row).min(viewport.lines.len().saturating_sub(1)))
                .copied()
                .unwrap_or(self.term.grid().bottommost_line()),
            Column(column.min(self.size.columns - 1) as usize),
        )
    }
    pub(super) fn selection_anchor(&self, selection: &Selection) -> Option<SelectionAnchor> {
        let (start, end) = self.selection_range(selection)?;
        self.anchor_between(start, end)
    }

    pub(super) fn content_selection_anchor(
        &self,
        selection: &Selection,
    ) -> Option<SelectionAnchor> {
        let (start, end) = self.selection_range(selection)?;
        let (start, end) = self.clip_selection(start, end)?;
        self.anchor_between(start, end)
    }

    fn selection_range(&self, selection: &Selection) -> Option<(Point, Point)> {
        let mut range = selection.to_range(&self.term)?;
        range.start = self.term.expand_wide(range.start, Direction::Left);
        range.end = self.term.expand_wide(range.end, Direction::Right);
        Some((range.start, range.end))
    }

    fn anchor_between(&self, start: Point, end: Point) -> Option<SelectionAnchor> {
        let history = self.term.grid().history_size() as i32;
        let first = (start.line.0 + history) as usize;
        let last = (end.line.0 + history) as usize;
        let stamps: Option<Vec<_>> = (first..=last)
            .map(|i| {
                self.range_stamp(
                    i,
                    if i == first { start.column.0 } else { 0 },
                    if i == last {
                        end.column.0
                    } else {
                        self.size.columns as usize - 1
                    },
                    i < last,
                )
            })
            .collect();
        let stamps = stamps?;
        Some(SelectionAnchor {
            start: (stamps.first()?.id, start.column.0),
            end: (stamps.last()?.id, end.column.0),
            stamps,
        })
    }
    pub fn begin_selection(&mut self, column: u16, row: u16, kind: SelectionKind) {
        let point = self.viewport_point(column, row);
        if kind == SelectionKind::Character {
            self.prepare_selection(point);
            return;
        }
        if kind == SelectionKind::Enclosed {
            match self.enclosed_range(point) {
                Some(super::enclosed::Enclosure::Empty) => {
                    self.clear_selection();
                    return;
                }
                Some(super::enclosed::Enclosure::Content(start, end)) => {
                    let mut selection = Selection::new(SelectionType::Simple, start, Side::Left);
                    selection.update(end, Side::Right);
                    self.selection_anchor = self.content_selection_anchor(&selection);
                    self.selection_origin =
                        self.selection_anchor.as_ref().map(SelectionOrigin::from);
                    // Mouse-up on the pressed delimiter must not add it back to the copy.
                    let pressed_point = self.tracked_point(point);
                    if let (Some(SelectionOrigin::Range { pressed, .. }), Some(point)) =
                        (&mut self.selection_origin, pressed_point)
                    {
                        *pressed = point;
                    }
                    self.selection_kind = Some(kind);
                    self.term.selection = Some(selection);
                    self.selection_invalidated = false;
                    self.version = self.version.wrapping_add(1);
                    return;
                }
                None => return self.begin_selection(column, row, SelectionKind::Word),
            }
        }
        let ty = match kind {
            SelectionKind::Character | SelectionKind::CommandBlock { .. } => SelectionType::Simple,
            SelectionKind::Word | SelectionKind::Enclosed => SelectionType::Semantic,
            SelectionKind::Line => SelectionType::Lines,
        };
        let mut selection = Selection::new(ty, point, Side::Left);
        selection.update(point, Side::Right);
        // Upstream line selection crosses soft wraps; the product's triple click is a visual row.
        if kind == SelectionKind::Line {
            selection = Selection::new(
                SelectionType::Simple,
                Point::new(point.line, Column(0)),
                Side::Left,
            );
            selection.update(
                Point::new(point.line, Column(self.size.columns as usize - 1)),
                Side::Right,
            );
        }
        self.selection_anchor = self.content_selection_anchor(&selection);
        self.selection_kind = Some(kind);
        self.selection_origin = self.selection_anchor.as_ref().map(SelectionOrigin::from);
        self.term.selection = Some(selection);
        self.selection_invalidated = false;
        self.version = self.version.wrapping_add(1);
    }

    pub fn begin_selection_at(
        &mut self,
        position: SelectionPoint,
        kind: SelectionKind,
        extend: bool,
    ) {
        if extend && self.extend_selection_at(position) {
            return;
        }
        self.begin_selection(position.column, position.row, kind);
        if kind == SelectionKind::Character
            && let Some(SelectionOrigin::Character { anchor, pressed }) =
                self.selection_origin.take()
        {
            self.selection_origin = Some(SelectionOrigin::Boundary {
                anchor,
                pressed,
                side: position.side.side(),
            });
        }
    }

    pub fn update_selection_at(&mut self, position: SelectionPoint) {
        let point = self.viewport_point(position.column, position.row);
        if !self.update_character_selection(point, position.side.side()) {
            self.update_selection(position.column, position.row);
        }
    }

    fn extend_selection_at(&mut self, position: SelectionPoint) -> bool {
        let (pressed, side) = match &self.selection_origin {
            Some(
                SelectionOrigin::Boundary { anchor, .. }
                | SelectionOrigin::Character { anchor, .. },
            ) if !self.anchor_valid(anchor, 0) => {
                self.clear_selection();
                return true;
            }
            Some(SelectionOrigin::Boundary { pressed, side, .. }) => (*pressed, *side),
            Some(SelectionOrigin::Character { pressed, .. }) => (*pressed, Side::Left),
            Some(SelectionOrigin::Range { start, .. }) => (*start, Side::Left),
            None => return false,
        };
        // A pending press can be extended too; an existing range must still
        // refer to the same content before its fixed endpoint is reused.
        if self.selection_anchor.is_some() && !self.selection_valid(0) {
            self.clear_selection();
            return true;
        }
        let Some(point) = self.point_for(pressed.0, pressed.1) else {
            return false;
        };
        let Some(origin) = self.boundary_origin(point, side) else {
            return false;
        };
        self.selection_origin = Some(origin);
        self.update_selection_at(position);
        true
    }

    fn tracked_point(&self, point: Point) -> Option<(u64, usize)> {
        let index = (point.line.0 + self.term.grid().history_size() as i32) as usize;
        self.tracker().id_at(index).map(|id| (id, point.column.0))
    }

    pub(super) fn boundary_origin(&self, point: Point, side: Side) -> Option<SelectionOrigin> {
        let mut fixed = Selection::new(SelectionType::Simple, point, Side::Left);
        fixed.update(point, Side::Right);
        Some(SelectionOrigin::Boundary {
            anchor: self.selection_anchor(&fixed)?,
            pressed: self.tracked_point(point)?,
            side,
        })
    }

    fn prepare_selection(&mut self, point: Point) {
        self.clear_selection();
        // Retain the pressed cell's identity and content, without publishing a
        // selected range. A click focuses the terminal; only a drag selects.
        let mut selection = Selection::new(SelectionType::Simple, point, Side::Left);
        selection.update(point, Side::Right);
        if let (Some(anchor), Some(pressed)) =
            (self.selection_anchor(&selection), self.tracked_point(point))
        {
            self.selection_origin = Some(SelectionOrigin::Character { anchor, pressed });
        }
    }

    fn update_character_selection(&mut self, point: Point, end_side: Side) -> bool {
        let (anchor, pressed, start_side, precise) = match &self.selection_origin {
            Some(SelectionOrigin::Character { anchor, pressed }) => {
                (anchor, pressed, Side::Left, false)
            }
            Some(SelectionOrigin::Boundary {
                anchor,
                pressed,
                side,
            }) => (anchor, pressed, *side, true),
            _ => return false,
        };
        let Some(origin) = self.point_for(pressed.0, pressed.1).filter(|_| {
            self.anchor_valid(anchor, 0)
                && (self.selection_anchor.is_none() || self.selection_valid(0))
        }) else {
            self.clear_selection();
            return true;
        };
        if point == origin
            && (!precise || start_side == end_side)
            && self.selection_anchor.is_none()
        {
            return true;
        }
        let mut selection = Selection::new(SelectionType::Simple, origin, start_side);
        selection.update(point, end_side);
        if !precise {
            selection.include_all();
        }
        self.selection_anchor = self.content_selection_anchor(&selection);
        self.selection_kind = Some(SelectionKind::Character);
        self.term.selection = Some(selection);
        self.version = self.version.wrapping_add(1);
        true
    }
    /// Select the retained buffer without materializing its text on the input path.
    pub fn select_all(&mut self) {
        let grid = self.term.grid();
        // Ignore untouched rows below the cursor, but retain output below a
        // repositioned cursor and blank lines already emitted by the shell.
        let last = (grid.cursor.point.line.0..=grid.bottommost_line().0)
            .rev()
            .map(Line)
            .find(|&line| {
                line == grid.cursor.point.line
                    || grid[line].line_length().0 > 0
                    || (!self.is_alternate() && self.line_times.get(grid, line).is_some())
            })
            .unwrap_or(grid.cursor.point.line);
        let mut selection = Selection::new(
            SelectionType::Simple,
            Point::new(grid.topmost_line(), Column(0)),
            Side::Left,
        );
        selection.update(
            Point::new(last, Column(self.size.columns as usize - 1)),
            Side::Right,
        );
        self.selection_anchor = self.content_selection_anchor(&selection);
        self.selection_kind = Some(SelectionKind::Line);
        self.selection_origin = self.selection_anchor.as_ref().map(SelectionOrigin::from);
        self.term.selection = Some(selection);
        self.selection_invalidated = false;
        self.version = self.version.wrapping_add(1);
    }
    pub fn update_selection(&mut self, column: u16, row: u16) {
        let point = self.viewport_point(column, row);
        if self.update_character_selection(point, Side::Right) {
            return;
        }
        if !self.selection_valid(0) {
            let invalidated = self.selection_invalidated || self.selection_anchor.is_some();
            self.clear_selection();
            self.selection_invalidated = invalidated;
            return;
        }
        if let Some(mut selection) = self.term.selection.take() {
            if self.selection_kind == Some(SelectionKind::Line)
                && let Some(SelectionOrigin::Range { start, .. }) = &self.selection_origin
                && let Some(origin) = self.point_for(start.0, 0)
            {
                let first = origin.line.min(point.line);
                let last = origin.line.max(point.line);
                selection = Selection::new(
                    SelectionType::Simple,
                    Point::new(first, Column(0)),
                    Side::Left,
                );
                selection.update(
                    Point::new(last, Column(self.size.columns as usize - 1)),
                    Side::Right,
                );
            } else if self.selection_kind == Some(SelectionKind::Enclosed)
                && let Some(SelectionOrigin::Range {
                    start,
                    end,
                    pressed,
                }) = &self.selection_origin
                && let (Some(start), Some(end)) = (
                    self.point_for(start.0, start.1),
                    self.point_for(end.0, end.1),
                )
            {
                let pressed = self.point_for(pressed.0, pressed.1);
                let point = if pressed == Some(point) { start } else { point };
                selection = Selection::new(SelectionType::Simple, start.min(point), Side::Left);
                selection.update(end.max(point), Side::Right);
            } else {
                selection.update(point, Side::Right);
                selection.include_all();
            }
            self.selection_anchor = self.content_selection_anchor(&selection);
            self.term.selection = Some(selection);
            self.version = self.version.wrapping_add(1);
        }
    }
    pub(super) fn selection_valid(&self, unchanged_prefix: usize) -> bool {
        self.selection_anchor
            .as_ref()
            .is_some_and(|anchor| self.anchor_valid(anchor, unchanged_prefix))
    }

    fn anchor_valid(&self, anchor: &SelectionAnchor, unchanged_prefix: usize) -> bool {
        let positions = &self.tracker().positions;
        let history = self.term.grid().history_size();
        let mut previous = None;
        anchor.stamps.iter().enumerate().all(|(offset, &stamp)| {
            let Some(index) = positions.get(&stamp.id) else {
                return false;
            };
            let adjacent = previous.is_none_or(|p| p + 1 == index);
            previous = Some(index);
            adjacent
                && ((offset < unchanged_prefix && index < history)
                    || self.range_stamp(
                        index,
                        stamp.start_column,
                        stamp.end_column,
                        stamp.check_wrap,
                    ) == Some(stamp))
        })
    }
    pub fn clear_selection(&mut self) {
        self.term.selection = None;
        self.selection_anchor = None;
        self.selection_invalidated = false;
        self.selection_kind = None;
        self.selection_origin = None;
        self.version = self.version.wrapping_add(1);
    }
    pub fn selection_search_query(&self) -> Option<String> {
        const MAX_CHARACTERS: usize = 256;
        const MAX_BYTES: usize = MAX_CHARACTERS * 4;
        let anchor = self.selection_anchor.as_ref()?;
        let first = self.point_for(anchor.start.0, anchor.start.1)?;
        let last = self.point_for(anchor.end.0, anchor.end.1)?;
        // Reject large ranges before walking rows or allocating their text. Wide
        // characters and soft wraps still belong to the same logical query.
        let rows = usize::try_from(last.line.0.checked_sub(first.line.0)?).ok()?;
        let cells = rows
            .checked_mul(usize::from(self.size.columns))?
            .checked_add(last.column.0 + 1)?
            .checked_sub(first.column.0)?;
        if cells > MAX_BYTES
            || self
                .selected_command_output(anchor)
                .is_some_and(|text| text.len() > MAX_BYTES)
        {
            return None;
        }
        let text = self.selected_text()?;
        (text.len() <= MAX_BYTES
            && text.chars().count() <= MAX_CHARACTERS
            && !text.trim().is_empty()
            && !text.chars().any(char::is_control))
        .then_some(text)
    }

    pub fn selected_text(&self) -> Option<String> {
        if !self.selection_valid(0) {
            return None;
        }
        let a = self.selection_anchor.as_ref()?;
        if let Some(output) = self.selected_command_output(a) {
            return Some(output.to_owned());
        }
        let first = self.point_for(a.start.0, a.start.1)?;
        let last = self.point_for(a.end.0, a.end.1)?;
        let mut text = String::new();
        // Main-screen metadata retains emitted spaces while excluding padding.
        // Full-screen applications retain their existing grid-selection behavior.
        let trim_padding = first.line != last.line
            || matches!(
                self.selection_kind,
                Some(SelectionKind::Line | SelectionKind::CommandBlock { .. })
            );
        let trailing_newline = matches!(
            self.selection_kind,
            Some(SelectionKind::CommandBlock {
                trailing_newline: true
            })
        );
        for line in first.line.0..=last.line.0 {
            let start = if line == first.line.0 {
                first.column.0
            } else {
                0
            };
            let end = if line == last.line.0 {
                last.column.0
            } else {
                self.size.columns as usize - 1
            };
            let end = if !self.is_alternate() {
                (end + 1).min(self.content_columns(Line(line)))
            } else if trim_padding {
                (end + 1).min(self.term.grid()[Line(line)].line_length().0)
            } else {
                end + 1
            };
            text.push_str(&self.row_text(Line(line), start..end, false));
            if (line < last.line.0 || trailing_newline)
                && !self.term.grid()[Point::new(Line(line), Column(self.size.columns as usize - 1))]
                    .flags
                    .contains(Flags::WRAPLINE)
            {
                text.push('\n');
            }
        }
        Some(text)
    }
    pub(super) fn row_text(
        &self,
        line: Line,
        columns: std::ops::Range<usize>,
        conceal_hidden: bool,
    ) -> String {
        let mut text = String::new();
        for col in columns {
            let cell = &self.term.grid()[Point::new(line, Column(col))];
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            if conceal_hidden && cell.flags.contains(Flags::HIDDEN) {
                text.push(' ');
                continue;
            }
            // Alacritty stores a tab marker in its first cell and leaves the
            // rest of its span as spaces. Copy visual columns, including when
            // a selection covers only part of a tab or tab stops have changed.
            text.push(if cell.c == '\t' { ' ' } else { cell.c });
            for &c in cell.zerowidth().unwrap_or_default() {
                text.push(c);
            }
        }
        text
    }
    pub(super) fn refresh_selection(&mut self, unchanged_prefix: usize) {
        if self.selection_anchor.is_some() && !self.selection_valid(unchanged_prefix) {
            self.clear_selection();
            self.selection_invalidated = true;
        }
    }
    pub(super) fn selection_points(&self) -> Option<(Point, Point)> {
        let anchor = self.selection_anchor.as_ref()?;
        Some((
            self.point_for(anchor.start.0, anchor.start.1)?,
            self.point_for(anchor.end.0, anchor.end.1)?,
        ))
    }
    pub fn start_search(&mut self, query: String, case_sensitive: bool) -> u64 {
        self.start_scoped_search(query, case_sensitive, SearchScope::Session)
    }

    fn search_rows(&self, scope: SearchScope) -> Option<Vec<u64>> {
        let range = match scope {
            SearchScope::Session => 0..self.term.grid().total_lines(),
            SearchScope::Command(id) => self.command_block_range(id)?,
        };
        Some(
            range
                .filter_map(|i| self.main_tracker.id_at(i))
                .take(MAX_SEARCH_ROWS + 1)
                .collect(),
        )
    }

    pub fn start_scoped_search(
        &mut self,
        query: String,
        case_sensitive: bool,
        scope: SearchScope,
    ) -> u64 {
        self.next_query_id = self.next_query_id.wrapping_add(1);
        let rows = if self.is_alternate()
            || query.is_empty()
            || query.len() > MAX_QUERY_BYTES
            || query.contains(['\n', '\r'])
        {
            Vec::new()
        } else {
            self.search_rows(scope).unwrap_or_default()
        };
        let mut state = SearchState::new(self.next_query_id, query, case_sensitive, rows);
        state.status.scope = scope;
        state.status.scope_unavailable =
            matches!(scope, SearchScope::Command(id) if self.command_block_range(id).is_none());
        self.search = Some(state);
        self.version = self.version.wrapping_add(1);
        self.next_query_id
    }
    pub fn cancel_search(&mut self) {
        self.search = None;
        self.next_query_id = self.next_query_id.wrapping_add(1);
        self.version = self.version.wrapping_add(1);
    }
    pub fn search_status(&self) -> SearchStatus {
        let mut status = self
            .search
            .as_ref()
            .map(|s| s.status.clone())
            .unwrap_or_default();
        if status.query.is_empty() {
            status.complete = true;
        } else if self
            .search
            .as_ref()
            .is_some_and(|s| s.dirty && s.matcher.is_some() && !s.status.truncated)
        {
            // The actor only schedules incomplete searches. Expose pending
            // refresh work without discarding the current pass's fixed waterline.
            status.complete = false;
        }
        status.paused = self.is_alternate() && !status.query.is_empty();
        status
    }
    pub fn search_results(&self) -> Vec<SearchMatch> {
        self.search
            .as_ref()
            .map(|s| s.results.clone())
            .unwrap_or_default()
    }

    pub(super) fn current_search_points(&self) -> Option<(Point, Point)> {
        let state = self.search.as_ref()?;
        let item = state.results.get(state.status.current?)?;
        if !item.rows.iter().all(|&stamp| self.stamp_valid(stamp)) {
            return None;
        }
        Some((
            self.point_for(item.rows.first()?.id, item.start_column)?,
            self.point_for(item.rows.last()?.id, item.end_column)?,
        ))
    }

    pub(super) fn current_search_visible(&self) -> bool {
        !self.is_alternate()
            && self.current_search_points().is_some_and(|(start, end)| {
                self.viewport()
                    .lines
                    .iter()
                    .any(|&line| line >= start.line && line <= end.line)
            })
    }

    pub(super) fn keep_current_search_visible(&mut self) {
        if !self.current_search_visible()
            && let Some((point, _)) = self.current_search_points()
        {
            self.reveal_search_point(point);
        }
    }

    pub(super) fn search_after_reflow(&mut self, current: Option<(Point, Point, bool)>) {
        let Some(previous) = self.search.take() else {
            return;
        };
        if previous.matcher.is_none() {
            // A rejected query keeps only a bounded preview. Never search that preview.
            self.search = Some(previous);
            return;
        }
        self.start_scoped_search(
            previous.status.query,
            previous.status.case_sensitive,
            previous.status.scope,
        );
        let restored = current
            .and_then(|(start, end, visible)| Some((self.anchor_between(start, end)?, visible)));
        let Some(state) = &mut self.search else {
            return;
        };
        state.pending_navigation = previous.pending_navigation;
        state.status.current_unavailable =
            previous.status.current_unavailable || previous.status.current.is_some();
        if let Some((anchor, visible)) = restored.filter(|_| !state.status.scope_unavailable) {
            let item = SearchMatch {
                query_id: state.status.query_id,
                start_column: anchor.start.1,
                end_column: anchor.end.1,
                rows: anchor.stamps,
            };
            state.result_bytes = item.charge();
            state.result_keys.insert(item.key());
            state.results.push(item);
            state.index_results(&self.main_tracker.positions);
            state.status.matches = 1;
            state.status.current = Some(0);
            state.status.current_unavailable = false;
            if visible {
                self.keep_current_search_visible();
            }
        }
    }
    fn retain_valid_results(&self, state: &mut SearchState, start: usize) {
        let scope = match state.status.scope {
            SearchScope::Session => 0..self.term.grid().total_lines(),
            SearchScope::Command(id) => self.command_block_range(id).unwrap_or(0..0),
        };
        let mut stamps = HashMap::new();
        let removed = state.results.extract_if(start.., |item| {
            let mut previous = None;
            !item.rows.iter().all(|stamp| {
                let Some(index) = self.main_tracker.positions.get(&stamp.id) else {
                    return false;
                };
                if !scope.contains(&index) || previous.is_some_and(|i| i + 1 != index) {
                    return false;
                }
                previous = Some(index);
                let current = stamps
                    .entry((
                        stamp.id,
                        stamp.start_column,
                        stamp.end_column,
                        stamp.check_wrap,
                    ))
                    .or_insert_with(|| {
                        self.range_stamp(
                            index,
                            stamp.start_column,
                            stamp.end_column,
                            stamp.check_wrap,
                        )
                    });
                *current == Some(*stamp)
            })
        });
        for item in removed {
            state.result_keys.remove(&item.key());
            state.result_bytes -= item.charge();
            // A row may change back before the next pass reaches it. Its old
            // fingerprint must not suppress rediscovery of the removed match.
            for stamp in item.rows {
                state.scanned.remove(&stamp.id);
            }
        }
    }
    fn refresh_search_results(&self, state: &mut SearchState) -> bool {
        if state.validated_content_version == Some(self.content_version) {
            return false;
        }
        self.retain_valid_results(state, 0);
        state.validated_content_version = Some(self.content_version);
        true
    }
    /// Advances at most `max_rows` physical rows; cancellation is replacing the query ID.
    /// Call from the session actor between I/O batches, never while painting.
    pub fn search_step(&mut self, max_rows: usize) -> SearchStatus {
        self.search_step_until(max_rows, None)
    }

    pub(crate) fn search_step_budgeted(
        &mut self,
        max_rows: usize,
        budget: std::time::Duration,
    ) -> SearchStatus {
        self.search_step_until(max_rows, Some(std::time::Instant::now() + budget))
    }

    fn search_step_until(
        &mut self,
        max_rows: usize,
        deadline: Option<std::time::Instant>,
    ) -> SearchStatus {
        if self.is_alternate() {
            return self.search_status();
        }
        let Some(mut state) = self.search.take() else {
            return self.search_status();
        };
        if matches!(state.status.scope, SearchScope::Command(id) if self.command_block_range(id).is_none())
        {
            let mut expired = SearchState::new(
                state.status.query_id,
                state.status.query,
                state.status.case_sensitive,
                Vec::new(),
            );
            expired.status.scope = state.status.scope;
            expired.status.scope_unavailable = true;
            expired.status.complete = true;
            self.search = Some(expired);
            return self.search_status();
        }
        let current = state
            .status
            .current
            .and_then(|i| state.results.get(i))
            .map(SearchMatch::key);
        let content_changed = self.refresh_search_results(&mut state);
        let retained_count = state.results.len();
        if state.status.complete
            && state.dirty
            && state.matcher.is_some()
            && !state.status.truncated
        {
            // Refresh a fixed waterline in bounded slices. Unchanged hard rows are
            // fingerprint-checked without rebuilding text; appends never restart a pass.
            state.watermark = self.search_rows(state.status.scope).unwrap_or_default();
            state.row_limit_reached = state.watermark.len() > MAX_SEARCH_ROWS;
            state.watermark.truncate(MAX_SEARCH_ROWS);
            state
                .scanned
                .retain(|id, _| self.main_tracker.positions.contains_key(id));
            state.cursor = 0;
            state.status.complete = state.watermark.is_empty();
            state.dirty = false;
            state.status.total_rows = state.watermark.len();
            state.status.scanned_rows = 0;
        }
        if !state.status.complete && !state.status.truncated {
            let end = (state.cursor + max_rows.clamp(1, 256)).min(state.watermark.len());
            let begin = state.cursor;
            while state.cursor < end {
                if state.cursor > begin
                    && deadline.is_some_and(|end| std::time::Instant::now() >= end)
                {
                    break;
                }
                let id = state.watermark[state.cursor];
                state.cursor += 1;
                let Some(index) = self.main_tracker.positions.get(&id) else {
                    state.finish_logical_line();
                    continue;
                };
                let Some(stamp) = self.row_stamp(index) else {
                    continue;
                };
                if state.pending.is_empty()
                    && !self.row_wrapped(index)
                    && state.scanned.get(&id) == Some(&stamp.fingerprint)
                {
                    continue;
                }

                // Never concatenate rows that ceased to be adjacent during output/eviction.
                if let Some(previous) = state.pending_rows.last()
                    && self
                        .main_tracker
                        .positions
                        .get(&previous.id)
                        .is_none_or(|p| p + 1 != index)
                {
                    state.finish_logical_line();
                }
                let (text, map) = self.row_search_text(index);
                if state.pending.len() + text.len() > MAX_LOGICAL_BYTES {
                    state.pending.clear();
                    state.map.clear();
                    state.pending_rows.clear();
                    state.status.truncated = true;
                    break;
                }
                state.pending.push_str(&text);
                state.map.extend(map);
                state.pending_rows.push(stamp);
                state.scanned.insert(id, stamp.fingerprint);
                if !self.row_wrapped(index) {
                    state.finish_logical_line();
                }
            }
            state.status.scanned_rows = state.cursor;
            if state.cursor >= state.watermark.len() {
                state.finish_logical_line();
                state.status.complete = true;
                state.status.truncated |= state.row_limit_reached;
            }
        }
        // A logical line can span slices and include rows changed between them.
        // Validate just the new matches; existing matches share the content revision.
        self.retain_valid_results(&mut state, retained_count);
        for item in &mut state.results[retained_count..] {
            let last = item.rows.len() - 1;
            for (offset, stamp) in item.rows.iter_mut().enumerate() {
                let index = self.main_tracker.positions.get(&stamp.id).unwrap();
                let start = if offset == 0 { item.start_column } else { 0 };
                let end = if offset == last {
                    item.end_column
                } else {
                    self.size.columns as usize - 1
                };
                let check_wrap = offset < last;
                if stamp.start_column != start
                    || stamp.end_column != end
                    || stamp.check_wrap != check_wrap
                {
                    *stamp = self
                        .range_stamp(index, start, end, check_wrap)
                        .expect("validated match row");
                }
            }
        }
        if content_changed || state.results.len() > retained_count {
            self.order_results(&mut state, current);
        }
        state.status.matches = state.results.len();
        if state.status.truncated {
            state.status.complete = true;
            state.pending.clear();
            state.map.clear();
            state.pending_rows.clear();
        }
        let pending_navigation = state.pending_navigation;
        self.search = Some(state);
        if let Some(backwards) = pending_navigation {
            self.search_next(backwards);
        }
        self.search
            .as_ref()
            .expect("search navigation retains the query")
            .status
            .clone()
    }
    fn row_wrapped(&self, index: usize) -> bool {
        let line = Line(index as i32 - self.term.grid().history_size() as i32);
        self.term.grid()[Point::new(line, Column(self.size.columns as usize - 1))]
            .flags
            .contains(Flags::WRAPLINE)
    }
    fn row_search_text(&self, index: usize) -> (String, Vec<(u64, usize, usize)>) {
        let id = self.main_tracker.id_at(index).unwrap_or_default();
        let line = Line(index as i32 - self.term.grid().history_size() as i32);
        let mut text = String::new();
        let mut map = Vec::new();
        for col in 0..self.size.columns as usize {
            let c = &self.term.grid()[Point::new(line, Column(col))];
            if c.flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            for ch in std::iter::once(c.c).chain(c.zerowidth().unwrap_or_default().iter().copied())
            {
                text.push(ch);
                map.extend(std::iter::repeat_n(
                    (
                        id,
                        col,
                        (col + usize::from(c.flags.contains(Flags::WIDE_CHAR)))
                            .min(self.size.columns as usize - 1),
                    ),
                    ch.len_utf8(),
                ));
            }
        }
        if !self.row_wrapped(index) {
            let len = text.trim_end_matches(' ').len();
            text.truncate(len);
            map.truncate(len);
        }
        (text, map)
    }
    pub fn search_next(&mut self, backwards: bool) -> bool {
        let Some(mut state) = self.search.take() else {
            return false;
        };
        if self.is_alternate() {
            self.search = Some(state);
            return false;
        }
        let current = state
            .status
            .current
            .and_then(|i| state.results.get(i))
            .map(SearchMatch::key);
        if self.refresh_search_results(&mut state) {
            self.order_results(&mut state, current);
        }
        state.status.matches = state.results.len();
        if state.results.is_empty() {
            state.status.current = None;
            state.pending_navigation = (!state.status.complete).then_some(backwards);
            self.search = Some(state);
            return false;
        }
        let n = state.results.len();
        let index = match state.status.current {
            None => {
                let extent = self.viewport().physical_extent();
                let top = extent.start;
                let bottom = extent.end;
                let candidate = if backwards {
                    // The scan runs forward. A later match, including a soft
                    // wrapped one, can still start inside the current viewport.
                    state
                        .status
                        .complete
                        .then(|| {
                            state.results.iter().rposition(|item| {
                                self.main_tracker.positions.get(&item.rows[0].id).unwrap() < bottom
                            })
                        })
                        .flatten()
                } else {
                    state.results.iter().position(|item| {
                        self.main_tracker
                            .positions
                            .get(&item.rows[item.rows.len() - 1].id)
                            .unwrap()
                            >= top
                    })
                };
                if candidate.is_none() && !state.status.complete {
                    state.pending_navigation = Some(backwards);
                    self.search = Some(state);
                    return false;
                }
                candidate.unwrap_or(if backwards { n - 1 } else { 0 })
            }
            Some(i) => {
                if backwards {
                    (i + n - 1) % n
                } else {
                    (i + 1) % n
                }
            }
        };
        state.pending_navigation = None;
        state.status.current = Some(index);
        state.status.current_unavailable = false;
        let item = &state.results[index];
        let point = self
            .point_for(item.rows[0].id, item.start_column)
            .expect("validated search anchor");
        self.reveal_search_point(point);
        self.viewport_evicted = false;
        self.version = self.version.wrapping_add(1);
        self.search = Some(state);
        true
    }

    fn reveal_search_point(&mut self, point: Point) {
        self.scroll_fraction = 0.;
        self.expand_at(point.line);
        self.term.scroll_to_point(point);
        self.anchor_viewport();
    }
    fn order_results(&self, state: &mut SearchState, current: Option<SearchKey>) {
        // Appending/replacing text can discover an earlier match after later ones.
        // Navigation follows buffer order and retains its selected match identity.
        state.results.sort_unstable_by_key(|item| {
            (
                self.main_tracker.positions.get(&item.rows[0].id).unwrap(),
                item.start_column,
                item.end_column,
            )
        });
        state.index_results(&self.main_tracker.positions);
        state.status.current =
            current.and_then(|key| state.results.iter().position(|m| m.key() == key));
        state.status.current_unavailable |= current.is_some() && state.status.current.is_none();
    }
    pub(super) fn search_markers(&self, viewport: &super::folding::Viewport) -> SearchMarkers {
        let Some(state) = self.search.as_ref().filter(|state| {
            !self.is_alternate() && !state.results.is_empty() && viewport.history > 0
        }) else {
            return SearchMarkers::default();
        };
        let positions = &self.main_tracker.positions;
        let rows = if state.validated_content_version == Some(self.content_version) {
            state.marker_rows.clone()
        } else {
            // Output can evict or move rows before the next search slice runs.
            state.result_rows(positions)
        };
        let current = state
            .status
            .current
            .and_then(|index| state.results.get(index))
            .and_then(|item| positions.get(&item.rows[0].id));
        viewport.project_search_markers(rows, current)
    }

    pub(super) fn search_visible_marks(
        &self,
        viewport: &super::folding::Viewport,
    ) -> Option<Vec<Option<SearchHighlight>>> {
        if self.is_alternate() {
            return None;
        }
        let state = self.search.as_ref()?;
        if state.results.is_empty() {
            return None;
        }
        let columns = self.size.columns as usize;
        let grid = self.term.grid();
        // Index by retained row ID: still valid if output arrives between a
        // search step and a paint, or if folds hide intervening physical rows.
        let mut ranges: Vec<_> = viewport
            .lines
            .iter()
            .filter_map(|line| {
                let id = self
                    .main_tracker
                    .id_at((grid.history_size() as i32 + line.0) as usize)?;
                state.by_row.get(&id).cloned()
            })
            .collect();
        if ranges.is_empty() {
            return None;
        }
        let mut marks = vec![None; columns * viewport.lines.len()];
        ranges.sort_unstable_by_key(|range| range.start);
        let mut end = 0;
        let candidates = ranges.into_iter().flat_map(|range| {
            let start = range.start.max(end);
            end = end.max(range.end);
            start..range.end
        });
        let mut stamps = HashMap::new();
        for match_index in candidates {
            let item = &state.results[match_index];
            #[cfg(test)]
            MARK_CANDIDATES.set(MARK_CANDIDATES.get() + 1);
            let mut previous = None;
            let valid = state.validated_content_version == Some(self.content_version)
                || item.rows.iter().all(|stamp| {
                    let Some(index) = self.main_tracker.positions.get(&stamp.id) else {
                        return false;
                    };
                    if previous.is_some_and(|p| p + 1 != index) {
                        return false;
                    }
                    previous = Some(index);
                    let current = stamps
                        .entry((
                            stamp.id,
                            stamp.start_column,
                            stamp.end_column,
                            stamp.check_wrap,
                        ))
                        .or_insert_with(|| {
                            self.main_tracker.positions.get(&stamp.id).and_then(|i| {
                                self.range_stamp(
                                    i,
                                    stamp.start_column,
                                    stamp.end_column,
                                    stamp.check_wrap,
                                )
                            })
                        });
                    *current == Some(*stamp)
                });
            if !valid {
                continue;
            }
            for (offset, stamp) in item.rows.iter().enumerate() {
                let Some(index) = self.main_tracker.positions.get(&stamp.id) else {
                    continue;
                };
                let Some(row) = viewport.row(Line(index as i32 - grid.history_size() as i32))
                else {
                    continue;
                };
                let start = if offset == 0 { item.start_column } else { 0 };
                let end = if offset + 1 == item.rows.len() {
                    item.end_column
                } else {
                    self.size.columns as usize - 1
                };
                let row_start = row * columns;
                let highlight = if state.status.current == Some(match_index) {
                    SearchHighlight::Current
                } else {
                    SearchHighlight::Match
                };
                for mark in &mut marks[row_start + start..=row_start + end] {
                    *mark = Some(highlight);
                }
            }
        }
        Some(marks)
    }
}

#[cfg(test)]
mod visible_search_tests {
    use super::*;

    #[test]
    fn search_highlights_inspect_only_matches_on_displayed_rows() {
        let mut term = Terminal::new(20, 4, 100);
        term.feed("found found\r\n".repeat(64).as_bytes());
        term.start_search("found".into(), true);
        while !term.search_step(128).complete {}
        assert_eq!(term.search_status().matches, 128);
        for delta in [0, 11, -3] {
            term.scroll(delta);
            MARK_CANDIDATES.set(0);
            let snap = term.snapshot();
            assert!(
                snap.lines
                    .iter()
                    .any(|line| line.cells[0].search_highlight.is_some())
            );
            assert!(
                MARK_CANDIDATES.get() <= 8,
                "visited {}",
                MARK_CANDIDATES.get()
            );
        }
        term.scroll_to_bottom();
        term.feed(b"\r\nunmatched\r\nunmatched\r\nunmatched\r\nunmatched");
        assert!(term.search_visible_marks(&term.viewport()).is_none());
        assert!(term.snapshot().lines.iter().all(|line| {
            line.cells
                .iter()
                .all(|cell| cell.search_highlight.is_none())
        }));
    }
}
