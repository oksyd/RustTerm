use super::ui;
use gpui::{
    AnyElement, Bounds, BoxShadow, Corners, CursorStyle, Decorations, Edges, MouseButton, Pixels,
    ResizeEdge, Size, Tiling, Window, div, point, prelude::*, px, rgb, rgba, size,
    transparent_black,
};

const SHADOW_INSET: f32 = 10.;
const CORNER_RADIUS: f32 = 8.;
const BORDER_WIDTH: f32 = 1.;

/// One geometry for the surface, content, overlays, and resize hit areas.
pub(super) struct WindowFrame {
    client: bool,
    tiling: Tiling,
    padding: Edges<Pixels>,
    border: Edges<Pixels>,
    pub corners: Corners<Pixels>,
}

impl WindowFrame {
    pub fn new(window: &Window) -> Self {
        Self::from_decorations(
            window.window_decorations(),
            window.is_maximized() || window.is_fullscreen(),
        )
    }

    fn from_decorations(decorations: Decorations, constrained: bool) -> Self {
        let client = matches!(decorations, Decorations::Client { .. });
        let tiling = match decorations {
            Decorations::Client { tiling } if !constrained => tiling,
            _ => Tiling::tiled(),
        };
        let edges = |width| Edges {
            top: px(if tiling.top { 0. } else { width }),
            bottom: px(if tiling.bottom { 0. } else { width }),
            left: px(if tiling.left { 0. } else { width }),
            right: px(if tiling.right { 0. } else { width }),
        };
        let radius = |tiled| {
            px(if tiled {
                0.
            } else {
                CORNER_RADIUS - BORDER_WIDTH
            })
        };
        Self {
            client,
            tiling,
            padding: edges(SHADOW_INSET),
            border: edges(BORDER_WIDTH),
            corners: Corners {
                top_left: radius(tiling.top || tiling.left),
                top_right: radius(tiling.top || tiling.right),
                bottom_left: radius(tiling.bottom || tiling.left),
                bottom_right: radius(tiling.bottom || tiling.right),
            },
        }
    }

    pub fn configure(&self, window: &mut Window) {
        // Keep the base inset stable across maximize/fullscreen transitions.
        // GPUI applies tiling when negotiating the visible compositor geometry.
        window.set_client_inset(px(if self.client { SHADOW_INSET } else { 0. }));
    }

    pub fn content_size(&self, viewport: Size<Pixels>) -> Size<Pixels> {
        size(
            (viewport.width
                - self.padding.left
                - self.padding.right
                - self.border.left
                - self.border.right)
                .max(px(0.)),
            (viewport.height
                - self.padding.top
                - self.padding.bottom
                - self.border.top
                - self.border.bottom)
                .max(px(0.)),
        )
    }

    pub fn content_bounds(&self, viewport: Size<Pixels>) -> Bounds<Pixels> {
        Bounds::new(
            point(
                self.padding.left + self.border.left,
                self.padding.top + self.border.top,
            ),
            self.content_size(viewport),
        )
    }

    pub fn rounded<T: Styled>(&self, element: T) -> T {
        element
            .rounded_tl(self.corners.top_left)
            .rounded_tr(self.corners.top_right)
            .rounded_bl(self.corners.bottom_left)
            .rounded_br(self.corners.bottom_right)
    }

    pub fn wrap(&self, content: impl IntoElement, window: &Window) -> AnyElement {
        let outer_radius = |inner: Pixels| {
            if inner > px(0.) {
                inner + px(BORDER_WIDTH)
            } else {
                px(0.)
            }
        };
        let surface = div()
            .relative()
            .size_full()
            .border_t(self.border.top)
            .border_b(self.border.bottom)
            .border_l(self.border.left)
            .border_r(self.border.right)
            .border_color(rgb(ui::colors().border))
            .rounded_tl(outer_radius(self.corners.top_left))
            .rounded_tr(outer_radius(self.corners.top_right))
            .rounded_bl(outer_radius(self.corners.bottom_left))
            .rounded_br(outer_radius(self.corners.bottom_right))
            .when(self.client && !self.tiling.is_tiled(), |d| {
                d.shadow(vec![BoxShadow {
                    color: rgba(if window.is_window_active() {
                        0x00000050
                    } else {
                        0x00000030
                    })
                    .into(),
                    offset: point(px(0.), px(1.)),
                    blur_radius: px(6.),
                    spread_radius: px(0.),
                }])
            })
            .child(content);
        div()
            .relative()
            .size_full()
            .bg(transparent_black())
            .pt(self.padding.top)
            .pb(self.padding.bottom)
            .pl(self.padding.left)
            .pr(self.padding.right)
            .child(surface)
            .children(self.resize_handles())
            .into_any_element()
    }

    fn resize_handles(&self) -> Vec<AnyElement> {
        if !self.client || self.tiling == Tiling::tiled() {
            return Vec::new();
        }
        let tiling = self.tiling;
        let mut handles = Vec::with_capacity(8);
        for (index, edge, allowed, cursor) in [
            (
                0usize,
                ResizeEdge::Top,
                !tiling.top,
                CursorStyle::ResizeUpDown,
            ),
            (
                1,
                ResizeEdge::Bottom,
                !tiling.bottom,
                CursorStyle::ResizeUpDown,
            ),
            (
                2,
                ResizeEdge::Left,
                !tiling.left,
                CursorStyle::ResizeLeftRight,
            ),
            (
                3,
                ResizeEdge::Right,
                !tiling.right,
                CursorStyle::ResizeLeftRight,
            ),
            (
                4,
                ResizeEdge::TopLeft,
                !tiling.top && !tiling.left,
                CursorStyle::ResizeUpLeftDownRight,
            ),
            (
                5,
                ResizeEdge::TopRight,
                !tiling.top && !tiling.right,
                CursorStyle::ResizeUpRightDownLeft,
            ),
            (
                6,
                ResizeEdge::BottomLeft,
                !tiling.bottom && !tiling.left,
                CursorStyle::ResizeUpRightDownLeft,
            ),
            (
                7,
                ResizeEdge::BottomRight,
                !tiling.bottom && !tiling.right,
                CursorStyle::ResizeUpLeftDownRight,
            ),
        ] {
            if !allowed {
                continue;
            }
            let edge_width = px(SHADOW_INSET + BORDER_WIDTH + 2.);
            let corner_width = px(SHADOW_INSET + CORNER_RADIUS);
            handles.push(
                div()
                    .id(("window-edge", index))
                    .absolute()
                    .occlude()
                    .cursor(cursor)
                    .map(|d| match edge {
                        ResizeEdge::Top => d.top_0().left_0().right_0().h(edge_width),
                        ResizeEdge::Bottom => d.bottom_0().left_0().right_0().h(edge_width),
                        ResizeEdge::Left => d.left_0().top_0().bottom_0().w(edge_width),
                        ResizeEdge::Right => d.right_0().top_0().bottom_0().w(edge_width),
                        ResizeEdge::TopLeft => d.top_0().left_0().size(corner_width),
                        ResizeEdge::TopRight => d.top_0().right_0().size(corner_width),
                        ResizeEdge::BottomLeft => d.bottom_0().left_0().size(corner_width),
                        ResizeEdge::BottomRight => d.bottom_0().right_0().size(corner_width),
                    })
                    .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                        window.start_window_resize(edge);
                        window.prevent_default();
                        cx.stop_propagation();
                    })
                    .into_any_element(),
            );
        }
        handles
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry_excludes_only_exposed_edges() {
        let viewport = size(px(800.), px(600.));
        let floating = WindowFrame::from_decorations(
            Decorations::Client {
                tiling: Tiling::default(),
            },
            false,
        );
        assert_eq!(floating.content_size(viewport), size(px(778.), px(578.)));
        assert_eq!(floating.corners.top_left, px(7.));
        let tiled = WindowFrame::from_decorations(
            Decorations::Client {
                tiling: Tiling {
                    left: true,
                    top: true,
                    bottom: true,
                    right: false,
                },
            },
            false,
        );
        assert_eq!(tiled.content_size(viewport), size(px(789.), px(600.)));
        assert_eq!(tiled.corners, Corners::default());
        let top = WindowFrame::from_decorations(
            Decorations::Client {
                tiling: Tiling {
                    top: true,
                    ..Tiling::default()
                },
            },
            false,
        );
        assert_eq!(top.corners.top_left, px(0.));
        assert_eq!(top.corners.top_right, px(0.));
        assert_eq!(top.corners.bottom_left, px(7.));
        assert_eq!(top.corners.bottom_right, px(7.));
    }

    #[test]
    fn maximized_fullscreen_and_server_frames_preserve_the_viewport() {
        let viewport = size(px(800.), px(600.));
        for (decorations, constrained) in [
            (
                Decorations::Client {
                    tiling: Tiling::default(),
                },
                true,
            ),
            (Decorations::Server, false),
        ] {
            let frame = WindowFrame::from_decorations(decorations, constrained);
            assert_eq!(frame.content_size(viewport), viewport);
            assert_eq!(frame.corners, Corners::default());
            assert!(frame.resize_handles().is_empty());
        }
    }
}
