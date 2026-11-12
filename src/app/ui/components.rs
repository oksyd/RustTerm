use super::{Icon, colors, icon, metrics};
use gpui::{Div, IntoElement, div, prelude::*, px, rgb};

/// Every tool owns its actions; shared chrome owns spacing, hierarchy and contrast.
pub fn panel_header() -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(metrics::GAP))
        .h(px(super::PANE_HEADER_HEIGHT))
        .flex_shrink_0()
        .px(px(metrics::INSET))
        .border_b_1()
        .border_color(rgb(colors().border))
        .bg(rgb(colors().chrome))
        .text_color(rgb(colors().text))
        .text_size(px(metrics::BODY))
}

pub fn dialog_surface() -> Div {
    div()
        .flex()
        .flex_col()
        .min_w_0()
        .min_h_0()
        .overflow_hidden()
        .bg(rgb(colors().surface))
        .text_color(rgb(colors().text))
        .text_size(px(metrics::BODY))
        .border_1()
        .border_color(rgb(colors().border))
        .rounded(px(metrics::DIALOG_RADIUS))
        .shadow_lg()
}

pub fn toolbar() -> Div {
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap(px(metrics::GAP))
        .min_h(px(36.))
        .flex_shrink_0()
        .px(px(metrics::INSET))
        .py(px(4.))
        .text_size(px(metrics::BODY))
}

pub fn empty_state(glyph: Icon, message: impl IntoElement) -> Div {
    div()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(metrics::GAP))
        .p(px(metrics::SECTION))
        .min_w_0()
        .text_size(px(metrics::BODY))
        .text_color(rgb(colors().muted))
        .child(icon(glyph).size(px(24.)).text_color(rgb(colors().muted)))
        .child(
            div()
                .max_w(px(420.))
                .min_w_0()
                .whitespace_normal()
                .text_center()
                .child(message),
        )
}

pub fn feedback(message: impl IntoElement, error: bool) -> Div {
    div()
        .min_w_0()
        .flex_shrink_0()
        .px(px(metrics::INSET))
        .py(px(metrics::GAP))
        .rounded(px(metrics::RADIUS))
        .text_size(px(metrics::BODY))
        .whitespace_normal()
        .bg(rgb(if error {
            colors().danger_bg
        } else {
            colors().raised
        }))
        .text_color(rgb(if error {
            colors().danger
        } else {
            colors().muted
        }))
        .child(message)
}

/// Flex wrapping lets the *available field width* decide whether the control sits
/// beside or below its label, including forms inside split/overlay layouts.
pub fn form_row(label: impl IntoElement, control: impl IntoElement) -> Div {
    div()
        .flex()
        .flex_wrap()
        .items_start()
        .gap(px(metrics::GAP))
        .min_w_0()
        .child(
            div()
                .w(px(150.))
                .flex_shrink_0()
                .pt(px(7.))
                .text_size(px(metrics::BODY))
                .text_color(rgb(colors().muted))
                .whitespace_normal()
                .child(label),
        )
        .child(div().flex_1().min_w(px(280.)).max_w_full().child(control))
}

pub fn setting_row(
    label: impl IntoElement,
    description: Option<impl IntoElement>,
    control: impl IntoElement,
) -> Div {
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .gap(px(metrics::INSET))
        .min_w_0()
        .min_h(px(52.))
        .py(px(10.))
        .border_b_1()
        .border_color(rgb(colors().border))
        .child(
            div()
                .flex_1()
                .min_w(px(200.))
                .flex()
                .flex_col()
                .gap(px(4.))
                .text_size(px(metrics::BODY))
                .whitespace_normal()
                .child(label)
                .when_some(description, |d, text| {
                    d.child(
                        div()
                            .text_size(px(metrics::CAPTION))
                            .text_color(rgb(colors().muted))
                            .child(text),
                    )
                }),
        )
        .child(control)
}

pub fn toggle_row(
    id: &'static str,
    label: impl IntoElement,
    description: Option<impl IntoElement>,
    selected: bool,
) -> gpui::Stateful<Div> {
    setting_row(
        label,
        description,
        div()
            .w(px(34.))
            .h(px(20.))
            .flex()
            .items_center()
            .px(px(3.))
            .rounded(px(10.))
            .bg(rgb(if selected {
                colors().accent
            } else {
                colors().border
            }))
            .when(selected, |rail| rail.justify_end())
            .child(div().size(px(14.)).rounded(px(7.)).bg(rgb(if selected {
                colors().on_accent
            } else {
                colors().text
            }))),
    )
    .id(id)
    .cursor_pointer()
    .tab_index(0)
    .hover(|style| style.bg(rgb(colors().hover)))
    .focus(|style| style.border_color(rgb(colors().accent)))
}
