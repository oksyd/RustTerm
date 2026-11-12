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
                .and_then(|id| self.groups.iter().position(|group| group.0 == id))
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
                        .map(|index| self.groups[index].0);
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
        if self.group_open
            && let Some(scroll) = self.scroll_handle()
        {
            scroll.scroll_to_bottom();
        }
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
            .and_then(|id| self.groups.iter().find(|g| g.0 == id));
        let label = selected
            .map(|group| group.1.clone())
            .unwrap_or_else(|| tr(i18n_key::WORKSPACE_UNGROUPED).to_owned());
        let tooltip = label.clone();
        let mut picker = column().child(
            div()
                .id("profile-group")
                .track_focus(&self.group_focus)
                .flex()
                .items_center()
                .justify_between()
                .h(px(32.))
                .px_2()
                .bg(rgb(ui::colors().input))
                .border_1()
                .border_color(rgb(if self.group_open {
                    ui::colors().accent
                } else {
                    ui::colors().border
                }))
                .rounded(px(2.))
                .cursor_pointer()
                .min_w_0()
                .tooltip(move |_, cx| cx.new(|_| ui::Tooltip(tooltip.clone())).into())
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .whitespace_nowrap()
                        .overflow_hidden()
                        .text_ellipsis()
                        .child(label),
                )
                .child(ui::icon(Icon::ChevronDown))
                .on_key_down(cx.listener(Self::group_key))
                .capture_key_up(cx.listener(Self::group_key_up))
                .tab_index(0)
                .focus(|style| style.border_color(rgb(ui::colors().accent)))
                .on_click(cx.listener(|this, _, window, cx| {
                    this.group_open = !this.group_open;
                    this.tunnel_open = false;
                    this.group_cursor = this
                        .group
                        .and_then(|id| this.groups.iter().position(|g| g.0 == id))
                        .map_or(0, |index| index + 1);
                    this.group_scroll.scroll_to_item(this.group_cursor);
                    if this.group_open
                        && let Some(scroll) = this.scroll_handle()
                    {
                        scroll.scroll_to_bottom();
                    }
                    this.group_focus.focus(window);
                    cx.notify();
                })),
        );
        if self.group_open {
            let options = std::iter::once((None, tr(i18n_key::WORKSPACE_UNGROUPED).to_owned()))
                .chain(self.groups.iter().map(|g| (Some(g.0), g.1.clone())));
            picker = picker.child(
                div()
                    .id("profile-group-options")
                    .max_h(px(180.))
                    .overflow_y_scroll()
                    .map(ui::preserve_scroll_axes)
                    .track_scroll(&self.group_scroll)
                    .border_1()
                    .border_color(rgb(ui::colors().border))
                    .bg(rgb(ui::colors().raised))
                    .children(options.enumerate().map(|(index, (id, name))| {
                        div()
                            .id(("profile-group-option", index))
                            .px_2()
                            .py_1()
                            .cursor_pointer()
                            .bg(rgb(if self.group_cursor == index {
                                ui::colors().selected
                            } else {
                                ui::colors().raised
                            }))
                            .text_color(rgb(if self.group == id {
                                ui::colors().accent
                            } else {
                                ui::colors().text
                            }))
                            .hover(|style| style.bg(rgb(ui::colors().hover)))
                            .tooltip({
                                let path = name.clone();
                                move |_, cx| cx.new(|_| ui::Tooltip(path.clone())).into()
                            })
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(name)
                            .tab_index(0)
                            .tab_stop(false)
                            .border_1()
                            .border_color(rgb(ui::colors().border))
                            .focus(|style| style.border_color(rgb(ui::colors().accent)))
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
