use super::*;

impl DatabaseView {
    pub(super) fn capture_edit_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.interactive() || self.input_composing(cx) || !self.cell_edit_focused(window, cx) {
            return;
        }
        let modifiers = event.keystroke.modifiers;
        if modifiers.alt || modifiers.platform {
            return;
        }
        let inline = self.inline_edit_cell();
        match event.keystroke.key.as_str() {
            "escape" if !modifiers.control && !modifiers.shift => {
                self.cancel_cell_edit(window, cx);
            }
            "enter" if !modifiers.shift && (modifiers.control || inline.is_some()) => {
                self.stage_cell(window, cx);
            }
            "tab" if !modifiers.control => {
                let Some(current) = inline else { return };
                self.advance_inline_cell(current, modifiers.shift, window, cx);
            }
            _ => return,
        }
        window.prevent_default();
        cx.stop_propagation();
    }

    fn advance_inline_cell(
        &mut self,
        current: (usize, usize),
        backwards: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let document = &self.documents[self.active_document];
        let Some(result) = document.current_result() else {
            return;
        };
        let next = next_editable_cell(
            &document.visible_rows,
            result.columns.len(),
            current,
            backwards,
            |row, column| self.can_edit_inline_cell(row, column),
        );
        if !self.stage_cell(window, cx) {
            return;
        }
        if let Some((row, column)) = next
            && self.documents[self.active_document]
                .visible_rows
                .contains(&row)
            && self.select_grid_cell(row, column, window, cx)
        {
            self.reveal_grid_cell(row, column, cx);
            self.edit_cell(window, cx);
        }
    }

    pub(super) fn grid_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.page != WorkspacePage::Document
            || self.object_menu.is_some()
            || !self.focus.is_focused(window)
            || self.documents[self.active_document].value_view.is_some()
        {
            return false;
        }
        let key = event.keystroke.key.as_str();
        let modifiers = event.keystroke.modifiers;
        if modifiers.control || modifiers.alt || modifiers.platform {
            return false;
        }
        if key == "enter" || (key == "f2" && !modifiers.shift) {
            let Some((row, column)) = self.documents[self.active_document].selected_cell else {
                return false;
            };
            if self.can_edit_cell(row, column) {
                self.reveal_grid_cell(row, column, cx);
                if modifiers.shift {
                    self.expand_cell_editor(window, cx);
                } else {
                    self.edit_cell(window, cx);
                }
            } else {
                self.open_value(window, cx);
            }
            return true;
        }
        if modifiers.shift {
            return false;
        }
        if key == "delete" && self.table_editable() {
            self.delete_table_row(cx);
            return true;
        }
        let document = &self.documents[self.active_document];
        let Some(result) = document.current_result() else {
            return false;
        };
        let columns = result.columns.len();
        if columns == 0 || document.visible_rows.is_empty() {
            return false;
        }
        let (row, column) = document
            .selected_cell
            .unwrap_or((document.visible_rows[0], 0));
        let visible = document
            .visible_rows
            .iter()
            .position(|index| *index == row)
            .unwrap_or(0);
        let last = document.visible_rows.len() - 1;
        let (visible, column) = match key {
            "up" => (visible.saturating_sub(1), column),
            "down" => ((visible + 1).min(last), column),
            "left" => (visible, column.saturating_sub(1)),
            "right" => (visible, (column + 1).min(columns - 1)),
            "home" => (visible, 0),
            "end" => (visible, columns - 1),
            "pageup" => (visible.saturating_sub(20), column),
            "pagedown" => ((visible + 20).min(last), column),
            _ => return false,
        };
        let row = document.visible_rows[visible];
        if self.select_grid_cell(row, column, window, cx) {
            self.reveal_grid_cell(row, column, cx);
        }
        true
    }

    pub(super) fn reveal_grid_cell(&self, row: usize, column: usize, cx: &mut Context<Self>) {
        let document = &self.documents[self.active_document];
        let Some(visible) = document.visible_rows.iter().position(|index| *index == row) else {
            return;
        };
        document
            .result_scroll
            .scroll_to_item(visible, gpui::ScrollStrategy::Center);
        let widths = &document.column_widths[document.result_index];
        let left: f32 = widths[..column].iter().sum();
        let width = widths[column];
        let viewport = f32::from(document.horizontal_scroll.bounds().size.width);
        let mut offset = document.horizontal_scroll.offset();
        let scroll = -f32::from(offset.x);
        offset.x = gpui::px(-if left < scroll {
            left
        } else if left + width > scroll + viewport {
            (left + width - viewport).max(0.)
        } else {
            scroll
        });
        document.horizontal_scroll.set_offset(offset);
        cx.notify();
    }
}

fn next_editable_cell(
    rows: &[usize],
    columns: usize,
    current: (usize, usize),
    backwards: bool,
    editable: impl Fn(usize, usize) -> bool,
) -> Option<(usize, usize)> {
    if columns == 0 || current.1 >= columns {
        return None;
    }
    let row = rows.iter().position(|row| *row == current.0)?;
    let current = row * columns + current.1;
    let end = rows.len() * columns;
    let step = |position: &usize| {
        if backwards {
            position.checked_sub(1)
        } else {
            position.checked_add(1).filter(|next| *next < end)
        }
    };
    std::iter::successors(step(&current), step)
        .map(|position| (rows[position / columns], position % columns))
        .find(|&(row, column)| editable(row, column))
}

#[cfg(test)]
mod tests {
    use super::next_editable_cell;

    #[test]
    fn tab_uses_visible_order_and_skips_read_only_cells() {
        let rows = [8, 3, 6];
        let editable = |row, column| row != 3 && column != 1;
        assert_eq!(
            next_editable_cell(&rows, 3, (8, 0), false, editable),
            Some((8, 2))
        );
        assert_eq!(
            next_editable_cell(&rows, 3, (8, 2), false, editable),
            Some((6, 0))
        );
        assert_eq!(
            next_editable_cell(&rows, 3, (6, 0), true, editable),
            Some((8, 2))
        );
    }

    #[test]
    fn tab_stops_at_page_edges_and_missing_rows() {
        let rows = [4, 1];
        assert_eq!(
            next_editable_cell(&rows, 2, (4, 0), true, |_, _| true),
            None
        );
        assert_eq!(
            next_editable_cell(&rows, 2, (1, 1), false, |_, _| true),
            None
        );
        assert_eq!(
            next_editable_cell(&rows, 2, (9, 0), false, |_, _| true),
            None
        );
        assert_eq!(
            next_editable_cell(&rows, 2, (4, 0), false, |_, _| false),
            None
        );
    }
}
