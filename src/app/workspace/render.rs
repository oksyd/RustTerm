use super::{DragTarget, LeaveAction, Workspace};
use crate::app::layout::{PaneNode, WorkspaceLayout};
use crate::app::pane::{Pane, PaneEvent};
use crate::app::ui;
use crate::app::window_frame::WindowFrame;
use gpui::prelude::*;
use gpui::{
    Context, KeyDownEvent, MouseButton, MouseMoveEvent, MouseUpEvent, Window, canvas, div, px, rgb,
    rgba,
};
use rustterm::i18n::messages as i18n_key;
use rustterm::i18n::tr;

impl gpui::Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !window.is_window_active() && self.menu.take().is_some() {
            self.restore_popup_focus(window, cx);
        }
        let frame = WindowFrame::new(window);
        frame.configure(window);
        let content_size = frame.content_size(window.viewport_size());
        self.sync_tools(cx);
        self.sync_resources(window, cx);
        self.sync_docker(window, cx);
        if self.lock_screen.locked {
            self.command_notice = None;
            return frame.wrap(self.render_locked(window, cx), window);
        }
        self.schedule_workspace_persistence(cx);
        self.bind_modal(window, cx);
        let panel_open = self.utility_is_open();
        let minimum = self.content_minimum(cx);
        let layout = WorkspaceLayout::new(
            f32::from(content_size.width),
            &self.config.settings,
            self.sidebar_overlay_open,
            self.preferred_content_minimum(cx).0,
            panel_open,
        );
        if layout.sidebar_inline {
            self.sidebar_overlay_open = false;
        }
        let sidebar_loses_focus = self.sidebar_visible
            && !layout.sidebar_visible
            && self.sidebar_focus.contains_focused(window, cx);
        self.sidebar_visible = layout.sidebar_visible;
        self.sidebar_width = layout.sidebar_width;
        // Compute from the current viewport, not the previous frame's bounds,
        // so resizing does not lag behind the window by one render.
        let width = f32::from(content_size.width)
            - if layout.sidebar_inline {
                layout.sidebar_width + 5.
            } else {
                0.
            }
            - if layout.files_docked && panel_open {
                layout.files_width + 5.
            } else {
                0.
            };
        let height = (f32::from(content_size.height)
            - ui::TITLE_HEIGHT
            - ui::TAB_HEIGHT
            - ui::STATUS_HEIGHT
            - if self.error.is_some() {
                self.error_banner_height
            } else {
                0.
            })
        .max(0.);
        let compact = self.zoomed.is_none()
            && self
                .tabs
                .get(self.active)
                .is_some_and(|tab| tab.layout.leaves().len() > 1)
            && (width < minimum.0 || height < minimum.1);
        let switcher_loses_focus =
            self.compact_panes && !compact && self.pane_switcher_focus.contains_focused(window, cx);
        if self.compact_panes != compact {
            self.compact_panes = compact;
            self.dragging = None;
            self.sync_visibility(cx);
        }
        let visible = self.visible_ids();
        let external_header = (visible.len() == 1)
            .then(|| self.panes.get(&visible[0]))
            .flatten()
            .and_then(|pane| match pane {
                Pane::Terminal(view) => Some(view.clone()),
                _ => None,
            });
        for pane in self.panes.values() {
            if let Pane::Terminal(terminal) = pane {
                terminal.update(cx, |view, cx| {
                    view.set_header_external(external_header.as_ref() == Some(terminal), cx)
                });
            }
        }
        self.terminal_size = (
            width.max(0.),
            (height
                - if compact { ui::TITLE_HEIGHT } else { 0. }
                - if external_header.is_some() {
                    ui::PANE_HEADER_HEIGHT
                } else {
                    0.
                })
            .max(0.),
        );
        let utility_overlay = panel_open && !layout.files_docked;
        if self.utility_overlay != utility_overlay {
            let was_overlay = self.utility_overlay;
            self.utility_overlay = utility_overlay;
            self.sync_visibility(cx);
            if (utility_overlay
                && self.modal.is_none()
                && !self.palette.open
                && !self.authentication.read(cx).is_active()
                && !self.window_state.prompts.is_busy())
                || (was_overlay && !panel_open)
            {
                self.restore_focus(window, cx);
            }
        }
        if sidebar_loses_focus || switcher_loses_focus {
            self.restore_focus(window, cx);
        }
        let toolbar = self.render_titlebar(window, cx);
        let tabbar = self.render_tabs(cx);
        let statusbar = self.render_statusbar(window, cx);
        let mut utility_panel = self.render_utility_panel(cx);
        let session_header = external_header
            .map(|terminal| terminal.update(cx, |view, cx| view.external_header(cx)));
        let node = self
            .zoomed
            .or(self.compact_panes.then_some(self.focused))
            .map(PaneNode::Leaf)
            .or_else(|| {
                self.tabs
                    .get(self.active)
                    .map(|tab| self.fitted_layout(tab, cx))
            });
        let content =
            node.as_ref()
                .map(|node| self.render_node(node, self.terminal_size, cx))
                .unwrap_or_else(|| {
                    ui::empty_state(ui::Icon::Terminal, tr(
                        i18n_key::WORKSPACE_SELECT_A_CONNECTION_PROFILE_OR_CREATE_A_LOCAL_TERMINAL,
                    )).size_full()
                    .into_any_element()
                });
        let weak = cx.weak_entity();
        let mut workspace = div().flex().flex_1().min_h_0();
        if layout.sidebar_inline {
            workspace = workspace
                .child(self.render_sidebar(cx))
                .child(self.panel_divider("sidebar-divider", DragTarget::Sidebar, cx));
        }
        let terminal = div()
            .relative()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .child(
                canvas(
                    move |bounds, _, cx| {
                        let _ = weak.update(cx, |this, _| {
                            this.terminal_size =
                                (f32::from(bounds.size.width), f32::from(bounds.size.height));
                        });
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .child(content);
        let mut body = div().relative().flex_1().min_h_0().flex().child(terminal);
        if layout.files_docked {
            if let Some(panel) = utility_panel.take() {
                body = body
                    .child(self.panel_divider("files-divider", DragTarget::Files, cx))
                    .child(
                        div()
                            .w(px(layout.files_width))
                            .h_full()
                            .flex_shrink_0()
                            .child(panel),
                    );
            }
        } else if let Some(panel) = utility_panel.take() {
            body = body
                .child(
                    div()
                        .absolute()
                        .inset_0()
                        .bg(rgba(0x00000055))
                        .occlude()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, window, cx| {
                                this.close_utility_panel(window, cx);
                                cx.stop_propagation();
                            }),
                        ),
                )
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .right_0()
                        .w(px(layout.files_width.min(width)))
                        .max_w_full()
                        .bg(rgb(ui::colors().surface))
                        .border_l_1()
                        .border_color(rgb(ui::colors().border))
                        .occlude()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(panel),
                );
        }
        workspace = workspace.child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(tabbar)
                .when(self.compact_panes, |d| {
                    d.child(self.render_pane_switcher(cx))
                })
                .children(session_header)
                .child(body),
        );
        let mut root = div()
            .track_focus(&self.workspace_focus)
            .capture_key_down(
                cx.listener(|this, event, window, cx| this.workspace_shortcuts(event, window, cx)),
            )
            .capture_any_mouse_down(cx.listener(|this, _, _, cx| {
                if this.lock_screen.locked {
                    cx.stop_propagation();
                }
            }))
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(ui::colors().bg))
            .map(|d| frame.rounded(d))
            .overflow_hidden()
            .text_color(rgb(ui::colors().text))
            .font_family(".SystemUIFont")
            .text_size(px(13.))
            .on_mouse_move(
                cx.listener(|this, event: &MouseMoveEvent, window, cx| this.drag_resize(event, window, cx)),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, event: &MouseUpEvent, _, cx| {
                    if matches!(
                        this.dragging.take(),
                        Some(DragTarget::Sidebar | DragTarget::Files)
                    ) {
                        this.save(cx);
                    }
                    if let Some(Pane::Terminal(p)) = this.panes.get(&this.focused) {
                        p.update(cx, |p, cx| p.drag_selection(event.position, true, cx));
                    }
                }),
            )
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if this.rdp_captures_keyboard(cx) {
                    return;
                }
                if this.closing || this.modal_saving {
                    cx.stop_propagation();
                    return;
                }
                if this.palette.open || this.authentication.read(cx).is_active() {
                    return;
                }
                if this.menu_key(event, window, cx) || this.sidebar_key(event, window, cx) {
                    return;
                }
                let modifiers = event.keystroke.modifiers;
                if this.modal.is_none()
                    && !this.window_state.prompts.is_busy()
                    && !this.input_composing(window, cx)
                    && modifiers.control
                    && !modifiers.alt
                    && !modifiers.platform
                {
                    let handled = match event.keystroke.key.as_str() {
                        "tab" => {
                            let count = this.tabs.len().max(1);
                            let next = if modifiers.shift {
                                (this.active + count - 1) % count
                            } else {
                                (this.active + 1) % count
                            };
                            this.set_active(next, window, cx);
                            true
                        }
                        "j" if modifiers.shift => {
                            this.pane_event(&PaneEvent::NextPane, window, cx);
                            true
                        }
                        _ => false,
                    };
                    if handled {
                        window.prevent_default();
                        cx.stop_propagation();
                        return;
                    }
                }
                if this.modal.is_none()
                    && !this.window_state.prompts.is_busy()
                    && modifiers.control
                    && modifiers.shift
                    && !modifiers.alt
                    && !modifiers.platform
                {
                    match event.keystroke.key.as_str() {
                        "b" => this.toggle_sidebar(window, cx),
                        "enter" => this.toggle_zoom(window, cx),
                        _ => return,
                    }
                    window.prevent_default();
                    cx.stop_propagation();
                    return;
                }
                if event.keystroke.key == "escape"
                    && this
                        .session_filter
                        .read(cx)
                        .focus_handle()
                        .is_focused(window)
                    && !this.session_filter.read(cx).is_composing()
                {
                    this.session_filter.update(cx, |field, cx| field.clear(cx));
                    this.restore_focus(window, cx);
                    cx.stop_propagation();
                    return;
                }
                if event.keystroke.key == "tab" && this.modal.is_some() && !this.modal_composing(cx)
                {
                    this.cycle_modal_focus(event.keystroke.modifiers.shift, window, cx);
                    cx.stop_propagation();
                    window.prevent_default();
                    return;
                }
                // Terminal views consume their own Tab key. Unconsumed Tab in
                // chrome, forms and database views follows the focus order.
                if event.keystroke.key == "tab"
                    && this.modal.is_none()
                    && !modifiers.control
                    && !modifiers.alt
                    && !modifiers.platform
                    && !this.input_composing(window, cx)
                {
                    if modifiers.shift {
                        window.focus_prev();
                    } else {
                        window.focus_next();
                    }
                    if this.sidebar_overlay_open {
                        if !this.sidebar_focus.contains_focused(window, cx) {
                            this.sidebar_focus.focus(window);
                        }
                    } else if this.utility_overlay
                        && !this.utility_focus.contains_focused(window, cx)
                        && !this.panes.get(&this.focused).is_some_and(|pane| {
                            matches!(pane, Pane::Terminal(terminal) if terminal.read(cx).header_owns_focus(window, cx))
                        })
                    {
                        this.restore_focus(window, cx);
                    }
                    window.prevent_default();
                    cx.stop_propagation();
                    return;
                }
                if event.keystroke.key == "escape"
                    && this.modal.is_some()
                    && !this.modal_composing(cx)
                {
                    this.request_leave_modal(LeaveAction::CloseModal, window, cx);
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            .child(self.activity_listener(cx))
            .child(toolbar)
            .child(workspace);
        if let Some(error) = &self.error {
            let entity = cx.weak_entity();
            root = root.child(
                div()
                    .id("workspace-error")
                    .relative()
                    .max_h(px(80.))
                    .flex_shrink_0()
                    .overflow_y_scroll()
                    .map(ui::preserve_scroll_axes)
                    .px_3()
                    .py_2()
                    .bg(rgb(ui::colors().danger_bg))
                    .text_color(rgb(ui::colors().danger))
                    .child(error.render())
                    .child(
                        canvas(
                            move |bounds, _, cx| {
                                let _ = entity.update(cx, |this, cx| {
                                    let height = f32::from(bounds.size.height);
                                    if (this.error_banner_height - height).abs() > 0.5 {
                                        this.error_banner_height = height;
                                        cx.notify();
                                    }
                                });
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    )
                    .map(|content| ui::scrollable(content, None)),
            );
        }
        root = root
            .child(statusbar)
            .children(self.render_command_notice(window, cx));
        if layout.sidebar_visible && !layout.sidebar_inline {
            root = root.child(
                div()
                    .absolute()
                    .top(px(ui::TITLE_HEIGHT))
                    .bottom(px(ui::STATUS_HEIGHT))
                    .left_0()
                    .right_0()
                    .bg(rgba(0x00000066))
                    .occlude()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            this.sidebar_overlay_open = false;
                            this.restore_focus(window, cx);
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    )
                    .child(
                        div()
                            .h_full()
                            .w(px(layout.sidebar_width))
                            .occlude()
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .child(self.render_sidebar(cx)),
                    ),
            );
        }
        if let Some(menu) = self.render_menu(window, cx) {
            root = root.child(menu);
        }
        if let Some(modal) = self.render_modal(window, cx) {
            root = root.child(modal);
        }
        if self.modal_saving {
            root = root.child(
                div()
                    .absolute()
                    .inset_0()
                    .occlude()
                    .track_focus(&self.modal_focus)
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(rgba((ui::colors().bg << 8) | 0xdd))
                    .map(|d| frame.rounded(d))
                    .child(tr(i18n_key::WORKSPACE_SAVING_CHANGES)),
            );
        }
        if self.closing {
            root = root.child(
                div()
                    .absolute()
                    .inset_0()
                    .occlude()
                    .track_focus(&self.modal_focus)
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(rgba((ui::colors().bg << 8) | 0xee))
                    .map(|d| frame.rounded(d))
                    .child(tr(
                        i18n_key::WORKSPACE_CLOSING_CONNECTIONS_AND_SAVING_SETTINGS,
                    )),
            );
        }
        if let Some(status) = self.recovery_status() {
            root = root.child(
                div()
                    .absolute()
                    .occlude()
                    .capture_any_mouse_down(
                        cx.listener(|this, _, _, cx| this.release_rdp_control(cx)),
                    )
                    .bottom(px(ui::STATUS_HEIGHT + 8.))
                    .right(px(12.))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .bg(rgb(ui::colors().raised))
                    .border_1()
                    .border_color(rgb(ui::colors().border))
                    .child(status)
                    .child(
                        ui::text_button(
                            "cancel-workspace-recovery",
                            i18n_key::CONNECTIONS_STOP,
                            ui::ButtonStyle::Neutral,
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.cancel_workspace_recovery(cx))),
                    ),
            );
        }
        if self.palette.open && !self.authentication.read(cx).is_active() {
            root = root.child(self.render_palette(window, cx));
        }
        if self.authentication.read(cx).is_active() {
            root = root.child(self.authentication.clone());
        }
        frame.wrap(root, window)
    }
}
