//! Selection follows emitted content, rather than the rectangular display buffer.
use super::Terminal;
use alacritty_terminal::{
    grid::Dimensions,
    index::{Column, Line, Point},
};

impl Terminal {
    pub(super) fn content_columns(&self, line: Line) -> usize {
        self.line_times.columns(self.term.grid(), line)
    }

    pub(super) fn selection_columns(&self, line: Line) -> usize {
        if self.is_alternate() {
            return usize::from(self.size.columns);
        }
        // An emitted blank line has a line break, represented by one cell.
        self.content_columns(line).max(usize::from(
            self.line_times.has_linefeed(self.term.grid(), line),
        ))
    }

    pub(super) fn content_end(&self) -> Option<Point> {
        let grid = self.term.grid();
        for row in (grid.topmost_line().0..=grid.bottommost_line().0).rev() {
            let line = Line(row);
            let columns = self.content_columns(line);
            if self.line_times.has_linefeed(grid, line) && line < grid.bottommost_line() {
                // The following row is an exclusive boundary for the emitted LF;
                // it contains no selectable cells until output is written there.
                return Some(Point::new(line + 1, Column(0)));
            }
            if columns > 0 {
                return Some(Point::new(line, Column(columns - 1)));
            }
        }
        None
    }

    pub(super) fn clip_selection(
        &self,
        mut start: Point,
        mut end: Point,
    ) -> Option<(Point, Point)> {
        if self.is_alternate() {
            return Some((start, end));
        }
        end = end.min(self.content_end()?);
        if start > end {
            return None;
        }
        let first_columns = self.selection_columns(start.line);
        if start.column.0 >= first_columns {
            if start.line == end.line {
                return None;
            }
            start.column = Column(self.content_columns(start.line));
        }
        let last_columns = self.selection_columns(end.line);
        end.column = Column(end.column.0.min(last_columns.saturating_sub(1)));
        (start <= end).then_some((start, end))
    }
}
