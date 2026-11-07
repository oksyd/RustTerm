use super::*;
use gpui::{MouseButton, MouseMoveEvent, SharedString, TextRun, px, rgb};

pub(super) const DEFAULT_COLUMN_WIDTH: f32 = 170.;
const MIN_COLUMN_WIDTH: f32 = 64.;
const MAX_COLUMN_WIDTH: f32 = 960.;
const AUTO_COLUMN_WIDTH: f32 = 560.;

#[derive(Clone, Copy)]
pub(super) enum ResizeTarget {
    Browser,
    Editor,
    Column(usize),
}

#[derive(Clone, Copy)]
pub(super) struct ResizeDrag {
    pub target: ResizeTarget,
    pub start_x: f32,
    pub width: f32,
}

impl DatabaseView {
    pub(super) fn column_width(&self, column: usize) -> f32 {
        self.documents[self.active_document].column_widths
            [self.documents[self.active_document].result_index][column]
    }

    pub(super) fn result_width(&self) -> f32 {
        self.documents[self.active_document].column_widths
            [self.documents[self.active_document].result_index]
            .iter()
            .sum()
    }

    pub(super) fn browser_width(&self) -> f32 {
        self.browser_width.clamp(180., self.browser_max_width())
    }

    fn browser_max_width(&self) -> f32 {
        (self.viewport_size.0 - 360.).clamp(180., 480.)
    }

    pub(super) fn drag_resize(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        let Some(drag) = self.resize_drag else { return };
        if !self.interactive() || event.pressed_button != Some(MouseButton::Left) {
            self.resize_drag = None;
            return;
        }
        let position = if matches!(drag.target, ResizeTarget::Editor) {
            event.position.y
        } else {
            event.position.x
        };
        let requested = drag.width + f32::from(position) - drag.start_x;
        let changed = match drag.target {
            ResizeTarget::Editor => {
                let height = requested.clamp(100., self.editor_max_height());
                let changed = (self.editor_height - height).abs() > 0.5;
                self.editor_height = height;
                self.update_editor_height(cx);
                changed
            }
            ResizeTarget::Browser => {
                let width = requested.clamp(180., self.browser_max_width());
                let changed = (self.browser_width - width).abs() > 0.5;
                self.browser_width = width;
                changed
            }
            ResizeTarget::Column(column) => {
                let width = requested.clamp(MIN_COLUMN_WIDTH, MAX_COLUMN_WIDTH);
                let document = &mut self.documents[self.active_document];
                let previous = &mut document.column_widths[document.result_index][column];
                let changed = (*previous - width).abs() > 0.5;
                *previous = width;
                changed
            }
        };
        if changed {
            cx.notify();
        }
    }

    pub(super) fn fit_column(&mut self, column: usize, window: &Window, cx: &mut Context<Self>) {
        if !self.interactive() {
            return;
        }
        let Some(result) = self.current_result() else {
            return;
        };
        let width = fitted_column_width(result, column, |value| {
            let text: SharedString = value.to_owned().into();
            let run = TextRun {
                len: text.len(),
                font: window.text_style().font(),
                color: rgb(ui::colors().text).into(),
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            f32::from(
                window
                    .text_system()
                    .shape_line(text, px(12.), &[run], None)
                    .width,
            )
        });
        let document = &mut self.documents[self.active_document];
        document.column_widths[document.result_index][column] = width;
        cx.notify();
    }
}

fn fitted_column_width(result: &ResultSet, column: usize, measure: impl Fn(&str) -> f32) -> f32 {
    // Sampling is done only on explicit auto-fit. It never scales with all loaded
    // rows or with the size of an individual BLOB/long text cell.
    let title = result
        .columns
        .get(column)
        .map_or(0., |value| measure(&render::preview(Some(value), 80)) + 20.);
    let content = result
        .rows
        .iter()
        .take(128)
        .filter_map(|row| row.get(column))
        .map(|cell| measure(&render::preview(cell.as_deref(), 80)))
        .fold(0., f32::max);
    (title.max(content) + 24.).clamp(MIN_COLUMN_WIDTH, AUTO_COLUMN_WIDTH)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn measure(value: &str) -> f32 {
        value
            .chars()
            .map(|ch| if ch.is_ascii() { 7.5 } else { 14. })
            .sum()
    }

    #[test]
    fn auto_fit_is_bounded_and_handles_non_ascii_text() {
        let mut result = ResultSet {
            columns: vec!["id".into(), "详情".into()],
            rows: vec![vec![Some("1".into()), Some("示例内容".into())]],
            ..Default::default()
        };
        assert_eq!(fitted_column_width(&result, 0, measure), MIN_COLUMN_WIDTH);
        assert!(fitted_column_width(&result, 1, measure) > MIN_COLUMN_WIDTH);
        result.rows[0][1] = Some("x".repeat(2000));
        assert_eq!(fitted_column_width(&result, 1, measure), AUTO_COLUMN_WIDTH);
    }
}
