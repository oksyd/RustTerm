use super::*;
use rustterm::terminal::ShellCommand;

#[derive(Clone, Copy)]
pub(super) struct HistoryRecord {
    run: Uuid,
    id: u64,
    can_insert: bool,
    has_output: bool,
}

impl HistoryRecord {
    fn resolve(self, current_run: Option<Uuid>, records: &[ShellCommand]) -> Option<&ShellCommand> {
        if current_run != Some(self.run) {
            return None;
        }
        records.iter().find(|command| command.id == self.id)
    }

    pub(super) fn same_execution(self, other: Self) -> bool {
        (self.run, self.id) == (other.run, other.id)
    }

    pub(super) fn hint(self) -> &'static str {
        if self.can_insert {
            i18n_key::WORKSPACE_HISTORY_INSERT_KEYS
        } else {
            i18n_key::TERMINAL_INSERT_REQUIRES_A_SINGLE_LINE_COMMAND_AND_AN_IDLE_SHELL
        }
    }

    fn new(run: Uuid, command: &ShellCommand, can_insert: bool) -> Self {
        Self {
            run,
            id: command.id,
            can_insert: can_insert
                && !command.command.is_empty()
                && !command.command.chars().any(char::is_control),
            has_output: command
                .output
                .as_deref()
                .is_some_and(|output| !output.is_empty()),
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum HistoryAction {
    Insert,
    CopyOutput,
    Save,
}

pub(super) fn history_items(
    run: Uuid,
    records: &[ShellCommand],
    query: &str,
    can_insert: bool,
) -> Vec<Item> {
    if query.is_empty() {
        return Vec::new();
    }
    records
        .iter()
        .rev()
        .filter(|command| command.command.to_lowercase().contains(query))
        .take(32)
        .map(|command| {
            let status = command.exit_status.map_or_else(
                || tr(i18n_key::WORKSPACE_RUNNING).to_owned(),
                |code| format!("{} {code}", tr(i18n_key::WORKSPACE_EXIT_STATUS)),
            );
            Item {
                label: command.command.replace(['\r', '\n'], " ↵ ").into(),
                detail: format!("#{} · {status}", command.id).into(),
                action: Action::History(HistoryRecord::new(run, command, can_insert)),
            }
        })
        .collect()
}

impl Workspace {
    pub(super) fn run_history_action(
        &mut self,
        row: HistoryRecord,
        action: HistoryAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.palette.open
            || self.palette.editing.is_some()
            || self.lock_screen.locked
            || self.palette.is_composing(cx)
        {
            return;
        }
        let Some(Pane::Terminal(view)) = self.panes.get(&self.focused) else {
            return;
        };
        let view = view.clone();
        let terminal = view.read(cx);
        if !terminal.history_actions_allowed(cx) {
            return;
        }
        let records = terminal.command_records();
        let Some(command) = row.resolve(terminal.history_run(), &records) else {
            return;
        };
        let current = HistoryRecord::new(
            row.run,
            command,
            !self.utility_overlay && terminal.can_insert_history_command(row.run),
        );
        match action {
            HistoryAction::Insert if current.can_insert => {
                self.close_palette(window, cx);
                self.focus_pane(self.focused, window, cx);
                view.update(cx, |terminal, cx| {
                    terminal.insert_history_command(row.run, row.id, cx)
                });
            }
            HistoryAction::CopyOutput if current.has_output => {
                self.close_palette(window, cx);
                view.update(cx, |terminal, cx| terminal.copy_command_output(row.id, cx));
            }
            HistoryAction::Save => {
                self.edit_quick_command(None, command.command.clone(), window, cx)
            }
            _ => {}
        }
    }

    pub(super) fn history_buttons(
        &self,
        record: HistoryRecord,
        selected: bool,
        cx: &Context<Self>,
    ) -> gpui::Div {
        let mut buttons = div().flex().items_center().gap_1().flex_shrink_0();
        for (action, icon, label) in [
            (
                HistoryAction::CopyOutput,
                ui::Icon::Copy,
                i18n_key::TERMINAL_COPY_COMMAND_OUTPUT,
            ),
            (
                HistoryAction::Save,
                ui::Icon::Plus,
                i18n_key::WORKSPACE_SAVE_HISTORY_COMMAND,
            ),
        ] {
            if matches!(action, HistoryAction::CopyOutput) && !record.has_output {
                continue;
            }
            buttons = buttons.child(
                ui::icon_button((label, record.id), icon, label)
                    .tab_stop(selected)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.run_history_action(record, action, window, cx);
                    })),
            );
        }
        buttons
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustterm::terminal::Terminal;

    #[test]
    fn search_shows_each_execution_once_and_leaves_empty_search_to_the_history_entry() {
        let mut terminal = Terminal::new(80, 24, 100);
        terminal.feed(
            b"\x1b]133;A\x07$ \x1b]133;B\x07git status\r\n\x1b]133;C\x07first\r\n\x1b]133;D;1\x07",
        );
        terminal.feed(
            b"\x1b]133;A\x07$ \x1b]133;B\x07git status\r\n\x1b]133;C\x07second\r\n\x1b]133;D;0\x07",
        );
        let records = terminal.command_history();
        assert_eq!(records.len(), 2);
        let run = Uuid::new_v4();
        let rows = history_items(run, &records, "git status", true);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].label, "git status");
        assert_ne!(rows[0].detail, rows[1].detail);
        let latest = rows[0].history_record().unwrap();
        let previous = rows[1].history_record().unwrap();
        assert!(!previous.same_execution(latest));
        for (row, status, output) in [(latest, 0, "second"), (previous, 1, "first")] {
            let command = row.resolve(Some(run), &records).unwrap();
            assert_eq!(command.exit_status, Some(status));
            assert!(row.has_output && command.output.as_deref().unwrap().contains(output));
        }
        assert!(previous.resolve(Some(run), &records[1..]).is_none());
        assert!(latest.resolve(Some(Uuid::new_v4()), &records).is_none());
        terminal.feed(b"\x1b[?2004h\x1b]133;A\x07$ \x1b]133;B\x07");
        assert_eq!(
            terminal.command_draft(latest.id),
            b"\x1b[200~git status\x1b[201~"
        );
        assert!(history_items(run, &records, "", true).is_empty());
    }

    #[test]
    fn history_remains_searchable_with_an_unavailable_draft_or_output() {
        let mut terminal = Terminal::new(80, 24, 100);
        terminal.set_command_output_cache(false);
        terminal
            .feed(b"\x1b]133;A\x07$ \x1b]133;B\x07pwd\r\n\x1b]133;C\x07/home\r\n\x1b]133;D;0\x07");
        let run = Uuid::new_v4();
        let records = terminal.command_history();
        let rows = history_items(run, &records, "pwd", false);
        let record = rows[0].history_record().unwrap();
        assert!(!record.can_insert && !record.has_output);
        assert_eq!(record.resolve(Some(run), &records).unwrap().command, "pwd");
    }
}
