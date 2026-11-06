use super::{PADDING, TerminalKind, TerminalView};
use crate::app::{layout::Axis, ui};
use gpui::{
    App, Bounds, ContentMask, Context, Font, FontFeatures, Pixels, SharedString, TextRun,
    UnderlineStyle, Window, fill, font, outline, point, px, rgb, size,
};
use rustterm::terminal::{Cell, CursorShape, MAX_COLUMNS, MAX_ROWS, SearchHighlight};
use std::sync::Arc;

const SEARCH_MATCH_BACKGROUND: u32 = 0x4b463b;
const CURRENT_MATCH_BACKGROUND: u32 = 0xd4b57d;
const TIMESTAMP_WIDTH: usize = 10;

fn gutter_columns(history_lines: usize, numbers: bool, timestamps: bool) -> usize {
    let numbers = if numbers {
        (history_lines + MAX_ROWS as usize).to_string().len() + 2
    } else {
        0
    };
    numbers + if timestamps { TIMESTAMP_WIDTH + 2 } else { 0 }
}

impl TerminalView {
    pub fn minimum_size(&self) -> (f32, f32) {
        let width =
            f32::from(self.cell_width) * 20. + f32::from(self.gutter_width()) + 2. * PADDING;
        let chrome = ui::PANE_HEADER_HEIGHT
            + if self.search_open {
                ui::SEARCH_HEIGHT
            } else {
                0.
            };
        (
            width.max(self.header_minimum_width()),
            f32::from(self.line_height) * 6. + 2. * PADDING + chrome,
        )
    }

    pub fn can_split(&self, axis: Axis) -> bool {
        let Some(bounds) = self.bounds else {
            return false;
        };
        let width = f32::from(bounds.size.width);
        let body_height = f32::from(bounds.size.height);
        let minimum_width = self.minimum_size().0;
        let minimum_body = f32::from(self.line_height) * 6. + 2. * PADDING;
        let chrome = ui::PANE_HEADER_HEIGHT
            + if self.search_open {
                ui::SEARCH_HEIGHT
            } else {
                0.
            };
        match axis {
            Axis::Horizontal => width >= minimum_width * 2. + 6. && body_height >= minimum_body,
            Axis::Vertical => {
                body_height + chrome >= (minimum_body + chrome) * 2. + 6. && width >= minimum_width
            }
        }
    }

    fn header_minimum_width(&self) -> f32 {
        // Compact tools keep 28 px hit areas and leave room for the address.
        (if self.kind == TerminalKind::Ssh {
            320.
        } else {
            256.
        }) + if self.viewed_run + 1 < self.runs.len() {
            32.
        } else {
            0.
        }
    }

    pub(super) fn gutter_width(&self) -> Pixels {
        self.cell_width
            * gutter_columns(
                self.settings.history_lines,
                self.settings.line_numbers,
                self.settings.line_timestamps,
            ) as f32
    }

    pub(super) fn layout_canvas(
        &mut self,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.bounds = Some(bounds);
        if self.metrics_font_size != Some(self.font_size) {
            let font_id = window.text_system().resolve_font(&self.font);
            self.cell_width = window
                .text_system()
                .advance(font_id, px(self.font_size), 'M')
                .map(|size| size.width)
                .unwrap_or(px(self.font_size * 0.6))
                .max(px(1.));
            self.metrics_font_size = Some(self.font_size);
        }
        self.line_height = px((self.font_size * self.settings.line_height).ceil());
        let columns = (f32::from(bounds.size.width - px(2. * PADDING) - self.gutter_width())
            / f32::from(self.cell_width))
        .floor()
        .clamp(2., MAX_COLUMNS as f32) as u16;
        let rows = (f32::from(bounds.size.height - px(2. * PADDING)) / f32::from(self.line_height))
            .floor()
            .clamp(1., MAX_ROWS as f32) as u16;
        if !self.show_final_alternate {
            self.resize.request((columns, rows));
            self.flush_resize();
        }
        if self.visible
            && self.window_state.visibility.is_visible()
            && !self.window_state.is_locked()
            && !self.show_final_alternate
            && let Some(session) = self.runs.get(self.viewed_run)
        {
            let revision = session.revision();
            if revision != self.requested_revision && session.request_snapshot() {
                self.requested_revision = revision;
            }
        }
        let _ = cx;
    }

    pub(super) fn cursor_bounds(&self) -> Option<Bounds<Pixels>> {
        let bounds = self.bounds?;
        let snapshot = self.snapshot.as_ref()?;
        let cursor = &snapshot.cursor;
        let column = cursor
            .column
            .min(snapshot.columns.saturating_sub(1) as usize);
        let row = cursor.row.min(snapshot.rows.saturating_sub(1) as usize);
        Some(Bounds::new(
            point(
                bounds.left() + px(PADDING) + self.gutter_width() + self.cell_width * column as f32,
                bounds.top() + px(PADDING) + self.line_height * row as f32,
            ),
            size(self.cell_width * f32::from(cursor.width), self.line_height),
        ))
    }
}

/// Render ASCII runs together. Non-ASCII clusters are anchored to their terminal
/// cell, so fallback fonts and combining glyphs cannot shift later columns.
pub(super) fn paint_terminal(
    view: &TerminalView,
    bounds: Bounds<Pixels>,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(snapshot) = &view.snapshot else {
        return;
    };
    let origin = bounds.origin + point(px(PADDING) + view.gutter_width(), px(PADDING));
    // A previous snapshot may be larger than the live viewport during a drag.
    // Clip work before shaping, not just after issuing draw commands.
    let visible_rows = (f32::from(bounds.bottom() - origin.y) / f32::from(view.line_height))
        .floor()
        .max(0.) as usize;
    let visible_columns = (f32::from(bounds.right() - origin.x) / f32::from(view.cell_width))
        .ceil()
        .max(0.) as usize;
    let cursor_shape = if !snapshot.cursor.visible
        || snapshot.cursor.row >= visible_rows
        || snapshot.cursor.column >= visible_columns
    {
        CursorShape::Hidden
    } else if view.focus.is_focused(window) && window.is_window_active() {
        snapshot.cursor.shape
    } else {
        CursorShape::HollowBlock
    };
    let number_width = (view.settings.history_lines + MAX_ROWS as usize)
        .to_string()
        .len();
    if view.settings.line_numbers || view.settings.line_timestamps {
        let background = view.settings.theme.background;
        let lighter = |shift: u32| -> u32 {
            let channel = (background >> shift) & 255u32;
            channel + (255 - channel) / 64
        };
        let gutter = (lighter(16) << 16) | (lighter(8) << 8) | lighter(0);
        window.paint_quad(fill(
            Bounds::new(bounds.origin, size(view.gutter_width(), bounds.size.height)),
            rgb(gutter),
        ));
        window.paint_quad(fill(
            Bounds::new(
                point(bounds.left() + view.gutter_width() - px(1.), bounds.top()),
                size(px(1.), bounds.size.height),
            ),
            rgb(ui::colors().border),
        ));
    }
    if (view.settings.line_numbers || view.settings.line_timestamps) && !snapshot.alternate_screen {
        for (row, line) in snapshot.lines.iter().take(visible_rows).enumerate() {
            let mut text = String::new();
            if view.settings.line_timestamps {
                // Keep the time column's width even for rows without output.
                text = format!(
                    "{:<TIMESTAMP_WIDTH$}",
                    line.received_at
                        .map(|time| format!("[{time}]"))
                        .unwrap_or_default()
                );
            }
            if view.settings.line_numbers {
                if view.settings.line_timestamps {
                    text.push_str("  ");
                }
                text.push_str(&format!(
                    "{:>width$}",
                    line.number
                        .map(|number| number.to_string())
                        .unwrap_or_default(),
                    width = number_width
                ));
            }
            if text.trim().is_empty() {
                continue;
            }
            let run = TextRun {
                len: text.len(),
                font: view.font.clone(),
                color: rgb(view.settings.theme.line_number).into(),
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let shaped = window.text_system().shape_line(
                text.into(),
                px(view.font_size),
                &[run],
                Some(view.cell_width),
            );
            let _ = shaped.paint(
                point(
                    bounds.left() + px(PADDING),
                    origin.y + view.line_height * row as f32,
                ),
                view.line_height,
                window,
                cx,
            );
        }
    }
    let font = view.font.clone();
    for (row, line) in snapshot.lines.iter().take(visible_rows).enumerate() {
        let y = origin.y + view.line_height * row as f32;
        let cells = &line.cells[..line.cells.len().min(visible_columns)];
        let cursor_columns = if cursor_shape == CursorShape::Block && snapshot.cursor.row == row {
            snapshot.cursor.column..snapshot.cursor.column + usize::from(snapshot.cursor.width)
        } else {
            0..0
        };
        let cell_background = |column: usize| {
            let cell = &cells[column];
            if cursor_columns.contains(&column) {
                view.settings.theme.cursor
            } else if cell.selected {
                view.settings.theme.selection
            } else if let Some(highlight) = cell.search_highlight {
                search_background(highlight)
            } else {
                cell.background
            }
        };
        let mut column = 0;
        while column < cells.len() {
            let start = column;
            let background = cell_background(column);
            while column < cells.len() && cell_background(column) == background {
                column += 1;
            }
            if background != view.settings.theme.background {
                window.paint_quad(fill(
                    Bounds::new(
                        point(origin.x + view.cell_width * start as f32, y),
                        size(view.cell_width * (column - start) as f32, view.line_height),
                    ),
                    rgb(background),
                ));
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
            let on_cursor = cursor_columns.contains(&start);
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
                        || cursor_columns.contains(&column) != on_cursor
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
            let mut run_font = font.clone();
            if cell.bold {
                run_font = run_font.bold();
            }
            if cell.italic {
                run_font = run_font.italic();
            }
            let foreground = if on_cursor {
                readable_foreground(view.settings.theme.cursor)
            } else if cell.selected {
                readable_foreground(view.settings.theme.selection)
            } else if let Some(highlight) = cell.search_highlight {
                readable_foreground(search_background(highlight))
            } else {
                cell.foreground
            };
            let underline = cell.underline.then_some(UnderlineStyle {
                thickness: px(1.),
                color: Some(rgb(foreground).into()),
                wavy: false,
            });
            let run = TextRun {
                len: text.len(),
                font: run_font,
                color: rgb(foreground).into(),
                background_color: None,
                underline,
                strikethrough: None,
            };
            let shaped = window.text_system().shape_line(
                text.into(),
                px(view.font_size),
                &[run],
                ascii.then_some(view.cell_width),
            );
            let position = point(origin.x + view.cell_width * start as f32, y);
            let mask = (!ascii).then_some(ContentMask {
                bounds: Bounds::new(
                    position,
                    size(view.cell_width * cell.width as f32, view.line_height),
                ),
            });
            window.with_content_mask(mask, |window| {
                let _ = shaped.paint(position, view.line_height, window, cx);
            });
            if cell.strikeout {
                window.paint_quad(fill(
                    Bounds::new(
                        point(
                            origin.x + view.cell_width * start as f32,
                            y + view.line_height * 0.5,
                        ),
                        size(
                            view.cell_width
                                * if ascii {
                                    (column - start) as f32
                                } else {
                                    cell.width as f32
                                },
                            px(1.),
                        ),
                    ),
                    rgb(foreground),
                ));
            }
        }
    }
    if let Some(cursor) = view.cursor_bounds() {
        let color = rgb(view.settings.theme.cursor);
        match cursor_shape {
            CursorShape::Beam => window.paint_quad(fill(
                Bounds::new(
                    cursor.origin,
                    size(px(2.).min(cursor.size.width), cursor.size.height),
                ),
                color,
            )),
            CursorShape::Underline => {
                let height = px(2.).min(cursor.size.height);
                window.paint_quad(fill(
                    Bounds::new(
                        point(cursor.left(), cursor.bottom() - height),
                        size(cursor.size.width, height),
                    ),
                    color,
                ));
            }
            CursorShape::HollowBlock => {
                window.paint_quad(outline(cursor, color, Default::default()));
            }
            CursorShape::Block | CursorShape::Hidden => {}
        }
    }
    if !view.preedit.is_empty()
        && let Some(cursor) = view.cursor_bounds()
    {
        let text: SharedString = view.preedit.clone().into();
        let run = TextRun {
            len: text.len(),
            font,
            color: rgb(ui::colors().text).into(),
            background_color: Some(rgb(ui::colors().raised).into()),
            underline: Some(UnderlineStyle {
                thickness: px(1.),
                color: None,
                wavy: false,
            }),
            strikethrough: None,
        };
        let line = window
            .text_system()
            .shape_line(text, px(view.font_size), &[run], None);
        let _ = line.paint(cursor.origin, view.line_height, window, cx);
    }
}

pub(super) fn terminal_font(family: &str) -> Font {
    let mut font = font(family.to_owned());
    font.features = FontFeatures(Arc::new(vec![
        ("liga".into(), 0),
        ("calt".into(), 0),
        ("dlig".into(), 0),
    ]));
    font
}

fn readable_foreground(background: u32) -> u32 {
    let luminance = ((background >> 16) & 255) * 299
        + ((background >> 8) & 255) * 587
        + (background & 255) * 114;
    if luminance > 150_000 {
        0x0d1117
    } else {
        0xffffff
    }
}

fn search_background(highlight: SearchHighlight) -> u32 {
    match highlight {
        SearchHighlight::Match => SEARCH_MATCH_BACKGROUND,
        SearchHighlight::Current => CURRENT_MATCH_BACKGROUND,
    }
}

fn same_style(a: &Cell, b: &Cell) -> bool {
    a.selected == b.selected
        && a.search_highlight == b.search_highlight
        && a.foreground == b.foreground
        && a.bold == b.bold
        && a.italic == b.italic
        && a.underline == b.underline
        && a.hidden == b.hidden
        && a.strikeout == b.strikeout
}
