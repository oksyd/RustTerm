use super::TerminalView;
use crate::app::clipboard::{self, CopyTicket};
use crate::app::pane::PaneEvent;
use gpui::{AppContext, ClipboardItem, Context, Window};
use rustterm::i18n::messages as i18n_key;
use rustterm::session::{Command, MAX_INPUT_BYTES};

#[derive(Clone, Copy)]
enum SelectionBuffer {
    Clipboard,
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    Primary,
}

fn copy_reply(
    result: Result<Option<String>, ()>,
    block: bool,
    expected_selection: bool,
) -> Result<Option<String>, &'static str> {
    match result {
        Err(()) => Err(i18n_key::TERMINAL_COPY_INTERRUPTED),
        Ok(None) if block => {
            Err(i18n_key::TERMINAL_COMMAND_OUTPUT_IS_NO_LONGER_RETAINED_OR_EXCEEDS_64_KIB)
        }
        Ok(None) if expected_selection => Err(i18n_key::TERMINAL_COPY_SELECTION_UNAVAILABLE),
        Ok(text) => Ok(text.filter(|text| !text.is_empty())),
    }
}

impl TerminalView {
    pub fn copy_selection(&mut self, cx: &mut Context<Self>) {
        self.copy_to(SelectionBuffer::Clipboard, None, cx);
    }

    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    pub(super) fn copy_primary(&mut self, cx: &mut Context<Self>) {
        self.copy_to(SelectionBuffer::Primary, None, cx);
    }

    pub(super) fn copy_block(
        &mut self,
        id: u64,
        part: rustterm::terminal::BlockCopy,
        cx: &mut Context<Self>,
    ) {
        self.copy_to(SelectionBuffer::Clipboard, Some((id, part)), cx);
    }

    fn copy_to(
        &mut self,
        target: SelectionBuffer,
        block: Option<(u64, rustterm::terminal::BlockCopy)>,
        cx: &mut Context<Self>,
    ) {
        match target {
            SelectionBuffer::Clipboard => self.copy_task.take(),
            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
            SelectionBuffer::Primary => self.primary_copy_task.take(),
        };
        if self.chrome_blocked(cx) || !self.visible || self.show_final_alternate {
            return;
        }
        if !self.flush_motion(cx) {
            return;
        }
        let Some(run) = self.runs.get(self.viewed_run) else {
            return;
        };
        let run_id = run.id();
        let generation = self.generation;
        let expected_selection = self
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.has_selection || snapshot.selection_invalidated);
        let lock_epoch = self.window_state.prompts.lock_epoch();
        let ticket = match target {
            SelectionBuffer::Clipboard => Some(clipboard::request(cx)),
            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
            SelectionBuffer::Primary => None,
        };
        let receipt = match block.map_or_else(
            || run.copy_selection(),
            |(id, part)| run.copy_command_block(id, part),
        ) {
            Ok(receipt) => receipt,
            Err(error) => {
                if !matches!(target, SelectionBuffer::Clipboard) {
                    return;
                }
                self.error = Some(rustterm::i18n::Message::with_arg(
                    i18n_key::TERMINAL_COPY_FAILED_ERROR,
                    "error",
                    rustterm::i18n::Message::from_error(error),
                ));
                cx.emit(PaneEvent::Activity);
                cx.notify();
                return;
            }
        };
        if matches!(target, SelectionBuffer::Clipboard) && self.error.take().is_some() {
            cx.emit(PaneEvent::Activity);
            cx.notify();
        }
        let task = cx.spawn(async move |this, cx| {
            let result = receipt.await.map_err(|_| ());
            let _ = this.update(cx, |this, cx| {
                if this.generation == generation
                    && this.window_state.prompts.lock_epoch() == lock_epoch
                    && !this.window_state.is_locked()
                    && ticket.as_ref().is_none_or(CopyTicket::is_current)
                    && (matches!(target, SelectionBuffer::Clipboard)
                        || (this.visible
                            && this.window_state.visibility.is_visible()
                            && !this.chrome_blocked(cx)))
                    && !this.show_final_alternate
                    && this
                        .runs
                        .get(this.viewed_run)
                        .is_some_and(|run| run.id() == run_id && !run.is_evicted())
                {
                    let text = match copy_reply(result, block.is_some(), expected_selection) {
                        Ok(Some(text)) => text,
                        Ok(None) => return,
                        Err(message) => {
                            if !matches!(target, SelectionBuffer::Clipboard) {
                                return;
                            }
                            this.error = Some(message.into());
                            cx.emit(PaneEvent::Activity);
                            cx.notify();
                            return;
                        }
                    };
                    let item = ClipboardItem::new_string(text);
                    match target {
                        SelectionBuffer::Clipboard => crate::app::clipboard::write(cx, item),
                        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
                        SelectionBuffer::Primary => cx.write_to_primary(item),
                    }
                }
            });
        });
        match target {
            SelectionBuffer::Clipboard => self.copy_task = Some(task),
            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
            SelectionBuffer::Primary => self.primary_copy_task = Some(task),
        }
    }

    pub fn paste(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.accepts_text_input(window) {
            return;
        }
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        self.paste_text(text, window, cx);
    }

    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    pub(super) fn paste_primary(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.accepts_text_input(window) {
            return;
        }
        if let Some(text) = cx.read_from_primary().and_then(|item| item.text()) {
            self.paste_text(text, window, cx);
        }
    }

    pub(super) fn paste_text(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        if text.len() > MAX_INPUT_BYTES {
            self.error =
                Some(i18n_key::TERMINAL_PASTE_NOT_SENT_A_SINGLE_PASTE_IS_LIMITED_TO_64.into());
            cx.emit(PaneEvent::Activity);
            cx.notify();
            return;
        }
        let (text, filtered) = clean_paste(&text);
        if text.is_empty() {
            return;
        }
        if !filtered
            && (!text.contains('\n')
                || (!self.settings.confirm_multiline_paste && !self.production_protected()))
        {
            self.send(Command::Paste(text), cx);
            return;
        }
        let Some(permit) = self.window_state.prompts.acquire() else {
            return;
        };
        let input = cx.new(|cx| {
            let mut input = crate::app::text_input::TextInput::new("", false, cx);
            input.set_literal(true, cx);
            input.set_show_line_endings(true, cx);
            input.set_multiline_height(
                (f32::from(window.viewport_size().height) * 0.45).clamp(100., 340.),
                cx,
            );
            input.set_value(&text, cx);
            input
        });
        let subscription = cx.subscribe(&input, |_, input, event, cx| {
            use crate::app::text_input::InputEvent;
            match event {
                InputEvent::Rejected(message) => input.update(cx, |input, cx| {
                    input.set_validation_error((*message).into(), cx)
                }),
                InputEvent::Changed => {
                    input.update(cx, |input, cx| input.clear_validation_error(cx))
                }
                InputEvent::Submit => {}
            }
        });
        input.read(cx).focus(window);
        self.paste_preview = Some(super::paste_preview::PastePreview {
            input,
            permit,
            generation: self.generation,
            run: self.runs[self.viewed_run].id(),
            filtered,
            _input_subscription: subscription,
        });
        cx.notify();
    }
}

pub(super) fn clean_paste(text: &str) -> (String, bool) {
    let mut clean = String::with_capacity(text.len());
    let mut filtered = false;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                clean.push('\n');
            }
            '\n' | '\t' => clean.push(ch),
            ch if ch.is_control() => filtered = true,
            ch => clean.push(ch),
        }
    }
    (clean, filtered)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_copy_reports_interruption_and_expiry_but_keeps_empty_selection_quiet() {
        assert_eq!(
            copy_reply(Err(()), false, true),
            Err(i18n_key::TERMINAL_COPY_INTERRUPTED)
        );
        assert_eq!(
            copy_reply(Ok(None), false, true),
            Err(i18n_key::TERMINAL_COPY_SELECTION_UNAVAILABLE)
        );
        assert_eq!(
            copy_reply(Ok(None), true, false),
            Err(i18n_key::TERMINAL_COMMAND_OUTPUT_IS_NO_LONGER_RETAINED_OR_EXCEEDS_64_KIB)
        );
        assert_eq!(copy_reply(Ok(None), false, false), Ok(None));
        assert_eq!(copy_reply(Ok(Some(String::new())), true, false), Ok(None));
        assert_eq!(
            copy_reply(Ok(Some(" \n".into())), false, true),
            Ok(Some(" \n".into()))
        );
    }

    #[test]
    fn paste_cannot_inject_a_bracketed_paste_terminator() {
        let (text, filtered) = clean_paste("a\r\nb\rc\x1b[201~\0\t");
        assert_eq!(text, "a\nb\nc[201~\t");
        assert!(filtered);
        assert_eq!(
            clean_paste("中文\r\n\t🙂\r"),
            ("中文\n\t🙂\n".into(), false)
        );
    }
}
