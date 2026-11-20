//! Command boundaries reference the grid; folding only projects visible rows.
use super::*;
use alacritty_terminal::{
    index::Side,
    selection::{Selection, SelectionType},
    term::cell::LineLength,
};
use std::{ops::Range, time::Duration};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandResult {
    Running,
    Finished {
        exit_status: Option<i32>,
        duration: Duration,
    },
    Unknown,
}

#[derive(Clone, Debug)]
pub struct CommandDetails {
    pub command: String,
    pub cwd: Option<String>,
    pub result: CommandResult,
}

#[derive(Clone, Copy, Debug)]
pub enum BlockCopy {
    Command,
    Output,
}

const MAX_BLOCKS: usize = 64;

#[derive(Clone, Copy, Debug)]
pub struct FoldMarker {
    pub id: u64,
    pub collapsed: bool,
    pub output_rows: usize,
}

#[derive(Clone, Debug)]
pub struct CommandHeading {
    pub id: u64,
    pub text: String,
}

/// Visible portion of one command block, in snapshot row coordinates.
#[derive(Clone, Debug)]
pub struct CommandBlock {
    pub details: Arc<CommandDetails>,
    pub marker: FoldMarker,
    pub rows: Range<usize>,
    pub header: Option<usize>,
    pub ends_here: bool,
    pub selected: bool,
}

#[derive(Default)]
pub(super) struct Blocks {
    next_id: u64,
    items: Vec<Block>,
}

struct Block {
    details: Arc<CommandDetails>,
    id: u64,
    header: reading::RowStamp,
    start: u64,
    start_after: bool,
    end: Option<u64>,
    collapsed: bool,
}

struct BlockRange {
    details: Arc<CommandDetails>,
    marker: FoldMarker,
    header: usize,
    output: Range<usize>,
}

pub(super) struct ReflowBlock {
    details: Arc<CommandDetails>,
    id: u64,
    header: ReflowAnchor,
    start: ReflowAnchor,
    start_after: bool,
    end: Option<ReflowAnchor>,
    collapsed: bool,
    header_row: Option<usize>,
}

struct ReflowAnchor {
    from_end: usize,
    fingerprint: u64,
}

struct Projection {
    hidden: Vec<Range<usize>>,
    total: usize,
    history: usize,
}

impl Projection {
    fn visual(&self, physical: usize) -> usize {
        physical
            - self
                .hidden
                .iter()
                .map(|range| physical.saturating_sub(range.start).min(range.len()))
                .sum::<usize>()
    }

    fn physical(&self, mut visual: usize) -> usize {
        for range in &self.hidden {
            if visual < range.start {
                break;
            }
            visual += range.len();
        }
        visual
    }
}

pub(super) struct Viewport {
    pub lines: Vec<Line>,
    pub history: usize,
    pub offset: usize,
    markers: Vec<BlockRange>,
    raw_history: usize,
}

impl Viewport {
    pub(super) fn project_search_markers(
        &self,
        rows: Arc<[usize]>,
        current: Option<usize>,
    ) -> SearchMarkers {
        let mut folds = self
            .markers
            .iter()
            .filter(|b| b.marker.collapsed)
            .peekable();
        if folds.peek().is_none() {
            return SearchMarkers { rows, current };
        }
        let mut projected = Vec::with_capacity(rows.len());
        let mut selected = None;
        let mut hidden = 0;
        for &row in rows.iter() {
            while let Some(block) = folds.next_if(|block| block.output.end <= row) {
                hidden += block.output.len();
            }
            let visible = folds
                .peek()
                .filter(|block| block.output.contains(&row))
                .map_or(row, |block| block.header)
                .saturating_sub(hidden);
            if current == Some(row) {
                selected = Some(visible);
            }
            projected.push(visible);
        }
        // A multiline command can have visible rows between its header and output.
        projected.sort_unstable();
        projected.dedup();
        SearchMarkers {
            rows: projected.into(),
            current: selected,
        }
    }

    pub fn command_blocks(&self, selection: Option<(Point, Point)>) -> Vec<CommandBlock> {
        let extent = self.physical_extent();
        self.markers
            .iter()
            .filter_map(|block| {
                let end = if block.marker.collapsed {
                    block.output.start
                } else {
                    block.output.end
                };
                if block.header >= extent.end || end <= extent.start {
                    return None;
                }
                let line = |row: usize| Line(row as i32 - self.raw_history as i32);
                let start_row = self.lines.partition_point(|row| *row < line(block.header));
                let end_row = self.lines.partition_point(|row| *row < line(end));
                (start_row < end_row).then(|| CommandBlock {
                    details: block.details.clone(),
                    marker: block.marker,
                    rows: start_row..end_row,
                    header: self.row(line(block.header)),
                    ends_here: end <= extent.end,
                    selected: selection.is_some_and(|(first, last)| {
                        first.line < line(block.output.end) && last.line >= line(block.header)
                    }),
                })
            })
            .collect()
    }

    pub fn physical_extent(&self) -> Range<usize> {
        let first = self
            .lines
            .first()
            .map_or(0, |line| (line.0 + self.raw_history as i32) as usize);
        let end = self.lines.last().map_or(first, |line| {
            (line.0 + self.raw_history as i32 + 1) as usize
        });
        first..end
    }

    pub fn row(&self, line: Line) -> Option<usize> {
        self.lines.binary_search(&line).ok()
    }

    pub fn runs(&self) -> Vec<(usize, Range<i32>)> {
        let mut runs = Vec::new();
        let mut start = 0;
        while start < self.lines.len() {
            let mut end = start + 1;
            while end < self.lines.len() && self.lines[end].0 == self.lines[end - 1].0 + 1 {
                end += 1;
            }
            runs.push((start, self.lines[start].0..self.lines[end - 1].0 + 1));
            start = end;
        }
        runs
    }
}

impl Terminal {
    pub fn copy_command_block(&self, id: u64, part: BlockCopy) -> Option<String> {
        let block = self
            .block_ranges()
            .into_iter()
            .find(|b| b.marker.id == id)?;
        if matches!(part, BlockCopy::Command) {
            return (!block.details.command.is_empty()).then(|| block.details.command.clone());
        }
        let grid = self.term.grid();
        let mut text = String::new();
        for row in block.output {
            let line = Line(row as i32 - grid.history_size() as i32);
            text.push_str(&self.row_text(line, 0..self.content_columns(line), false));
            if !grid[line][Column(grid.columns() - 1)]
                .flags
                .contains(Flags::WRAPLINE)
                && (line < grid.cursor.point.line
                    || !matches!(block.details.result, CommandResult::Running))
            {
                text.push('\n');
            }
            if text.len() > 64 * 1024 {
                return None;
            }
        }
        Some(text)
    }

    pub(super) fn complete_command_block(&mut self, exit_status: Option<i32>, duration: Duration) {
        if let Some(block) = self.blocks.items.last_mut().filter(|b| b.end.is_none()) {
            Arc::make_mut(&mut block.details).result = CommandResult::Finished {
                exit_status,
                duration,
            };
        }
        self.finish_command_block();
    }

    pub(super) fn command_block_range(&self, id: u64) -> Option<Range<usize>> {
        self.block_ranges()
            .into_iter()
            .find(|b| b.marker.id == id)
            .map(|b| b.header..b.output.end)
    }

    pub(super) fn sticky_command(&self, viewport: &Viewport) -> Option<CommandHeading> {
        let top = viewport.physical_extent().start;
        let block = viewport
            .markers
            .iter()
            .find(|b| b.header < top && b.output.contains(&top) && !b.marker.collapsed)?;
        Some(CommandHeading {
            id: block.marker.id,
            text: self
                .row_text(
                    Line(block.header as i32 - viewport.raw_history as i32),
                    0..usize::from(self.size.columns),
                    false,
                )
                .trim_end_matches(' ')
                .to_owned(),
        })
    }

    pub fn reveal_command_block(&mut self, id: u64) {
        let Some(range) = self.command_block_range(id) else {
            return;
        };
        let history = self.term.grid().history_size();
        let offset = history.saturating_sub(range.start);
        self.term.scroll_display(Scroll::Delta(
            offset as i32 - self.term.grid().display_offset() as i32,
        ));
        self.anchor_viewport();
        self.version = self.version.wrapping_add(1);
    }

    pub fn select_command_block(&mut self, id: u64) {
        let Some(block) = self
            .block_ranges()
            .into_iter()
            .find(|block| block.marker.id == id)
        else {
            return;
        };
        let grid = self.term.grid();
        let start = Point::new(
            Line(block.header as i32 - grid.history_size() as i32),
            Column(0),
        );
        let last = Line(block.output.end as i32 - grid.history_size() as i32 - 1);
        let end = Point::new(last, Column(grid[last].line_length().0.saturating_sub(1)));
        let trailing_newline = self
            .blocks
            .items
            .iter()
            .any(|item| item.id == id && item.end.is_some())
            || last < grid.cursor.point.line;
        let mut selection = Selection::new(SelectionType::Simple, start, Side::Left);
        selection.update(end, Side::Right);
        self.selection_anchor = self.selection_anchor(&selection);
        self.selection_origin = self
            .selection_anchor
            .as_ref()
            .map(reading::SelectionOrigin::from);
        self.selection_kind = Some(SelectionKind::CommandBlock { trailing_newline });
        self.term.selection = Some(selection);
        self.selection_invalidated = false;
        self.version = self.version.wrapping_add(1);
    }

    pub(super) fn clear_command_blocks(&mut self, screen_only: bool) {
        let history = self.term.grid().history_size();
        self.blocks.items.retain(|block| {
            screen_only
                && self
                    .main_tracker
                    .positions
                    .get(&block.header.id)
                    .is_some_and(|row| row < history)
        });
    }

    pub(super) fn blocks_before_reflow(
        &self,
        map: &super::reflow::LogicalMap<'_>,
    ) -> Vec<ReflowBlock> {
        if self.is_alternate() || self.blocks.items.is_empty() {
            return Vec::new();
        }
        let logical = map.block_lines;
        let viewport = self.viewport();
        let anchor = |id| {
            let point = self.point_for(id, 0)?;
            let (_, index) = map.position(&timestamps::address(self.term.grid(), point.line))?;
            Some(ReflowAnchor {
                from_end: logical.len() - index - 1,
                fingerprint: logical[index].fingerprint,
            })
        };
        self.blocks
            .items
            .iter()
            .filter(|block| self.stamp_valid(block.header))
            .filter_map(|block| {
                Some(ReflowBlock {
                    details: block.details.clone(),
                    id: block.id,
                    header: anchor(block.header.id)?,
                    start: anchor(block.start)?,
                    start_after: block.start_after,
                    end: match block.end {
                        Some(end) => Some(anchor(end)?),
                        None => None,
                    },
                    collapsed: block.collapsed,
                    header_row: block
                        .collapsed
                        .then(|| {
                            self.point_for(block.header.id, 0)
                                .and_then(|point| viewport.row(point.line))
                        })
                        .flatten(),
                })
            })
            .collect()
    }

    pub(super) fn blocks_after_reflow(
        &mut self,
        map: &super::reflow::LogicalMap<'_>,
        previous: Vec<ReflowBlock>,
    ) -> Option<u64> {
        self.blocks.items.clear();
        if previous.is_empty() || self.is_alternate() {
            return None;
        }
        let logical = map.block_lines;
        let grid = self.term.grid();
        let row = |anchor: &ReflowAnchor, last: bool| {
            let line = logical.get(logical.len().checked_sub(anchor.from_end + 1)?)?;
            if line.fingerprint != anchor.fingerprint {
                return None;
            }
            let address = if last {
                line.rows.last()?
            } else {
                line.rows.first()?
            };
            let (physical, _) = map.position(address)?;
            Some((physical.0 + grid.history_size() as i32) as usize)
        };
        let mut viewport_row = None;
        let items = previous
            .into_iter()
            .filter_map(|block| {
                let header = self.row_stamp(row(&block.header, false)?)?;
                let rebuilt = Block {
                    details: block.details,
                    id: block.id,
                    header,
                    start: self
                        .main_tracker
                        .id_at(row(&block.start, block.start_after)?)?,
                    start_after: block.start_after,
                    end: match block.end {
                        Some(end) => Some(self.main_tracker.id_at(row(&end, false)?)?),
                        None => None,
                    },
                    collapsed: block.collapsed,
                };
                if viewport_row.is_none()
                    && let Some(screen_row) = block.header_row
                {
                    viewport_row = Some((header.id, screen_row));
                }
                Some(rebuilt)
            })
            .collect();
        self.blocks.items = items;
        if let Some((id, screen_row)) = viewport_row
            && let Some(header) = self.main_tracker.positions.get(&id)
        {
            let projection = self.projection(&self.block_ranges());
            let top = projection.physical(projection.visual(header).saturating_sub(screen_row));
            return self.main_tracker.id_at(top);
        }
        None
    }

    pub(super) fn begin_command_block(
        &mut self,
        header: Option<reading::RowStamp>,
        start: Option<(u64, usize)>,
        command: String,
    ) {
        let (Some(header), Some((start, column))) = (header, start) else {
            return;
        };
        self.finish_command_block();
        self.blocks
            .items
            .retain(|block| self.main_tracker.positions.contains_key(&block.header.id));
        if self.blocks.items.len() == MAX_BLOCKS {
            self.blocks.items.remove(0);
        }
        self.blocks.next_id = self.blocks.next_id.wrapping_add(1);
        self.blocks.items.push(Block {
            details: Arc::new(CommandDetails {
                command,
                cwd: self.shell.snapshot.cwd.clone(),
                result: CommandResult::Running,
            }),
            id: self.blocks.next_id,
            header,
            start,
            start_after: column != 0,
            end: None,
            collapsed: false,
        });
    }

    pub(super) fn finish_command_block(&mut self) {
        let grid = self.term.grid();
        let end = self
            .main_tracker
            .id_at((grid.cursor.point.line.0 + grid.history_size() as i32) as usize);
        if let Some(block) = self
            .blocks
            .items
            .last_mut()
            .filter(|block| block.end.is_none())
        {
            block.end = end;
            if matches!(block.details.result, CommandResult::Running) {
                Arc::make_mut(&mut block.details).result = CommandResult::Unknown;
            }
        }
    }

    fn block_ranges(&self) -> Vec<BlockRange> {
        if self
            .term
            .mode()
            .intersects(TermMode::ALT_SCREEN | TermMode::MOUSE_MODE)
        {
            return Vec::new();
        }
        let grid = self.term.grid();
        let live_end = || {
            (0..grid.screen_lines())
                .rev()
                .find(|&row| self.line_times.get(grid, Line(row as i32)).is_some())
                .map_or(grid.history_size(), |row| grid.history_size() + row + 1)
        };
        let mut ranges = Vec::new();
        let mut previous_end = 0;
        for block in &self.blocks.items {
            let Some(header) = self.main_tracker.positions.get(&block.header.id) else {
                continue;
            };
            let Some(start) = self.main_tracker.positions.get(&block.start) else {
                continue;
            };
            if !self.stamp_valid(block.header) {
                continue;
            }
            let end = match block.end {
                Some(id) => match self.main_tracker.positions.get(&id) {
                    Some(row) => row,
                    None => continue,
                },
                None => live_end(),
            };
            let start = (start + usize::from(block.start_after))
                .max(header + 1)
                .max(previous_end);
            if start > end {
                continue;
            }
            ranges.push(BlockRange {
                details: block.details.clone(),
                marker: FoldMarker {
                    id: block.id,
                    collapsed: block.collapsed,
                    output_rows: end - start,
                },
                header,
                output: start..end,
            });
            previous_end = end;
        }
        ranges
    }

    fn projection(&self, blocks: &[BlockRange]) -> Projection {
        let hidden: Vec<_> = blocks
            .iter()
            .filter(|block| block.marker.collapsed)
            .map(|block| block.output.clone())
            .collect();
        let grid = self.term.grid();
        let total = grid.total_lines() - hidden.iter().map(Range::len).sum::<usize>();
        let mut projection = Projection {
            hidden,
            total,
            history: 0,
        };
        // Folded live rows become blank space below the cursor. Keeping the raw
        // screen's top prevents a click near the bottom from jumping the header.
        projection.history = projection.visual(grid.history_size());
        projection
    }

    pub(super) fn viewport(&self) -> Viewport {
        let grid = self.term.grid();
        let markers = self.block_ranges();
        let projection = self.projection(&markers);
        let history = projection.history;
        let start = if grid.display_offset() == 0 {
            history
        } else {
            projection
                .visual(grid.history_size() - grid.display_offset())
                .min(history)
        };
        let rows = usize::from(self.size.rows)
            + usize::from(self.scroll_fraction > 0. && !self.is_alternate());
        let lines = (start..(start + rows).min(projection.total))
            .map(|row| Line(projection.physical(row) as i32 - grid.history_size() as i32))
            .collect();
        Viewport {
            lines,
            history,
            offset: history - start,
            markers,
            raw_history: grid.history_size(),
        }
    }

    pub(super) fn scroll_projected(&mut self, offset: usize) {
        self.scroll_fraction = 0.;
        let projection = self.projection(&self.block_ranges());
        let history = projection.history;
        let offset = offset.min(history);
        if offset == 0 {
            self.new_output = false;
        }
        let raw_offset = if offset == 0 {
            0
        } else {
            self.term
                .grid()
                .history_size()
                .saturating_sub(projection.physical(history - offset))
        };
        self.term.scroll_display(Scroll::Delta(
            raw_offset as i32 - self.term.grid().display_offset() as i32,
        ));
    }

    pub fn toggle_command_fold(&mut self, id: u64) {
        if !self
            .block_ranges()
            .iter()
            .any(|block| block.marker.id == id)
        {
            return;
        }
        if let Some(block) = self.blocks.items.iter_mut().find(|block| block.id == id) {
            block.collapsed = !block.collapsed;
            self.version = self.version.wrapping_add(1);
        }
    }

    pub(super) fn expand_at(&mut self, line: Line) {
        let row = (line.0 + self.term.grid().history_size() as i32) as usize;
        let ids: Vec<_> = self
            .block_ranges()
            .iter()
            .filter(|block| block.output.contains(&row))
            .map(|block| block.marker.id)
            .collect();
        for block in &mut self.blocks.items {
            if ids.contains(&block.id) {
                block.collapsed = false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(terminal: &Terminal) -> Vec<String> {
        terminal
            .snapshot()
            .lines
            .iter()
            .map(|line| {
                line.cells
                    .iter()
                    .map(|cell| cell.text.as_str())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    #[test]
    fn width_reflow_shares_the_grid_scan_with_timestamps_blocks_and_reading() {
        let mut term = Terminal::new(22, 5, 100);
        command(
            &mut term,
            "echo example",
            &"some long line to wrap\r\n".repeat(14),
        );
        term.scroll(5);
        assert!(!term.blocks.items.is_empty());
        timestamps::LOGICAL_SCANS.set(0);
        term.resize(16, 5);
        assert_eq!(timestamps::LOGICAL_SCANS.get(), 2);
        assert!(!term.blocks.items.is_empty());
        assert!(term.snapshot().display_offset > 0);
    }

    fn command(terminal: &mut Terminal, command: &str, output: &str) {
        terminal.feed(
            format!(
                "\x1b]133;A\x07$ \x1b]133;B\x07{command}\r\n\x1b]133;C\x07{output}\x1b]133;D;0\x07"
            )
            .as_bytes(),
        );
    }

    #[test]
    fn copying_block_output_preserves_emitted_spaces_without_screen_padding() {
        let mut terminal = Terminal::new(40, 8, 100);
        command(&mut terminal, "printf", "   \r\nend  \r\n");
        let block = &terminal.snapshot().command_blocks[0];
        assert_eq!(
            terminal
                .copy_command_block(block.marker.id, BlockCopy::Output)
                .as_deref(),
            Some("   \nend  \n")
        );
    }

    #[test]
    fn command_results_cover_empty_output_failures_and_missing_completion() {
        let mut terminal = Terminal::new(40, 12, 100);
        terminal.feed(b"\x1b]133;A\x07$ \x1b]133;B\x07false\r\n\x1b]133;C\x07\x1b]133;D;9\x07");
        let first = terminal.snapshot().command_blocks[0].clone();
        assert_eq!(first.marker.output_rows, 0);
        assert!(matches!(
            first.details.result,
            CommandResult::Finished {
                exit_status: Some(9),
                ..
            }
        ));
        assert_eq!(
            terminal
                .copy_command_block(first.marker.id, BlockCopy::Output)
                .as_deref(),
            Some("")
        );
        terminal.feed(b"\x1b]133;A\x07$ \x1b]133;B\x07run\r\n\x1b]133;C\x07partial");
        let id = terminal.snapshot().command_blocks.last().unwrap().marker.id;
        assert_eq!(
            terminal
                .copy_command_block(id, BlockCopy::Output)
                .as_deref(),
            Some("partial")
        );
        terminal.feed(b"\r\n\x1b]133;A\x07next> ");
        assert_eq!(
            terminal
                .snapshot()
                .command_blocks
                .last()
                .unwrap()
                .details
                .result,
            CommandResult::Unknown
        );
    }

    #[test]
    fn command_blocks_keep_execution_directory_and_report_only_shell_results() {
        let mut terminal = Terminal::new(40, 8, 100);
        terminal.feed(b"\x1b]7;file://host/old\x07\x1b]133;A\x07$ \x1b]133;B\x07build\r\n\x1b]133;C\x07error in text\r\n");
        let block = terminal.snapshot().command_blocks[0].clone();
        assert_eq!(block.details.command, "build");
        assert_eq!(block.details.cwd.as_deref(), Some("/old"));
        assert_eq!(block.details.result, CommandResult::Running);
        terminal.feed(b"\x1b]7;file://host/new\x07\x1b]133;D;0\x07");
        let details = terminal.snapshot().command_blocks[0].details.clone();
        assert_eq!(details.cwd.as_deref(), Some("/old"));
        assert!(matches!(
            details.result,
            CommandResult::Finished {
                exit_status: Some(0),
                ..
            }
        ));
        terminal.toggle_command_fold(block.marker.id);
        assert_eq!(
            terminal
                .copy_command_block(block.marker.id, BlockCopy::Command)
                .as_deref(),
            Some("build")
        );
        assert_eq!(
            terminal
                .copy_command_block(block.marker.id, BlockCopy::Output)
                .as_deref(),
            Some("error in text\n")
        );
        terminal.resize(20, 8);
        assert_eq!(
            terminal.snapshot().command_blocks[0].details.cwd.as_deref(),
            Some("/old")
        );
    }

    #[test]
    fn search_navigation_keeps_a_first_row_match_in_place_inside_command_output() {
        let mut terminal = Terminal::new(30, 4, 100);
        command(
            &mut terminal,
            "long output",
            "one\r\nneedle\r\nthree\r\nfour\r\nfive\r\nsix\r\n",
        );
        let top = terminal.snapshot().history_size - 2;
        terminal.scroll_to(top);
        terminal.start_search("needle".into(), true);
        while !terminal.search_step(256).complete {}
        assert!(terminal.search_next(false));
        let snapshot = terminal.snapshot();
        assert_eq!(snapshot.display_offset, top);
        assert_eq!(
            snapshot.lines[0].cells[0].search_highlight,
            Some(SearchHighlight::Current)
        );
    }

    #[test]
    fn scrollbar_search_markers_project_folded_matches_onto_the_command() {
        let mut t = Terminal::new(24, 3, 100);
        command(&mut t, "first", "hit\r\nhit\r\n");
        command(&mut t, "second", "hit\r\n");
        t.start_search("hit".into(), true);
        while !t.search_step(64).complete {}
        assert_eq!(&*t.snapshot().search_markers.rows, &[1, 2, 4]);
        t.toggle_command_fold(1);
        assert_eq!(&*t.snapshot().search_markers.rows, &[0, 2]);
        t.start_scoped_search("hit".into(), true, SearchScope::Command(1));
        while !t.search_step(64).complete {}
        assert_eq!(&*t.snapshot().search_markers.rows, &[0]);
        assert!(t.search_next(false));
        let marks = t.snapshot().search_markers;
        assert_eq!(&*marks.rows, &[1, 2]);
        assert_eq!(marks.current, Some(1));
    }

    #[test]
    fn command_search_stays_scoped_across_navigation_output_and_reflow() {
        let mut terminal = Terminal::new(30, 6, 100);
        command(&mut terminal, "first", "needle old\r\n");
        command(&mut terminal, "second", "needle chosen\r\n");
        terminal.start_scoped_search("needle".into(), true, SearchScope::Command(2));
        while !terminal.search_step(256).complete {}
        assert_eq!(terminal.search_status().matches, 1);
        assert!(terminal.search_next(false));
        terminal.scroll_to(100);
        command(&mut terminal, "third", "needle later\r\n");
        while !terminal.search_step(256).complete {}
        assert_eq!(terminal.search_status().matches, 1);
        terminal.resize(20, 6);
        while !terminal.search_step(256).complete {}
        assert_eq!(terminal.search_status().scope, SearchScope::Command(2));
        assert_eq!(terminal.search_status().matches, 1);
        assert_eq!(terminal.search_status().current, Some(0));
        terminal.feed(b"\x1bc");
        let status = terminal.search_step(256);
        assert!(status.scope_unavailable);
        assert!(status.complete);
        assert_eq!(status.matches, 0);
    }

    #[test]
    fn sticky_heading_returns_to_its_command_and_output_notice_ignores_controls() {
        let mut terminal = Terminal::new(30, 4, 100);
        command(
            &mut terminal,
            "long command",
            "a\r\nb\r\nc\r\nd\r\ne\r\nf\r\n",
        );
        let heading = terminal.snapshot().sticky_command.unwrap();
        assert_eq!(heading.text, "$ long command");
        terminal.reveal_command_block(heading.id);
        assert!(lines(&terminal)[0].contains("long command"));
        terminal.feed(b"\x1b[?25h\x1b]0;title\x07");
        assert!(!terminal.snapshot().new_output);
        terminal.feed(b"progress\rprogress");
        assert!(terminal.snapshot().new_output);
        terminal.scroll_to(0);
        assert!(!terminal.snapshot().new_output);
    }

    #[test]
    fn block_selection_copies_the_visible_command_and_output_without_padding_or_next_prompt() {
        let mut terminal = Terminal::new(24, 8, 100);
        terminal.set_command_output_cache(false);
        command(&mut terminal, "printf result", "one\r\n\r\nlast\r\n");
        terminal.feed(b"\x1b]133;A\x07next> \x1b]133;B\x07");
        terminal.select_command_block(1);
        assert_eq!(
            terminal.selected_text().as_deref(),
            Some("$ printf result\none\n\nlast\n")
        );
        terminal.toggle_command_fold(1);
        assert_eq!(
            terminal.selected_text().as_deref(),
            Some("$ printf result\none\n\nlast\n")
        );
        terminal.select_command_block(1);
        assert!(terminal.snapshot().command_blocks[0].marker.collapsed);
    }

    #[test]
    fn guides_share_selection_boundaries_and_include_output_blank_lines() {
        let mut terminal = Terminal::new(24, 10, 100);
        command(&mut terminal, "first", "one\r\n\r\n");
        command(&mut terminal, "second", "two\r\n");
        terminal.feed(b"next> ");
        let snapshot = terminal.snapshot();
        assert_eq!(
            snapshot
                .command_blocks
                .iter()
                .map(|block| block.rows.clone())
                .collect::<Vec<_>>(),
            vec![0..3, 3..5]
        );
        assert!(snapshot.command_blocks.iter().all(|block| !block.selected
            && block.ends_here
            && block.header == Some(block.rows.start)));
        terminal.begin_selection(0, 1, SelectionKind::Word);
        assert_eq!(
            terminal
                .snapshot()
                .command_blocks
                .iter()
                .map(|block| block.selected)
                .collect::<Vec<_>>(),
            vec![true, false]
        );
        terminal.update_selection(2, 4);
        assert!(
            terminal
                .snapshot()
                .command_blocks
                .iter()
                .all(|block| block.selected)
        );
        terminal.clear_selection();
        terminal.toggle_command_fold(1);
        let snapshot = terminal.snapshot();
        assert_eq!(
            snapshot
                .command_blocks
                .iter()
                .map(|block| block.rows.clone())
                .collect::<Vec<_>>(),
            vec![0..1, 1..3]
        );
        terminal.select_command_block(1);
        assert!(terminal.snapshot().command_blocks[0].selected);
        assert!(!terminal.snapshot().command_blocks[1].selected);
    }

    #[test]
    fn offscreen_headers_still_have_a_guide_and_can_select_the_whole_retained_block() {
        let mut terminal = Terminal::new(24, 5, 100);
        command(
            &mut terminal,
            "many",
            &(0..20).map(|n| format!("row {n}\r\n")).collect::<String>(),
        );
        terminal.feed(b"next> ");
        let snapshot = terminal.snapshot();
        let block = &snapshot.command_blocks[0];
        assert_eq!(block.rows, 0..4);
        assert_eq!(block.header, None);
        assert!(block.ends_here);
        terminal.select_command_block(1);
        assert!(
            terminal
                .selected_text()
                .unwrap()
                .starts_with("$ many\nrow 0\n")
        );
        assert!(terminal.selected_text().unwrap().ends_with("row 19\n"));
        terminal.scroll_to(usize::MAX);
        let snapshot = terminal.snapshot();
        assert_eq!(snapshot.command_blocks[0].header, Some(0));
        assert!(!snapshot.command_blocks[0].ends_here);
        assert!(snapshot.command_blocks[0].selected);
        terminal.freeze();
        terminal.select_command_block(1);
        assert!(terminal.selected_text().unwrap().contains("row 19\n"));
    }

    #[test]
    fn block_copy_joins_soft_wraps_and_preserves_unicode_after_reflow() {
        let mut terminal = Terminal::new(12, 10, 100);
        command(&mut terminal, "echo 中文文字", "一行很长的输出文字\r\n\r\n");
        let expected = "$ echo 中文文字\n一行很长的输出文字\n\n";
        terminal.select_command_block(1);
        assert_eq!(terminal.selected_text().as_deref(), Some(expected));
        terminal.resize(30, 10);
        terminal.select_command_block(1);
        assert_eq!(terminal.selected_text().as_deref(), Some(expected));
    }

    #[test]
    fn block_selection_does_not_grow_with_output_and_is_invalidated_by_overwrites() {
        let mut terminal = Terminal::new(24, 8, 100);
        terminal.feed(b"\x1b]133;A\x07$ \x1b]133;B\x07pull\r\n\x1b]133;C\x07one 10%\r\ntwo 20%");
        terminal.select_command_block(1);
        assert_eq!(
            terminal.selected_text().as_deref(),
            Some("$ pull\none 10%\ntwo 20%")
        );
        terminal.feed(b"\r\nthree 30%\r\n");
        assert_eq!(
            terminal.selected_text().as_deref(),
            Some("$ pull\none 10%\ntwo 20%")
        );
        terminal.feed(b"\x1b[2A\rtwo 40%");
        assert!(terminal.selected_text().is_none());
        assert!(terminal.snapshot().selection_invalidated);
    }

    #[test]
    fn folding_keeps_the_clicked_header_at_the_same_screen_row() {
        let mut terminal = Terminal::new(24, 8, 100);
        for _ in 0..20 {
            terminal.feed(b"older\r\n");
        }
        command(&mut terminal, "first", "one\r\ntwo\r\nthree\r\n");
        terminal.feed(b"$ ");
        let before = lines(&terminal)
            .iter()
            .position(|line| line == "$ first")
            .unwrap();
        terminal.toggle_command_fold(1);
        assert_eq!(
            lines(&terminal).iter().position(|line| line == "$ first"),
            Some(before)
        );
        terminal.toggle_command_fold(1);
        assert_eq!(
            lines(&terminal).iter().position(|line| line == "$ first"),
            Some(before)
        );
    }

    #[test]
    fn folding_keeps_the_command_and_next_prompt_without_deleting_output() {
        let mut terminal = Terminal::new(32, 8, 100);
        command(&mut terminal, "printf hello", "hello\r\n\r\nworld\r\n");
        terminal.feed(b"\x1b]133;A\x07$ \x1b]133;B\x07");
        let original = lines(&terminal);
        terminal.toggle_command_fold(1);
        assert_eq!(&lines(&terminal)[..2], &["$ printf hello", "$"]);
        terminal.toggle_command_fold(1);
        assert_eq!(lines(&terminal), original);
    }

    #[test]
    fn running_output_stays_folded_and_cache_is_not_required() {
        let mut terminal = Terminal::new(32, 8, 100);
        terminal.set_command_output_cache(false);
        terminal.feed(b"\x1b]133;A\x07$ \x1b]133;B\x07pull\r\n\x1b]133;C\x07one 0%\r\ntwo 0%\r\n");
        terminal.toggle_command_fold(1);
        assert!(lines(&terminal).iter().all(|line| !line.contains("0%")));
        terminal.feed(b"\x1b[2A\r\x1b[2Kone 100%\r\n\x1b[2Ktwo 100%\r\n\x1b]133;D;0\x07\x1b]133;A\x07$ \x1b]133;B\x07");
        assert_eq!(&lines(&terminal)[..2], &["$ pull", "$"]);
        terminal.toggle_command_fold(1);
        assert_eq!(&lines(&terminal)[1..3], &["one 100%", "two 100%"]);
        assert!(terminal.snapshot().shell.commands[0].output.is_none());
    }

    #[test]
    fn folded_rows_keep_selection_search_and_scroll_coordinates() {
        let mut terminal = Terminal::new(24, 4, 100);
        command(&mut terminal, "first", "hidden needle\r\n\r\n");
        command(&mut terminal, "second", "visible needle\r\n");
        terminal.feed(b"$ ");
        terminal.toggle_command_fold(1);
        terminal.scroll_to(usize::MAX);
        assert_eq!(
            &lines(&terminal)[..3],
            &["$ first", "$ second", "visible needle"]
        );
        terminal.begin_selection(0, 2, SelectionKind::Line);
        assert_eq!(
            terminal.selected_text().as_deref().map(str::trim_end),
            Some("visible needle")
        );
        terminal.clear_selection();
        terminal.start_search("hidden needle".into(), true);
        while !terminal.search_step(100).complete {}
        assert!(terminal.search_next(false));
        assert!(lines(&terminal).iter().any(|line| line == "hidden needle"));
        assert!(
            terminal
                .snapshot()
                .lines
                .iter()
                .flat_map(|line| line.cells.iter())
                .any(|cell| cell.search_highlight == Some(SearchHighlight::Current))
        );
        terminal.toggle_command_fold(1);
        terminal.select_command_output(Some(1));
        assert_eq!(
            terminal.selected_text().as_deref(),
            Some("hidden needle\n\n")
        );
        assert!(lines(&terminal).iter().any(|line| line == "hidden needle"));
    }

    #[test]
    fn reflow_keeps_command_wrapping_and_fold_boundaries() {
        let mut terminal = Terminal::new(32, 10, 100);
        command(
            &mut terminal,
            "a long command with 中文",
            "output to hide\r\n\r\n",
        );
        terminal.feed(b"\x1b]133;A\x07$ \x1b]133;B\x07");
        terminal.toggle_command_fold(1);
        for columns in [12, 48, 8, 32] {
            terminal.resize(columns, 10);
            let snapshot = terminal.snapshot();
            assert!(
                snapshot
                    .command_blocks
                    .iter()
                    .any(|block| block.marker.collapsed),
                "width {columns}: {:?}",
                lines(&terminal)
            );
            assert!(!lines(&terminal).iter().any(|line| line.contains("output")));
        }
        terminal.toggle_command_fold(1);
        assert!(lines(&terminal).iter().any(|line| line == "output to hide"));
    }

    #[test]
    fn screen_reset_cannot_reuse_an_old_fold_for_identical_command_text() {
        for reset in ["\x1bc", "\x1b[2J\x1b[H"] {
            let mut terminal = Terminal::new(24, 6, 100);
            command(&mut terminal, "first", "old output\r\n");
            terminal.toggle_command_fold(1);
            terminal.feed(format!("{reset}$ first\r\nnew output\r\n").as_bytes());
            assert!(lines(&terminal).iter().any(|line| line == "new output"));
            let visible = lines(&terminal);
            let command_row = visible.iter().rposition(|line| line == "$ first").unwrap();
            assert!(
                terminal
                    .snapshot()
                    .command_blocks
                    .iter()
                    .all(|block| block.header != Some(command_row))
            );
        }
    }

    #[test]
    fn projected_rows_preserve_semantic_and_occurrence_highlights() {
        let mut terminal = Terminal::new(32, 10, 100);
        command(&mut terminal, "echo hidden", "needle ERROR\r\n");
        command(&mut terminal, "echo needle", "needle ERROR needle\r\n");
        terminal.toggle_command_fold(1);
        terminal.begin_selection(1, 2, SelectionKind::Word);
        let snapshot = terminal.snapshot();
        assert_eq!(terminal.selected_text().as_deref(), Some("needle"));
        assert!(
            snapshot.lines[2].cells[13..19]
                .iter()
                .all(|cell| cell.selection_match)
        );
        assert_eq!(snapshot.lines[2].cells[7].foreground, terminal.theme.error);
        assert!(!snapshot.lines[0].cells[0].selection_match);
    }

    #[test]
    fn full_screen_mouse_programs_and_eviction_never_hide_unrelated_rows() {
        let mut terminal = Terminal::new(24, 6, 4);
        command(&mut terminal, "first", "hidden\r\n");
        terminal.toggle_command_fold(1);
        terminal.feed(b"\x1b[?1000h");
        assert!(lines(&terminal).iter().any(|line| line == "hidden"));
        assert!(terminal.snapshot().command_blocks.is_empty());
        terminal.feed(b"\x1b[?1000l\x1b[?1049hfull screen");
        assert!(terminal.snapshot().command_blocks.is_empty());
        terminal.feed(b"\x1b[?1049l");
        assert!(!lines(&terminal).iter().any(|line| line == "hidden"));
        for _ in 0..30 {
            terminal.feed(b"retained\r\n");
        }
        terminal.scroll_to(usize::MAX);
        assert!(lines(&terminal).iter().all(|line| line == "retained"));
        assert!(terminal.snapshot().command_blocks.is_empty());
    }
}
