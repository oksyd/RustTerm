//! Visible rows are shared by snapshots; retained history is never duplicated here.
use super::*;
use alacritty_terminal::term::cell::Cell as GridCell;
use std::collections::HashMap;

const MAX_CACHED_CELLS: usize = 32_768;
#[derive(Default)]
pub(super) struct Cache {
    style: Option<Style>,
    rows: HashMap<u64, CachedRow>,
}
struct CachedRow {
    raw: Vec<GridCell>,
    linked: bool,
    content_version: u64,
    cells: Arc<Vec<Cell>>,
}
#[derive(PartialEq, Eq)]
struct Style {
    theme: Theme,
    palette: [Option<Rgb>; alacritty_terminal::term::color::COUNT],
    highlight: bool,
    alternate: bool,
    mouse: bool,
}
impl Style {
    fn new(term: &Terminal) -> Self {
        let colors = term
            .frozen_colors
            .as_ref()
            .unwrap_or_else(|| term.term.colors());
        Self {
            theme: term.theme.clone(),
            palette: std::array::from_fn(|index| colors[index]),
            highlight: term.highlight_enabled,
            alternate: term.is_alternate(),
            mouse: term.term.mode().intersects(TermMode::MOUSE_MODE),
        }
    }
}
impl Terminal {
    /// Copies visible cells only. Hidden sessions must not call this on output.
    pub fn snapshot(&self) -> Snapshot {
        let grid = self.term.grid();
        let viewport = self.viewport();
        let alternate = self.term.mode().contains(TermMode::ALT_SCREEN);
        let mut lines = Vec::with_capacity(self.size.rows as usize);
        let selection = self.selection_points();
        let search_marks = self.search_visible_marks(&viewport);
        let selection_marks = self.selection_visible_marks(&viewport);
        let mut cache = self
            .snapshot_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let style = Style::new(self);
        let mut old = if cache.style.as_ref() == Some(&style) {
            std::mem::take(&mut cache.rows)
        } else {
            HashMap::new()
        };
        let mut reused = Vec::with_capacity(viewport.lines.len());
        let mut raw_rows = Vec::with_capacity(viewport.lines.len());
        for (row, &line) in viewport.lines.iter().enumerate() {
            let selection_columns = if selection.is_some() {
                self.selection_columns(line)
            } else {
                0
            };
            let columns = self.size.columns as usize;
            let id = self
                .tracker()
                .id_at((grid.history_size() as i32 + line.0) as usize)
                .unwrap_or_default();
            let linked = grid[line][Column(columns - 1)]
                .flags
                .contains(Flags::WRAPLINE)
                || (line > grid.topmost_line()
                    && grid[Line(line.0 - 1)][Column(columns - 1)]
                        .flags
                        .contains(Flags::WRAPLINE));
            let overlay = |col| {
                let point = Point::new(line, Column(col));
                (
                    col < selection_columns
                        && selection.is_some_and(|(first, last)| first <= point && point <= last),
                    search_marks
                        .as_ref()
                        .and_then(|marks| marks[row * columns + col]),
                    selection_marks
                        .as_ref()
                        .is_some_and(|marks| marks[row * columns + col]),
                )
            };
            let cached = old.remove(&id).filter(|entry| {
                entry.raw.len() == columns
                    && (!(linked || entry.linked) || entry.content_version == self.content_version)
                    && entry.raw.iter().enumerate().all(|(col, cell)| {
                        *cell == grid[line][Column(col)]
                            && overlay(col)
                                == (
                                    entry.cells[col].selected,
                                    entry.cells[col].search_highlight,
                                    entry.cells[col].selection_match,
                                )
                    })
            });
            reused.push(cached.is_some());
            let (cells, raw) = if let Some(cached) = cached {
                (cached.cells, cached.raw)
            } else {
                let mut cells = Vec::with_capacity(columns);
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
                        selected: overlay(col).0,
                        search_highlight: overlay(col).1,
                        selection_match: overlay(col).2,
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
                (
                    Arc::new(cells),
                    (0..columns)
                        .map(|col| grid[line][Column(col)].clone())
                        .collect(),
                )
            };
            raw_rows.push((raw, linked));
            let received_at = (!alternate)
                .then(|| self.line_times.get(grid, line))
                .flatten();
            lines.push(VisualLine {
                id,
                source_line: line,
                number: received_at
                    .filter(|_| !self.redraws.contains(timestamps::address(grid, line)))
                    .map(|_| (grid.history_size() as i32 + line.0 + 1) as usize),
                received_at,
                wrapped: grid[Point::new(line, Column(self.size.columns as usize - 1))]
                    .flags
                    .contains(Flags::WRAPLINE),
                cells,
            });
        }
        let cursor = self.term.renderable_content().cursor;
        let point = cursor.point;
        let cursor_row = viewport.row(point.line).unwrap_or(usize::MAX);
        let mut snapshot = Snapshot {
            scroll_fraction: self.scroll_fraction as f32,
            command_blocks: viewport.command_blocks(selection),
            sticky_command: self.sticky_command(&viewport),
            new_output: self.new_output && viewport.offset > 0 && !alternate,
            shell: self.shell_snapshot(viewport.physical_extent().start),
            search: self.search_status(),
            search_markers: self.search_markers(&viewport),
            has_selection: self.selection_anchor.is_some(),
            has_content: self.content_end().is_some(),
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
                    && cursor_row < viewport.lines.len(),
            },
            alternate_screen: alternate,
            display_offset: viewport.offset,
            history_size: viewport.history,
            title: self.title.clone(),
            modes: self.modes(),
            history_limit: self.history_limit,
            history_budget_bytes: self.history_bytes,
        };
        self.decorate_snapshot(&mut snapshot, &reused);
        cache.rows.clear();
        let mut remaining = MAX_CACHED_CELLS;
        for (line, (raw, linked)) in snapshot.lines.iter().zip(raw_rows) {
            if line.id != 0 && raw.len() <= remaining {
                remaining -= raw.len();
                cache.rows.insert(
                    line.id,
                    CachedRow {
                        raw,
                        linked,
                        content_version: self.content_version,
                        cells: line.cells.clone(),
                    },
                );
            }
        }
        cache.style = Some(style);
        snapshot
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches_uncached(term: &Terminal) {
        let cached = term.snapshot();
        *term.snapshot_cache.lock().unwrap() = Cache::default();
        let fresh = term.snapshot();
        for (a, b) in cached.lines.iter().zip(&fresh.lines) {
            assert_eq!(a.cells, b.cells);
            assert_eq!(a.received_at, b.received_at);
            assert_eq!(a.number, b.number);
        }
    }

    #[test]
    fn cached_rows_follow_wrapped_context_palette_selection_and_screen_changes() {
        let mut term = Terminal::new(12, 5, 30);
        for bytes in [
            b"https://example.com/path\r\nhello word\r\nnext".as_slice(),
            b"\x1b[1;1Hxxxxx",
            b"\x1b]10;#abcdef\x07",
            b"\x1b[3;1H\x1b[31mchanged\x1b[0m",
        ] {
            term.feed(bytes);
            matches_uncached(&term);
        }
        term.begin_selection(0, 2, SelectionKind::Word);
        matches_uncached(&term);
        term.clear_selection();
        term.set_highlight(false);
        matches_uncached(&term);
        term.set_highlight(true);
        term.set_theme(Theme {
            foreground: 0x123456,
            ..Theme::default()
        });
        matches_uncached(&term);
        term.resize(8, 4);
        matches_uncached(&term);
        term.feed(b"\x1b[?1049halt screen");
        matches_uncached(&term);
        term.feed(b"\x1b[?1049l");
        matches_uncached(&term);
        term.scroll_fractional(0.5);
        matches_uncached(&term);
    }
}
