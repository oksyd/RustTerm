//! One themed renderer for every existing window.prompt call on every platform.
use super::{ButtonStyle, colors, metrics, scrollable};
use gpui::{
    App, AppContext, Context, EventEmitter, FocusHandle, Focusable, KeyDownEvent, PromptButton,
    PromptHandle, PromptLevel, PromptResponse, Render, RenderablePromptHandle, ScrollHandle,
    Window, div, prelude::*, px, rgb, rgba,
};
use rustterm::i18n::{messages as key, tr};

pub fn install(cx: &mut App) {
    cx.set_prompt_builder(build);
}

fn build(
    level: PromptLevel,
    message: &str,
    detail: Option<&str>,
    actions: &[PromptButton],
    handle: PromptHandle,
    window: &mut Window,
    cx: &mut App,
) -> RenderablePromptHandle {
    let cancel = cancel_index(actions);
    let prompt = cx.new(|cx| Dialog {
        level,
        message: message.into(),
        detail: detail.map(str::to_owned),
        actions: actions.to_vec(),
        selected: cancel.unwrap_or(0),
        cancel,
        focus: cx.focus_handle(),
        scroll: ScrollHandle::new(),
    });
    handle.with_view(prompt, window, cx)
}

fn cancel_index(actions: &[PromptButton]) -> Option<usize> {
    actions
        .iter()
        .position(|action| {
            matches!(action, PromptButton::Cancel(_))
                || [
                    key::COMMON_CANCEL,
                    key::WORKSPACE_KEEP_EDITING,
                    key::WORKSPACE_GO_BACK,
                ]
                .iter()
                .any(|label| action.label().as_ref() == tr(label))
        })
        .or_else(|| (actions.len() == 1).then_some(0))
}

fn dangerous(label: &str) -> bool {
    [
        key::WORKSPACE_END_AND_CLOSE,
        key::WORKSPACE_QUIT,
        key::WORKSPACE_QUIT_ANYWAY,
        key::WORKSPACE_DISCARD_CHANGES,
        key::COMMON_DELETE,
    ]
    .iter()
    .any(|key| label == tr(key))
}

struct Dialog {
    level: PromptLevel,
    message: String,
    detail: Option<String>,
    actions: Vec<PromptButton>,
    selected: usize,
    cancel: Option<usize>,
    focus: FocusHandle,
    scroll: ScrollHandle,
}

impl Dialog {
    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // No keystroke from a confirmation can reach a terminal beneath it.
        window.prevent_default();
        cx.stop_propagation();
        let count = self.actions.len();
        if count == 0 {
            return;
        }
        match event.keystroke.key.as_str() {
            "escape" => {
                if let Some(index) = self.cancel {
                    cx.emit(PromptResponse(index));
                }
            }
            "enter" | "return" | "space" => cx.emit(PromptResponse(self.selected)),
            "left" | "up" | "tab"
                if event.keystroke.key != "tab" || event.keystroke.modifiers.shift =>
            {
                self.selected = (self.selected + count - 1) % count;
            }
            "right" | "down" | "tab" => self.selected = (self.selected + 1) % count,
            "home" => self.selected = 0,
            "end" => self.selected = count - 1,
            "pageup" | "pagedown" => {
                let mut offset = self.scroll.offset();
                let step = (f32::from(window.viewport_size().height) * 0.5).max(40.);
                offset.y += px(if event.keystroke.key == "pageup" {
                    step
                } else {
                    -step
                });
                self.scroll.set_offset(offset);
            }
            _ => {}
        }
        cx.notify();
    }
}

impl Render for Dialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let viewport = window.viewport_size();
        let width = (f32::from(viewport.width) - 32.).clamp(1., 540.);
        let height = (f32::from(viewport.height) - 32.).max(1.);
        let stacked = width < 420. || self.actions.len() > 3;
        let footer_height = if stacked {
            self.actions.len() as f32 * 40. + 32.
        } else {
            64.
        };
        let body = div()
            .id("prompt-body")
            .min_w_0()
            .min_h_0()
            .max_h(px((height - footer_height).max(40.)))
            .overflow_y_scroll()
            .map(super::preserve_scroll_axes)
            .track_scroll(&self.scroll)
            .p(px(20.))
            .flex()
            .flex_col()
            .gap(px(metrics::INSET))
            .child(
                div()
                    .min_w_0()
                    .w_full()
                    .whitespace_normal()
                    .text_size(px(metrics::HEADING))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(self.message.clone()),
            )
            .when_some(self.detail.clone(), |body, detail| {
                body.child(
                    div()
                        .min_w_0()
                        .w_full()
                        .whitespace_normal()
                        .text_color(rgb(colors().muted))
                        .child(detail),
                )
            });
        let mut actions = div()
            .flex()
            .gap(px(metrics::GAP))
            .p(px(metrics::SECTION))
            .flex_shrink_0()
            .border_t_1()
            .border_color(rgb(colors().border))
            .when(stacked, |d| d.flex_col())
            .when(!stacked, |d| d.justify_end());
        for (index, action) in self.actions.iter().enumerate() {
            let style = if Some(index) == self.cancel {
                ButtonStyle::Neutral
            } else if dangerous(action.label()) || self.level == PromptLevel::Critical {
                ButtonStyle::Danger
            } else {
                ButtonStyle::Primary
            };
            let (background, foreground) = match style {
                ButtonStyle::Neutral => (colors().hover, colors().text),
                ButtonStyle::Primary => (colors().accent, colors().on_accent),
                ButtonStyle::Danger => (colors().danger_bg, colors().danger),
            };
            actions = actions.child(
                div()
                    .id(("prompt-answer", index))
                    .min_h(px(32.))
                    .min_w(px(88.))
                    .max_w_full()
                    .px(px(metrics::INSET))
                    .py(px(6.))
                    .rounded(px(metrics::RADIUS))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .border_1()
                    .border_color(if self.selected == index {
                        rgb(colors().accent)
                    } else {
                        rgba(0)
                    })
                    .bg(rgb(background))
                    .text_color(rgb(foreground))
                    .whitespace_normal()
                    .hover(|s| s.border_color(rgb(colors().accent)))
                    .child(action.label().clone())
                    .on_click(cx.listener(move |_, _, window, cx| {
                        window.prevent_default();
                        cx.stop_propagation();
                        cx.emit(PromptResponse(index));
                    })),
            );
        }
        div()
            .id("application-prompt")
            .size_full()
            .occlude()
            .bg(rgba(0x00000070))
            .flex()
            .items_center()
            .justify_center()
            .p(px(16.))
            .text_size(px(metrics::BODY))
            .text_color(rgb(colors().text))
            .track_focus(&self.focus)
            .capture_key_down(cx.listener(Self::key_down))
            .capture_key_up(|_, window, cx| {
                window.prevent_default();
                cx.stop_propagation();
            })
            .on_any_mouse_down(|_, window, cx| {
                window.prevent_default();
                cx.stop_propagation();
            })
            .on_mouse_up(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_up(gpui::MouseButton::Right, |_, _, cx| cx.stop_propagation())
            .on_mouse_up(gpui::MouseButton::Middle, |_, _, cx| cx.stop_propagation())
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .child(
                super::dialog_surface()
                    .w(px(width))
                    .max_h(px(height))
                    .child(scrollable(body, Some(self.scroll.clone())))
                    .child(actions),
            )
    }
}
impl EventEmitter<PromptResponse> for Dialog {}
impl Focusable for Dialog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cancel_is_found_by_intent_even_when_not_the_first_answer() {
        let actions = [PromptButton::new("Save"), PromptButton::cancel("取消")];
        assert_eq!(cancel_index(&actions), Some(1));
        assert_eq!(cancel_index(&[PromptButton::ok("Close")]), Some(0));
        assert_eq!(cancel_index(&[]), None);
    }
}
