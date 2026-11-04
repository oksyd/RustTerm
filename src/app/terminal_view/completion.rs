use super::TerminalView;
use crate::app::pane::PaneEvent;
use gpui::{Context, Window};
use rustterm::{config::Uuid, session::Command};
use std::time::Duration;

impl TerminalView {
    pub(super) fn watch_command_completions(&mut self, cx: &mut Context<Self>) {
        self.command_task = None;
        let Some(session) = self.runs.last() else {
            return;
        };
        let run = session.id();
        let mut completions = session.subscribe_command_completions();
        completions.borrow_and_update();
        self.command_task = Some(cx.spawn(async move |this, cx| {
            while completions.changed().await.is_ok() {
                let command = completions.borrow_and_update().clone();
                let current = this.update(cx, |this, cx| {
                    if this.runs.last().is_none_or(|session| session.id() != run) {
                        return false;
                    }
                    let Some(command) = command else {
                        return true;
                    };
                    cx.notify();
                    let background = !this.visible
                        || !this.window_state.visibility.is_visible()
                        || this.viewed_run + 1 != this.runs.len();
                    if background
                        && !this.window_state.is_locked()
                        && command.duration >= Duration::from_secs(10)
                    {
                        this.unread = true;
                        cx.emit(PaneEvent::CommandFinished {
                            index: this.index,
                            run,
                            command,
                        });
                        cx.notify();
                    }
                    true
                });
                if !matches!(current, Ok(true)) {
                    break;
                }
            }
        }));
    }

    pub fn reveal_completed_command(
        &mut self,
        run: Uuid,
        command: u64,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(index) = self.runs.iter().position(|session| session.id() == run) {
            if index != self.viewed_run {
                self.choose_run(index, window, cx);
            }
            self.send(Command::RevealCommand { id: command }, cx);
        }
    }
}
