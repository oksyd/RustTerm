//! UI-independent terminal state. Alacritty owns the only authoritative grid.
//!
//! The adapter ignores OSC 8 hyperlinks and OSC 52 clipboard access.
//! String controls and combining sequences are bounded before they can grow without limit.
use alacritty_terminal::{
    Term,
    event::{Event, EventListener, WindowSize},
    grid::{Dimensions, Scroll},
    index::{Column, Line, Point},
    term::{Config, Osc52, SEMANTIC_ESCAPE_CHARS, TermMode, cell::Flags},
    vte::ansi::{Color, NamedColor, Processor, Rgb, Timeout},
};
use std::sync::{Arc, Mutex};
mod folding;
mod links;
pub use links::TerminalLink;
pub mod integration;
mod redraw;
mod reflow;
mod selection_matches;
pub mod shell;
mod snapshot;
mod timestamps;
pub use alacritty_terminal::vte::ansi::CursorShape;
pub use folding::{
    BlockCopy, CommandBlock, CommandDetails, CommandHeading, CommandResult, FoldMarker,
};
pub use shell::{ShellCommand, ShellInteraction, ShellSnapshot};
pub use timestamps::LineTimestamp;

/// Scrollback capacity target; upstream caches may retain additional memory.
pub const DEFAULT_HISTORY_BYTES: usize = 64 * 1024 * 1024;
/// Conservative charge for a cell, its bounded extras and allocation overhead.
/// This estimates logical history capacity, not allocated memory or process RSS.
pub const CELL_BUDGET_BYTES: usize = 512;
pub const MAX_COLUMNS: u16 = 500;
pub const MAX_ROWS: u16 = 200;
pub const MAX_WORD_DELIMITER_BYTES: usize = 256;
const MAX_COMBINING: usize = 32;
const MAX_STRING_BYTES: usize = 4096;

/// Editable boundaries. Spaces and horizontal tabs always separate words.
pub fn default_word_delimiters() -> String {
    SEMANTIC_ESCAPE_CHARS
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect()
}

pub fn valid_word_delimiters(value: &str) -> bool {
    value.len() <= MAX_WORD_DELIMITER_BYTES && !value.chars().any(char::is_control)
}

fn terminal_options(history: usize, semantic_escape_chars: String) -> Config {
    Config {
        scrolling_history: history,
        semantic_escape_chars,
        osc52: Osc52::Disabled,
        ..Config::default()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchHighlight {
    Match,
    Current,
}

#[derive(Clone, Debug, Default)]
pub struct SearchMarkers {
    /// Distinct visual rows measured from the top of the retained scrollback.
    pub rows: Arc<[usize]>,
    pub current: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    pub default_foreground: bool,
    pub selected: bool,
    pub search_highlight: Option<SearchHighlight>,
    pub selection_match: bool,
    pub text: String,
    /// 0 for the continuation of a wide cell, 1 or 2 otherwise.
    pub width: u8,
    pub foreground: u32,
    pub background: u32,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub hidden: bool,
    pub strikeout: bool,
}
#[derive(Clone, Debug)]
pub struct VisualLine {
    /// Stable retained-row identity, including across scrollback movement.
    pub id: u64,
    source_line: Line,
    pub number: Option<usize>,
    pub received_at: Option<LineTimestamp>,
    pub cells: Arc<Vec<Cell>>,
    pub wrapped: bool,
}
#[derive(Clone, Debug)]
pub struct Cursor {
    pub row: usize,
    pub column: usize,
    pub width: u8,
    pub shape: CursorShape,
    pub visible: bool,
}
#[derive(Clone, Copy, Debug, Default)]
pub struct Modes {
    pub application_cursor: bool,
    pub bracketed_paste: bool,
    pub mouse_click: bool,
    pub mouse_drag: bool,
    pub mouse_motion: bool,
    pub sgr_mouse: bool,
    pub utf8_mouse: bool,
    pub alternate_scroll: bool,
    pub focus_reporting: bool,
}
#[derive(Clone, Debug)]
pub struct Snapshot {
    /// Fraction of the first row clipped above the viewport; zero for aligned rows.
    pub scroll_fraction: f32,
    pub command_blocks: Vec<CommandBlock>,
    pub sticky_command: Option<CommandHeading>,
    pub new_output: bool,
    pub shell: ShellSnapshot,
    pub search: SearchStatus,
    pub search_markers: SearchMarkers,
    pub has_selection: bool,
    pub has_content: bool,
    pub selection_invalidated: bool,
    /// The paused viewport's first row was evicted. Explicit navigation clears this.
    pub viewport_evicted: bool,
    pub is_read_only: bool,
    pub mouse_reporting: bool,
    pub version: u64,
    pub columns: u16,
    pub rows: u16,
    pub lines: Vec<VisualLine>,
    pub cursor: Cursor,
    pub alternate_screen: bool,
    pub display_offset: usize,
    pub history_size: usize,
    pub title: String,
    pub modes: Modes,
    pub history_limit: usize,
    /// History capacity target, excluding extra retained upstream cache allocations.
    pub history_budget_bytes: usize,
}

#[derive(Clone, Default)]
struct Events(Arc<Mutex<Vec<Event>>>);
impl EventListener for Events {
    fn send_event(&self, event: Event) {
        // Clipboard requests are discarded even if a future upstream default changes.
        if matches!(
            event,
            Event::PtyWrite(_)
                | Event::Title(_)
                | Event::ResetTitle
                | Event::ColorRequest(..)
                | Event::TextAreaSizeRequest(_)
        ) {
            let mut events = self.0.lock().unwrap_or_else(|error| error.into_inner());
            // Budget changes also emit a title event, even without subsequent PTY data.
            // Coalesce these notifications rather than retaining an idle-session queue.
            if matches!(event, Event::Title(_) | Event::ResetTitle)
                && let Some(last @ (Event::Title(_) | Event::ResetTitle)) = events.last_mut()
            {
                *last = event;
            } else {
                events.push(event);
            }
        }
    }
}

#[derive(Clone, Copy)]
struct Size {
    columns: u16,
    rows: u16,
}
impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows as usize
    }
    fn screen_lines(&self) -> usize {
        self.rows as usize
    }
    fn columns(&self) -> usize {
        self.columns as usize
    }
}
impl Size {
    fn new(columns: u16, rows: u16) -> Self {
        Self {
            columns: columns.clamp(2, MAX_COLUMNS),
            rows: rows.clamp(1, MAX_ROWS),
        }
    }
}

/// Tiny safety gate for string controls, not an ANSI parser. All accepted bytes still
/// go through VTE. An oversized string is cancelled and skipped through its terminator.
#[derive(Default)]
struct StringGate {
    escaped: bool,
    string: Option<StringState>,
}
struct StringState {
    osc: bool,
    dcs: bool,
    dcs_passthrough: bool,
    len: usize,
    escaped: bool,
    dropped: bool,
    prefix: Vec<u8>,
}
impl StringGate {
    fn accept(&mut self, byte: u8) -> GateAction {
        if let Some(s) = &mut self.string {
            // VTE ends a non-discarded string at ESC itself; the next byte may
            // start a completely different escape sequence, not necessarily ST.
            if byte == 0x1b && !s.dropped {
                self.string = None;
                self.escaped = true;
                return GateAction::Byte;
            }
            let end = (s.osc && byte == 7)
                || (s.dcs_passthrough && byte == 0x9c)
                || (s.escaped && byte == b'\\')
                || matches!(byte, 0x18 | 0x1a);
            if end {
                let dropped = s.dropped;
                self.string = None;
                return if dropped {
                    GateAction::Skip
                } else {
                    GateAction::Byte
                };
            }
            s.len = s.len.saturating_add(1);
            if s.dcs && matches!(byte, 0x40..=0x7e) {
                s.dcs_passthrough = true;
            }
            s.escaped = byte == 0x1b;
            let osc_ignored = matches!(byte, 0x00..=0x06 | 0x08..=0x17 | 0x19 | 0x1c..=0x1f);
            if s.osc && !osc_ignored && s.prefix.len() < 2 {
                s.prefix.push(byte);
            }
            // Unused hyperlink metadata may otherwise be shared by many cells.
            let hyperlink = s.osc && s.prefix == b"8;";
            if !s.dropped && (s.len > MAX_STRING_BYTES || hyperlink) {
                s.dropped = true;
                return GateAction::Cancel;
            }
            return if s.dropped {
                GateAction::Skip
            } else {
                GateAction::Byte
            };
        }
        if self.escaped && matches!(byte, b']' | b'P' | b'_' | b'^' | b'X') {
            self.string = Some(StringState {
                osc: byte == b']',
                dcs: byte == b'P',
                dcs_passthrough: false,
                len: 0,
                escaped: false,
                dropped: false,
                prefix: Vec::with_capacity(2),
            });
        }
        // VTE executes C0 controls and ignores DEL/non-ASCII while remaining in
        // Escape. Losing that state would let ESC + C0 + ']' bypass the OSC cap.
        if !(self.escaped && matches!(byte, 0x00..=0x17 | 0x19 | 0x1c..=0x1f | 0x7f..=0xff)) {
            self.escaped = byte == 0x1b;
        }
        GateAction::Byte
    }
}
enum GateAction {
    Byte,
    Skip,
    Cancel,
}

// The adapter does not advertise synchronized-update support. Parse immediately;
// upstream buffering otherwise requires a separate timeout wakeup path.
#[derive(Default)]
struct ImmediateUpdates;
impl Timeout for ImmediateUpdates {
    fn set_timeout(&mut self, _: std::time::Duration) {}
    fn clear_timeout(&mut self) {}
    fn pending_timeout(&self) -> bool {
        false
    }
}

pub struct Terminal {
    line_times: timestamps::LineTimes,
    redraws: redraw::Redraws,
    blocks: folding::Blocks,
    shell: shell::ShellTracker,
    main_tracker: reading::Tracker,
    alt_tracker: reading::Tracker,
    next_row_id: u64,
    viewport_anchor: Option<u64>,
    viewport_evicted: bool,
    scroll_fraction: f64,
    selection_anchor: Option<reading::SelectionAnchor>,
    selection_invalidated: bool,
    selection_kind: Option<SelectionKind>,
    selection_origin: Option<reading::SelectionOrigin>,
    search: Option<reading::SearchState>,
    next_query_id: u64,
    highlight_enabled: bool,
    selection_highlight_enabled: bool,
    selection_match_cache: Mutex<selection_matches::MatchCache>,
    cache_command_output: bool,
    highlight_cache: Mutex<highlight::HighlightCache>,
    snapshot_cache: Mutex<snapshot::Cache>,
    final_alternate: Option<Arc<Snapshot>>,
    theme: Theme,
    frozen_colors: Option<alacritty_terminal::term::color::Colors>,
    max_rows_seen: u16,
    term: Term<Events>,
    parser: Option<Processor<ImmediateUpdates>>,
    events: Events,
    gate: StringGate,
    version: u64,
    content_version: u64,
    new_output: bool,
    size: Size,
    requested_history: usize,
    history_bytes: usize,
    history_limit: usize,
    max_columns_seen: u16,
    title: String,
}
impl Terminal {
    pub fn new(columns: u16, rows: u16, history: usize) -> Self {
        Self::with_history_budget(columns, rows, history, DEFAULT_HISTORY_BYTES)
    }
    pub fn with_history_budget(columns: u16, rows: u16, history: usize, bytes: usize) -> Self {
        let size = Size::new(columns, rows);
        let limit = budget_limit(history, bytes, size.columns);
        let events = Events::default();
        let term = Term::new(
            terminal_options(limit, SEMANTIC_ESCAPE_CHARS.to_owned()),
            &size,
            events.clone(),
        );
        let mut terminal = Self {
            line_times: Default::default(),
            redraws: Default::default(),
            blocks: Default::default(),
            shell: shell::ShellTracker::default(),
            main_tracker: Default::default(),
            alt_tracker: Default::default(),
            next_row_id: 0,
            viewport_anchor: None,
            viewport_evicted: false,
            scroll_fraction: 0.,
            selection_anchor: None,
            selection_invalidated: false,
            selection_kind: None,
            selection_origin: None,
            search: None,
            next_query_id: 0,
            highlight_enabled: true,
            selection_highlight_enabled: true,
            selection_match_cache: Mutex::new(Default::default()),
            cache_command_output: true,
            highlight_cache: Mutex::new(Default::default()),
            snapshot_cache: Mutex::new(Default::default()),
            final_alternate: None,
            theme: Theme::default(),
            frozen_colors: None,
            max_rows_seen: size.rows,
            term,
            parser: Some(Processor::new()),
            events,
            gate: StringGate::default(),
            version: 0,
            content_version: 0,
            new_output: false,
            size,
            requested_history: history,
            history_bytes: bytes,
            history_limit: limit,
            max_columns_seen: size.columns,
            title: String::new(),
        };
        terminal.reconcile_rows();
        terminal
    }
    /// Feed ordered bytes. Caller bounds each batch and forwards every returned reply.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        self.feed_at(bytes, LineTimestamp::now())
    }

    fn feed_at(&mut self, bytes: &[u8], received_at: LineTimestamp) -> Vec<Vec<u8>> {
        if self.parser.is_none() {
            return Vec::new();
        }
        // ANSI output can change live rows, including rows scrolled into history
        // during this batch. Earlier scrollback only needs identity validation.
        let unchanged_selection_rows = self.selection_anchor.as_ref().map_or(0, |anchor| {
            let history = self.term.grid().history_size();
            anchor.stamps.partition_point(|stamp| {
                self.tracker()
                    .positions
                    .get(&stamp.id)
                    .is_some_and(|index| index < history)
            })
        });
        let mut pending = bytes;
        let mut plain_ready = false;
        while let Some((&byte, tail)) = pending.split_first() {
            if plain_ready && !self.term.grid().cursor.input_needs_wrap {
                let room = self.term.columns() - self.term.grid().cursor.point.column.0;
                let count = pending
                    .iter()
                    .take(room)
                    .take_while(|&&byte| matches!(byte, b' '..=b'~'))
                    .count();
                if count > 0 {
                    let old_point = self.term.grid().cursor.point;
                    if let Some(parser) = self.parser.as_mut() {
                        parser.advance(&mut self.term, &pending[..count]);
                    }
                    self.line_times.observe_text(&pending[..count]);
                    if !self.is_alternate() {
                        self.line_times
                            .written(self.term.grid(), received_at, false);
                        if self.term.grid().display_offset() > 0 {
                            self.new_output = true;
                        }
                        if !self.term.mode().intersects(TermMode::MOUSE_MODE)
                            && !self.shell.snapshot.at_prompt
                        {
                            self.redraws.observe(
                                self.term.grid(),
                                old_point,
                                &timestamps::Observation::text(),
                            );
                        }
                    }
                    pending = &pending[count..];
                    continue;
                }
            }
            pending = tail;
            plain_ready = false;
            let old_alt = self.is_alternate();
            let old_top = &self.term.grid()[Point::new(self.term.grid().topmost_line(), Column(0))]
                as *const _ as usize;
            let old_point = self.term.grid().cursor.point;
            let old_cursor = old_point.line;
            let old_cursor_address = timestamps::address(self.term.grid(), old_cursor);
            let accepted = match self.gate.accept(byte) {
                GateAction::Byte => byte,
                GateAction::Cancel => 0x18,
                GateAction::Skip => {
                    self.shell_byte(byte);
                    continue;
                }
            };
            let observation = self.line_times.observe(accepted, self.term.grid(), old_alt);
            // A literal ASCII print proves all parsers have left their control
            // sequence/UTF-8 states. Batch only within this row; wrapping and
            // every non-ASCII/control byte retain the precise observation path.
            plain_ready = observation.printed
                && matches!(accepted, b' '..=b'~')
                && self.gate.string.is_none()
                && !self.gate.escaped;
            if !old_alt
                && self.term.grid().display_offset() > 0
                && (observation.printed || observation.linefeed || observation.bulk)
            {
                self.new_output = true;
            }
            // LF emits the row being left, even when it contains no characters.
            // The destination remains unused until something is written there.
            if !old_alt && observation.linefeed {
                self.line_times.written(self.term.grid(), received_at, true);
            }
            self.parser
                .as_mut()
                .unwrap()
                .advance(&mut self.term, &[accepted]);
            self.shell_byte(byte);
            if !old_alt
                && !self
                    .term
                    .mode()
                    .intersects(TermMode::ALT_SCREEN | TermMode::MOUSE_MODE)
                && !self.shell.snapshot.at_prompt
            {
                self.redraws
                    .observe(self.term.grid(), old_point, &observation);
            }
            // CSI REP ends in ASCII 'b' and can repeat a preceding combining mark.
            // VTE caps its count at u16::MAX; clamp the resulting extras as well.
            if byte >= 0x80 || byte == b'b' {
                self.bound_combining();
            }
            let new_top = &self.term.grid()[Point::new(self.term.grid().topmost_line(), Column(0))]
                as *const _ as usize;
            if old_alt != self.is_alternate() {
                self.scroll_fraction = 0.;
                self.redraws.clear();
                self.shell.suspend_input();
                self.clear_selection();
                if self.is_alternate() {
                    self.alt_tracker.clear();
                }
                self.reconcile_rows();
            } else if old_top != new_top {
                // Retirement is O(1) per scroll. Reconcile positions once per input batch.
                if old_alt {
                    self.alt_tracker.retire(old_top);
                } else {
                    self.main_tracker.retire(old_top);
                    self.line_times.retire(old_top);
                    self.redraws.retire(old_top);
                }
            }
            if observation.reset {
                self.redraws.clear();
                self.clear_command_blocks(false);
                self.shell.reset_interaction();
                self.line_times.reset();
            }
            if observation.clear_screen && !self.is_alternate() {
                self.redraws.clear();
                self.reconcile_rows();
                self.clear_command_blocks(true);
            }
            if !self.is_alternate() {
                if old_alt {
                    self.line_times.primary_restored(self.term.grid());
                }
                if let Some(rows) = observation.cleanup.clone() {
                    self.line_times
                        .cleanup_screen(self.term.grid(), rows, &observation);
                }
                let cursor = self.term.grid().cursor.point.line;
                let cursor_address = timestamps::address(self.term.grid(), cursor);
                if cursor == old_cursor
                    && cursor_address != old_cursor_address
                    && (observation.printed || matches!(accepted, b'\n' | 0x0b | 0x0c))
                {
                    self.line_times.retire(cursor_address);
                    self.redraws.retire(cursor_address);
                }
                if observation.printed {
                    self.line_times
                        .written(self.term.grid(), received_at, false);
                }
                if observation.bulk {
                    self.line_times
                        .stamp_bulk(self.term.grid(), &observation, received_at);
                }
            }
        }
        if !bytes.is_empty() {
            self.version = self.version.wrapping_add(1);
            self.content_version = self.content_version.wrapping_add(1);
        }
        self.reconcile_rows();
        self.redraws.retain_screen(self.term.grid());
        self.restore_viewport();
        self.refresh_selection(unchanged_selection_rows);
        if !bytes.is_empty()
            && let Some(search) = &mut self.search
        {
            search.dirty = true;
        }
        let events = std::mem::take(
            &mut *self
                .events
                .0
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
        );
        let mut replies = Vec::new();
        for event in events {
            match event {
                Event::PtyWrite(text) => replies.push(text.into_bytes()),
                Event::Title(title) => self.title = title,
                Event::ResetTitle => self.title.clear(),
                Event::ColorRequest(index, format) => {
                    let rgb =
                        self.term.colors()[index]
                            .map(rgb_u32)
                            .unwrap_or_else(|| match index {
                                256 => self.theme.foreground,
                                257 => self.theme.background,
                                _ => palette(index),
                            });
                    replies.push(
                        format(Rgb {
                            r: (rgb >> 16) as u8,
                            g: (rgb >> 8) as u8,
                            b: rgb as u8,
                        })
                        .into_bytes(),
                    );
                }
                Event::TextAreaSizeRequest(format) => replies.push(
                    format(WindowSize {
                        num_lines: self.size.rows,
                        num_cols: self.size.columns,
                        cell_width: 0,
                        cell_height: 0,
                    })
                    .into_bytes(),
                ),
                _ => (),
            }
        }
        replies
    }
    fn bound_combining(&mut self) {
        let grid = self.term.grid_mut();
        let mut point = grid.cursor.point;
        if !grid.cursor.input_needs_wrap {
            point.column.0 = point.column.0.saturating_sub(1);
        }
        if grid[point].flags.contains(Flags::WIDE_CHAR_SPACER) {
            point.column.0 = point.column.0.saturating_sub(1);
        }
        let cell = &mut grid[point];
        if let Some(chars) = cell.zerowidth().filter(|chars| chars.len() > MAX_COMBINING) {
            let marks: Vec<char> = chars[..MAX_COMBINING].to_vec();
            let underline = cell.underline_color();
            cell.extra = None;
            for c in marks {
                cell.push_zerowidth(c);
            }
            cell.set_underline_color(underline);
        }
    }
    pub fn resize(&mut self, columns: u16, rows: u16) {
        let size = Size::new(columns, rows);
        if size.columns == self.size.columns && size.rows == self.size.rows {
            return;
        }
        self.scroll_fraction = 0.;
        if size.columns != self.size.columns && size.rows != self.size.rows {
            // The grid removes bottom rows on a height shrink when the cursor is
            // above them. Reconcile those identities before mapping column reflow.
            self.resize(self.size.columns, size.rows);
        }
        let width_changed = size.columns != self.size.columns;
        let keep_search_visible = !width_changed && self.current_search_visible();
        let logical = if self.is_alternate() {
            self.line_times.alternate_resized();
            None
        } else {
            Some(self.line_times.logical_lines(self.term.grid()))
        };
        let (reading, blocks) = if width_changed {
            let map = reflow::LogicalMap::new(logical.as_ref(), self.term.grid().topmost_line());
            (
                self.reading_before_reflow(&map),
                self.blocks_before_reflow(&map),
            )
        } else {
            Default::default()
        };
        self.redraws.clear();
        let readline_prompt = width_changed
            .then(|| self.readline_prompt_distance())
            .flatten();
        let inactive_primary_shrinks = self.is_alternate() && size.rows < self.size.rows;
        if size.columns > self.max_columns_seen {
            self.max_columns_seen = size.columns;
            let limit = budget_limit(
                self.requested_history,
                self.history_bytes,
                self.max_columns_seen,
            );
            if limit != self.history_limit {
                self.apply_history_limit(limit);
            }
        }
        self.max_rows_seen = self.max_rows_seen.max(size.rows);
        self.term.resize(size);
        self.size = size;
        let logical = self.line_times.after_resize(self.term.grid(), logical);
        if width_changed {
            // Reflow replaces identities, but reuse the active tracker's allocations.
            self.reset_rows();
            self.restore_readline_prompt(readline_prompt);
            self.viewport_anchor = None;
            let map = reflow::LogicalMap::new(logical.as_ref(), self.term.grid().topmost_line());
            let folded_anchor = self.blocks_after_reflow(&map, blocks);
            self.reading_after_reflow(&map, reading, folded_anchor);
        } else {
            if inactive_primary_shrinks {
                // We cannot reconcile the inaccessible primary grid after every
                // shrink. Repeated height changes may recycle its old pointers.
                // Keep its text/query, but rescan identities on return to main.
                self.main_tracker.clear();
                self.viewport_anchor = None;
            }
            // Height changes do not reflow cells. Reconcile physical row identities
            // and validate affected anchors rather than restarting an entire search.
            self.reconcile_rows();
            self.restore_viewport();
            self.refresh_selection(0);
            if let Some(search) = &mut self.search {
                search.dirty = true;
            }
            if keep_search_visible {
                self.keep_current_search_visible();
            }
        }
        // Highlight spans are keyed by exact logical text; mappings to cells are
        // rebuilt per snapshot. Layout changes cannot make a cached span stale.
        self.version = self.version.wrapping_add(1);
        self.content_version = self.content_version.wrapping_add(1);
    }
    fn is_alternate(&self) -> bool {
        self.term.mode().contains(TermMode::ALT_SCREEN)
    }
    fn apply_history_limit(&mut self, limit: usize) {
        self.history_limit = limit;
        let delimiters = self.term.semantic_escape_chars().to_owned();
        self.term.set_options(terminal_options(limit, delimiters));
    }

    pub fn set_word_delimiters(&mut self, punctuation: &str) {
        let mut delimiters = punctuation.to_owned();
        for space in [' ', '\t'] {
            if !delimiters.contains(space) {
                delimiters.push(space);
            }
        }
        if self.term.semantic_escape_chars() != delimiters {
            self.term
                .set_options(terminal_options(self.history_limit, delimiters));
            self.version = self.version.wrapping_add(1);
        }
    }

    pub fn set_history_budget(&mut self, bytes: usize) {
        self.set_history_limits(self.requested_history, bytes);
    }
    /// Limit accessible history using a conservative per-row estimate. Upstream
    /// may retain discarded cells and vector capacity for reuse after shrinking.
    pub fn set_history_limits(&mut self, lines: usize, bytes: usize) {
        if lines == self.requested_history && bytes == self.history_bytes {
            return;
        }
        self.requested_history = lines;
        self.history_bytes = bytes;
        let limit = budget_limit(lines, bytes, self.max_columns_seen);
        if limit == self.history_limit {
            self.version = self.version.wrapping_add(1);
            return;
        }
        let shrinking = limit < self.history_limit;
        if shrinking && self.is_alternate() {
            self.line_times.alternate_resized();
        }
        self.apply_history_limit(limit);
        if shrinking {
            self.reconcile_rows();
            self.restore_viewport();
            self.refresh_selection(0);
            if let Some(search) = &mut self.search {
                search.dirty = true;
            }
        }
        self.version = self.version.wrapping_add(1);
        self.content_version = self.content_version.wrapping_add(1);
    }
    pub fn history_budget(&self) -> usize {
        self.history_bytes
    }
    pub fn freeze(&mut self) {
        if self.parser.take().is_none() {
            return;
        }
        self.shell.reset_interaction();
        self.redraws.clear();
        if self.is_alternate() {
            self.final_alternate = Some(Arc::new(self.snapshot()));
            self.term.swap_alt();
            self.line_times.primary_restored(self.term.grid());
        }
        self.frozen_colors = Some(*self.term.colors());
        // Transfer only the authoritative main grid to a fresh read-only container.
        // Dropping the old Term also releases title stacks, alternate state and protocol modes.
        let mut readonly = Term::new(
            terminal_options(
                self.history_limit,
                self.term.semantic_escape_chars().to_owned(),
            ),
            &self.size,
            self.events.clone(),
        );
        std::mem::swap(readonly.grid_mut(), self.term.grid_mut());
        self.term = readonly;
        self.clear_selection();
        self.reconcile_rows();
        self.restore_viewport();
        self.version = self.version.wrapping_add(1);
        self.content_version = self.content_version.wrapping_add(1);
    }
    /// Estimated base cost for archive admission. Scrollback is charged separately;
    /// upstream cache allocations may exceed these capacity targets.
    pub fn retained_base_bytes(&self) -> usize {
        let screens = 2usize
            .saturating_mul(self.max_rows_seen as usize)
            .saturating_mul(row_charge(self.max_columns_seen));
        let final_alternate = self
            .final_alternate
            .as_ref()
            .map_or(0, |s| s.rows as usize * row_charge(s.columns));
        screens
            .saturating_add(final_alternate)
            .saturating_add(256 * 1024)
            .saturating_add(MAX_STRING_BYTES * 2)
    }
    pub fn final_alt_snapshot(&self) -> Option<Arc<Snapshot>> {
        self.final_alternate.clone()
    }
    pub fn encode_focus(&self, focused: bool) -> Vec<u8> {
        if self.modes().focus_reporting {
            if focused {
                b"\x1b[I".to_vec()
            } else {
                b"\x1b[O".to_vec()
            }
        } else {
            Vec::new()
        }
    }
    pub fn dimensions(&self) -> (u16, u16) {
        (self.size.columns, self.size.rows)
    }
    pub fn scroll(&mut self, lines: i32) {
        self.shell.snapshot.focused_command = None;
        self.shell.snapshot.command_unavailable = false;
        let offset = self.viewport().offset.saturating_add_signed(lines as isize);
        self.scroll_projected(offset);
        self.viewport_evicted = false;
        self.anchor_viewport();
        self.version = self.version.wrapping_add(1);
    }
    pub fn scroll_fractional(&mut self, lines: f64) {
        if !lines.is_finite() || lines == 0. || self.is_alternate() {
            return;
        }
        let viewport = self.viewport();
        let position = (viewport.offset as f64 - self.scroll_fraction + lines)
            .clamp(0., viewport.history as f64);
        let offset = position.ceil() as usize;
        self.scroll_projected(offset);
        self.scroll_fraction = (offset as f64 - position).clamp(0., 1.);
        self.shell.snapshot.focused_command = None;
        self.shell.snapshot.command_unavailable = false;
        self.viewport_evicted = false;
        self.anchor_viewport();
        self.version = self.version.wrapping_add(1);
    }
    pub fn scroll_to_bottom(&mut self) {
        self.scroll_to(0);
    }
    /// Set the viewport's distance from the live bottom, clamped to retained history.
    pub fn scroll_to(&mut self, offset: usize) {
        let viewport = self.viewport();
        let target = offset.min(viewport.history);
        self.scroll(target as i32 - viewport.offset as i32);
    }
    pub fn modes(&self) -> Modes {
        let m = self.term.mode();
        Modes {
            application_cursor: m.contains(TermMode::APP_CURSOR),
            bracketed_paste: m.contains(TermMode::BRACKETED_PASTE),
            mouse_click: m.contains(TermMode::MOUSE_REPORT_CLICK),
            mouse_drag: m.contains(TermMode::MOUSE_DRAG),
            mouse_motion: m.contains(TermMode::MOUSE_MOTION),
            sgr_mouse: m.contains(TermMode::SGR_MOUSE),
            utf8_mouse: m.contains(TermMode::UTF8_MOUSE),
            alternate_scroll: m.contains(TermMode::ALTERNATE_SCROLL),
            focus_reporting: m.contains(TermMode::FOCUS_IN_OUT),
        }
    }
    fn color(&self, color: Color) -> u32 {
        let index = match color {
            Color::Spec(rgb) => return rgb_u32(rgb),
            Color::Indexed(i) => i as usize,
            Color::Named(name) => name as usize,
        };
        self.frozen_colors
            .as_ref()
            .unwrap_or_else(|| self.term.colors())[index]
            .map(rgb_u32)
            .unwrap_or_else(|| match index {
                256 => self.theme.foreground,
                257 => self.theme.background,
                _ => palette(index),
            })
    }
    pub fn encode_key(&self, key: &str, ctrl: bool, alt: bool, shift: bool) -> Vec<u8> {
        input::key(key, ctrl, alt, shift, self.modes().application_cursor)
    }
    pub fn encode_paste(&self, text: &str) -> Vec<u8> {
        input::paste(text, self.modes().bracketed_paste)
    }
    /// Recheck modes when the actor handles the wheel, since a TUI may have exited
    /// after the UI snapshot was produced. Never turn a stale wheel into shell input.
    pub fn encode_alternate_scroll(&self, lines: i32) -> Vec<u8> {
        let mode = self.term.mode();
        if lines == 0
            || !mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL)
            || mode.intersects(TermMode::MOUSE_MODE)
        {
            return Vec::new();
        }
        self.encode_key(if lines > 0 { "up" } else { "down" }, false, false, false)
            .repeat(lines.unsigned_abs().min(100) as usize)
    }
    /// Buttons: 0/1/2 left/middle/right, 64/65 wheel up/down, 66/67 wheel left/right.
    /// Coordinates are zero based.
    pub fn encode_mouse(&self, event: MouseEvent) -> Vec<u8> {
        input::mouse(event, self.modes(), self.size.columns, self.size.rows)
    }
}
// Charge logical history by the widest cells seen. Upstream spare rows and
// retained vector capacity are outside this target and follow its cache policy.
fn budget_limit(lines: usize, bytes: usize, columns: u16) -> usize {
    lines.min(bytes / row_charge(columns))
}
fn row_charge(columns: u16) -> usize {
    columns as usize * CELL_BUDGET_BYTES + 64
}
fn rgb_u32(rgb: Rgb) -> u32 {
    ((rgb.r as u32) << 16) | ((rgb.g as u32) << 8) | rgb.b as u32
}
fn dim(c: u32) -> u32 {
    (((c >> 16) & 255) * 2 / 3) << 16 | (((c >> 8) & 255) * 2 / 3) << 8 | ((c & 255) * 2 / 3)
}
fn palette(index: usize) -> u32 {
    use theme::ANSI;
    match index {
        0..=15 => ANSI[index],
        16..=231 => {
            let n = index - 16;
            let v = |x: usize| if x == 0 { 0 } else { 55 + x * 40 };
            ((v(n / 36) << 16) | (v(n / 6 % 6) << 8) | v(n % 6)) as u32
        }
        232..=255 => {
            let v = (8 + 10 * (index - 232)) as u32;
            v * 0x010101
        }
        257 => 0x0d1117,
        259..=266 => dim(ANSI[index - 259]),
        268 => dim(ANSI[7]),
        _ => ANSI[7],
    }
}

mod content;
mod delimiters;
mod enclosed;
mod theme;
pub use theme::Theme;
mod reading;
pub use reading::{
    SearchMatch, SearchScope, SearchStatus, SelectionKind, SelectionPoint, SelectionSide,
};
mod highlight;
mod input;
pub use input::MouseEvent;
#[cfg(test)]
mod tests;
