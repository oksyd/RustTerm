use gpui::{
    AnyElement, App, AvailableSpace, Bounds, ContentMask, DispatchPhase, Element, ElementId,
    Entity, GlobalElementId, Hitbox, HitboxBehavior, InspectorElementId, IntoElement, LayoutId,
    ListState, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point,
    RenderOnce, ScrollHandle, ScrollWheelEvent, Subscription, Window, canvas, div, fill, point,
    prelude::*, px, rgb, size,
};
use std::rc::Rc;

pub const SCROLLBAR_WIDTH: f32 = 12.;
const THUMB_WIDTH: f32 = 6.;
const MIN_THUMB: f32 = 20.;
const INSET: f32 = 2.;
type PositionChange = dyn Fn(f32, &mut Window, &mut App);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollAxis {
    Horizontal,
    Vertical,
}

impl ScrollAxis {
    fn coordinate(self, point: Point<Pixels>) -> f32 {
        f32::from(match self {
            Self::Horizontal => point.x,
            Self::Vertical => point.y,
        })
    }

    fn extent(self, bounds: Bounds<Pixels>) -> f32 {
        f32::from(match self {
            Self::Horizontal => bounds.size.width,
            Self::Vertical => bounds.size.height,
        })
    }
}

/// All three values use the same units; offset increases from the top or left.
#[derive(Clone, Copy, Debug, Default)]
pub struct ScrollMetrics {
    pub viewport: f32,
    pub content: f32,
    pub offset: f32,
}

#[derive(Clone)]
enum Source {
    Handle(ScrollHandle),
    List(ListState),
    Custom {
        read: Rc<dyn Fn(&App) -> ScrollMetrics>,
        change: Rc<PositionChange>,
    },
}

impl Source {
    fn bounds(&self) -> Option<Bounds<Pixels>> {
        match self {
            Self::Handle(handle) => Some(handle.bounds()),
            Self::List(list) => Some(list.viewport_bounds()),
            Self::Custom { .. } => None,
        }
    }

    fn metrics(&self, axis: ScrollAxis, cx: &App) -> ScrollMetrics {
        match self {
            Self::Handle(handle) => {
                let viewport = axis.extent(handle.bounds());
                let maximum = handle.max_offset();
                let maximum = match axis {
                    ScrollAxis::Horizontal => maximum.width,
                    ScrollAxis::Vertical => maximum.height,
                };
                ScrollMetrics {
                    viewport,
                    content: viewport + f32::from(maximum),
                    offset: -axis.coordinate(handle.offset()),
                }
            }
            Self::List(list) => {
                let viewport = f32::from(list.viewport_bounds().size.height);
                ScrollMetrics {
                    viewport,
                    content: viewport + f32::from(list.max_offset_for_scrollbar().height),
                    offset: -f32::from(list.scroll_px_offset_for_scrollbar().y),
                }
            }
            Self::Custom { read, .. } => read(cx),
        }
    }

    fn change(&self, axis: ScrollAxis, offset: f32, window: &mut Window, cx: &mut App) {
        match self {
            Self::Handle(handle) => {
                let mut point = handle.offset();
                match axis {
                    ScrollAxis::Horizontal => point.x = px(-offset),
                    ScrollAxis::Vertical => point.y = px(-offset),
                }
                handle.set_offset(point);
            }
            Self::List(list) => {
                let mut point = list.scroll_px_offset_for_scrollbar();
                point.y = px(-offset);
                list.set_offset_from_scrollbar(point);
            }
            Self::Custom { change, .. } => change(offset, window, cx),
        }
    }

    fn begin_drag(&self) {
        if let Self::List(list) = self {
            list.scrollbar_drag_started();
        }
    }

    fn end_drag(&self) {
        if let Self::List(list) = self {
            list.scrollbar_drag_ended();
        }
    }
}

#[derive(IntoElement)]
pub struct Scrollbars {
    id: ElementId,
    sources: [Option<Source>; 2],
}

/// Place after the tracked content, inside a relative viewport.
pub fn scrollbars(
    id: impl Into<ElementId>,
    horizontal: Option<ScrollHandle>,
    vertical: Option<ScrollHandle>,
) -> Scrollbars {
    Scrollbars {
        id: id.into(),
        sources: [horizontal.map(Source::Handle), vertical.map(Source::Handle)],
    }
}

/// A scrollbar for content whose position is not backed by a GPUI scroll handle.
pub fn scrollbar(
    id: impl Into<ElementId>,
    axis: ScrollAxis,
    read: impl Fn(&App) -> ScrollMetrics + 'static,
    change: impl Fn(f32, &mut Window, &mut App) + 'static,
) -> Scrollbars {
    let mut sources = [None, None];
    sources[usize::from(axis == ScrollAxis::Vertical)] = Some(Source::Custom {
        read: Rc::new(read),
        change: Rc::new(change),
    });
    Scrollbars {
        id: id.into(),
        sources,
    }
}

pub fn list_scrollbar(id: impl Into<ElementId>, list: ListState) -> Scrollbars {
    Scrollbars {
        id: id.into(),
        sources: [None, Some(Source::List(list))],
    }
}

impl Scrollbars {
    /// Decorate a scrolling element without adding a child to its content layout.
    pub fn decorate(self, content: impl IntoElement) -> DecoratedScrollArea {
        DecoratedScrollArea {
            id: self.id.clone(),
            content: content.into_any_element(),
            overlay: Some(self.into_any_element()),
        }
    }
}

pub struct DecoratedScrollArea {
    id: ElementId,
    content: AnyElement,
    overlay: Option<AnyElement>,
}

impl IntoElement for DecoratedScrollArea {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for DecoratedScrollArea {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }
    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (self.content.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.content.prepaint(window, cx);
        let mut overlay = div()
            .relative()
            .w(bounds.size.width)
            .h(bounds.size.height)
            .child(self.overlay.take().unwrap())
            .into_any_element();
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            overlay.prepaint_as_root(
                bounds.origin,
                bounds.size.map(AvailableSpace::Definite),
                window,
                cx,
            );
        });
        self.overlay = Some(overlay);
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.content.paint(window, cx);
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            self.overlay.as_mut().unwrap().paint(window, cx)
        });
    }
}

#[derive(Clone, Copy, Debug)]
struct Thumb {
    start: f32,
    length: f32,
    travel: f32,
    maximum: f32,
}

impl Thumb {
    fn new(metrics: ScrollMetrics, track: f32) -> Option<Self> {
        let maximum = metrics.content - metrics.viewport;
        if !metrics.viewport.is_finite()
            || !metrics.content.is_finite()
            || !metrics.offset.is_finite()
            || !track.is_finite()
            || metrics.viewport <= 0.
            || maximum <= 0.
            || track <= 0.
        {
            return None;
        }
        let length =
            (track * metrics.viewport / metrics.content).clamp(MIN_THUMB.min(track), track);
        let travel = track - length;
        Some(Self {
            start: metrics.offset.clamp(0., maximum) / maximum * travel,
            length,
            travel,
            maximum,
        })
    }

    fn offset_for_start(self, start: f32) -> f32 {
        if self.travel <= 0. {
            0.
        } else {
            (start / self.travel).clamp(0., 1.) * self.maximum
        }
    }
}

fn track_bounds(
    axis: ScrollAxis,
    viewport: Bounds<Pixels>,
    outer: Bounds<Pixels>,
    perpendicular: bool,
) -> Bounds<Pixels> {
    let corner = px(if perpendicular { SCROLLBAR_WIDTH } else { 0. });
    match axis {
        ScrollAxis::Horizontal => Bounds::new(
            point(viewport.left(), outer.bottom() - px(SCROLLBAR_WIDTH)),
            size(
                (viewport.right().min(outer.right() - corner) - viewport.left()).max(px(0.)),
                px(SCROLLBAR_WIDTH),
            ),
        ),
        ScrollAxis::Vertical => Bounds::new(
            point(outer.right() - px(SCROLLBAR_WIDTH), viewport.top()),
            size(
                px(SCROLLBAR_WIDTH),
                (viewport.bottom().min(outer.bottom() - corner) - viewport.top()).max(px(0.)),
            ),
        ),
    }
}

#[derive(Clone)]
struct Bar {
    axis: ScrollAxis,
    source: Source,
    metrics: ScrollMetrics,
    track: Bounds<Pixels>,
    thumb: Thumb,
    viewport_pixels: f32,
    hitbox: Hitbox,
}

impl Bar {
    fn position(&self, position: Point<Pixels>) -> f32 {
        self.axis.coordinate(position) - self.axis.coordinate(self.track.origin) - INSET
    }

    fn thumb_bounds(&self) -> Bounds<Pixels> {
        let margin = (SCROLLBAR_WIDTH - THUMB_WIDTH) / 2.;
        match self.axis {
            ScrollAxis::Horizontal => Bounds::new(
                self.track.origin + point(px(INSET + self.thumb.start), px(margin)),
                size(px(self.thumb.length), px(THUMB_WIDTH)),
            ),
            ScrollAxis::Vertical => Bounds::new(
                self.track.origin + point(px(margin), px(INSET + self.thumb.start)),
                size(px(THUMB_WIDTH), px(self.thumb.length)),
            ),
        }
    }
}

struct Drag {
    axis: ScrollAxis,
    grab: f32,
    source: Source,
}

#[derive(Default)]
struct State {
    drag: Option<Drag>,
    pressed: bool,
    hover: Option<ScrollAxis>,
    _activation: Option<Subscription>,
}

impl State {
    fn release(&mut self) {
        if let Some(drag) = self.drag.take() {
            drag.source.end_drag();
        }
        self.pressed = false;
    }
}

impl Drop for State {
    fn drop(&mut self) {
        self.release();
    }
}

impl RenderOnce for Scrollbars {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let state = window.use_keyed_state(self.id, cx, |window, cx| State {
            drag: None,
            pressed: false,
            hover: None,
            _activation: Some(cx.observe_window_activation(
                window,
                |state: &mut State, window, cx| {
                    if !window.is_window_active() {
                        state.release();
                        cx.notify();
                    }
                },
            )),
        });
        let prepaint_state = state.clone();
        canvas(
            move |outer, window, cx| {
                let metrics: [Option<ScrollMetrics>; 2] = std::array::from_fn(|index| {
                    self.sources[index].as_ref().map(|source| {
                        source.metrics(
                            if index == 0 {
                                ScrollAxis::Horizontal
                            } else {
                                ScrollAxis::Vertical
                            },
                            cx,
                        )
                    })
                });
                let active = metrics.map(|metric| {
                    metric.is_some_and(|metric| {
                        metric.viewport > 0. && metric.content > metric.viewport
                    })
                });
                let mut bars = Vec::with_capacity(2);
                for (index, (source, metrics)) in self.sources.into_iter().zip(metrics).enumerate()
                {
                    let (Some(source), Some(metrics)) = (source, metrics) else {
                        continue;
                    };
                    let axis = if index == 0 {
                        ScrollAxis::Horizontal
                    } else {
                        ScrollAxis::Vertical
                    };
                    let viewport = source.bounds().unwrap_or(outer).intersect(&outer);
                    let track = track_bounds(axis, viewport, outer, active[1 - index]);
                    let Some(thumb) = Thumb::new(metrics, axis.extent(track) - INSET * 2.) else {
                        continue;
                    };
                    bars.push(Bar {
                        axis,
                        metrics,
                        viewport_pixels: axis.extent(source.bounds().unwrap_or(outer)),
                        source,
                        track,
                        thumb,
                        hitbox: window.insert_hitbox(track, HitboxBehavior::BlockMouseExceptScroll),
                    });
                }
                prepaint_state.update(cx, |state, _| {
                    if state
                        .drag
                        .as_ref()
                        .is_some_and(|drag| !bars.iter().any(|bar| bar.axis == drag.axis))
                    {
                        state.release();
                    }
                });
                bars
            },
            move |_, bars, window, cx| {
                for bar in &bars {
                    window.set_cursor_style(gpui::CursorStyle::Arrow, &bar.hitbox);
                    let current = state.read(cx);
                    let dragging = current
                        .drag
                        .as_ref()
                        .is_some_and(|drag| drag.axis == bar.axis);
                    let hovered = current.hover == Some(bar.axis);
                    window.paint_quad(fill(bar.track, rgb(super::colors().surface)));
                    window.paint_quad(
                        fill(
                            bar.thumb_bounds(),
                            rgb(if dragging {
                                super::colors().accent
                            } else if hovered {
                                super::colors().muted
                            } else {
                                super::colors().faint
                            }),
                        )
                        .corner_radii(px(3.)),
                    );
                }
                bind_mouse(bars, state, window);
            },
        )
        .absolute()
        .inset_0()
        .size_full()
    }
}

fn bind_mouse(bars: Vec<Bar>, state: Entity<State>, window: &mut Window) {
    let bars = Rc::new(bars);
    let down_bars = bars.clone();
    let down_state = state.clone();
    window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
        if phase != DispatchPhase::Capture {
            return;
        }
        let Some(bar) = down_bars.iter().find(|bar| bar.hitbox.is_hovered(window)) else {
            return;
        };
        window.prevent_default();
        cx.stop_propagation();
        if event.button != MouseButton::Left {
            return;
        }
        let position = bar.position(event.position);
        let on_thumb =
            position >= bar.thumb.start && position <= bar.thumb.start + bar.thumb.length;
        down_state.update(cx, |state, cx| {
            state.release();
            state.pressed = true;
            if on_thumb {
                bar.source.begin_drag();
                state.drag = Some(Drag {
                    axis: bar.axis,
                    grab: (position - bar.thumb.start) / bar.thumb.length,
                    source: bar.source.clone(),
                });
            }
            cx.notify();
        });
        if !on_thumb {
            let direction = if position < bar.thumb.start { -1. } else { 1. };
            let offset = (bar.metrics.offset + direction * bar.metrics.viewport * 0.9)
                .clamp(0., bar.thumb.maximum);
            bar.source.change(bar.axis, offset, window, cx);
        }
    });

    let move_bars = bars.clone();
    let move_state = state.clone();
    window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
        if phase != DispatchPhase::Capture {
            return;
        }
        let hover = move_bars
            .iter()
            .find(|bar| bar.hitbox.is_hovered(window))
            .map(|bar| bar.axis);
        let movement = move_state.update(cx, |state, cx| {
            if state.hover != hover {
                state.hover = hover;
                cx.notify();
            }
            if state.pressed
                && (event.pressed_button != Some(MouseButton::Left) || !window.is_window_active())
            {
                state.release();
                cx.notify();
            }
            let drag = state.drag.as_ref()?;
            let bar = move_bars.iter().find(|bar| bar.axis == drag.axis)?;
            let offset = bar
                .thumb
                .offset_for_start(bar.position(event.position) - drag.grab * bar.thumb.length);
            cx.notify();
            Some((bar.clone(), offset))
        });
        if let Some((bar, offset)) = movement {
            bar.source.change(bar.axis, offset, window, cx);
            window.prevent_default();
            cx.stop_propagation();
        } else if hover.is_some() || move_state.read(cx).pressed {
            cx.stop_propagation();
        }
    });

    let up_bars = bars.clone();
    let up_state = state.clone();
    window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx| {
        if phase != DispatchPhase::Capture {
            return;
        }
        if up_state.read(cx).pressed || up_bars.iter().any(|bar| bar.hitbox.is_hovered(window)) {
            if event.button == MouseButton::Left {
                up_state.update(cx, |state, cx| {
                    state.release();
                    cx.notify();
                });
            }
            window.prevent_default();
            cx.stop_propagation();
        }
    });

    window.on_mouse_event(move |event: &ScrollWheelEvent, phase, window, cx| {
        if phase != DispatchPhase::Capture {
            return;
        }
        let Some(hovered) = bars.iter().find(|bar| bar.hitbox.is_hovered(window)) else {
            return;
        };
        let delta = event.delta.pixel_delta(px(20.));
        let mut changed = false;
        for bar in bars.iter() {
            let delta = bar.axis.coordinate(delta);
            if bar.viewport_pixels <= 0. || delta == 0. {
                continue;
            }
            let metrics = bar.source.metrics(bar.axis, cx);
            let maximum = (metrics.content - metrics.viewport).max(0.);
            let offset = (metrics.offset - delta * metrics.viewport / bar.viewport_pixels)
                .clamp(0., maximum);
            if offset != metrics.offset {
                bar.source.change(bar.axis, offset, window, cx);
                changed = true;
            }
        }
        if changed {
            state.update(cx, |_, cx| cx.notify());
        }
        // Terminal/custom rails own their input even at the end of history.
        if changed || matches!(hovered.source, Source::Custom { .. }) {
            cx.stop_propagation();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_gutters_are_not_subtracted_twice_and_headers_stay_clear() {
        let outer = Bounds::new(point(px(10.), px(20.)), size(px(100.), px(200.)));
        let viewport = Bounds::new(point(px(10.), px(48.)), size(px(88.), px(160.)));
        let horizontal = track_bounds(ScrollAxis::Horizontal, viewport, outer, true);
        let vertical = track_bounds(ScrollAxis::Vertical, viewport, outer, true);
        assert_eq!(horizontal.size.width, px(88.));
        assert_eq!(horizontal.top(), px(208.));
        assert_eq!(vertical.top(), px(48.));
        assert_eq!(vertical.size.height, px(160.));
        assert_eq!(vertical.left(), px(98.));
        // Native scroll areas include the gutter in their viewport bounds.
        let native = track_bounds(ScrollAxis::Vertical, outer, outer, true);
        assert_eq!(native.size.height, px(188.));
    }

    #[test]
    fn scrollbar_thumb_tracks_the_full_scroll_range_with_a_minimum_target() {
        let metrics = ScrollMetrics {
            viewport: 100.,
            content: 100_000.,
            offset: 99_900.,
        };
        let thumb = Thumb::new(metrics, 200.).unwrap();
        assert_eq!(thumb.length, MIN_THUMB);
        assert_eq!(thumb.start, 180.);
        assert_eq!(thumb.offset_for_start(180.), 99_900.);
        assert_eq!(thumb.offset_for_start(-10.), 0.);
        assert_eq!(thumb.offset_for_start(1000.), 99_900.);
        let middle = Thumb::new(
            ScrollMetrics {
                offset: 49_950.,
                ..metrics
            },
            200.,
        )
        .unwrap();
        assert_eq!(middle.start, 90.);
        assert_eq!(middle.offset_for_start(middle.start), 49_950.);
    }

    #[test]
    fn scrollbar_geometry_handles_small_viewports_and_non_overflowing_content() {
        let metrics = ScrollMetrics {
            viewport: 10.,
            content: 100.,
            offset: 50.,
        };
        let thumb = Thumb::new(metrics, 8.).unwrap();
        assert_eq!(thumb.length, 8.);
        assert_eq!(thumb.offset_for_start(5.), 0.);
        assert!(
            Thumb::new(
                ScrollMetrics {
                    viewport: 100.,
                    content: 100.,
                    offset: 0.
                },
                80.
            )
            .is_none()
        );
        assert!(Thumb::new(ScrollMetrics::default(), 80.).is_none());
        assert!(Thumb::new(metrics, 0.).is_none());
    }
}
