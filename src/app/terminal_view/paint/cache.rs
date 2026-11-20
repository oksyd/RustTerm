use super::*;
use gpui::{Point, ShapedLine};
use std::{collections::HashMap, ops::Range};

#[derive(Clone, PartialEq)]
pub(super) struct RowStyle {
    pub font: Font,
    pub font_size: f32,
    pub cell_width: Pixels,
    pub scale: f32,
    pub columns: usize,
    pub theme: rustterm::config::Theme,
    pub selection_matches: bool,
    pub active: bool,
}

const CACHE_BYTES: usize = 8 * 1024 * 1024;
struct Cached<T> {
    row: usize,
    cursor: Range<usize>,
    value: Option<T>,
}

fn row_weight(cells: &[Cell], cursor: &Range<usize>) -> usize {
    let mut previous: Option<(usize, &Cell)> = None;
    let mut runs = 0;
    let mut bytes = 0;
    for (column, cell) in cells.iter().enumerate() {
        bytes += cell.text.len();
        if cell.width == 0
            || cell.hidden
            || (cell.text == " " && !cell.underline && !cell.strikeout)
        {
            previous = None;
            continue;
        }
        let ascii = cell.width == 1 && cell.text.len() == 1 && cell.text.is_ascii();
        if !ascii
            || previous.is_none_or(|(index, last)| {
                !same_style(last, cell) || cursor.contains(&index) != cursor.contains(&column)
            })
        {
            runs += 1;
        }
        previous = ascii.then_some((column, cell));
    }
    runs * (std::mem::size_of::<Glyph>() + 256) + bytes * 8 + cells.len() * 24
}

pub(in crate::app::terminal_view) struct RowCache<T> {
    previous: Option<Arc<rustterm::terminal::Snapshot>>,
    style: Option<RowStyle>,
    rows: Vec<Cached<T>>,
}

impl<T> Default for RowCache<T> {
    fn default() -> Self {
        Self {
            previous: None,
            style: None,
            rows: Vec::new(),
        }
    }
}

impl<T> RowCache<T> {
    pub(super) fn prepare(
        &mut self,
        snapshot: Arc<rustterm::terminal::Snapshot>,
        style: RowStyle,
        rows: usize,
        cursor: Option<(usize, Range<usize>)>,
        mut build: impl FnMut(&[Cell], Range<usize>) -> T,
    ) {
        let mut old: HashMap<_, _> = if self.style.as_ref() == Some(&style) {
            self.rows
                .drain(..)
                .filter_map(|entry| {
                    let id = self.previous.as_ref()?.lines.get(entry.row)?.id;
                    (id != 0).then_some((id, entry))
                })
                .collect()
        } else {
            self.rows.clear();
            HashMap::new()
        };
        let mut remaining = CACHE_BYTES;
        for (row, line) in snapshot.lines.iter().take(rows).enumerate() {
            let cursor = cursor
                .as_ref()
                .filter(|(index, _)| *index == row)
                .map_or(0..0, |(_, range)| range.clone());
            let cells = &line.cells[..line.cells.len().min(style.columns)];
            let cached = old.remove(&line.id).filter(|entry| {
                self.previous
                    .as_ref()
                    .and_then(|old| old.lines.get(entry.row))
                    .is_some_and(|old| {
                        Arc::ptr_eq(&old.cells, &line.cells)
                            || old.cells[..old.cells.len().min(style.columns)] == *cells
                    })
                    && entry.cursor == cursor
            });
            let charge = row_weight(cells, &cursor);
            let value = if charge <= remaining {
                remaining -= charge;
                Some(
                    cached
                        .and_then(|entry| entry.value)
                        .unwrap_or_else(|| build(cells, cursor.clone())),
                )
            } else {
                None
            };
            self.rows.push(Cached { row, cursor, value });
        }
        self.style = Some(style);
        self.previous = Some(snapshot);
    }

    pub(super) fn rows(&self) -> impl Iterator<Item = (usize, Range<usize>, Option<&T>)> {
        self.rows
            .iter()
            .map(|entry| (entry.row, entry.cursor.clone(), entry.value.as_ref()))
    }

    pub(in crate::app::terminal_view) fn clear(&mut self) {
        *self = Self::default();
    }
}

pub(in crate::app::terminal_view) struct PaintedRow {
    backgrounds: Vec<(usize, usize, u32)>,
    glyphs: Vec<Glyph>,
}
struct Glyph {
    column: usize,
    width: usize,
    clipped: bool,
    line: ShapedLine,
    strikeout: Option<u32>,
}

impl PaintedRow {
    pub(super) fn build(
        cells: &[Cell],
        cursor: Range<usize>,
        colors: &CellColors<'_>,
        style: &RowStyle,
        window: &mut Window,
    ) -> Self {
        let mut backgrounds = Vec::new();
        let mut glyphs = Vec::new();
        let mut column = 0;
        while column < cells.len() {
            let start = column;
            let background = colors.background(&cells[column], cursor.contains(&column));
            while column < cells.len()
                && colors.background(&cells[column], cursor.contains(&column)) == background
            {
                column += 1;
            }
            if background != style.theme.background {
                backgrounds.push((start, column - start, background));
            }
        }
        column = 0;
        while column < cells.len() {
            let cell = &cells[column];
            if cell.width == 0
                || cell.hidden
                || (cell.text == " " && !cell.underline && !cell.strikeout)
            {
                column += 1;
                continue;
            }
            let start = column;
            let on_cursor = cursor.contains(&start);
            let mut text = cell.text.clone();
            column += 1;
            let ascii = cell.width == 1 && cell.text.len() == 1 && cell.text.is_ascii();
            if ascii {
                while column < cells.len() {
                    let next = &cells[column];
                    if next.width != 1
                        || next.text.len() != 1
                        || !next.text.is_ascii()
                        || !same_style(cell, next)
                        || cursor.contains(&column) != on_cursor
                    {
                        break;
                    }
                    text.push_str(&next.text);
                    column += 1;
                }
            }
            if ascii && !cell.underline && !cell.strikeout {
                text.truncate(text.trim_end_matches(' ').len());
            }
            let mut font = style.font.clone();
            if cell.bold {
                font = font.bold();
            }
            if cell.italic {
                font = font.italic();
            }
            let foreground = colors.foreground(cell, on_cursor);
            let run = TextRun {
                len: text.len(),
                font,
                color: rgb(foreground).into(),
                background_color: None,
                underline: cell.underline.then_some(UnderlineStyle {
                    thickness: px(1.),
                    color: Some(rgb(foreground).into()),
                    wavy: false,
                }),
                strikethrough: None,
            };
            glyphs.push(Glyph {
                column: start,
                width: if ascii {
                    column - start
                } else {
                    usize::from(cell.width)
                },
                clipped: !ascii,
                line: window.text_system().shape_line(
                    text.into(),
                    px(style.font_size),
                    &[run],
                    ascii.then_some(style.cell_width),
                ),
                strikeout: cell.strikeout.then_some(foreground),
            });
        }
        Self {
            backgrounds,
            glyphs,
        }
    }

    pub(super) fn paint(
        &self,
        origin: Point<Pixels>,
        cell_width: Pixels,
        line_height: Pixels,
        window: &mut Window,
        cx: &mut App,
    ) {
        for &(start, count, color) in &self.backgrounds {
            window.paint_quad(fill(
                Bounds::new(
                    origin + point(cell_width * start as f32, px(0.)),
                    size(cell_width * count as f32, line_height),
                ),
                rgb(color),
            ));
        }
        for glyph in &self.glyphs {
            let position = origin + point(cell_width * glyph.column as f32, px(0.));
            let mask = glyph.clipped.then_some(ContentMask {
                bounds: Bounds::new(position, size(cell_width * glyph.width as f32, line_height)),
            });
            window.with_content_mask(mask, |window| {
                let _ = glyph.line.paint(position, line_height, window, cx);
            });
            if let Some(color) = glyph.strikeout {
                window.paint_quad(fill(
                    Bounds::new(
                        position + point(px(0.), line_height * 0.5),
                        size(cell_width * glyph.width as f32, px(1.)),
                    ),
                    rgb(color),
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustterm::terminal::{SelectionKind, Terminal};

    fn style(columns: usize) -> RowStyle {
        RowStyle {
            font: font("monospace"),
            font_size: 14.,
            cell_width: px(8.),
            scale: 1.,
            columns,
            theme: Default::default(),
            selection_matches: true,
            active: true,
        }
    }

    #[test]
    fn row_reuse_survives_scrolling_and_invalidates_changed_cells_cursor_and_palette() {
        let mut terminal = Terminal::new(16, 3, 32);
        terminal.feed("first\r\n中文😀 second\r\nlast".as_bytes());
        let mut style = style(16);
        let mut cache = RowCache::default();
        let builds = std::cell::Cell::new(0);
        let verify = |cache: &mut RowCache<Vec<Cell>>,
                      terminal: &Terminal,
                      style: &RowStyle,
                      cursor: Option<(usize, Range<usize>)>| {
            builds.set(0);
            let snapshot = Arc::new(terminal.snapshot());
            cache.prepare(snapshot.clone(), style.clone(), 3, cursor, |cells, _| {
                builds.set(builds.get() + 1);
                cells.to_vec()
            });
            let actual: Vec<_> = cache
                .rows()
                .map(|(_, _, value)| value.unwrap().clone())
                .collect();
            let expected: Vec<_> = snapshot
                .lines
                .iter()
                .map(|line| line.cells.as_ref().clone())
                .collect();
            assert_eq!(actual, expected);
            builds.get()
        };
        assert_eq!(verify(&mut cache, &terminal, &style, None), 3);
        assert_eq!(verify(&mut cache, &terminal, &style, None), 0);
        terminal.feed(b"\r\nnext");
        assert_eq!(verify(&mut cache, &terminal, &style, None), 1);
        assert_eq!(verify(&mut cache, &terminal, &style, Some((0, 0..1))), 1);
        assert_eq!(verify(&mut cache, &terminal, &style, Some((1, 0..1))), 2);
        terminal.begin_selection(1, 2, SelectionKind::Word);
        assert_eq!(verify(&mut cache, &terminal, &style, Some((1, 0..1))), 1);
        style.active = false;
        assert_eq!(verify(&mut cache, &terminal, &style, Some((1, 0..1))), 3);
        style.theme.background ^= 0xffffff;
        assert_eq!(verify(&mut cache, &terminal, &style, Some((1, 0..1))), 3);
        cache.clear();
        assert_eq!(verify(&mut cache, &terminal, &style, None), 3);
    }

    #[test]
    fn dense_unicode_rows_exceeding_the_cache_budget_are_still_drawable_without_retention() {
        let mut terminal = Terminal::new(500, 1, 0);
        terminal.feed("中".repeat(250).as_bytes());
        let mut snapshot = terminal.snapshot();
        snapshot.rows = 200;
        snapshot.lines = (1..=200)
            .map(|id| {
                let mut line = snapshot.lines[0].clone();
                line.id = id;
                line
            })
            .collect();
        let snapshot = Arc::new(snapshot);
        let mut cache = RowCache::default();
        cache.prepare(snapshot.clone(), style(500), 200, None, |cells, _| {
            cells.to_vec()
        });
        let mut cached_bytes = 0;
        let mut bypassed = 0;
        for (row, cursor, prepared) in cache.rows() {
            let cells = &snapshot.lines[row].cells;
            if let Some(prepared) = prepared {
                assert_eq!(prepared, cells.as_ref());
                cached_bytes += row_weight(cells, &cursor);
            } else {
                bypassed += 1;
            }
        }
        assert!(cached_bytes <= CACHE_BYTES);
        assert!(bypassed > 0 && bypassed < 200);
        assert_eq!(cache.rows().count(), 200);
        let previous = Arc::downgrade(&snapshot);
        drop(snapshot);
        cache.clear();
        assert!(previous.upgrade().is_none());
    }
}
