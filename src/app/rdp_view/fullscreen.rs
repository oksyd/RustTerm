use super::*;
use gpui::{AnyElement, MouseButton, div, px, rgb};
use std::time::Duration;

impl RdpView {
    pub(super) fn connection_layout(&self) -> (f32, f32) {
        // resize_dimensions subtracts the normal pane header. The floating bar
        // overlays the desktop and must never change remote resolution on hover.
        (
            self.layout_size.0,
            self.layout_size.1
                + if self.fullscreen {
                    ui::PANE_HEADER_HEIGHT
                } else {
                    0.
                },
        )
    }

    pub(super) fn show_fullscreen_bar(&mut self, cx: &mut Context<Self>) {
        self.toolbar_task = None;
        if !self.toolbar_visible {
            self.toolbar_visible = true;
            cx.notify();
        }
    }

    pub(super) fn hide_fullscreen_bar_later(&mut self, cx: &mut Context<Self>) {
        if !self.fullscreen || self.toolbar_pinned || self.toolbar_hovered {
            return;
        }
        self.toolbar_task = Some(cx.spawn(async move |this, cx| {
            gpui::Timer::after(Duration::from_millis(1200)).await;
            let _ = this.update(cx, |this, cx| {
                if this.fullscreen
                    && !this.toolbar_pinned
                    && !this.toolbar_hovered
                    && !this.menu_open
                {
                    this.toolbar_visible = false;
                    cx.notify();
                }
            });
        }));
    }

    pub(super) fn render_fullscreen_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.fullscreen {
            return None;
        }
        let index = self.index;
        let visible = self.toolbar_visible || self.toolbar_pinned || self.menu_open;
        let controls = div()
            .id(("rdp-floating-controls", index))
            .max_w(px(620.))
            .h(px(ui::PANE_HEADER_HEIGHT))
            .px(px(8.))
            .flex()
            .items_center()
            .gap(px(6.))
            .bg(rgb(ui::colors().raised))
            .border_1()
            .border_color(rgb(ui::colors().border))
            .rounded_b(px(6.))
            .shadow_md()
            .on_hover(cx.listener(|this, hovered, _, cx| {
                this.toolbar_hovered = *hovered;
                if *hovered {
                    this.show_fullscreen_bar(cx);
                } else {
                    this.hide_fullscreen_bar_later(cx);
                }
            }))
            .on_mouse_down(MouseButton::Left, |_, window, cx| {
                window.prevent_default();
                cx.stop_propagation();
            })
            .on_mouse_up(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .min_w_0()
                    .max_w(px(260.))
                    .truncate()
                    .text_size(px(12.))
                    .child(self.title.clone()),
            )
            .child(
                ui::icon_button(
                    ("rdp-toolbar-pin", index),
                    if self.toolbar_pinned {
                        ui::Icon::Unpin
                    } else {
                        ui::Icon::Pin
                    },
                    i18n_key::RDP_PIN_CONNECTION_BAR,
                )
                .when(self.toolbar_pinned, |d| d.bg(rgb(ui::colors().selected)))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.toolbar_pinned = !this.toolbar_pinned;
                    this.display_changed(cx);
                    this.hide_fullscreen_bar_later(cx);
                })),
            )
            .child(
                ui::icon_button(
                    ("rdp-toolbar-release", index),
                    ui::Icon::Key,
                    i18n_key::RDP_RELEASE_KEYBOARD_CONTROL,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.release_control(cx);
                })),
            )
            .child(
                ui::icon_button(
                    ("rdp-toolbar-exit", index),
                    ui::Icon::ExitFullscreen,
                    i18n_key::WORKSPACE_EXIT_FULLSCREEN,
                )
                .on_click(cx.listener(|this, _, window, cx| {
                    this.release_control(cx);
                    window.toggle_fullscreen();
                    cx.emit(PaneEvent::Activity);
                    cx.notify();
                })),
            )
            .child(self.render_display_button(cx))
            .child(self.render_menu_button(cx));
        // Keep the controls' intrinsic width in both states. Only the centered
        // strip owns a hitbox; the rest of the top edge belongs to the desktop.
        let bar = div()
            .id(("rdp-fullscreen-bar", index))
            .min_w_0()
            .occlude()
            .when(!visible, |strip| {
                strip
                    .h(px(3.))
                    .overflow_hidden()
                    .on_mouse_move(cx.listener(|this, _, _, cx| {
                        this.show_fullscreen_bar(cx);
                        this.hide_fullscreen_bar_later(cx);
                    }))
            })
            .child(controls.when(!visible, |controls| controls.invisible()));
        Some(
            div()
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(bar)
                .into_any_element(),
        )
    }
}
