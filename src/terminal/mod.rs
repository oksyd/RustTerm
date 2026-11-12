//! UI-independent terminal state. Alacritty owns the only authoritative grid.
//!
//! The adapter ignores OSC 8 hyperlinks and OSC 52 clipboard access.
//! String controls and combining sequences are bounded before they can grow without limit.
use alacritty_terminal::{
    Term,
    event::{Event, EventListener, WindowSize},
    grid::{Dimensions, Scroll},
    index::{Column, Line, Point},
    term::{Config, Osc52, TermMode, cell::Flags},
    vte::ansi::{Color, NamedColor, Processor, Rgb, Timeout},
};
use std::sync::{Arc, Mutex};
pub mod integration;
mod selection_matches;
pub mod shell;
mod timestamps;
pub use alacritty_terminal::vte::ansi::CursorShape;
pub use shell::{ShellCommand, ShellInteraction, ShellSnapshot};
pub use timestamps::LineTimestamp;

/// Scrollback capacity target; upstream caches may retain additional memory.
pub const DEFAULT_HISTORY_BYTES: usize = 64 * 1024 * 1024;
/// Conservative charge for a cell, its bounded extras and allocation overhead.
/// This estimates logical history capacity, not allocated memory or process RSS.
pub const CELL_BUDGET_BYTES: usize = 512;
pub const MAX_COLUMNS: u16 = 500;
pub const MAX_ROWS: u16 = 200;
const MAX_COMBINING: usize = 32;
const MAX_STRING_BYTES: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchHighlight {
    Match,
    Current,
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
    pub number: Option<usize>,
    pub received_at: Option<LineTimestamp>,
    pub cells: Vec<Cell>,
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
    pub shell: ShellSnapshot,
    pub search: SearchStatus,
    pub has_selection: bool,
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
            let mut events = self.0.lock().expect("terminal events poisoned");
            // Budget changes also emit a title event, even without subsequent PTY data.
            // Coalesce these notifications rather than retaining an idle-session queue.
            if matches!(event, Event::Title(_) | Event::ResetTitle)
                && events
                    .last()
                    .is_some_and(|e| matches!(e, Event::Title(_) | Event::ResetTitle))
            {
                *events.last_mut().unwrap() = event;
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
    shell: shell::ShellTracker,
    main_tracker: reading::Tracker,
    alt_tracker: reading::Tracker,
    next_row_id: u64,
    viewport_anchor: Option<u64>,
    viewport_evicted: bool,
    selection_anchor: Option<reading::SelectionAnchor>,
    selection_invalidated: bool,
    selection_kind: Option<SelectionKind>,
    selection_origin: Option<u64>,
    search: Option<reading::SearchState>,
    next_query_id: u64,
    highlight_enabled: bool,
    selection_highlight_enabled: bool,
    selection_match_cache: Mutex<selection_matches::MatchCache>,
    cache_command_output: bool,
    highlight_cache: Mutex<highlight::HighlightCache>,
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
            Config {
                scrolling_history: limit,
                osc52: Osc52::Disabled,
                ..Config::default()
            },
            &size,
            events.clone(),
        );
        let mut terminal = Self {
            line_times: Default::default(),
            shell: shell::ShellTracker::default(),
            main_tracker: Default::default(),
            alt_tracker: Default::default(),
            next_row_id: 0,
            viewport_anchor: None,
            viewport_evicted: false,
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
                    .is_some_and(|&index| index < history)
            })
        });
        for &byte in bytes {
            let old_alt = self.is_alternate();
            let old_top = &self.term.grid()[Point::new(self.term.grid().topmost_line(), Column(0))]
                as *const _ as usize;
            let old_cursor = self.term.grid().cursor.point.line;
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
            // LF emits the row being left, even when it contains no characters.
            // The destination remains unused until something is written there.
            if !old_alt && observation.linefeed {
                self.line_times.written(self.term.grid(), received_at);
            }
            self.parser
                .as_mut()
                .unwrap()
                .advance(&mut self.term, &[accepted]);
            self.shell_byte(byte);
            // CSI REP ends in ASCII 'b' and can repeat a preceding combining mark.
            // VTE caps its count at u16::MAX; clamp the resulting extras as well.
            if byte >= 0x80 || byte == b'b' {
                self.bound_combining();
            }
            let new_top = &self.term.grid()[Point::new(self.term.grid().topmost_line(), Column(0))]
                as *const _ as usize;
            if old_alt != self.is_alternate() {
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
                }
            }
            if observation.reset {
                self.shell.reset_interaction();
                self.line_times.reset();
            }
            if !self.is_alternate() {
                if old_alt {
                    self.line_times.primary_restored(self.term.grid());
                }
                if let Some(rows) = observation.cleanup.clone() {
                    self.line_times.cleanup_screen(self.term.grid(), rows);
                }
                let cursor = self.term.grid().cursor.point.line;
                let cursor_address = timestamps::address(self.term.grid(), cursor);
                if cursor == old_cursor
                    && cursor_address != old_cursor_address
                    && (observation.printed || matches!(accepted, b'\n' | 0x0b | 0x0c))
                {
                    self.line_times.retire(cursor_address);
                }
                if observation.printed {
                    self.line_times.written(self.term.grid(), received_at);
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
        self.restore_viewport();
        self.refresh_selection(unchanged_selection_rows);
        if !bytes.is_empty()
            && let Some(search) = &mut self.search
        {
            search.dirty = true;
        }
        let events = std::mem::take(&mut *self.events.0.lock().expect("terminal events poisoned"));
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
        if cell
            .zerowidth()
            .is_some_and(|chars| chars.len() > MAX_COMBINING)
        {
            let marks: Vec<char> = cell.zerowidth().unwrap()[..MAX_COMBINING].to_vec();
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
        let width_changed = size.columns != self.size.columns;
        self.line_times
            .before_resize(self.term.grid(), self.is_alternate());
        let inactive_primary_shrinks = self.is_alternate() && size.rows < self.size.rows;
        if size.columns > self.max_columns_seen {
            self.max_columns_seen = size.columns;
            let limit = budget_limit(
                self.requested_history,
                self.history_bytes,
                self.max_columns_seen,
            );
            if limit != self.history_limit {
                self.history_limit = limit;
                // set_options updates the primary buffer, including while alternate is active.
                self.term.set_options(Config {
                    scrolling_history: limit,
                    osc52: Osc52::Disabled,
                    ..Config::default()
                });
            }
        }
        self.max_rows_seen = self.max_rows_seen.max(size.rows);
        self.term.resize(size);
        self.size = size;
        self.line_times
            .after_resize(self.term.grid(), self.is_alternate());
        if width_changed {
            // Reflow replaces identities, but reuse the active tracker's allocations.
            self.reset_rows();
            self.viewport_anchor = None;
            self.anchor_viewport();
            let had_selection = self.selection_anchor.is_some();
            self.clear_selection();
            self.selection_invalidated = had_selection;
            if let Some(search) = self.search.take() {
                if search.matcher.is_none() {
                    // Rejected queries retain only a bounded display preview;
                    // never reinterpret that preview as a new literal query.
                    self.search = Some(search);
                } else {
                    self.start_search(search.status.query, search.status.case_sensitive);
                }
            }
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
        }
        // Highlight spans are keyed by exact logical text; mappings to cells are
        // rebuilt per snapshot. Layout changes cannot make a cached span stale.
        self.version = self.version.wrapping_add(1);
        self.content_version = self.content_version.wrapping_add(1);
    }
    fn is_alternate(&self) -> bool {
        self.term.mode().contains(TermMode::ALT_SCREEN)
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
            self.line_times.before_resize(self.term.grid(), true);
        }
        self.history_limit = limit;
        self.term.set_options(Config {
            scrolling_history: self.history_limit,
            osc52: Osc52::Disabled,
            ..Config::default()
        });
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
        if self.is_alternate() {
            self.final_alternate = Some(Arc::new(self.snapshot()));
            self.term.swap_alt();
            self.line_times.primary_restored(self.term.grid());
        }
        self.frozen_colors = Some(*self.term.colors());
        // Transfer only the authoritative main grid to a fresh read-only container.
        // Dropping the old Term also releases title stacks, alternate state and protocol modes.
        let mut readonly = Term::new(
            Config {
                scrolling_history: self.history_limit,
                osc52: Osc52::Disabled,
                ..Config::default()
            },
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
        self.term.scroll_display(Scroll::Delta(lines));
        self.viewport_evicted = false;
        self.anchor_viewport();
        self.version = self.version.wrapping_add(1);
    }
    pub fn scroll_to_bottom(&mut self) {
        self.scroll_to(0);
    }
    /// Set the viewport's distance from the live bottom, clamped to retained history.
    pub fn scroll_to(&mut self, offset: usize) {
        let grid = self.term.grid();
        let target = offset.min(grid.history_size());
        self.scroll(target as i32 - grid.display_offset() as i32);
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
    /// Copies visible cells only. Hidden sessions must not call this on output.
    pub fn snapshot(&self) -> Snapshot {
        let grid = self.term.grid();
        let offset = grid.display_offset();
        let alternate = self.term.mode().contains(TermMode::ALT_SCREEN);
        let mut lines = Vec::with_capacity(self.size.rows as usize);
        let selection = self.selection_points();
        let search_marks = self.search_visible_marks();
        let selection_marks = self.selection_visible_marks();
        for row in 0..self.size.rows as usize {
            let line = Line(row as i32 - offset as i32);
            let mut cells = Vec::with_capacity(self.size.columns as usize);
            for col in 0..self.size.columns as usize {
                let point = Point::new(line, Column(col));
                let cell = &grid[point];
                let flags = cell.flags;
                let spacer =
                    flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER);
                let mut text = if spacer {
                    String::new()
                } else {
                    cell.c.to_string()
                };
                if !spacer {
                    for &c in cell.zerowidth().unwrap_or_default() {
                        text.push(c);
                    }
                }
                let mut foreground = self.color(cell.fg);
                let mut background = self.color(cell.bg);
                if flags.contains(Flags::INVERSE) {
                    std::mem::swap(&mut foreground, &mut background);
                }
                if flags.contains(Flags::DIM) {
                    foreground = dim(foreground);
                }
                cells.push(Cell {
                    default_foreground: matches!(cell.fg, Color::Named(NamedColor::Foreground))
                        && !flags.intersects(Flags::INVERSE | Flags::HIDDEN),
                    selected: selection
                        .is_some_and(|(first, last)| first <= point && point <= last),
                    search_highlight: search_marks
                        .as_ref()
                        .and_then(|marks| marks[row * self.size.columns as usize + col]),
                    selection_match: selection_marks
                        .as_ref()
                        .is_some_and(|marks| marks[row * self.size.columns as usize + col]),
                    text,
                    width: if spacer {
                        0
                    } else if flags.contains(Flags::WIDE_CHAR) {
                        2
                    } else {
                        1
                    },
                    foreground,
                    background,
                    bold: flags.contains(Flags::BOLD),
                    italic: flags.contains(Flags::ITALIC),
                    underline: flags.intersects(Flags::ALL_UNDERLINES),
                    hidden: flags.contains(Flags::HIDDEN),
                    strikeout: flags.contains(Flags::STRIKEOUT),
                });
            }
            let received_at = (!alternate)
                .then(|| self.line_times.get(grid, line))
                .flatten();
            lines.push(VisualLine {
                number: received_at.map(|_| grid.history_size() + row + 1 - offset),
                received_at,
                wrapped: grid[Point::new(line, Column(self.size.columns as usize - 1))]
                    .flags
                    .contains(Flags::WRAPLINE),
                cells,
            });
        }
        let cursor = self.term.renderable_content().cursor;
        let point = cursor.point;
        let cursor_row = point.line.0 as usize + offset;
        let mut snapshot = Snapshot {
            shell: self.shell.snapshot.clone(),
            search: self.search_status(),
            has_selection: self.selection_anchor.is_some(),
            selection_invalidated: self.selection_invalidated,
            viewport_evicted: self.viewport_evicted && !alternate,
            is_read_only: self.parser.is_none(),
            mouse_reporting: self.term.mode().intersects(TermMode::MOUSE_MODE),
            version: self.version,
            columns: self.size.columns,
            rows: self.size.rows,
            lines,
            cursor: Cursor {
                row: cursor_row,
                column: point.column.0,
                width: if grid[point].flags.contains(Flags::WIDE_CHAR) {
                    2
                } else {
                    1
                },
                shape: cursor.shape,
                visible: self.parser.is_some()
                    && cursor.shape != CursorShape::Hidden
                    && cursor_row < self.size.rows as usize,
            },
            alternate_screen: alternate,
            display_offset: offset,
            history_size: grid.history_size(),
            title: self.title.clone(),
            modes: self.modes(),
            history_limit: self.history_limit,
            history_budget_bytes: self.history_bytes,
        };
        self.decorate_snapshot(&mut snapshot);
        snapshot
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
    const ANSI: [u32; 16] = [
        0x161b22, 0xf47067, 0x8ddb8c, 0xeac55f, 0x6cb6ff, 0xd2a8ff, 0x76e3ea, 0xc9d1d9, 0x768390,
        0xff938a, 0xb4f1b4, 0xf6d976, 0x9dcaff, 0xe2c5ff, 0xa9f0f5, 0xf0f6fc,
    ];
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
        268 => dim(0xc9d1d9),
        _ => 0xc9d1d9,
    }
}

mod theme;
pub use theme::Theme;
mod reading;
pub use reading::{SearchMatch, SearchStatus, SelectionKind};
mod highlight;
mod input;
pub use input::MouseEvent;
#[cfg(test)]
mod tests;
