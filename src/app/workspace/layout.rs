use super::{DragTarget, Modal, Tab, Workspace};
use crate::app::layout::{Axis, PaneNode, WorkspaceLayout};
use crate::app::pane::Pane;
use crate::app::ui;
use crate::app::window_frame::WindowFrame;
use gpui::prelude::*;
use gpui::{
    AnyElement, App, Context, CursorStyle, KeyDownEvent, MouseButton, MouseMoveEvent, Window,
    canvas, div, px, relative, rgb,
};

impl Workspace {
    pub(super) fn rdp_captures_keyboard(&self, cx: &App) -> bool {
        matches!(
            self.panes.get(&self.focused),
            Some(Pane::Rdp(view)) if view.read(cx).captures_keyboard()
        )
    }

    pub(super) fn release_rdp_control(&self, cx: &mut Context<Self>) {
        for pane in self.panes.values() {
            if let Pane::Rdp(view) = pane
                && view.read(cx).captures_keyboard()
            {
                view.update(cx, |view, cx| view.release_control(cx));
            }
        }
    }

    pub(super) fn remember_popup_focus(&mut self, window: &Window, cx: &App) {
        if self.popup_return_focus.is_none() {
            self.popup_return_focus = window.focused(cx).map(|focus| focus.downgrade());
        }
    }
    pub(super) fn restore_popup_focus(&mut self, window: &mut Window, cx: &App) {
        let previous = self
            .popup_return_focus
            .take()
            .and_then(|focus| focus.upgrade());
        if self.lock_screen.locked {
            self.focus_lock_password(window, cx);
            return;
        }
        if !self.utility_overlay
            && let Some(previous) = previous
            && self.workspace_focus.contains(&previous, window)
        {
            previous.focus(window);
        } else {
            self.restore_focus(window, cx);
        }
    }
    pub(super) fn restore_focus(&self, window: &mut Window, cx: &App) {
        if self.lock_screen.locked {
            self.focus_lock_password(window, cx);
            return;
        }
        if self.authentication.read(cx).is_active() {
            self.authentication.read(cx).focus(window, cx);
            return;
        }
        if let Some(modal) = &self.modal {
            match modal {
                Modal::Settings(form) => form.read(cx).focus(window, cx),
                _ => {
                    if let Some(input) = self.modal_inputs(cx).first() {
                        input.read(cx).focus(window);
                    } else {
                        self.modal_focus.focus(window);
                    }
                }
            }
            return;
        }
        if self.palette.open {
            self.focus_palette(window, cx);
            return;
        }
        if self.sidebar_overlay_open {
            self.sidebar_focus.focus(window);
            return;
        }
        if self.utility_overlay {
            if let Some(commands) = self.active_commands() {
                commands.read(cx).focus_handle().focus(window);
                return;
            }
            if let Some(resources) = self.active_resources() {
                resources.read(cx).focus_handle().focus(window);
                return;
            }
            if let Some(files) = self.active_files() {
                files.read(cx).focus(window);
                return;
            }
        }
        if let Some(p) = self.panes.get(&self.focused) {
            p.focus_handle(cx).focus(window);
        }
    }
    pub(super) fn visible_ids(&self) -> Vec<usize> {
        if let Some(id) = self.zoomed {
            return vec![id];
        }
        if self.compact_panes {
            return vec![self.focused];
        }
        self.tabs
            .get(self.active)
            .map(|t| t.layout.leaves())
            .unwrap_or_default()
    }
    pub(super) fn content_minimum(&self, cx: &App) -> (f32, f32) {
        if let Some(id) = self.zoomed {
            return self.panes[&id].minimum_size(cx);
        }
        self.tabs
            .get(self.active)
            .map(|tab| {
                tab.layout
                    .minimum_size(&|id| self.panes[&id].minimum_size(cx))
            })
            .unwrap_or((360., 200.))
    }
    pub(super) fn fitted_layout(&self, tab: &Tab, cx: &App) -> PaneNode {
        let mut node = tab.layout.clone();
        let preferred = node.minimum_size(&|id| self.panes[&id].preferred_size(cx));
        let full_views = self.terminal_size.0 >= preferred.0 && self.terminal_size.1 >= preferred.1;
        node.fit(self.terminal_size, &|id| {
            if full_views {
                self.panes[&id].preferred_size(cx)
            } else {
                self.panes[&id].minimum_size(cx)
            }
        });
        node
    }
    pub(super) fn preferred_content_minimum(&self, cx: &App) -> (f32, f32) {
        if let Some(id) = self.zoomed {
            return self.panes[&id].preferred_size(cx);
        }
        self.tabs
            .get(self.active)
            .map(|tab| {
                tab.layout
                    .minimum_size(&|id| self.panes[&id].preferred_size(cx))
            })
            .unwrap_or((360., 200.))
    }
    pub(super) fn sync_visibility(&self, cx: &mut Context<Self>) {
        let ids = self.visible_ids();
        for (id, pane) in &self.panes {
            pane.set_visible(
                !self.lock_screen.locked && !self.utility_overlay && ids.contains(id),
                cx,
            );
        }
    }
    pub(super) fn focus_pane(&mut self, id: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self
            .tabs
            .iter()
            .position(|tab| tab.layout.leaves().contains(&id))
        else {
            return;
        };
        if self.focused != id {
            self.release_rdp_control(cx);
        }
        if self.zoomed.is_some() {
            self.zoomed = Some(id);
        }
        self.focused = id;
        self.tabs[tab].focused = id;
        self.sidebar_overlay_open = false;
        self.set_active(tab, window, cx);
    }
    pub(super) fn set_active(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let requested = self
            .tabs
            .get(index.min(self.tabs.len().saturating_sub(1)))
            .map(|tab| tab.id);
        let reveal_group = requested != self.tabs.get(self.active).map(|tab| tab.id)
            || self
                .tabs
                .get(index)
                .is_some_and(|tab| !tab.layout.leaves().contains(&self.focused));
        self.regroup_tabs();
        let index = requested
            .and_then(|id| self.tabs.iter().position(|tab| tab.id == id))
            .unwrap_or(0);
        if self.active != index.min(self.tabs.len().saturating_sub(1)) {
            self.release_rdp_control(cx);
        }
        if self.lock_screen.locked {
            self.active = index.min(self.tabs.len().saturating_sub(1));
            self.sync_visibility(cx);
            return;
        }
        if self.tabs.is_empty() {
            self.zoomed = None;
            self.utility_overlay = false;
            window.set_window_title("RustTerm");
            cx.notify();
            return;
        }
        self.active = index.min(self.tabs.len() - 1);
        if reveal_group {
            self.expand_active_tab_group();
        }
        self.scroll_active_tab_into_view();
        let tab = &self.tabs[self.active];
        window.set_window_title(&format!("{} — RustTerm", self.workspace_tab_title(tab)));
        let ids = tab.layout.leaves();
        if self.zoomed.is_some_and(|id| !ids.contains(&id)) {
            self.zoomed = None;
        }
        self.focused = if ids.contains(&tab.focused) {
            tab.focused
        } else {
            ids[0]
        };
        self.tabs[self.active].focused = self.focused;
        self.sync_resources(window, cx);
        self.utility_overlay = self.utility_is_open()
            && !WorkspaceLayout::new(
                f32::from(
                    WindowFrame::new(window)
                        .content_size(window.viewport_size())
                        .width,
                ),
                &self.config.settings,
                self.sidebar_overlay_open,
                self.preferred_content_minimum(cx).0,
                self.utility_is_open(),
            )
            .files_docked;
        self.sync_visibility(cx);
        self.restore_focus(window, cx);
        cx.notify();
    }
    pub(super) fn input_composing(&self, window: &Window, cx: &App) -> bool {
        self.modal_composing(cx)
            || self.session_filter.read(cx).is_composing()
            || self
                .panes
                .get(&self.focused)
                .is_some_and(|pane| pane.input_composing(cx))
            || self.active_files().is_some_and(|files| {
                files.read(cx).owns_focus(window, cx) && files.read(cx).input_composing(cx)
            })
    }
    pub(super) fn toggle_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal.is_some()
            || self.input_composing(window, cx)
            || self.window_state.prompts.is_busy()
        {
            return;
        }
        let layout = WorkspaceLayout::new(
            f32::from(
                WindowFrame::new(window)
                    .content_size(window.viewport_size())
                    .width,
            ),
            &self.config.settings,
            self.sidebar_overlay_open,
            self.preferred_content_minimum(cx).0,
            self.utility_is_open(),
        );
        if !layout.sidebar_fits {
            self.sidebar_overlay_open = !self.sidebar_overlay_open;
            if self.sidebar_overlay_open {
                self.sidebar_focus.focus(window);
            } else {
                self.restore_focus(window, cx);
            }
        } else {
            self.config.settings.sidebar_visible = !self.config.settings.sidebar_visible;
            if self.config.settings.sidebar_visible {
                self.sidebar_focus.focus(window);
            } else {
                self.restore_focus(window, cx);
            }
            self.save(cx);
        }
        cx.notify();
    }
    pub(super) fn toggle_zoom(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal.is_some()
            || self.input_composing(window, cx)
            || self.window_state.prompts.is_busy()
        {
            return;
        }
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        if self.zoomed.take().is_none() && tab.layout.leaves().len() > 1 {
            self.zoomed = Some(self.focused);
        }
        self.sync_visibility(cx);
        self.restore_focus(window, cx);
        cx.notify();
    }
    pub(super) fn can_split_target(&self, target: usize, axis: Axis, cx: &App) -> bool {
        let Some(pane) = self.panes.get(&target) else {
            return false;
        };
        if self.zoomed.is_none() && !self.compact_panes {
            return pane.can_split(axis, cx);
        }
        let Some(size) = self.tabs.get(self.active).and_then(|tab| {
            self.fitted_layout(tab, cx)
                .pane_size(target, self.terminal_size)
        }) else {
            return false;
        };
        let minimum = pane.minimum_size(cx);
        match axis {
            Axis::Horizontal => size.0 >= 2. * minimum.0 + 5. && size.1 >= minimum.1,
            Axis::Vertical => size.1 >= 2. * minimum.1 + 5. && size.0 >= minimum.0,
        }
    }
    /// Activation reuses an existing pane; explicit new-tab/split commands always
    /// create another session. Prefer the current context in a stable order.
    pub(super) fn drag_resize(
        &mut self,
        event: &MouseMoveEvent,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let bounds = WindowFrame::new(window).content_bounds(window.viewport_size());
        match self.dragging {
            Some(DragTarget::Sidebar) => {
                self.config.settings.sidebar_width =
                    f32::from(event.position.x - bounds.left()).clamp(180., 400.);
                cx.notify();
                return;
            }
            Some(DragTarget::Files) => {
                self.config.settings.files_width =
                    f32::from(bounds.right() - event.position.x).clamp(360., 800.);
                cx.notify();
                return;
            }
            _ => {}
        }
        if let Some(DragTarget::Split(id)) = self.dragging
            && let Some((axis, bounds)) = self.split_bounds.get(&id)
        {
            let ratio = match axis {
                Axis::Horizontal => {
                    f32::from(event.position.x - bounds.left()) / f32::from(bounds.size.width)
                }
                Axis::Vertical => {
                    f32::from(event.position.y - bounds.top()) / f32::from(bounds.size.height)
                }
            };
            let leaf = |id| self.panes[&id].minimum_size(cx);
            if let Some(tab) = self.tabs.get_mut(self.active) {
                let extent = f32::from(match axis {
                    Axis::Horizontal => bounds.size.width,
                    Axis::Vertical => bounds.size.height,
                });
                tab.layout.set_ratio(id, ratio, extent, &leaf);
            }
            cx.notify();
        }
        if let Some(Pane::Terminal(p)) = self.panes.get(&self.focused) {
            p.update(cx, |p, cx| p.drag_selection(event.position, false, cx));
        }
    }
    pub(super) fn panel_divider(
        &self,
        id: &'static str,
        target: DragTarget,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .id(id)
            .w(px(5.))
            .h_full()
            .flex_shrink_0()
            .bg(rgb(ui::colors().bg))
            .cursor(CursorStyle::ResizeLeftRight)
            .hover(|style| style.bg(rgb(ui::colors().accent)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    this.dragging = Some(target);
                    cx.stop_propagation();
                }),
            )
            .into_any_element()
    }
    pub(super) fn render_pane_switcher(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut strip = div()
            .id("compact-pane-switcher")
            .track_focus(&self.pane_switcher_focus)
            .h(px(ui::TITLE_HEIGHT))
            .flex_shrink_0()
            .flex()
            .items_center()
            .overflow_x_scroll()
            .map(ui::preserve_scroll_axes)
            .bg(rgb(ui::colors().surface))
            .border_b_1()
            .border_color(rgb(ui::colors().border));
        if let Some(tab) = self.tabs.get(self.active) {
            for id in tab.layout.leaves() {
                let profile = &self.pane_profiles[&id];
                let glyph = ui::Icon::for_connection(&profile.connection);
                strip = strip.child(
                    div()
                        .id(("compact-pane", id))
                        .tab_index(0)
                        .h_full()
                        .max_w(px(200.))
                        .flex_shrink_0()
                        .px_3()
                        .flex()
                        .gap_2()
                        .items_center()
                        .cursor_pointer()
                        .text_size(px(12.))
                        .text_color(rgb(ui::colors().muted))
                        .when(id == self.focused, |d| {
                            d.bg(rgb(ui::colors().selected))
                                .text_color(rgb(ui::colors().accent))
                        })
                        .focus(|d| d.bg(rgb(ui::colors().selected)))
                        .hover(|d| d.bg(rgb(ui::colors().hover)))
                        .child(ui::icon(glyph))
                        .child(
                            div()
                                .text_ellipsis()
                                .child(self.pane_display_name(id).unwrap_or_default()),
                        )
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.focus_pane(id, window, cx);
                        }))
                        .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                            if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                                this.focus_pane(id, window, cx);
                                window.prevent_default();
                                cx.stop_propagation();
                            }
                        })),
                );
            }
        }
        ui::scrollable(strip, None).into_any_element()
    }
    pub(super) fn render_node(
        &self,
        node: &PaneNode,
        size: (f32, f32),
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match node {
            PaneNode::Leaf(id) => div()
                .size_full()
                .min_w_0()
                .min_h_0()
                .child(self.panes[id].element(size, cx))
                .into_any_element(),
            PaneNode::Split {
                id,
                axis,
                ratio,
                first,
                second,
            } => {
                let id = *id;
                let axis = *axis;
                let weak = cx.weak_entity();
                let (first_size, second_size) = match axis {
                    Axis::Horizontal => (
                        (size.0 * ratio, size.1),
                        ((size.0 * (1. - ratio) - 5.).max(0.), size.1),
                    ),
                    Axis::Vertical => (
                        (size.0, size.1 * ratio),
                        (size.0, (size.1 * (1. - ratio) - 5.).max(0.)),
                    ),
                };
                let first = self.render_node(first, first_size, cx);
                let second = self.render_node(second, second_size, cx);
                div()
                    .size_full()
                    .min_w_0()
                    .min_h_0()
                    .relative()
                    .flex()
                    .when(axis == Axis::Vertical, |d| d.flex_col())
                    .child(
                        canvas(
                            move |bounds, _, cx| {
                                let _ = weak.update(cx, |this, _| {
                                    this.split_bounds.insert(id, (axis, bounds));
                                });
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .size_full(),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .min_h_0()
                            .flex_shrink_0()
                            .when(axis == Axis::Horizontal, |d| d.w(relative(*ratio)).h_full())
                            .when(axis == Axis::Vertical, |d| d.h(relative(*ratio)).w_full())
                            .child(first),
                    )
                    .child(
                        div()
                            .id(("divider", id))
                            .flex_shrink_0()
                            .bg(rgb(ui::colors().bg))
                            .hover(|s| s.bg(rgb(ui::colors().accent)))
                            .when(axis == Axis::Horizontal, |d| {
                                d.w(px(5.)).h_full().cursor(CursorStyle::ResizeLeftRight)
                            })
                            .when(axis == Axis::Vertical, |d| {
                                d.h(px(5.)).w_full().cursor(CursorStyle::ResizeUpDown)
                            })
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, _, cx| {
                                    this.dragging = Some(DragTarget::Split(id));
                                    cx.stop_propagation();
                                }),
                            ),
                    )
                    .child(div().flex_1().min_w_0().min_h_0().child(second))
                    .into_any_element()
            }
        }
    }
}
/// Start near an even split, allocating enough room for each concrete view.
pub(super) fn split_ratio(
    axis: Axis,
    size: (f32, f32),
    first: (f32, f32),
    second: (f32, f32),
) -> Option<f32> {
    let (extent, cross, first, second, cross_min) = match axis {
        Axis::Horizontal => (size.0, size.1, first.0, second.0, first.1.max(second.1)),
        Axis::Vertical => (size.1, size.0, first.1, second.1, first.0.max(second.0)),
    };
    (extent >= first + second + 5. && cross >= cross_min).then(|| {
        // Clamp in pixels before normalizing: subtracting two normalized
        // fractions can invert the bounds at an exact minimum-size fit.
        (extent * 0.5).clamp(first, (extent - second - 5.).max(first)) / extent
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mixed_split_reserves_the_minimum_for_each_view() {
        let terminal = (200., 100.);
        let redis = (460., 300.);
        let ratio = split_ratio(Axis::Horizontal, (700., 400.), terminal, redis).unwrap();
        assert!(700. * ratio >= terminal.0);
        assert!(700. * (1. - ratio) - 5. >= redis.0 - 0.001);
        assert!(split_ratio(Axis::Horizontal, (660., 400.), terminal, redis).is_none());
        assert!(split_ratio(Axis::Horizontal, (900., 280.), terminal, redis).is_none());
        assert!(split_ratio(Axis::Vertical, (480., 405.), terminal, redis).is_some());
    }
}
