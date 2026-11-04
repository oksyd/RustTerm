use super::TerminalView;
use gpui::{ClipboardItem, Context, PromptLevel, Window};
use rustterm::i18n::messages as i18n_key;
use rustterm::i18n::tr;
use rustterm::session::Command;
use std::sync::Arc;

impl TerminalView {
    pub fn history_run(&self) -> Option<rustterm::config::Uuid> {
        self.runs.get(self.viewed_run).map(|session| session.id())
    }

    pub fn history_actions_allowed(&self, cx: &gpui::App) -> bool {
        !self.chrome_blocked(cx)
    }

    pub fn insert_history_command(
        &mut self,
        run: rustterm::config::Uuid,
        id: u64,
        cx: &mut Context<Self>,
    ) {
        if self.chrome_blocked(cx) || !self.can_insert_history_command(run) {
            return;
        }
        self.send(Command::InsertCommandDraft { id }, cx);
    }

    pub fn can_insert_history_command(&self, run: rustterm::config::Uuid) -> bool {
        self.history_run() == Some(run)
            && self.viewed_run + 1 == self.runs.len()
            && self
                .runs
                .last()
                .is_some_and(|session| session.can_insert_command())
            && !self.show_final_alternate
    }

    pub fn history_integration_enabled(&self) -> bool {
        self.snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.shell.shell.is_some())
    }
    pub(super) fn navigate_command(
        &mut self,
        previous: bool,
        failed_only: bool,
        cx: &mut Context<Self>,
    ) {
        if !self.chrome_blocked(cx) && !self.show_final_alternate {
            self.send(
                Command::NavigateCommand {
                    previous,
                    failed_only,
                },
                cx,
            );
        }
    }

    pub(super) fn select_command_output(&mut self, cx: &mut Context<Self>) {
        if !self.chrome_blocked(cx) && !self.show_final_alternate {
            self.send(Command::SelectCommandOutput { id: None }, cx);
        }
    }

    pub(super) fn command_status(&self) -> Option<String> {
        let shell = &self.snapshot.as_ref()?.shell;
        let command = shell
            .focused_command
            .and_then(|id| shell.commands.iter().find(|command| command.id == id))
            .or_else(|| shell.commands.last())?;
        Some(rustterm::trf!(
            i18n_key::TERMINAL_COMMAND_FINISHED_IN_SECONDS_S_EXIT_CODE,
            seconds = format!("{:.1}", command.duration.as_secs_f64()),
            code = command
                .exit_status
                .map(|code| code.to_string())
                .unwrap_or_else(|| "—".into())
        ))
    }

    pub fn offer_restore_directory(&mut self, path: Option<String>, cx: &mut Context<Self>) {
        self.previous_directory = path.filter(|path| {
            path.starts_with('/') && path.len() <= 4096 && !path.chars().any(char::is_control)
        });
        cx.notify();
    }

    pub(super) fn restore_previous_directory(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.chrome_blocked(cx)
            || !self.is_running()
            || self.viewed_run + 1 != self.runs.len()
            || self
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.alternate_screen)
        {
            return;
        }
        let Some(path) = self.previous_directory.clone() else {
            return;
        };
        let Some(permit) = self.window_state.prompts.acquire() else {
            return;
        };
        let generation = self.generation;
        let known = self
            .snapshot
            .as_ref()
            .filter(|snapshot| snapshot.shell.at_prompt && !snapshot.alternate_screen)
            .and_then(|snapshot| snapshot.shell.shell);
        let buttons: Vec<&str> = if known.is_some() {
            vec![
                tr(i18n_key::COMMON_CANCEL),
                tr(i18n_key::TERMINAL_CHANGE_DIRECTORY),
            ]
        } else {
            vec![tr(i18n_key::COMMON_CANCEL), "Bash", "Zsh", "Fish"]
        };
        let detail = rustterm::trf!(
            i18n_key::TERMINAL_DIRECTORY_PATH_RUN_THIS_ONLY_AT_A_SHELL_PROMPT_CHOOSE,
            path = path
        );
        let response = window.prompt(
            PromptLevel::Info,
            tr(i18n_key::TERMINAL_RESTORE_DIRECTORY_CONFIRMATION),
            Some(&detail),
            &buttons,
            cx,
        );
        cx.spawn(async move |this, cx| {
            let choice = response.await.ok();
            let _ = this.update(cx, |this, cx| {
                let valid = permit.is_valid()
                    && this.generation == generation
                    && this.is_running()
                    && this.viewed_run + 1 == this.runs.len();
                drop(permit);
                if !valid {
                    return;
                }
                let shell = match (known, choice) {
                    (Some(shell), Some(1)) => shell,
                    (None, Some(1)) => "bash",
                    (None, Some(2)) => "zsh",
                    (None, Some(3)) => "fish",
                    _ => return,
                };
                if let Some(command) =
                    rustterm::terminal::integration::directory_command(shell, &path)
                {
                    this.send(Command::Input(command.into_bytes()), cx);
                }
            });
        })
        .detach();
    }

    pub fn current_directory(&self) -> Option<String> {
        self.runs
            .get(self.viewed_run)
            .and_then(|session| session.current_directory())
    }

    pub fn reconnect_directory(&self) -> Option<String> {
        self.runs
            .last()
            .and_then(|session| session.current_directory())
            // Failed attempts have no shell metadata. Keep the original target
            // until a successful connection reports its own directory.
            .or_else(|| self.previous_directory.clone())
    }

    pub fn command_records(&self) -> Arc<[rustterm::terminal::ShellCommand]> {
        self.runs
            .get(self.viewed_run)
            .map(|session| session.command_history())
            .unwrap_or_default()
    }

    pub fn copy_command_output(&mut self, id: u64, cx: &mut Context<Self>) {
        self.copy_task.take();
        if self.window_state.is_locked() {
            return;
        }
        let records = self.command_records();
        let command = records.iter().find(|command| command.id == id);
        if let Some(output) = command.and_then(|command| command.output.as_deref()) {
            cx.write_to_clipboard(ClipboardItem::new_string(output.to_string()));
        } else {
            self.error = Some(
                if command.is_some_and(|command| command.output_cache_disabled) {
                    i18n_key::TERMINAL_COMMAND_OUTPUT_CACHE_WAS_DISABLED
                } else {
                    i18n_key::TERMINAL_COMMAND_OUTPUT_IS_NO_LONGER_RETAINED_OR_EXCEEDS_64_KIB
                }
                .into(),
            );
            cx.notify();
        }
    }

    pub fn can_enable_shell_integration(&self, cx: &gpui::App) -> bool {
        !self.chrome_blocked(cx)
            && !self.window_state.is_locked()
            && self.is_running()
            && self.viewed_run + 1 == self.runs.len()
            && !self
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.alternate_screen)
    }

    pub fn enable_shell_integration(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_enable_shell_integration(cx) {
            return;
        }
        let Some(permit) = self.window_state.prompts.acquire() else {
            return;
        };
        let generation = self.generation;
        let response = window.prompt(
            PromptLevel::Info,
            tr(i18n_key::TERMINAL_ENABLE_INTEGRATION_IN_THE_CURRENT_SHELL),
            Some(tr(
                i18n_key::TERMINAL_RUN_THIS_ONLY_AT_A_SHELL_PROMPT_CHOOSE_THE_SHELL,
            )),
            &[tr(i18n_key::COMMON_CANCEL), "Bash", "Zsh", "Fish"],
            cx,
        );
        cx.spawn(async move |this, cx| {
            let choice = response.await.ok();
            let _ = this.update(cx, |this, cx| {
                let valid = permit.is_valid() && this.generation == generation;
                drop(permit);
                cx.notify();
                if !valid {
                    return;
                }
                let shell = match choice {
                    Some(1) => "bash",
                    Some(2) => "zsh",
                    Some(3) => "fish",
                    _ => return,
                };
                if let Some(script) = rustterm::terminal::integration::activation(shell) {
                    this.send(Command::Input(script.into_bytes()), cx);
                }
            });
        })
        .detach();
    }
}
