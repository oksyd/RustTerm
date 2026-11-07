use super::{colors, metrics};
use gpui::{
    Bounds, Div, ElementId, KeyDownEvent, Pixels, Point, Size, Stateful, div, point, prelude::*,
    px, rgb,
};

pub const PADDING: f32 = 5.;
pub const GAP: f32 = 4.;

/// Clamp both size and origin; even a narrow split cannot push a popup outside it.
pub fn fit(
    anchor: Point<Pixels>,
    requested: Size<Pixels>,
    limits: Bounds<Pixels>,
) -> Bounds<Pixels> {
    let size = gpui::size(
        requested.width.min(limits.size.width).max(px(0.)),
        requested.height.min(limits.size.height).max(px(0.)),
    );
    Bounds::new(
        point(
            anchor.x.clamp(
                limits.left(),
                (limits.right() - size.width).max(limits.left()),
            ),
            anchor.y.clamp(
                limits.top(),
                (limits.bottom() - size.height).max(limits.top()),
            ),
        ),
        size,
    )
}

pub fn surface(id: impl Into<ElementId>) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_col()
        .min_w_0()
        .py(px(PADDING - 1.))
        .bg(rgb(colors().raised))
        .text_color(rgb(colors().text))
        .text_size(px(metrics::BODY))
        .border_1()
        .border_color(rgb(colors().border))
        .rounded(px(metrics::POPUP_RADIUS))
        .shadow_md()
        .occlude()
        .on_any_mouse_down(|_, window, cx| {
            window.prevent_default();
            cx.stop_propagation();
        })
        .on_mouse_up(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_mouse_up(gpui::MouseButton::Right, |_, _, cx| cx.stop_propagation())
        .on_mouse_up(gpui::MouseButton::Middle, |_, _, cx| cx.stop_propagation())
        .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
}

pub fn row(id: impl Into<ElementId>, enabled: bool, selected: bool) -> Stateful<Div> {
    div()
        .id(id)
        .h(px(metrics::ROW))
        .flex_shrink_0()
        .px(px(metrics::INSET))
        .flex()
        .items_center()
        .gap(px(metrics::GAP))
        .text_size(px(metrics::BODY))
        .text_color(rgb(if enabled {
            colors().text
        } else {
            colors().faint
        }))
        .when(selected && enabled, |d| d.bg(rgb(colors().selected)))
        .when(enabled, |d| {
            d.cursor_pointer().hover(|s| s.bg(rgb(colors().selected)))
        })
}

/// Shared navigation for menu variants. Disabled entries never receive activation.
pub fn next_index(event: &KeyDownEvent, current: usize, enabled: &[bool]) -> Option<usize> {
    let indices: Vec<_> = enabled
        .iter()
        .enumerate()
        .filter_map(|(i, enabled)| enabled.then_some(i))
        .collect();
    match event.keystroke.key.as_str() {
        "home" => indices.first().copied(),
        "end" => indices.last().copied(),
        "up" | "tab" if event.keystroke.key == "up" || event.keystroke.modifiers.shift => indices
            .iter()
            .rev()
            .find(|i| **i < current)
            .or(indices.last())
            .copied(),
        "down" | "tab" => indices
            .iter()
            .find(|i| **i > current)
            .or(indices.first())
            .copied(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(value: &str) -> KeyDownEvent {
        KeyDownEvent {
            keystroke: gpui::Keystroke::parse(value).unwrap(),
            is_held: false,
        }
    }

    #[test]
    fn keyboard_navigation_skips_disabled_entries_in_both_directions() {
        let enabled = [false, true, false, true, false];
        for (key_name, current, expected) in [
            ("down", 1, 3),
            ("down", 3, 1),
            ("up", 1, 3),
            ("shift-tab", 3, 1),
            ("tab", 2, 3),
            ("home", 3, 1),
            ("end", 1, 3),
        ] {
            assert_eq!(
                next_index(&key(key_name), current, &enabled),
                Some(expected)
            );
        }
        assert_eq!(next_index(&key("down"), 0, &[false; 3]), None);
        assert_eq!(next_index(&key("home"), 0, &[]), None);
        assert_eq!(next_index(&key("enter"), 1, &enabled), None);
    }

    #[test]
    fn popups_keep_requested_size_but_shift_away_from_edges() {
        let limits = Bounds::new(point(px(40.), px(80.)), gpui::size(px(300.), px(200.)));
        let requested = gpui::size(px(100.), px(70.));
        let fitted = fit(point(px(320.), px(260.)), requested, limits);
        assert_eq!(fitted.size, requested);
        assert_eq!(fitted.origin, point(px(240.), px(210.)));
        assert_eq!(
            fit(point(px(50.), px(90.)), requested, limits).origin,
            point(px(50.), px(90.))
        );
    }
    #[test]
    fn oversized_popups_and_anchors_fit_small_panes() {
        let limits = Bounds::new(point(px(40.), px(80.)), gpui::size(px(120.), px(75.)));
        for anchor in [point(px(-20.), px(0.)), point(px(900.), px(600.))] {
            assert_eq!(fit(anchor, gpui::size(px(300.), px(200.)), limits), limits);
        }
    }
}
