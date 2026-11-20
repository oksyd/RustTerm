use super::TerminalView;
use crate::app::{interaction::PromptPermit, text_input::TextInput, ui};
use gpui::{AnyElement, Context, Entity, KeyDownEvent, Window, div, prelude::*, px, rgb};
use rustterm::{
    config::Uuid,
    i18n::{messages as key, tr},
    session::MAX_INPUT_BYTES,
};

pub(super) struct PastePreview {
    pub input: Entity<TextInput>,
    pub permit: PromptPermit,
    pub run: Uuid,
    pub generation: usize,
    pub filtered: bool,
    pub _input_subscription: gpui::Subscription,
}

impl TerminalView {
    fn finish_paste(&mut self, confirm: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(preview) = self.paste_preview.as_ref() else {
            return;
        };
        if confirm && preview.input.read(cx).is_composing() {
            return;
        }
        let Some(preview) = self.paste_preview.take() else {
            return;
        };
        let text = preview.input.read(cx).value().to_owned();
        let valid = preview.permit.is_valid()
            && self.generation == preview.generation
            && self.visible
            && self.window_state.visibility.is_visible()
            && self
                .runs
                .get(self.viewed_run)
                .is_some_and(|run| run.id() == preview.run)
            && self.is_running();
        drop(preview);
        if self.visible && !self.window_state.is_locked() {
            self.focus.focus(window);
        }
        if valid
            && confirm
            && !text.is_empty()
            && text.len() <= MAX_INPUT_BYTES
            && super::clipboard::clean_paste(&text) == (text.clone(), false)
        {
            self.send_confirmed_paste(text, cx);
        }
        cx.notify();
    }

    fn paste_preview_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .paste_preview
            .as_ref()
            .is_some_and(|preview| preview.input.read(cx).is_composing())
        {
            return;
        }
        let key = &event.keystroke;
        if key.key == "escape"
            || (key.key == "enter"
                && key.modifiers.control
                && !key.modifiers.alt
                && !key.modifiers.platform)
        {
            self.finish_paste(key.key == "enter", window, cx);
            window.prevent_default();
            cx.stop_propagation();
        }
    }

    pub(super) fn render_paste_preview(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let preview = self.paste_preview.as_ref()?;
        let height = self
            .bounds
            .map_or(400., |bounds| f32::from(bounds.size.height))
            .max(80.);
        preview.input.update(cx, |input, cx| {
            input.set_multiline_height((height * 0.45).clamp(80., 340.), cx)
        });
        let target = self.target.as_ref().map_or_else(
            || self.display_name.clone(),
            |address| format!("{} · {address}", self.display_name),
        );
        let actions = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(8.))
            .child(
                ui::text_button(
                    "paste-trim-newline",
                    key::TERMINAL_REMOVE_TRAILING_NEWLINES,
                    ui::ButtonStyle::Neutral,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    if let Some(preview) = &this.paste_preview {
                        preview.input.update(cx, |input, cx| {
                            if !input.is_composing() {
                                let text = input.value().trim_end_matches('\n').to_owned();
                                input.set_value(text, cx);
                            }
                        });
                    }
                })),
            )
            .child(div().flex_1())
            .child(
                ui::text_button("paste-cancel", key::COMMON_CANCEL, ui::ButtonStyle::Neutral)
                    .on_click(
                        cx.listener(|this, _, window, cx| this.finish_paste(false, window, cx)),
                    ),
            )
            .child(
                ui::text_button("paste-confirm", key::COMMON_PASTE, ui::ButtonStyle::Primary)
                    .on_click(
                        cx.listener(|this, _, window, cx| this.finish_paste(true, window, cx)),
                    ),
            );
        let content = div().flex().flex_col().gap(px(10.)).flex_shrink_0()
            .child(div().text_size(px(14.)).child(rustterm::trf!(key::TERMINAL_PASTE_INTO_TARGET, target = target)))
            .child(div().text_size(px(12.)).text_color(rgb(ui::colors().muted))
                .child(tr(key::TERMINAL_NEWLINES_MAY_EXECUTE_COMMANDS_IMMEDIATELY_CHECK_THE_TARGET_AND_CONTENT)))
            .when(preview.filtered, |d| d.child(div().text_size(px(12.))
                .child(tr(key::TERMINAL_CONTROL_CHARACTERS_WERE_REMOVED))))
            .child(preview.input.clone())
            .child(actions);
        let card = div()
            .id(("paste-preview", self.index))
            .w_full()
            .max_w(px(640.))
            .min_w_0()
            .max_h(px((height - 24.).max(56.)))
            .overflow_y_scroll()
            .p(px(16.))
            .rounded(px(8.))
            .border_1()
            .border_color(rgb(ui::colors().border))
            .bg(rgb(ui::colors().raised))
            .shadow_lg()
            .child(content);
        Some(
            div()
                .absolute()
                .inset_0()
                .occlude()
                .flex()
                .items_center()
                .justify_center()
                .bg(gpui::rgba(0x00000080))
                .p(px(12.))
                .on_any_mouse_down(|_, _, cx| cx.stop_propagation())
                .on_mouse_move(|_, _, cx| cx.stop_propagation())
                .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                .capture_key_down(cx.listener(Self::paste_preview_key))
                .child(ui::scrollable(card, None))
                .into_any_element(),
        )
    }
}
