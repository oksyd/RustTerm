//! Selection anchors and incremental search state. Metadata never replaces the grid.
use super::{SearchHighlight, Terminal};
use alacritty_terminal::{
    grid::{Dimensions, Scroll},
    index::Side,
    index::{Column, Direction, Line, Point},
    selection::{Selection, SelectionType},
    term::cell::Flags,
};
use regex::{Regex, RegexBuilder};
use std::collections::{HashMap, HashSet};

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
    Character,
    Word,
    Line,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchStatus {
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
pub(super) struct Tracker {
    ids: HashMap<usize, u64>,
    /// Buffer order. A recycled row moving towards the tail gets a new identity.
    order: Vec<usize>,
    pub positions: HashMap<u64, usize>,
}
impl Tracker {
    pub fn contains_address(&self, address: usize) -> bool {
        self.ids.contains_key(&address)
    }
    pub fn retire(&mut self, address: usize) {
        if let Some(id) = self.ids.remove(&address) {
            self.positions.remove(&id);
        }
    }
    pub fn reconcile(&mut self, addresses: Vec<usize>, next_id: &mut u64) {
        if addresses == self.order && self.ids.len() == addresses.len() {
            return;
        }
        let shared = self.order.len().min(addresses.len());
        if self.ids.len() == self.order.len() && self.order[..shared] == addresses[..shared] {
            // Height-only growth/shrink often changes just the trailing rows.
            // Keep the retained prefix's IDs without building another full hash map.
            for address in &self.order[shared..] {
                if let Some(id) = self.ids.remove(address) {
                    self.positions.remove(&id);
                }
            }
            for (index, &address) in addresses.iter().enumerate().skip(shared) {
                *next_id = next_id.wrapping_add(1);
                self.ids.insert(address, *next_id);
                self.positions.insert(*next_id, index);
            }
            self.order = addresses;
            self.shrink_excess();
            return;
        }
        let old: HashMap<usize, usize> = self
            .order
            .iter()
            .enumerate()
            .map(|(i, &a)| (a, i))
            .collect();
        let mut ids = HashMap::with_capacity(addresses.len());
        self.positions.clear();
        for (index, &address) in addresses.iter().enumerate() {
            let recycled = old.get(&address).is_some_and(|&before| index > before);
            let id = if !recycled {
                self.ids.get(&address).copied()
            } else {
                None
            }
            .unwrap_or_else(|| {
                *next_id = next_id.wrapping_add(1);
                *next_id
            });
            ids.insert(address, id);
            self.positions.insert(id, index);
        }
        self.ids = ids;
        self.order = addresses;
        self.shrink_excess();
    }
    pub fn reset(&mut self, addresses: impl Iterator<Item = usize>, next_id: &mut u64) {
        self.ids.clear();
        self.positions.clear();
        self.order.clear();
        let count = addresses.size_hint().0;
        self.ids.reserve(count);
        self.positions.reserve(count);
        self.order.reserve(count);
        for address in addresses {
            *next_id = next_id.wrapping_add(1);
            self.ids.insert(address, *next_id);
            self.positions.insert(*next_id, self.order.len());
            self.order.push(address);
        }
        self.shrink_excess();
    }
    fn shrink_excess(&mut self) {
        let limit = self.order.len().saturating_mul(2);
        if self.ids.capacity() > limit {
            self.ids.shrink_to_fit();
        }
        if self.positions.capacity() > limit {
            self.positions.shrink_to_fit();
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SelectionAnchor {
    pub stamps: Vec<RowStamp>,
    pub start: (u64, usize),
    pub end: (u64, usize),
}
pub(super) struct SearchState {
    pub status: SearchStatus,
    pub results: Vec<SearchMatch>,
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
        if let Some(&index) = self.main_tracker.positions.get(&id) {
            let offset = history.saturating_sub(index);
            let delta = offset as i64 - self.term.grid().display_offset() as i64;
            self.term.scroll_display(Scroll::Delta(
                delta.clamp(i32::MIN as i64, i32::MAX as i64) as i32
            ));
        } else {
            self.term.scroll_display(Scroll::Top);
            self.viewport_evicted = true;
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
        let addresses = (grid.topmost_line().0..=grid.bottommost_line().0)
            .map(|line| &grid[Point::new(Line(line), Column(0))] as *const _ as usize)
            .collect();
        let tracker = if self.is_alternate() {
            &mut self.alt_tracker
        } else {
            &mut self.main_tracker
        };
        tracker.reconcile(addresses, &mut self.next_row_id);
        if !self.is_alternate() {
            self.line_times.retain(&self.main_tracker);
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
        Some(RowStamp {
            id,
            fingerprint: hash,
            start_column,
            end_column,
            check_wrap,
        })
    }
    pub(super) fn stamp_valid(&self, stamp: RowStamp) -> bool {
        self.tracker().positions.get(&stamp.id).and_then(|&i| {
            self.range_stamp(i, stamp.start_column, stamp.end_column, stamp.check_wrap)
        }) == Some(stamp)
    }
    pub(super) fn point_for(&self, id: u64, column: usize) -> Option<Point> {
        let &index = self.tracker().positions.get(&id)?;
        Some(Point::new(
            Line(index as i32 - self.term.grid().history_size() as i32),
            Column(column.min(self.size.columns as usize - 1)),
        ))
    }
    fn viewport_point(&self, column: u16, row: u16) -> Point {
        Point::new(
            Line(row.min(self.size.rows - 1) as i32 - self.term.grid().display_offset() as i32),
            Column(column.min(self.size.columns - 1) as usize),
        )
    }
    pub(super) fn selection_anchor(&self, selection: &Selection) -> Option<SelectionAnchor> {
        let mut range = selection.to_range(&self.term)?;
        range.start = self.term.expand_wide(range.start, Direction::Left);
        range.end = self.term.expand_wide(range.end, Direction::Right);
        let history = self.term.grid().history_size() as i32;
        let first = (range.start.line.0 + history) as usize;
        let last = (range.end.line.0 + history) as usize;
        let stamps: Option<Vec<_>> = (first..=last)
            .map(|i| {
                self.range_stamp(
                    i,
                    if i == first { range.start.column.0 } else { 0 },
                    if i == last {
                        range.end.column.0
                    } else {
                        self.size.columns as usize - 1
                    },
                    i < last,
                )
            })
            .collect();
        let stamps = stamps?;
        Some(SelectionAnchor {
            start: (stamps.first()?.id, range.start.column.0),
            end: (stamps.last()?.id, range.end.column.0),
            stamps,
        })
    }
    pub fn begin_selection(&mut self, column: u16, row: u16, kind: SelectionKind) {
        let ty = match kind {
            SelectionKind::Character => SelectionType::Simple,
            SelectionKind::Word => SelectionType::Semantic,
            SelectionKind::Line => SelectionType::Lines,
        };
        let point = self.viewport_point(column, row);
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
        self.selection_anchor = self.selection_anchor(&selection);
        self.selection_kind = Some(kind);
        self.selection_origin = self.selection_anchor.as_ref().map(|a| a.start.0);
        self.term.selection = Some(selection);
        self.selection_invalidated = false;
        self.version = self.version.wrapping_add(1);
    }
    /// Select the retained buffer without materializing its text on the input path.
    pub fn select_all(&mut self) {
        let grid = self.term.grid();
        let mut selection = Selection::new(
            SelectionType::Simple,
            Point::new(grid.topmost_line(), Column(0)),
            Side::Left,
        );
        selection.update(
            Point::new(
                grid.bottommost_line(),
                Column(self.size.columns as usize - 1),
            ),
            Side::Right,
        );
        self.selection_anchor = self.selection_anchor(&selection);
        self.selection_kind = Some(SelectionKind::Character);
        self.selection_origin = self.selection_anchor.as_ref().map(|a| a.start.0);
        self.term.selection = Some(selection);
        self.selection_invalidated = false;
        self.version = self.version.wrapping_add(1);
    }
    pub fn update_selection(&mut self, column: u16, row: u16) {
        if !self.selection_valid(0) {
            let invalidated = self.selection_invalidated || self.selection_anchor.is_some();
            self.clear_selection();
            self.selection_invalidated = invalidated;
            return;
        }
        let point = self.viewport_point(column, row);
        if let Some(mut selection) = self.term.selection.take() {
            if self.selection_kind == Some(SelectionKind::Line)
                && let Some(origin) = self.selection_origin.and_then(|id| self.point_for(id, 0))
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
            } else {
                selection.update(point, Side::Right);
                selection.include_all();
            }
            self.selection_anchor = self.selection_anchor(&selection);
            self.term.selection = Some(selection);
            self.version = self.version.wrapping_add(1);
        }
    }
    pub(super) fn selection_valid(&self, unchanged_prefix: usize) -> bool {
        self.selection_anchor.as_ref().is_some_and(|a| {
            let positions = &self.tracker().positions;
            let history = self.term.grid().history_size();
            let mut previous = None;
            a.stamps.iter().enumerate().all(|(offset, &stamp)| {
                let Some(&index) = positions.get(&stamp.id) else {
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
            text.push_str(&self.row_text(Line(line), start..end + 1, false));
            if line < last.line.0
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
        self.next_query_id = self.next_query_id.wrapping_add(1);
        let rows = if self.is_alternate()
            || query.is_empty()
            || query.len() > MAX_QUERY_BYTES
            || query.contains(['\n', '\r'])
        {
            Vec::new()
        } else {
            (0..self.term.grid().total_lines())
                .filter_map(|i| self.main_tracker.id_at(i))
                .take(MAX_SEARCH_ROWS + 1)
                .collect()
        };
        self.search = Some(SearchState::new(
            self.next_query_id,
            query,
            case_sensitive,
            rows,
        ));
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
    fn retain_valid_results(&self, state: &mut SearchState, start: usize) {
        let mut stamps = HashMap::new();
        let removed = state.results.extract_if(start.., |item| {
            let mut previous = None;
            !item.rows.iter().all(|stamp| {
                let Some(&index) = self.main_tracker.positions.get(&stamp.id) else {
                    return false;
                };
                if previous.is_some_and(|i| i + 1 != index) {
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
        if self.is_alternate() {
            return self.search_status();
        }
        let Some(mut state) = self.search.take() else {
            return self.search_status();
        };
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
            state.watermark = (0..self.term.grid().total_lines())
                .filter_map(|i| self.main_tracker.id_at(i))
                .take(MAX_SEARCH_ROWS + 1)
                .collect();
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
            while state.cursor < end {
                let id = state.watermark[state.cursor];
                state.cursor += 1;
                let Some(&index) = self.main_tracker.positions.get(&id) else {
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
                        .is_none_or(|&p| p + 1 != index)
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
                let index = self.main_tracker.positions[&stamp.id];
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
                let grid = self.term.grid();
                let top = grid.history_size() - grid.display_offset();
                let bottom = top + self.size.rows as usize;
                let candidate = if backwards {
                    // The scan runs forward. A later match, including a soft
                    // wrapped one, can still start inside the current viewport.
                    state
                        .status
                        .complete
                        .then(|| {
                            state.results.iter().rposition(|item| {
                                self.main_tracker.positions[&item.rows[0].id] < bottom
                            })
                        })
                        .flatten()
                } else {
                    state.results.iter().position(|item| {
                        self.main_tracker.positions[&item.rows[item.rows.len() - 1].id] >= top
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
        let item = &state.results[index];
        let point = self
            .point_for(item.rows[0].id, item.start_column)
            .expect("validated search anchor");
        self.term.scroll_to_point(point);
        self.viewport_evicted = false;
        self.anchor_viewport();
        self.version = self.version.wrapping_add(1);
        self.search = Some(state);
        true
    }
    fn order_results(&self, state: &mut SearchState, current: Option<SearchKey>) {
        // Appending/replacing text can discover an earlier match after later ones.
        // Navigation follows buffer order and retains its selected match identity.
        state.results.sort_unstable_by_key(|item| {
            (
                self.main_tracker.positions[&item.rows[0].id],
                item.start_column,
                item.end_column,
            )
        });
        state.status.current =
            current.and_then(|key| state.results.iter().position(|m| m.key() == key));
    }
    pub(super) fn search_visible_marks(&self) -> Option<Vec<Option<SearchHighlight>>> {
        if self.is_alternate() {
            return None;
        }
        let state = self.search.as_ref()?;
        if state.results.is_empty() {
            return None;
        }
        let columns = self.size.columns as usize;
        let mut marks = vec![None; columns * self.size.rows as usize];
        let grid = self.term.grid();
        let top = grid.history_size() - grid.display_offset();
        let mut stamps = HashMap::new();
        for (match_index, item) in state.results.iter().enumerate() {
            let first = self.main_tracker.positions.get(&item.rows[0].id);
            let last = self
                .main_tracker
                .positions
                .get(&item.rows[item.rows.len() - 1].id);
            if first.is_none_or(|&i| i >= top + self.size.rows as usize)
                || last.is_none_or(|&i| i < top)
            {
                continue;
            }
            let mut previous = None;
            let valid = state.validated_content_version == Some(self.content_version)
                || item.rows.iter().all(|stamp| {
                    let Some(&index) = self.main_tracker.positions.get(&stamp.id) else {
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
                            self.main_tracker.positions.get(&stamp.id).and_then(|&i| {
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
                let Some(&index) = self.main_tracker.positions.get(&stamp.id) else {
                    continue;
                };
                if index < top || index >= top + self.size.rows as usize {
                    continue;
                }
                let start = if offset == 0 { item.start_column } else { 0 };
                let end = if offset + 1 == item.rows.len() {
                    item.end_column
                } else {
                    self.size.columns as usize - 1
                };
                let row_start = (index - top) * columns;
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
