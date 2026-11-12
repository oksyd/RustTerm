//! Shared navigation and page spacing for settings and connection editors.
use super::{Icon, colors, icon, preserve_scroll_axes, scrollable};
use gpui::{Div, ElementId, IntoElement, ScrollHandle, Stateful, div, prelude::*, px, rgb};

pub fn preferences_navigation(compact: bool) -> Div {
    div()
        .flex()
        .flex_shrink_0()
        .gap_1()
        .p_2()
        .bg(rgb(colors().bg))
        .when(compact, |nav| {
            nav.w_full().border_b_1().border_color(rgb(colors().border))
        })
        .when(!compact, |nav| {
            nav.w(px(156.))
                .flex_col()
                .border_r_1()
                .border_color(rgb(colors().border))
        })
}

pub fn preferences_item(
    id: impl Into<ElementId>,
    label: impl IntoElement,
    glyph: Icon,
    selected: bool,
    compact: bool,
) -> Stateful<Div> {
    div()
        .id(id)
        .tab_index(0)
        .tab_stop(selected)
        .flex()
        .items_center()
        .gap_2()
        .min_w_0()
        .h(px(38.))
        .px_2()
        .when(compact, |item| item.flex_1().justify_center())
        .rounded(px(4.))
        .border_1()
        .border_color(gpui::transparent_black())
        .cursor_pointer()
        .bg(rgb(if selected {
            colors().selected
        } else {
            colors().bg
        }))
        .text_color(rgb(if selected {
            colors().text
        } else {
            colors().muted
        }))
        .hover(|style| style.bg(rgb(colors().hover)))
        .focus(|style| style.border_color(rgb(colors().accent)))
        .child(icon(glyph).text_color(rgb(if selected {
            colors().accent
        } else {
            colors().muted
        })))
        .child(div().min_w_0().truncate().child(label))
}

pub fn preferences_layout(
    navigation: impl IntoElement,
    id: impl Into<ElementId>,
    title: impl IntoElement,
    body: impl IntoElement,
    scroll: ScrollHandle,
    compact: bool,
) -> Div {
    let content = div()
        .id(id)
        .flex_1()
        .min_h_0()
        .min_w_0()
        .overflow_y_scroll()
        .map(preserve_scroll_axes)
        .track_scroll(&scroll)
        .px(px(if compact { 16. } else { 24. }))
        .pb(px(24.))
        .child(body);
    div()
        .size_full()
        .min_w_0()
        .min_h_0()
        .flex()
        .when(compact, |layout| layout.flex_col())
        .text_size(px(12.))
        .text_color(rgb(colors().text))
        .child(navigation)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .min_h_0()
                .flex()
                .flex_col()
                .child(
                    div()
                        .flex_shrink_0()
                        .px(px(if compact { 16. } else { 24. }))
                        .pt(px(20.))
                        .pb(px(16.))
                        .text_size(px(18.))
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child(title),
                )
                .child(scrollable(content, Some(scroll))),
        )
}
