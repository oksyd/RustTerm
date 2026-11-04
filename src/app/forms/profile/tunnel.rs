//! Database routing selects an existing SSH identity rather than copying credentials.
use super::super::{column, hint, row};
use super::ProfileForm;
use crate::app::ui::{self, Icon};
use gpui::{AnyElement, Context, KeyDownEvent, KeyUpEvent, Window, div, prelude::*, px, rgb};
use rustterm::i18n::messages as i18n_key;
use rustterm::{
    config::{Connection, Profile},
    i18n::tr,
};

impl ProfileForm {
    fn tunnel_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.tunnel_focus.is_focused(window) {
            return;
        }
        let key = event.keystroke.key.as_str();
        if key == "tab" {
            if self.tunnel_open {
                self.tunnel_open = false;
                cx.notify();
            }
            return;
        }
        if event.keystroke.modifiers.modified() {
            return;
        }
        if event.is_held && matches!(key, "enter" | "space") {
            cx.stop_propagation();
            window.prevent_default();
            return;
        }
        if !self.tunnel_open {
            if !matches!(key, "enter" | "space" | "down" | "up") {
                return;
            }
            self.tunnel_open = true;
            self.group_open = false;
            self.tunnel_cursor = self
                .ssh_profile
                .and_then(|id| {
                    self.ssh_profiles
                        .iter()
                        .position(|profile| profile.id == id)
                })
                .map_or(0, |index| index + 1);
        } else {
            let count = self.ssh_profiles.len() + 1;
            match key {
                "up" => self.tunnel_cursor = (self.tunnel_cursor + count - 1) % count,
                "down" => self.tunnel_cursor = (self.tunnel_cursor + 1) % count,
                "home" => self.tunnel_cursor = 0,
                "end" => self.tunnel_cursor = count - 1,
                "enter" | "space" => {
                    self.ssh_profile = self
                        .tunnel_cursor
                        .checked_sub(1)
                        .map(|index| self.ssh_profiles[index].id);
                    self.tunnel_open = false;
                }
                "escape" => self.tunnel_open = false,
                _ => return,
            }
        }
        if matches!(key, "enter" | "space") {
            self.suppress_tunnel_key_up = true;
        }
        self.tunnel_scroll.scroll_to_item(self.tunnel_cursor);
        cx.stop_propagation();
        window.prevent_default();
        cx.notify();
    }

    fn tunnel_key_up(&mut self, event: &KeyUpEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.suppress_tunnel_key_up && matches!(event.keystroke.key.as_str(), "enter" | "space")
        {
            self.suppress_tunnel_key_up = false;
            cx.stop_propagation();
            window.prevent_default();
        }
    }

    pub(super) fn tunnel_picker(&self, cx: &mut Context<Self>) -> AnyElement {
        let selected = match self.ssh_profile {
            None => tr(i18n_key::REDIS_DIRECT_CONNECTION).to_owned(),
            Some(id) => self
                .ssh_profiles
                .iter()
                .find(|profile| profile.id == id)
                .map(tunnel_label)
                .unwrap_or_else(|| tr(i18n_key::REDIS_UNAVAILABLE_SSH_PROFILE).to_owned()),
        };
        let mut picker = column().child(
            div()
                .id("database-tunnel")
                .track_focus(&self.tunnel_focus)
                .tab_index(0)
                .flex()
                .items_center()
                .justify_between()
                .gap_1()
                .h(px(32.))
                .px_2()
                .bg(rgb(ui::INPUT))
                .border_1()
                .border_color(rgb(if self.tunnel_open {
                    ui::ACCENT
                } else {
                    ui::BORDER
                }))
                .rounded(px(2.))
                .cursor_pointer()
                .focus(|style| style.border_color(rgb(ui::ACCENT)))
                .child(div().flex_1().min_w_0().text_ellipsis().child(selected))
                .child(ui::icon(Icon::ChevronDown))
                .on_key_down(cx.listener(Self::tunnel_key))
                .capture_key_up(cx.listener(Self::tunnel_key_up))
                .on_click(cx.listener(|this, _, window, cx| {
                    this.tunnel_open = !this.tunnel_open;
                    this.group_open = false;
                    this.tunnel_cursor = this
                        .ssh_profile
                        .and_then(|id| {
                            this.ssh_profiles
                                .iter()
                                .position(|profile| profile.id == id)
                        })
                        .map_or(0, |index| index + 1);
                    this.tunnel_scroll.scroll_to_item(this.tunnel_cursor);
                    this.tunnel_focus.focus(window);
                    cx.notify();
                })),
        );
        if self.tunnel_open {
            let choices = std::iter::once((None, tr(i18n_key::REDIS_DIRECT_CONNECTION).to_owned()))
                .chain(
                    self.ssh_profiles
                        .iter()
                        .map(|profile| (Some(profile.id), tunnel_label(profile))),
                );
            picker = picker.child(
                div()
                    .id("database-tunnel-options")
                    .max_h(px(180.))
                    .overflow_y_scroll()
                    .map(ui::preserve_scroll_axes)
                    .track_scroll(&self.tunnel_scroll)
                    .border_1()
                    .border_color(rgb(ui::BORDER))
                    .bg(rgb(ui::RAISED))
                    .children(choices.enumerate().map(|(index, (id, label))| {
                        div()
                            .id(("redis-tunnel-option", index))
                            .px_2()
                            .py_1()
                            .cursor_pointer()
                            .bg(rgb(if self.tunnel_cursor == index {
                                ui::SELECTED
                            } else {
                                ui::RAISED
                            }))
                            .text_color(rgb(if self.ssh_profile == id {
                                ui::ACCENT
                            } else {
                                ui::TEXT
                            }))
                            .hover(|style| style.bg(rgb(ui::HOVER)))
                            .tab_index(0)
                            .tab_stop(false)
                            .border_1()
                            .border_color(rgb(ui::BORDER))
                            .focus(|style| style.border_color(rgb(ui::ACCENT)))
                            .child(label)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.ssh_profile = id;
                                this.tunnel_open = false;
                                this.tunnel_focus.focus(window);
                                cx.notify();
                            }))
                    }))
                    .map(|content| ui::scrollable(content, Some(self.tunnel_scroll.clone()))),
            );
        }
        column()
            .child(row(i18n_key::REDIS_SSH_TUNNEL, picker))
            .when(self.ssh_profile.is_some(), |body| {
                body.child(hint(
                    i18n_key::DATABASE_THE_DATABASE_HOST_AND_PORT_ARE_REACHED_FROM_THE_SSH,
                ))
            })
            .when(self.ssh_profiles.is_empty(), |body| {
                body.child(hint(
                    i18n_key::REDIS_ADD_AN_SSH_PROFILE_FIRST_TO_CONNECT_THROUGH_A_TUNNEL,
                ))
            })
            .into_any_element()
    }
}

fn tunnel_label(profile: &Profile) -> String {
    match &profile.connection {
        Connection::Ssh(ssh) => format!(
            "{} · {}@{}:{}",
            profile.name, ssh.username, ssh.host, ssh.port
        ),
        _ => profile.name.clone(), // The constructor filters non-SSH profiles out.
    }
}
