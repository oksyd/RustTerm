use super::TerminalView;
use crate::app::pane::PaneEvent;
use gpui::{ClipboardItem, Context, PromptLevel, Window};
use rustterm::i18n::messages as i18n_key;
use rustterm::{
    i18n::tr,
    session::{Command, MAX_INPUT_BYTES},
};

#[derive(Clone, Copy)]
enum SelectionBuffer {
    Clipboard,
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    Primary,
}

impl TerminalView {
    pub fn copy_selection(&mut self, cx: &mut Context<Self>) {
        self.copy_to(SelectionBuffer::Clipboard, cx);
    }

    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    pub(super) fn copy_primary(&mut self, cx: &mut Context<Self>) {
        self.copy_to(SelectionBuffer::Primary, cx);
    }

    fn copy_to(&mut self, target: SelectionBuffer, cx: &mut Context<Self>) {
        match target {
            SelectionBuffer::Clipboard => self.copy_task.take(),
            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
            SelectionBuffer::Primary => self.primary_copy_task.take(),
        };
        if self.chrome_blocked(cx) || !self.visible || self.show_final_alternate {
            return;
        }
        let Some(run) = self.runs.get(self.viewed_run) else {
            return;
        };
        let run_id = run.id();
        let generation = self.generation;
        let receipt = match run.copy_selection() {
            Ok(receipt) => receipt,
            Err(error) => {
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
        if self.error.take().is_some() {
            cx.emit(PaneEvent::Activity);
            cx.notify();
        }
        let task = cx.spawn(async move |this, cx| {
            let Ok(Some(text)) = receipt.await else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                if this.generation == generation
                    && this.visible
                    && this.window_state.visibility.is_visible()
                    && !this.chrome_blocked(cx)
                    && !this.show_final_alternate
                    && this
                        .runs
                        .get(this.viewed_run)
                        .is_some_and(|run| run.id() == run_id && !run.is_evicted())
                {
                    let item = ClipboardItem::new_string(text);
                    match target {
                        SelectionBuffer::Clipboard => cx.write_to_clipboard(item),
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

    fn paste_text(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
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
        if !filtered && (!text.contains('\n') || !self.settings.confirm_multiline_paste) {
            self.send(Command::Paste(text), cx);
            return;
        }
        let Some(permit) = self.window_state.prompts.acquire() else {
            return;
        };
        self.paste_pending = true;
        let generation = self.generation;
        let target_run = self.runs[self.viewed_run].id();
        let preview: String = text.chars().take(600).collect();
        let detail = format!(
            "{}{}\n\n{}{}",
            if filtered {
                tr(i18n_key::TERMINAL_CONTROL_CHARACTERS_WERE_REMOVED)
            } else {
                ""
            },
            tr(i18n_key::TERMINAL_NEWLINES_MAY_EXECUTE_COMMANDS_IMMEDIATELY_CHECK_THE_TARGET_AND_CONTENT),
            preview.replace('\n', "↵\n"),
            if text.chars().count() > 600 {
                tr(i18n_key::TERMINAL_PASTE_PREVIEW_TRUNCATED)
            } else {
                ""
            }
        );
        let answer = window.prompt(
            PromptLevel::Warning,
            &rustterm::trf!(i18n_key::TERMINAL_PASTE_INTO_TARGET, target = self.shell),
            Some(&detail),
            &[tr(i18n_key::COMMON_CANCEL), tr(i18n_key::COMMON_PASTE)],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let confirmed = answer.await == Ok(1);
            let _ = cx.update(|window, cx| {
                this.update(cx, |this, cx| {
                    let confirmed = confirmed && permit.is_valid();
                    drop(permit);
                    this.paste_pending = false;
                    // Reconnects and segment changes invalidate the original paste target.
                    if confirmed
                        && this.generation == generation
                        && this
                            .runs
                            .get(this.viewed_run)
                            .is_some_and(|run| run.id() == target_run)
                        && this.visible
                        && this.focus.is_focused(window)
                        && this.is_running()
                    {
                        this.send(Command::Paste(text), cx);
                    }
                    cx.notify();
                })
            });
        })
        .detach();
    }
}

fn clean_paste(text: &str) -> (String, bool) {
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
