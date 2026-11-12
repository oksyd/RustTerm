use super::*;
use alacritty_terminal::{
    index::Side,
    selection::{Selection, SelectionType},
    term::cell::LineLength,
};
use std::time::{Duration, Instant};

#[cfg(test)]
mod interaction_tests;

const MAX_COMMANDS: usize = 64;
const MAX_COMMAND_BYTES: usize = 4096;
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_SHELL_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug)]
pub struct ShellCommand {
    pub id: u64,
    pub command: String,
    pub cwd: Option<String>,
    pub exit_status: Option<i32>,
    pub duration: Duration,
    /// None when caching is disabled, grid rows were evicted, or the copy limit was exceeded.
    pub output: Option<Arc<str>>,
    /// Whether output caching was disabled when this command completed.
    pub output_cache_disabled: bool,
    anchors: Arc<CommandAnchors>,
}

#[derive(Debug, Default)]
struct CommandAnchors {
    command: Option<reading::RowStamp>,
    output: Option<reading::SelectionAnchor>,
}

/// Shell integration state, with a prompt-scoped Readline fallback for Bash.
/// Keyboard shortcuts alone never imply an editor mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ShellInteraction {
    #[default]
    Unknown,
    /// The prompt is ready; the shell has not reported its editor's submode.
    Prompt,
    /// An editor hook explicitly reported normal command editing.
    Editing,
    Executing,
    HistorySearch,
}

#[derive(Clone, Debug, Default)]
pub struct ShellSnapshot {
    pub shell: Option<&'static str>,
    pub at_prompt: bool,
    pub interaction: ShellInteraction,
    /// Search hooks or recognition of the shell's standard Readline prompt.
    pub history_search_supported: bool,
    pub focused_command: Option<u64>,
    pub command_unavailable: bool,
    pub command_insert_blocked: bool,
    pub cwd: Option<String>,
    pub commands: Arc<[ShellCommand]>,
}

#[derive(Default)]
pub(super) struct ShellTracker {
    escaped: bool,
    osc: Option<Vec<u8>>,
    osc_bytes: usize,
    discarded: bool,
    prompt: Option<(u64, usize)>,
    readline_prompt: Option<u64>,
    readline_prompt_is_literal: bool,
    output_start: Option<(u64, usize)>,
    active: Option<String>,
    command_start: Option<reading::RowStamp>,
    started_at: Option<Instant>,
    next_id: u64,
    pub snapshot: ShellSnapshot,
}

impl ShellTracker {
    pub(super) fn suspend_input(&mut self) {
        // A fullscreen widget can run inside ZLE without a preexec marker.
        // Require a fresh prompt or ZLE notification before claiming input mode.
        if self.snapshot.interaction != ShellInteraction::Executing {
            self.snapshot.interaction = ShellInteraction::Unknown;
            self.snapshot.at_prompt = false;
        }
    }

    pub(super) fn reset_interaction(&mut self) {
        self.snapshot.interaction = ShellInteraction::Unknown;
        self.snapshot.at_prompt = false;
        self.snapshot.history_search_supported = false;
        self.readline_prompt = None;
        self.readline_prompt_is_literal = false;
    }

    fn accept(&mut self, byte: u8) -> Option<String> {
        if let Some(data) = &mut self.osc {
            if self.discarded {
                let end = matches!(byte, 0x07 | 0x18 | 0x1a) || (self.escaped && byte == b'\\');
                self.escaped = byte == 0x1b && !end;
                if end {
                    self.osc = None;
                }
                return None;
            }
            // VTE dispatches OSC at ESC itself, which can also begin a new
            // sequence. CAN/SUB terminate OSC and leave the parser in Ground.
            if matches!(byte, 0x07 | 0x18 | 0x1a | 0x1b) {
                let data = self.osc.take().unwrap();
                self.escaped = byte == 0x1b;
                return String::from_utf8(data).ok();
            }
            self.osc_bytes = self.osc_bytes.saturating_add(1);
            if self.osc_bytes > MAX_STRING_BYTES {
                self.discarded = true;
            } else if !matches!(byte, 0x00..=0x06 | 0x08..=0x17 | 0x19 | 0x1c..=0x1f) {
                data.push(byte);
            }
            return None;
        }
        if self.escaped && byte == b']' {
            self.osc = Some(Vec::new());
            self.osc_bytes = 0;
            self.discarded = false;
            self.escaped = false;
        } else if !(self.escaped && matches!(byte, 0x00..=0x17 | 0x19 | 0x1c..=0x1f | 0x7f..=0xff))
        {
            self.escaped = byte == 0x1b;
        }
        None
    }
}

impl Terminal {
    pub fn current_directory(&self) -> Option<&str> {
        self.shell.snapshot.cwd.as_deref()
    }

    pub(super) fn shell_byte(&mut self, byte: u8) {
        let Some(sequence) = self.shell.accept(byte) else {
            return;
        };
        if let Some(uri) = sequence.strip_prefix("7;") {
            if let Some(path) = decode_cwd(uri) {
                self.shell.snapshot.cwd = Some(path);
            }
            return;
        }
        if self.is_alternate() {
            return;
        }
        let Some(marker) = sequence
            .strip_prefix("133;")
            .or_else(|| sequence.strip_prefix("633;"))
        else {
            return;
        };
        if let Some(shell) = marker.strip_prefix("P;RustTerm=") {
            self.shell.reset_interaction();
            self.shell.snapshot.shell = match shell {
                "bash" => Some("bash"),
                "zsh" => Some("zsh"),
                "fish" => Some("fish"),
                _ => None,
            };
            return;
        }
        if let Some(value) = marker.strip_prefix("P;RustTermHistorySearch=") {
            self.shell.snapshot.history_search_supported =
                self.shell.snapshot.shell == Some("zsh") && value == "1";
            if !self.shell.snapshot.history_search_supported
                && matches!(
                    self.shell.snapshot.interaction,
                    ShellInteraction::Editing | ShellInteraction::HistorySearch
                )
            {
                self.shell.suspend_input();
            }
            return;
        }
        if let Some(value) = marker.strip_prefix("P;RustTermInput=") {
            if self.shell.snapshot.history_search_supported
                && self.shell.snapshot.interaction != ShellInteraction::Executing
            {
                match value {
                    "history-search" => {
                        self.shell.snapshot.interaction = ShellInteraction::HistorySearch;
                        self.shell.snapshot.at_prompt = true;
                    }
                    "editing" => {
                        self.shell.snapshot.interaction = ShellInteraction::Editing;
                        self.shell.snapshot.at_prompt = true;
                    }
                    "finished" => self.shell.suspend_input(),
                    _ => {}
                }
            }
            return;
        }
        self.reconcile_rows();
        match marker.split(';').next().unwrap_or_default() {
            "A" => {
                self.shell.prompt = None;
                self.shell.snapshot.at_prompt = false;
                // Prompt redraws do not override a mode reported by ZLE. A/B
                // alone only confirm the prompt, never a specific editor mode.
                if !matches!(
                    self.shell.snapshot.interaction,
                    ShellInteraction::Editing | ShellInteraction::HistorySearch
                ) {
                    self.shell.snapshot.interaction = ShellInteraction::Unknown;
                }
            }
            "B" => {
                self.shell.prompt = self.shell_anchor();
                self.shell.readline_prompt = (self.shell.snapshot.shell == Some("bash"))
                    .then(|| {
                        self.readline_line_start(self.term.grid().cursor.point.line)
                            .and_then(|line| self.readline_row_id(line))
                    })
                    .flatten();
                // A custom PS1 may itself contain the search message syntax.
                self.shell.readline_prompt_is_literal =
                    self.shell.snapshot.shell == Some("bash") && self.bash_history_search();
                self.shell.snapshot.at_prompt = true;
                if !matches!(
                    self.shell.snapshot.interaction,
                    ShellInteraction::Editing | ShellInteraction::HistorySearch
                ) {
                    self.shell.snapshot.interaction = ShellInteraction::Prompt;
                }
            }
            "C" => {
                self.shell.snapshot.at_prompt = false;
                self.shell.snapshot.interaction = ShellInteraction::Executing;
                self.shell.snapshot.command_unavailable = false;
                self.shell.snapshot.command_insert_blocked = false;
                self.shell.started_at = Some(Instant::now());
                self.shell.command_start = self
                    .shell
                    .prompt
                    .and_then(|(id, _)| self.main_tracker.positions.get(&id))
                    .and_then(|&row| self.row_stamp(row));
                self.shell.active = self
                    .shell
                    .prompt
                    .take()
                    .and_then(|start| self.shell_text(start, MAX_COMMAND_BYTES))
                    .map(|text| text.trim_end_matches('\n').to_owned());
                self.shell.output_start = self.shell_anchor();
            }
            "D" => {
                self.shell.snapshot.at_prompt = false;
                self.shell.snapshot.interaction = ShellInteraction::Unknown;
                let duration = self.shell.started_at.take().map(|time| time.elapsed());
                let Some(command) = self
                    .shell
                    .active
                    .take()
                    .filter(|command| !command.is_empty())
                else {
                    return;
                };
                let output = self
                    .shell
                    .output_start
                    .filter(|_| self.cache_command_output)
                    .and_then(|start| self.shell_text(start, MAX_OUTPUT_BYTES))
                    .map(Arc::from);
                let output_anchor = self
                    .shell
                    .output_start
                    .take()
                    // Selection can still use the live grid without a text cache.
                    .filter(|_| output.is_some() || !self.cache_command_output)
                    .and_then(|start| self.command_output_anchor(start));
                let exit_status = marker
                    .split(';')
                    .nth(1)
                    .and_then(|status| status.parse().ok());
                self.shell.next_id = self.shell.next_id.wrapping_add(1);
                let record = ShellCommand {
                    id: self.shell.next_id,
                    command,
                    cwd: self.shell.snapshot.cwd.clone(),
                    exit_status,
                    duration: duration.unwrap_or_default(),
                    output,
                    output_cache_disabled: !self.cache_command_output,
                    anchors: Arc::new(CommandAnchors {
                        command: self.shell.command_start.take(),
                        output: output_anchor,
                    }),
                };
                let mut records = self.shell.snapshot.commands.to_vec();
                records.push(record);
                let mut bytes = records.iter().map(charge).sum::<usize>();
                while records.len() > MAX_COMMANDS || bytes > MAX_SHELL_BYTES {
                    bytes = bytes.saturating_sub(charge(&records.remove(0)));
                }
                self.shell.snapshot.commands = records.into();
            }
            _ => {}
        }
    }

    pub(super) fn shell_snapshot(&self) -> ShellSnapshot {
        let mut snapshot = self.shell.snapshot.clone();
        snapshot.interaction = self.shell_interaction();
        // Bash has no ZLE-style search hooks. Its standard Readline search
        // prompt can still be recognized while shell integration is at a prompt.
        snapshot.history_search_supported |= snapshot.shell == Some("bash");
        snapshot
    }

    fn shell_interaction(&self) -> ShellInteraction {
        let interaction = self.shell.snapshot.interaction;
        if self.shell.snapshot.shell == Some("bash")
            && self.shell.snapshot.at_prompt
            && interaction == ShellInteraction::Prompt
            && !self.shell.readline_prompt_is_literal
            && !self.is_alternate()
            && self.bash_history_search()
        {
            ShellInteraction::HistorySearch
        } else {
            interaction
        }
    }

    fn bash_history_search(&self) -> bool {
        // Readline replaces the last logical prompt line with its search
        // message. Use that line even when the result puts the cursor in a
        // multiline command. Never search the scrolled viewport or old output.
        // Bound the work even for enormous wrapped commands.
        let grid = self.term.grid();
        let width = self.size.columns as usize;
        let start = self
            .shell
            .readline_prompt
            .and_then(|id| self.point_for(id, 0))
            .map_or(grid.cursor.point.line, |point| point.line);
        let Some(mut line) = self.readline_line_start(start) else {
            return false;
        };
        let mut text = String::new();
        while line <= grid.bottommost_line() && text.len() < MAX_COMMAND_BYTES {
            let count = width.min(MAX_COMMAND_BYTES - text.len());
            text.push_str(&self.row_text(line, 0..count, true));
            if count < width || !self.readline_row_wrapped(line) {
                break;
            }
            line.0 += 1;
        }
        readline_search_prompt(&text)
    }

    fn readline_row_wrapped(&self, line: Line) -> bool {
        self.term.grid()[Point::new(line, Column(self.size.columns as usize - 1))]
            .flags
            .contains(Flags::WRAPLINE)
    }

    fn readline_row_id(&self, line: Line) -> Option<u64> {
        self.main_tracker
            .id_at((line.0 + self.term.grid().history_size() as i32) as usize)
    }

    fn readline_line_start(&self, mut line: Line) -> Option<Line> {
        let mut cells = 0;
        while line > self.term.grid().topmost_line() && self.readline_row_wrapped(Line(line.0 - 1))
        {
            cells += self.size.columns as usize;
            if cells > MAX_COMMAND_BYTES {
                return None;
            }
            line.0 -= 1;
        }
        Some(line)
    }

    // Width changes replace row identities. Hard line breaks between the
    // prompt and cursor survive reflow, unlike physical row offsets.
    pub(super) fn readline_prompt_distance(&self) -> Option<usize> {
        if self.shell.snapshot.shell != Some("bash") || !self.shell.snapshot.at_prompt {
            return None;
        }
        let start = self.point_for(self.shell.readline_prompt?, 0)?.line.0;
        let end = self.term.grid().cursor.point.line.0;
        if start > end || end - start > MAX_COMMAND_BYTES as i32 {
            return None;
        }
        Some(
            (start..end)
                .filter(|&row| !self.readline_row_wrapped(Line(row)))
                .count(),
        )
    }

    pub(super) fn restore_readline_prompt(&mut self, distance: Option<usize>) {
        self.shell.readline_prompt = distance.and_then(|mut distance| {
            let mut line = self.term.grid().cursor.point.line;
            let mut rows = 0;
            while distance > 0 {
                if line <= self.term.grid().topmost_line() || rows >= MAX_COMMAND_BYTES {
                    return None;
                }
                line.0 -= 1;
                rows += 1;
                if !self.readline_row_wrapped(line) {
                    distance -= 1;
                }
            }
            self.readline_line_start(line)
                .and_then(|line| self.readline_row_id(line))
        });
    }

    pub fn last_completed_command(&self) -> Option<&ShellCommand> {
        self.shell.snapshot.commands.last()
    }

    pub fn command_history(&self) -> Arc<[ShellCommand]> {
        self.shell.snapshot.commands.clone()
    }

    /// Applies to commands completing after this call; existing caches age out
    /// under the normal command history limits.
    pub fn set_command_output_cache(&mut self, enabled: bool) {
        self.cache_command_output = enabled;
    }

    pub fn can_insert_command(&self) -> bool {
        self.parser.is_some()
            && !self.is_alternate()
            && self.shell.snapshot.at_prompt
            && self.modes().bracketed_paste
            && self.shell_interaction() != ShellInteraction::HistorySearch
    }

    pub fn command_draft(&mut self, id: u64) -> Vec<u8> {
        let text = self
            .shell
            .snapshot
            .commands
            .iter()
            .find(|command| command.id == id)
            .map(|command| command.command.as_str());
        if self.can_insert_command()
            && let Some(text) =
                text.filter(|text| !text.is_empty() && !text.chars().any(char::is_control))
        {
            let bytes = self.encode_paste(text);
            self.shell.snapshot.command_insert_blocked = false;
            return bytes;
        }
        self.shell.snapshot.command_insert_blocked = true;
        self.version = self.version.wrapping_add(1);
        Vec::new()
    }

    fn command_output_anchor(&self, start: (u64, usize)) -> Option<reading::SelectionAnchor> {
        let start = self.point_for(start.0, start.1)?;
        let grid = self.term.grid();
        let mut end = grid.cursor.point;
        if end.line.0.saturating_sub(start.line.0) > 2048 {
            return None;
        }
        let side = if grid.cursor.input_needs_wrap {
            Side::Right
        } else if end.column.0 == 0 && end.line > start.line {
            end.line.0 -= 1;
            end.column = Column(grid[end.line].line_length().0.saturating_sub(1));
            Side::Right
        } else {
            Side::Left
        };
        if start > end || (start == end && side == Side::Left) {
            return None;
        }
        let mut selection = Selection::new(SelectionType::Simple, start, Side::Left);
        selection.update(end, side);
        self.selection_anchor(&selection)
    }

    pub fn navigate_command(&mut self, previous: bool, failed_only: bool) {
        if self.is_alternate() {
            return;
        }
        self.reconcile_rows();
        let focused = self.shell.snapshot.focused_command;
        let grid = self.term.grid();
        let top = grid.history_size() - grid.display_offset();
        let following = grid.display_offset() == 0 && focused.is_none();
        let candidate = self
            .shell
            .snapshot
            .commands
            .iter()
            .filter(|command| !failed_only || command.exit_status.is_some_and(|code| code != 0))
            .filter_map(|command| {
                let anchor = command.anchors.command?;
                self.stamp_valid(anchor).then_some((command.id, anchor.id))
            })
            .filter(|(id, row)| {
                if let Some(focused) = focused {
                    if previous {
                        *id < focused
                    } else {
                        *id > focused
                    }
                } else {
                    let position = self.main_tracker.positions[row];
                    if previous {
                        following || position < top
                    } else {
                        !following && position > top
                    }
                }
            });
        let target = if previous {
            candidate.last()
        } else {
            candidate.into_iter().next()
        };
        if let Some((id, row)) = target {
            self.shell.snapshot.command_unavailable = false;
            self.shell.snapshot.focused_command = Some(id);
            self.viewport_anchor = Some(row);
            self.restore_viewport();
            self.viewport_evicted = false;
            self.version = self.version.wrapping_add(1);
        } else if !previous && !failed_only {
            self.scroll_to_bottom();
        } else {
            self.shell.snapshot.command_unavailable = true;
            self.version = self.version.wrapping_add(1);
        }
    }

    pub fn reveal_command(&mut self, id: u64) {
        if self.is_alternate() {
            return;
        }
        self.reconcile_rows();
        let row = self
            .shell
            .snapshot
            .commands
            .iter()
            .find(|command| command.id == id)
            .and_then(|command| command.anchors.command)
            .filter(|stamp| self.stamp_valid(*stamp));
        if let Some(row) = row {
            self.shell.snapshot.command_unavailable = false;
            self.viewport_anchor = Some(row.id);
            self.restore_viewport();
            self.shell.snapshot.focused_command = Some(id);
            self.viewport_evicted = false;
            self.version = self.version.wrapping_add(1);
        } else {
            self.shell.snapshot.command_unavailable = true;
            self.version = self.version.wrapping_add(1);
        }
    }

    pub fn select_command_output(&mut self, id: Option<u64>) {
        if self.is_alternate() {
            return;
        }
        self.reconcile_rows();
        let id = id
            .or(self.shell.snapshot.focused_command)
            .or_else(|| self.last_completed_command().map(|command| command.id));
        let anchor = self
            .shell
            .snapshot
            .commands
            .iter()
            .find(|command| Some(command.id) == id)
            .and_then(|command| command.anchors.output.clone());
        let Some(anchor) = anchor.filter(|anchor| {
            let mut previous = None;
            anchor.stamps.iter().all(|&stamp| {
                let Some(&index) = self.main_tracker.positions.get(&stamp.id) else {
                    return false;
                };
                let valid =
                    self.stamp_valid(stamp) && previous.is_none_or(|before| index == before + 1);
                previous = Some(index);
                valid
            })
        }) else {
            self.clear_selection();
            self.shell.snapshot.command_unavailable = true;
            return;
        };
        let (Some(start), Some(end)) = (
            self.point_for(anchor.start.0, anchor.start.1),
            self.point_for(anchor.end.0, anchor.end.1),
        ) else {
            return;
        };
        let mut selection = Selection::new(SelectionType::Simple, start, Side::Left);
        selection.update(end, Side::Right);
        self.selection_origin = Some(anchor.start.0);
        self.viewport_anchor = Some(anchor.start.0);
        self.restore_viewport();
        self.selection_anchor = Some(anchor);
        self.selection_kind = Some(SelectionKind::Character);
        self.selection_invalidated = false;
        self.shell.snapshot.command_unavailable = false;
        self.term.selection = Some(selection);
        self.shell.snapshot.focused_command = id;
        self.version = self.version.wrapping_add(1);
    }

    pub(super) fn selected_command_output(
        &self,
        anchor: &reading::SelectionAnchor,
    ) -> Option<&str> {
        self.shell
            .snapshot
            .commands
            .iter()
            .find(|command| command.anchors.output.as_ref() == Some(anchor))
            .and_then(|command| command.output.as_deref())
    }

    fn shell_anchor(&self) -> Option<(u64, usize)> {
        let grid = self.term.grid();
        let row = (grid.cursor.point.line.0 + grid.history_size() as i32) as usize;
        Some((self.main_tracker.id_at(row)?, grid.cursor.point.column.0))
    }

    fn shell_text(&self, (id, column): (u64, usize), limit: usize) -> Option<String> {
        let start = *self.main_tracker.positions.get(&id)?;
        let grid = self.term.grid();
        let end = (grid.cursor.point.line.0 + grid.history_size() as i32) as usize;
        if start > end || end - start > MAX_OUTPUT_BYTES {
            return None;
        }
        let mut text = String::new();
        for row in start..=end {
            let line = Line(row as i32 - grid.history_size() as i32);
            let first = if row == start { column } else { 0 };
            let last = if row == end {
                grid.cursor.point.column.0 + usize::from(grid.cursor.input_needs_wrap)
            } else {
                self.size.columns as usize
            };
            if first > last {
                return None;
            }
            let current = self.row_text(line, first..last, true);
            let wrapped = grid[Point::new(line, Column(self.size.columns as usize - 1))]
                .flags
                .contains(Flags::WRAPLINE);
            text.push_str(if wrapped {
                &current
            } else {
                current.trim_end_matches(' ')
            });
            if row != end && !wrapped {
                text.push('\n');
            }
            if text.len() > limit {
                return None;
            }
        }
        Some(text)
    }
}

fn readline_search_prompt(text: &str) -> bool {
    let Some(text) = text.strip_prefix('(') else {
        return false;
    };
    let text = text.strip_prefix("failed ").unwrap_or(text);
    let text = text.strip_prefix("reverse-").unwrap_or(text);
    text.strip_prefix("i-search)`")
        .is_some_and(|query| query.contains("': "))
}

fn charge(command: &ShellCommand) -> usize {
    command.command.len()
        + command.cwd.as_ref().map_or(0, String::len)
        + command.output.as_ref().map_or(0, |text| text.len())
        + command.anchors.output.as_ref().map_or(0, |anchor| {
            anchor.stamps.len() * std::mem::size_of::<reading::RowStamp>()
        })
        + 128
}

fn decode_cwd(uri: &str) -> Option<String> {
    let uri = uri.strip_prefix("file://")?;
    let slash = uri.find('/')?;
    let mut input = uri.as_bytes()[slash..].iter().copied();
    let mut bytes = Vec::new();
    while let Some(byte) = input.next() {
        bytes.push(if byte == b'%' {
            let high = (input.next()? as char).to_digit(16)?;
            let low = (input.next()? as char).to_digit(16)?;
            (high * 16 + low) as u8
        } else {
            byte
        });
    }
    let value = String::from_utf8(bytes).ok()?;
    (!value.chars().any(char::is_control)).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_cache_toggle_keeps_metadata_and_selection_without_backfilling() {
        let mut terminal = Terminal::new(80, 24, 100);
        terminal.feed(b"\x1b]133;B\x07first\r\n\x1b]133;C\x07cached\r\n\x1b]133;D;0\x07");
        let first_output = terminal.command_history()[0].output.clone().unwrap();
        terminal.set_command_output_cache(false);
        terminal.feed(b"\x1b]7;file://host/tmp\x07\x1b]133;B\x07second\r\n\x1b]133;C\x07");
        terminal.shell.started_at = Some(Instant::now() - Duration::from_secs(2));
        terminal.feed(b"uncached\r\n\x1b]133;D;7\x07");
        let history = terminal.command_history();
        assert!(Arc::ptr_eq(
            history[0].output.as_ref().unwrap(),
            &first_output
        ));
        assert!(!history[0].output_cache_disabled);
        let second = &history[1];
        assert_eq!(second.command, "second");
        assert_eq!(second.cwd.as_deref(), Some("/tmp"));
        assert_eq!(second.exit_status, Some(7));
        assert!(second.duration >= Duration::from_secs(2));
        assert!(second.output.is_none());
        assert!(second.output_cache_disabled);
        terminal.select_command_output(Some(second.id));
        assert_eq!(terminal.selected_text().as_deref(), Some("uncached"));
        terminal.reveal_command(second.id);
        assert!(!terminal.snapshot().shell.command_unavailable);

        terminal.set_command_output_cache(true);
        terminal.feed(b"\x1b]133;B\x07third\r\n\x1b]133;C\x07cached again\r\n\x1b]133;D;0\x07");
        let history = terminal.command_history();
        assert!(history[1].output.is_none());
        assert!(history[1].output_cache_disabled);
        assert_eq!(history[2].output.as_deref(), Some("cached again\n"));
        assert!(!history[2].output_cache_disabled);
    }

    #[test]
    fn fragmented_markers_capture_commands_output_status_and_cwd() {
        let mut terminal = Terminal::new(40, 4, 100);
        for byte in b"\x1b]7;file://host/tmp/a%20b\x07\x1b]133;A\x07$ \x1b]133;B\x07printf test\r\n\x1b]133;C\x07test\r\n\x1b]133;D;7\x1b\\" { terminal.feed(&[*byte]); }
        let snapshot = terminal.snapshot();
        assert_eq!(snapshot.shell.cwd.as_deref(), Some("/tmp/a b"));
        assert_eq!(snapshot.shell.commands.len(), 1);
        let command = &snapshot.shell.commands[0];
        assert_eq!(command.command, "printf test");
        assert_eq!(command.exit_status, Some(7));
        assert_eq!(command.output.as_deref(), Some("test\n"));
    }
    #[test]
    fn output_eviction_and_control_paths_are_not_reinterpreted() {
        assert!(decode_cwd("file://host/a%00b").is_none());
        assert!(decode_cwd("file://host/%GG").is_none());
        let mut terminal = Terminal::new(20, 2, 0);
        terminal.feed(
            b"\x1b]133;B\x07command\r\n\x1b]133;C\x07one\r\ntwo\r\nthree\r\n\x1b]133;D;0\x07",
        );
        assert!(terminal.snapshot().shell.commands[0].output.is_none());
    }
    #[test]
    fn interrupted_osc_recovers_shell_markers_with_vte_control_semantics() {
        let mut terminal = Terminal::new(80, 4, 10);
        for byte in b"\x1b]0;old\x1b[31m\x1b\x00]7;file://host/\x00tmp\x07" {
            terminal.feed(&[*byte]);
        }
        assert_eq!(terminal.snapshot().shell.cwd.as_deref(), Some("/tmp"));
        for terminator in [0x18, 0x1a] {
            terminal.feed(b"\x1b]7;file://host/finished");
            terminal.feed(&[terminator]);
            assert_eq!(terminal.snapshot().shell.cwd.as_deref(), Some("/finished"));
            // Cancelling an ESC prefix must not open a subsequent bare ']'.
            terminal.feed(&[0x1b, terminator]);
            terminal.feed(b"]7;file://host/ignored\x07");
            assert_eq!(terminal.snapshot().shell.cwd.as_deref(), Some("/finished"));
        }
        terminal.feed(b"\x1b]0;");
        terminal.feed(&vec![b'x'; MAX_STRING_BYTES]);
        terminal.feed(b"\x1b]7;file://host/ignored\x07");
        assert_eq!(terminal.snapshot().shell.cwd.as_deref(), Some("/finished"));
        terminal.feed(b"\x1b]7;file://host/recovered\x1b\\");
        assert_eq!(terminal.snapshot().shell.cwd.as_deref(), Some("/recovered"));
    }
    #[test]
    fn command_and_output_tabs_copy_visual_columns() {
        let mut terminal = Terminal::new(16, 4, 100);
        terminal.feed(b"\x1b]133;B\x07echo\tvalue\r\n\x1b]133;C\x07a\tb\x1b]133;D;0\x07");
        let snapshot = terminal.snapshot();
        let command = &snapshot.shell.commands[0];
        assert_eq!(command.command, "echo    value");
        assert_eq!(command.output.as_deref(), Some("a       b"));
    }
    #[test]
    fn output_filling_the_last_column_keeps_its_final_character() {
        let mut terminal = Terminal::new(8, 3, 100);
        terminal.feed(b"\x1b]133;B\x07cmd\r\n\x1b]133;C\x0712345678\x1b]133;D;0\x07");
        assert_eq!(
            terminal.snapshot().shell.commands[0].output.as_deref(),
            Some("12345678")
        );
    }

    #[test]
    fn confirmed_shell_and_prompt_state_follow_execution_markers() {
        let mut terminal = Terminal::new(40, 4, 100);
        terminal.feed(b"\x1b]133;P;RustTerm=bash\x07\x1b]133;A\x07$ \x1b]133;B\x07");
        assert_eq!(terminal.snapshot().shell.shell, Some("bash"));
        assert!(terminal.snapshot().shell.at_prompt);
        terminal.feed(b"false\r\n\x1b]133;C\x07");
        assert!(!terminal.snapshot().shell.at_prompt);
        terminal.shell.started_at = Some(Instant::now() - Duration::from_secs(12));
        terminal.feed(b"\x1b]133;D;1\x07\x1b]133;P;RustTerm=zsh\x07\x1b]133;A\x07% \x1b]133;B\x07");
        assert_eq!(
            terminal
                .last_completed_command()
                .unwrap()
                .duration
                .as_secs(),
            12
        );
        assert_eq!(terminal.snapshot().shell.shell, Some("zsh"));
        assert!(terminal.snapshot().shell.at_prompt);
        terminal.feed(b"\x1b]133;P;RustTerm=unrecognized\x07");
        assert_eq!(terminal.snapshot().shell.shell, None);
    }

    #[test]
    fn command_drafts_require_an_idle_prompt_and_never_include_enter() {
        let mut terminal = Terminal::new(40, 6, 100);
        terminal.feed(b"\x1b]133;B\x07printf one\r\n\x1b]133;C\x07one\r\n\x1b]133;D;0\x07");
        assert!(!terminal.can_insert_command());
        assert!(terminal.command_draft(1).is_empty());
        terminal.feed(b"\x1b]133;B\x07");
        assert!(!terminal.can_insert_command());
        assert!(terminal.command_draft(1).is_empty());
        terminal.feed(b"\x1b[?2004h");
        assert!(terminal.can_insert_command());
        assert_eq!(terminal.command_draft(1), b"\x1b[200~printf one\x1b[201~");
        assert!(!terminal.snapshot().shell.command_insert_blocked);
        assert!(terminal.command_draft(999).is_empty());
        terminal.feed(b"long-running\r\n\x1b]133;C\x07");
        assert!(!terminal.can_insert_command());
        assert!(terminal.command_draft(1).is_empty());
        terminal.feed(b"\x1b]133;D;0\x07\x1b]133;B\x07printf first\r\nprintf second\r\n\x1b]133;C\x07\x1b]133;D;0\x07\x1b]133;B\x07");
        assert!(terminal.command_draft(3).is_empty());
        terminal.feed(b"\x1b[?1049h");
        assert!(!terminal.can_insert_command());
        assert!(terminal.command_draft(1).is_empty());
    }

    #[test]
    fn command_navigation_and_selection_use_retained_content() {
        let mut terminal = Terminal::new(24, 3, 100);
        for (name, status) in [("first", 0), ("failed", 2), ("last", 0)] {
            terminal.feed(format!("\x1b]133;A\x07$ \x1b]133;B\x07{name}\r\n\x1b]133;C\x07{name} output\r\n\x1b]133;D;{status}\x07").as_bytes());
        }
        terminal.navigate_command(true, false);
        assert_eq!(terminal.snapshot().shell.focused_command, Some(3));
        terminal.navigate_command(true, true);
        assert_eq!(terminal.snapshot().shell.focused_command, Some(2));
        terminal.select_command_output(None);
        assert_eq!(terminal.selected_text().as_deref(), Some("failed output\n"));
        terminal.begin_selection(0, 0, SelectionKind::Character);
        assert_eq!(terminal.selected_text().as_deref(), Some("f"));
        terminal.navigate_command(false, false);
        assert_eq!(terminal.snapshot().shell.focused_command, Some(3));
        terminal.reveal_command(1);
        assert_eq!(terminal.snapshot().shell.focused_command, Some(1));
        terminal.resize(12, 3);
        terminal.select_command_output(Some(2));
        assert!(terminal.selected_text().is_none());
        terminal.scroll_to_bottom();
        terminal.navigate_command(true, false);
        assert_eq!(terminal.snapshot().shell.focused_command, None);
    }

    #[test]
    fn evicted_and_alternate_screen_command_anchors_are_not_reused() {
        let mut terminal = Terminal::new(20, 3, 3);
        terminal.feed(b"\x1b]133;B\x07cmd\r\n\x1b]133;C\x07old\r\n\x1b]133;D;0\x07");
        terminal.feed(b"\x1b[?1049h");
        terminal.select_command_output(Some(1));
        terminal.reveal_command(1);
        assert!(terminal.selected_text().is_none());
        assert_eq!(terminal.snapshot().shell.focused_command, None);
        terminal.feed(b"\x1b[?1049l");
        terminal.feed(b"1\r\n2\r\n3\r\n4\r\n5\r\n6\r\n7\r\n");
        terminal.select_command_output(Some(1));
        assert!(terminal.selected_text().is_none());
        terminal.navigate_command(true, false);
        assert_eq!(terminal.snapshot().shell.focused_command, None);
    }
}
