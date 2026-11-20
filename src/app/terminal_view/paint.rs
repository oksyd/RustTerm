use super::{PADDING, TerminalKind, TerminalView};
use crate::app::{layout::Axis, ui};
use gpui::{
    App, Bounds, ContentMask, Context, Font, FontFeatures, Pixels, SharedString, TextRun,
    UnderlineStyle, Window, fill, font, outline, point, px, rgb, size,
};
use rustterm::terminal::{Cell, CursorShape, MAX_COLUMNS, MAX_ROWS, SearchHighlight};
use std::sync::Arc;
pub(super) mod cache;

const TIMESTAMP_WIDTH: usize = 10;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum GutterLabel {
    Time(rustterm::terminal::LineTimestamp),
    Number(usize),
}

impl std::fmt::Display for GutterLabel {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Time(value) => write!(formatter, "[{value}]"),
            Self::Number(value) => write!(formatter, "{value}"),
        }
    }
}

pub(super) struct GutterCache<T> {
    labels: std::collections::HashMap<GutterLabel, (u64, T)>,
    style: Option<(Font, f32, Pixels, f32, u32)>,
    frame: u64,
}

impl<T> Default for GutterCache<T> {
    fn default() -> Self {
        Self {
            labels: Default::default(),
            style: None,
            frame: 0,
        }
    }
}

impl<T> GutterCache<T> {
    fn prepare(&mut self, font: &Font, font_size: f32, width: Pixels, scale: f32, color: u32) {
        let style = (font.clone(), font_size, width, scale, color);
        if self.style.as_ref() != Some(&style) {
            self.labels.clear();
            self.style = Some(style);
        } else if self.labels.len() >= 4 * (usize::from(MAX_ROWS) + 1) {
            self.labels.retain(|_, (seen, _)| *seen == self.frame);
        }
        self.frame = self.frame.wrapping_add(1);
    }

    fn label(&mut self, key: GutterLabel, shape: impl FnOnce(String) -> T) -> &T {
        let (seen, value) = self
            .labels
            .entry(key)
            .or_insert_with(|| (self.frame, shape(key.to_string())));
        *seen = self.frame;
        value
    }
}

fn number_columns(history_lines: usize) -> usize {
    history_lines.saturating_add(usize::from(MAX_ROWS)).ilog10() as usize + 1
}

fn gutter_columns(history_lines: usize, numbers: bool, timestamps: bool) -> usize {
    let numbers = if numbers {
        number_columns(history_lines) + 2
    } else {
        0
    };
    numbers + if timestamps { TIMESTAMP_WIDTH + 2 } else { 0 } + super::folding::GUTTER_COLUMNS
}

impl TerminalView {
    pub(super) fn scroll_offset(&self) -> Pixels {
        self.line_height
            * self
                .snapshot
                .as_ref()
                .map_or(0., |snapshot| snapshot.scroll_fraction)
    }
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
            if self.selection_drag.is_some()
                && self
                    .snapshot
                    .as_ref()
                    .is_some_and(|snapshot| snapshot.columns != columns || snapshot.rows != rows)
            {
                self.flush_motion(cx);
                self.selection_drag = None;
                self.selection_pointer = None;
                self.link_press = None;
            }
            if self.resize.request((columns, rows)) {
                // Layout may change during the resize cooldown. Wake the poller
                // once; subsequent retries already follow the frame cadence.
                self.poll_wake.notify_one();
            }
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
        let row = cursor.row.min(snapshot.lines.len().saturating_sub(1));
        Some(Bounds::new(
            point(
                bounds.left() + px(PADDING) + self.gutter_width() + self.cell_width * column as f32,
                bounds.top() + px(PADDING) + self.line_height * row as f32 - self.scroll_offset(),
            ),
            size(self.cell_width * f32::from(cursor.width), self.line_height),
        ))
    }
}

/// Render ASCII runs together. Non-ASCII clusters are anchored to their terminal
/// cell, so fallback fonts and combining glyphs cannot shift later columns.
pub(super) fn paint_terminal(
    view: &mut TerminalView,
    bounds: Bounds<Pixels>,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(snapshot) = view.snapshot.clone() else {
        return;
    };
    let theme = view.settings.theme.clone();
    let colors = CellColors::new(
        &theme,
        view.settings.highlight_selection_matches,
        window.is_window_active()
            && (view.focus.contains_focused(window, cx) || view.header_owns_focus(window, cx)),
    );
    let origin = bounds.origin
        + point(
            px(PADDING) + view.gutter_width(),
            px(PADDING) - view.scroll_offset(),
        );
    // A previous snapshot may be larger than the live viewport during a drag.
    // Clip work before shaping, not just after issuing draw commands.
    let visible_rows = (f32::from(bounds.bottom() - origin.y) / f32::from(view.line_height))
        .ceil()
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
    let number_width = number_columns(view.settings.history_lines);
    if view.settings.line_numbers || view.settings.line_timestamps {
        let gutter = gutter_background(view.settings.theme.background);
        window.paint_quad(fill(
            Bounds::new(
                bounds.origin,
                size(px(PADDING) + view.gutter_width(), bounds.size.height),
            ),
            rgb(gutter),
        ));
        window.paint_quad(fill(
            Bounds::new(
                point(origin.x - px(1.), bounds.top()),
                size(px(1.), bounds.size.height),
            ),
            rgb(ui::colors().border),
        ));
    }
    let clip = Bounds::new(
        bounds.origin + point(px(0.), px(PADDING)),
        size(
            bounds.size.width,
            (bounds.size.height - px(2. * PADDING))
                .max(px(0.))
                .min(view.line_height * f32::from(snapshot.rows)),
        ),
    );
    window.with_content_mask(Some(ContentMask { bounds: clip }), |window| {
        if (view.settings.line_numbers || view.settings.line_timestamps)
            && !snapshot.alternate_screen
        {
            let gutter_foreground = rustterm::color::readable_on(
                view.settings.theme.line_number,
                gutter_background(view.settings.theme.background),
            );
            view.gutter_cache.prepare(
                &view.font,
                view.font_size,
                view.cell_width,
                window.scale_factor(),
                gutter_foreground,
            );
            let number_start = if view.settings.line_timestamps {
                TIMESTAMP_WIDTH + 2
            } else {
                0
            };
            for (row, line) in snapshot.lines.iter().take(visible_rows).enumerate() {
                let time = line
                    .received_at
                    .filter(|_| view.settings.line_timestamps)
                    .map(|time| (GutterLabel::Time(time), 0));
                let number = line
                    .number
                    .filter(|_| view.settings.line_numbers)
                    .map(|number| {
                        let digits = number.max(1).ilog10() as usize + 1;
                        (
                            GutterLabel::Number(number),
                            number_start + number_width.saturating_sub(digits),
                        )
                    });
                for (label, column) in time.into_iter().chain(number) {
                    let shaped = view.gutter_cache.label(label, |text| {
                        let run = TextRun {
                            len: text.len(),
                            font: view.font.clone(),
                            color: rgb(gutter_foreground).into(),
                            background_color: None,
                            underline: None,
                            strikethrough: None,
                        };
                        window.text_system().shape_line(
                            text.into(),
                            px(view.font_size),
                            &[run],
                            Some(view.cell_width),
                        )
                    });
                    let _ = shaped.paint(
                        point(
                            bounds.left() + px(PADDING) + view.cell_width * column as f32,
                            origin.y + view.line_height * row as f32,
                        ),
                        view.line_height,
                        window,
                        cx,
                    );
                }
            }
        }
        let style = cache::RowStyle {
            font: view.font.clone(),
            font_size: view.font_size,
            cell_width: view.cell_width,
            scale: window.scale_factor(),
            columns: visible_columns,
            theme: view.settings.theme.clone(),
            selection_matches: view.settings.highlight_selection_matches,
            active: window.is_window_active()
                && (view.focus.contains_focused(window, cx) || view.header_owns_focus(window, cx)),
        };
        let cursor = (cursor_shape == CursorShape::Block).then_some((
            snapshot.cursor.row,
            snapshot.cursor.column..snapshot.cursor.column + usize::from(snapshot.cursor.width),
        ));
        view.row_cache.prepare(
            snapshot.clone(),
            style.clone(),
            visible_rows,
            cursor,
            |cells, cursor| cache::PaintedRow::build(cells, cursor, &colors, &style, window),
        );
        for (row, cursor, cached) in view.row_cache.rows() {
            let uncached;
            let prepared = if let Some(cached) = cached {
                cached
            } else {
                let cells = &snapshot.lines[row].cells;
                uncached = cache::PaintedRow::build(
                    &cells[..cells.len().min(visible_columns)],
                    cursor,
                    &colors,
                    &style,
                    window,
                );
                &uncached
            };
            prepared.paint(
                origin + point(px(0.), view.line_height * row as f32),
                view.cell_width,
                view.line_height,
                window,
                cx,
            );
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
                font: view.font.clone(),
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
    });
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

fn gutter_background(background: u32) -> u32 {
    [16, 8, 0].into_iter().fold(0, |color, shift| {
        let channel = (background >> shift) & 255;
        color | ((channel + (255 - channel) / 64) << shift)
    })
}

fn readable_foreground(background: u32) -> u32 {
    rustterm::color::readable_on(0xffffff, background)
}

struct CellColors<'a> {
    theme: &'a rustterm::config::Theme,
    selection: u32,
    selection_foreground: std::cell::Cell<Option<(u32, u32)>>,
    selection_matches: bool,
    search_match: u32,
    current_match: u32,
}

impl<'a> CellColors<'a> {
    fn new(theme: &'a rustterm::config::Theme, selection_matches: bool, active: bool) -> Self {
        // Resolve contrast once per paint, not once per terminal cell. Search
        // uses the terminal's amber family on both dark and custom light surfaces.
        let current_match = rustterm::color::readable_on(theme.warn, theme.background);
        Self {
            theme,
            selection: if active {
                theme.selection
            } else {
                rustterm::color::mix(theme.background, theme.selection, 60)
            },
            selection_foreground: std::cell::Cell::new(None),
            selection_matches,
            search_match: rustterm::color::mix(theme.background, current_match, 25),
            current_match,
        }
    }

    fn search_background(&self, highlight: SearchHighlight) -> u32 {
        match highlight {
            SearchHighlight::Match => self.search_match,
            SearchHighlight::Current => self.current_match,
        }
    }

    fn foreground(&self, cell: &Cell, cursor: bool) -> u32 {
        if cursor {
            readable_foreground(self.theme.cursor)
        } else if cell.selected {
            if let Some((original, foreground)) = self.selection_foreground.get()
                && original == cell.foreground
            {
                return foreground;
            }
            let foreground = rustterm::color::readable_on(cell.foreground, self.selection);
            self.selection_foreground
                .set(Some((cell.foreground, foreground)));
            foreground
        } else if let Some(highlight) = cell.search_highlight {
            rustterm::color::readable_on(cell.foreground, self.search_background(highlight))
        } else if self.selection_matches && cell.selection_match {
            rustterm::color::readable_on(
                cell.foreground,
                selection_match_background(cell.background, self.theme.selection),
            )
        } else {
            cell.foreground
        }
    }

    fn background(&self, cell: &Cell, cursor: bool) -> u32 {
        if cursor {
            self.theme.cursor
        } else if cell.selected {
            self.selection
        } else if let Some(highlight) = cell.search_highlight {
            self.search_background(highlight)
        } else if self.selection_matches && cell.selection_match {
            selection_match_background(cell.background, self.theme.selection)
        } else {
            cell.background
        }
    }
}

fn same_style(a: &Cell, b: &Cell) -> bool {
    a.selected == b.selected
        && a.search_highlight == b.search_highlight
        && a.selection_match == b.selection_match
        && a.foreground == b.foreground
        && a.bold == b.bold
        && a.italic == b.italic
        && a.underline == b.underline
        && a.hidden == b.hidden
        && a.strikeout == b.strikeout
}

fn selection_match_background(background: u32, selection: u32) -> u32 {
    rustterm::color::mix(background, selection, 40)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustterm::{
        config::Theme,
        terminal::{SelectionKind, Terminal},
    };

    #[test]
    fn repeated_gutter_labels_are_formatted_and_shaped_once() {
        let mut cache = GutterCache::default();
        let mut terminal = Terminal::new(12, 3, 0);
        terminal.feed(b"line\r\n");
        let time = terminal.snapshot().lines[0].received_at.unwrap();
        let mut builds = 0;
        for (color, size, expected) in [(1, 14., 3), (1, 14., 3), (2, 14., 6), (2, 16., 9)] {
            cache.prepare(&font("monospace"), size, px(8.), 1., color);
            for (label, text) in [
                (GutterLabel::Time(time), format!("[{time}]")),
                (GutterLabel::Number(12), "12".into()),
                (GutterLabel::Number(13), "13".into()),
            ] {
                let actual = cache.label(label, |s| {
                    builds += 1;
                    s
                });
                assert_eq!(*actual, text);
            }
            assert_eq!(builds, expected);
        }
    }

    #[test]
    fn selected_text_retains_readable_ansi_colors_and_softens_when_inactive() {
        let mut terminal = Terminal::new(10, 2, 0);
        terminal.feed(b"\x1b[93mwarning");
        terminal.select_all();
        let mut cell = terminal.snapshot().lines[0].cells[0].clone();
        for theme in [
            Theme::default(),
            Theme {
                background: 0xffffff,
                selection: 0xbac6d1,
                ..Theme::default()
            },
        ] {
            let active = CellColors::new(&theme, true, true);
            let inactive = CellColors::new(&theme, true, false);
            assert_ne!(
                active.background(&cell, false),
                inactive.background(&cell, false)
            );
            assert_ne!(inactive.background(&cell, false), theme.background);
            for colors in [active, inactive] {
                let background = colors.background(&cell, false);
                for foreground in [cell.foreground, 0xffff00, 0x440000, background] {
                    cell.foreground = foreground;
                    let actual = colors.foreground(&cell, false);
                    assert!(rustterm::color::contrast(actual, background) >= 4.5);
                    if rustterm::color::contrast(foreground, background) >= 4.5 {
                        assert_eq!(
                            actual, foreground,
                            "readable ANSI color must survive selection"
                        );
                    }
                }
                cell.search_highlight = Some(SearchHighlight::Current);
                cell.selection_match = true;
                assert_eq!(colors.background(&cell, false), background);
                assert_eq!(colors.background(&cell, true), theme.cursor);
            }
        }
    }

    #[test]
    fn search_colors_follow_custom_terminal_surfaces_and_keep_current_match_distinct() {
        let mut terminal = Terminal::new(10, 2, 0);
        terminal.feed(b"match");
        let mut cell = terminal.snapshot().lines[0].cells[0].clone();
        let dark = Theme::default();
        let light = Theme {
            background: 0xffffff,
            foreground: 0x202124,
            warn: 0x805516,
            ..dark.clone()
        };
        cell.search_highlight = Some(SearchHighlight::Match);
        assert_ne!(
            CellColors::new(&dark, true, true).background(&cell, false),
            CellColors::new(&light, true, true).background(&cell, false)
        );
        for theme in [dark, light] {
            cell.search_highlight = Some(SearchHighlight::Match);
            let matched = CellColors::new(&theme, true, true).background(&cell, false);
            let foreground = CellColors::new(&theme, true, true).foreground(&cell, false);
            assert!(rustterm::color::contrast(foreground, matched) >= 4.5);
            cell.search_highlight = Some(SearchHighlight::Current);
            let current = CellColors::new(&theme, true, true).background(&cell, false);
            assert!(rustterm::color::contrast(current, matched) >= 1.5);
            assert_ne!(current, theme.selection);
        }
    }

    #[test]
    fn occurrence_highlight_keeps_text_readable_without_changing_plain_ansi() {
        let mut terminal = rustterm::terminal::Terminal::new(10, 2, 10);
        terminal.feed(b"text");
        let mut cell = terminal.snapshot().lines[0].cells[0].clone();
        cell.foreground = 0x767676;
        cell.background = 0xffffff;
        cell.selection_match = true;
        let theme = rustterm::config::Theme {
            selection: 0x0000ff,
            ..Default::default()
        };
        let background = CellColors::new(&theme, true, true).background(&cell, false);
        let foreground = CellColors::new(&theme, true, true).foreground(&cell, false);
        assert!(rustterm::color::contrast(foreground, background) >= 4.5);
        assert_eq!(
            CellColors::new(&theme, false, true).foreground(&cell, false),
            cell.foreground
        );
    }

    #[test]
    fn overlay_text_is_readable_on_midtones_and_custom_selection_colors() {
        for background in [
            0x00aa00,
            0x888888,
            0xff3333,
            0xffffff,
            0,
            CellColors::new(&Theme::default(), true, true).search_match,
            CellColors::new(&Theme::default(), true, true).current_match,
        ] {
            let foreground = readable_foreground(background);
            assert!(
                rustterm::color::contrast(foreground, background) >= 4.5,
                "{foreground:06x} on {background:06x}"
            );
        }
    }

    #[test]
    fn occurrence_background_adapts_to_theme_and_yields_to_selection_search_and_cursor() {
        let mut terminal = Terminal::new(20, 2, 0);
        terminal.feed(b"word word");
        terminal.begin_selection(1, 0, SelectionKind::Word);
        let snapshot = terminal.snapshot();
        for theme in [
            Theme::default(),
            Theme {
                background: 0xffffff,
                foreground: 0x202124,
                selection: 0x9dbcf0,
                cursor: 0x111111,
                ..Theme::default()
            },
        ] {
            let mut cell = snapshot.lines[0].cells[5].clone();
            cell.background = theme.background;
            assert_eq!(
                CellColors::new(&theme, false, true).background(&cell, false),
                cell.background
            );
            let occurrence = CellColors::new(&theme, true, true).background(&cell, false);
            assert_ne!(occurrence, theme.background);
            assert_ne!(occurrence, theme.selection);
            assert_ne!(
                occurrence,
                CellColors::new(&Theme::default(), true, true).search_match
            );
            for shift in [0, 8, 16] {
                let background = (theme.background >> shift) & 255;
                let selected = (theme.selection >> shift) & 255;
                let matched = (occurrence >> shift) & 255;
                assert!((background.min(selected)..=background.max(selected)).contains(&matched));
                assert!(
                    (matched as i32 - background as i32).abs()
                        <= (selected as i32 - matched as i32).abs()
                );
            }
            cell.search_highlight = Some(SearchHighlight::Match);
            assert_eq!(
                CellColors::new(&theme, true, true).background(&cell, false),
                CellColors::new(&theme, true, true).search_match
            );
            cell.search_highlight = Some(SearchHighlight::Current);
            assert_eq!(
                CellColors::new(&theme, true, true).background(&cell, false),
                CellColors::new(&theme, true, true).current_match
            );
            cell.selected = true;
            assert_eq!(
                CellColors::new(&theme, true, true).background(&cell, false),
                theme.selection
            );
            assert_eq!(
                CellColors::new(&theme, true, true).background(&cell, true),
                theme.cursor
            );
        }
    }
}
