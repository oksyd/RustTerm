use super::super::{column, row};
use super::ProfileForm;
use crate::app::ui::{self, Icon};
use gpui::{AnyElement, Context, KeyDownEvent, KeyUpEvent, Window, div, prelude::*, px, rgb};
use rustterm::i18n::messages as i18n_key;
use rustterm::i18n::tr;

impl ProfileForm {
    fn group_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.group_focus.is_focused(window) {
            return;
        }
        let key = event.keystroke.key.as_str();
        if key == "tab" {
            if self.group_open {
                self.group_open = false;
                cx.notify();
            }
            return; // The modal owns Tab traversal.
        }
        if event.keystroke.modifiers.modified() {
            return;
        }
        if event.is_held && matches!(key, "enter" | "space") {
            cx.stop_propagation();
            window.prevent_default();
            return;
        }
        if !self.group_open {
            if !matches!(key, "enter" | "space" | "down" | "up") {
                return;
            }
            self.group_open = true;
            self.tunnel_open = false;
            self.group_cursor = self
                .group
                .and_then(|id| self.groups.iter().position(|group| group.id == id))
                .map_or(0, |index| index + 1);
        } else {
            let count = self.groups.len() + 1;
            match key {
                "up" => self.group_cursor = (self.group_cursor + count - 1) % count,
                "down" => self.group_cursor = (self.group_cursor + 1) % count,
                "home" => self.group_cursor = 0,
                "end" => self.group_cursor = count - 1,
                "enter" | "space" => {
                    self.group = self
                        .group_cursor
                        .checked_sub(1)
                        .map(|index| self.groups[index].id);
                    self.group_open = false;
                }
                "escape" => self.group_open = false,
                _ => return,
            }
        }
        // GPUI activates focused on_click handlers on key-up. Consume the matching
        // key-up after our key-down selection, otherwise it reopens the dropdown.
        if matches!(key, "enter" | "space") {
            self.suppress_group_key_up = true;
        }
        cx.stop_propagation();
        window.prevent_default();
        self.group_scroll.scroll_to_item(self.group_cursor);
        cx.notify();
    }

    fn group_key_up(&mut self, event: &KeyUpEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.suppress_group_key_up && matches!(event.keystroke.key.as_str(), "enter" | "space") {
            self.suppress_group_key_up = false;
            cx.stop_propagation();
            window.prevent_default();
        }
    }

    pub(super) fn group_picker(&self, cx: &mut Context<Self>) -> AnyElement {
        let selected = self
            .group
            .and_then(|id| self.groups.iter().find(|g| g.id == id));
        let mut picker = column().child(
            div()
                .id("profile-group")
                .track_focus(&self.group_focus)
                .flex()
                .items_center()
                .justify_between()
                .h(px(32.))
                .px_2()
                .bg(rgb(ui::INPUT))
                .border_1()
                .border_color(rgb(if self.group_open {
                    ui::ACCENT
                } else {
                    ui::BORDER
                }))
                .rounded(px(2.))
                .cursor_pointer()
                .child(
                    selected
                        .map(|group| group.name.clone())
                        .unwrap_or_else(|| tr(i18n_key::WORKSPACE_UNGROUPED).to_owned()),
                )
                .child(ui::icon(Icon::ChevronDown))
                .on_key_down(cx.listener(Self::group_key))
                .capture_key_up(cx.listener(Self::group_key_up))
                .tab_index(0)
                .focus(|style| style.border_color(rgb(ui::ACCENT)))
                .on_click(cx.listener(|this, _, window, cx| {
                    this.group_open = !this.group_open;
                    this.tunnel_open = false;
                    this.group_cursor = this
                        .group
                        .and_then(|id| this.groups.iter().position(|g| g.id == id))
                        .map_or(0, |index| index + 1);
                    this.group_scroll.scroll_to_item(this.group_cursor);
                    this.group_focus.focus(window);
                    cx.notify();
                })),
        );
        if self.group_open {
            let options = std::iter::once((None, tr(i18n_key::WORKSPACE_UNGROUPED).to_owned()))
                .chain(self.groups.iter().map(|g| (Some(g.id), g.name.clone())));
            picker = picker.child(
                div()
                    .id("profile-group-options")
                    .max_h(px(180.))
                    .overflow_y_scroll()
                    .map(ui::preserve_scroll_axes)
                    .track_scroll(&self.group_scroll)
                    .border_1()
                    .border_color(rgb(ui::BORDER))
                    .bg(rgb(ui::RAISED))
                    .children(options.enumerate().map(|(index, (id, name))| {
                        div()
                            .id(("profile-group-option", index))
                            .px_2()
                            .py_1()
                            .cursor_pointer()
                            .bg(rgb(if self.group_cursor == index {
                                ui::SELECTED
                            } else {
                                ui::RAISED
                            }))
                            .text_color(rgb(if self.group == id {
                                ui::ACCENT
                            } else {
                                ui::TEXT
                            }))
                            .hover(|style| style.bg(rgb(ui::HOVER)))
                            .child(name)
                            .tab_index(0)
                            .tab_stop(false)
                            .border_1()
                            .border_color(rgb(ui::BORDER))
                            .focus(|style| style.border_color(rgb(ui::ACCENT)))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.group = id;
                                this.group_open = false;
                                this.group_focus.focus(window);
                                cx.notify();
                            }))
                    }))
                    .map(|content| ui::scrollable(content, Some(self.group_scroll.clone()))),
            );
        }
        row(i18n_key::WORKSPACE_GROUP, picker).into_any_element()
    }
}
