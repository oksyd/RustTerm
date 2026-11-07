use super::*;
use gpui::{AnyElement, IntoElement, ListAlignment, ListState, div, list, prelude::*, px, rgb};
use rustterm::i18n::messages as i18n_key;
use std::ops::Range;

#[derive(Clone)]
pub(super) struct ValueView {
    chunks: Vec<Range<usize>>,
    scroll: ListState,
}

impl DatabaseView {
    fn selected_value(&self) -> Option<(&str, Option<&str>)> {
        let (row, column) = self.documents[self.active_document].selected_cell?;
        let result = self.current_result()?;
        Some((
            result.columns.get(column)?.as_str(),
            result.rows.get(row)?.get(column)?.as_deref(),
        ))
    }

    pub(super) fn open_value(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.interactive() {
            return;
        }
        let Some((_, value)) = self.selected_value() else {
            return;
        };
        let chunks = value_chunks(value.unwrap_or("NULL"));
        self.documents[self.active_document].value_view = Some(ValueView {
            scroll: ListState::new(chunks.len(), ListAlignment::Top, px(200.)),
            chunks,
        });
        self.resize_drag = None;
        self.focus.focus(window);
        cx.notify();
    }

    pub(super) fn close_value(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.documents[self.active_document]
            .value_view
            .take()
            .is_some()
        {
            self.focus.focus(window);
            cx.notify();
        }
    }

    pub(super) fn scroll_value(&self, key: &str, cx: &mut Context<Self>) -> bool {
        let Some(view) = &self.documents[self.active_document].value_view else {
            return false;
        };
        match key {
            "up" => view.scroll.scroll_by(px(-20.)),
            "down" => view.scroll.scroll_by(px(20.)),
            "pageup" => view
                .scroll
                .scroll_by(px(-(self.viewport_size.1 - 120.).max(80.))),
            "pagedown" => view
                .scroll
                .scroll_by(px((self.viewport_size.1 - 120.).max(80.))),
            "home" => view.scroll.scroll_to(gpui::ListOffset::default()),
            "end" => view.scroll.scroll_to(gpui::ListOffset {
                item_ix: view.chunks.len(),
                offset_in_item: px(0.),
            }),
            _ => return false,
        }
        cx.notify();
        true
    }

    pub(super) fn render_value(&self, cx: &Context<Self>) -> AnyElement {
        let viewer = self.documents[self.active_document]
            .value_view
            .as_ref()
            .unwrap();
        let (column, value) = self.selected_value().unwrap();
        div()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .p_2()
                    .border_b_1()
                    .border_color(rgb(ui::colors().border))
                    .child(div().min_w_0().flex_1().truncate().child(column.to_owned()))
                    .child(
                        div()
                            .text_color(rgb(ui::colors().muted))
                            .child(rustterm::trf!(
                                i18n_key::COMMON_BYTE_SIZE,
                                bytes = value.map_or(0, str::len)
                            )),
                    )
                    .child(
                        ui::text_button_enabled(
                            "database-full-value-copy",
                            i18n_key::DATABASE_COPY_CELL,
                            ui::ButtonStyle::Neutral,
                            self.interactive(),
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.copy_cell(cx))),
                    )
                    .child(
                        ui::icon_button_enabled(
                            "database-full-value-close",
                            ui::Icon::Close,
                            i18n_key::COMMON_CLOSE_VALUE_ESC,
                            self.interactive(),
                        )
                        .on_click(cx.listener(|this, _, window, cx| this.close_value(window, cx))),
                    ),
            )
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .pr(px(ui::SCROLLBAR_WIDTH))
                    .child(
                        list(
                            viewer.scroll.clone(),
                            cx.processor(|this, index: usize, _, _| {
                                let view = this.documents[this.active_document]
                                    .value_view
                                    .as_ref()
                                    .unwrap();
                                let (_, value) = this.selected_value().unwrap();
                                let text = value.unwrap_or("NULL");
                                let range = &view.chunks[index];
                                // Each list item is small even for a single multi-megabyte line.
                                // The list lazily remeasures its visible items when width changes.
                                let chunk = &text[range.clone()];
                                let chunk = if range.end < text.len() {
                                    chunk.strip_suffix('\n').unwrap_or(chunk)
                                } else {
                                    chunk
                                };
                                div()
                                    .w_full()
                                    .min_h(px(20.))
                                    .px_3()
                                    .font_family(ui::MONOSPACE_FONT)
                                    .text_size(px(12.))
                                    .line_height(px(20.))
                                    .whitespace_normal()
                                    .text_color(rgb(if value.is_none() {
                                        ui::colors().faint
                                    } else {
                                        ui::colors().text
                                    }))
                                    .child(chunk.to_owned())
                                    .into_any_element()
                            }),
                        )
                        .flex_1()
                        .min_h_0()
                        .min_w_0()
                        .py_2(),
                    )
                    .child(ui::list_scrollbar(
                        "database-full-value-bar",
                        viewer.scroll.clone(),
                    )),
            )
            .into_any_element()
    }
}

pub(super) fn value_chunks(text: &str) -> Vec<Range<usize>> {
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let mut end = (start + 1024).min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        // Prefer paragraph boundaries; long lines still get bounded soft wraps.
        if end < text.len()
            && let Some(newline) = text[start..end].rfind('\n')
            && newline >= 512
        {
            end = start + newline + 1;
        }
        chunks.push(start..end);
        start = end;
    }
    if chunks.is_empty() {
        chunks.push(0..0);
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_value_chunks_preserve_every_byte_and_bound_unicode_layout() {
        let text = format!("{}\n{}", "中文🙂".repeat(300), "\n".repeat(4096));
        let chunks = value_chunks(&text);
        assert!(chunks.iter().all(|range| range.len() <= 1024));
        assert_eq!(
            chunks
                .iter()
                .map(|range| &text[range.clone()])
                .collect::<String>(),
            text
        );
        assert_eq!(value_chunks(""), vec![0..0]);
        assert_eq!(value_chunks("NULL"), vec![0..4]);
    }
}
