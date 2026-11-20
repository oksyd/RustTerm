//! Map reading, selection and search anchors through reflow by logical position.
use super::{
    Terminal,
    reading::SelectionOrigin,
    timestamps::{self, LogicalTime},
};
use alacritty_terminal::{
    grid::Dimensions,
    index::{Column, Line, Point, Side},
    selection::{Selection, SelectionType},
    term::cell::Flags,
};
use std::{
    collections::{HashMap, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
};

struct Coordinate {
    from_end: usize,
    fingerprint: u64,
    characters_from_end: usize,
    suffix_fingerprint: u64,
}

struct ReflowSelection {
    start: Coordinate,
    end: Coordinate,
    kind: super::SelectionKind,
    fixed: Option<(Coordinate, Side)>,
    visible: Option<Coordinate>,
}

#[derive(Default)]
enum ReadingAnchor {
    #[default]
    Following,
    Retained(Coordinate),
    Unavailable,
}

struct ReflowMatch {
    start: Coordinate,
    end: Coordinate,
    visible: bool,
}

#[derive(Default)]
pub(super) struct ReflowReading {
    viewport: ReadingAnchor,
    selection: Option<ReflowSelection>,
    current: Option<ReflowMatch>,
}

/// Both views borrow the timestamp scan. Commands include rows up to the
/// cursor; reading anchors exclude unused padding below the last output.
pub(super) struct LogicalMap<'a> {
    pub block_lines: &'a [LogicalTime],
    lines: &'a [LogicalTime],
    top: Line,
    positions: std::cell::OnceCell<HashMap<usize, (Line, usize)>>,
}

impl<'a> LogicalMap<'a> {
    pub fn new(logical: Option<&'a timestamps::LogicalLines>, top: Line) -> Self {
        let (block_lines, lines) = match logical {
            Some(logical) => {
                let last = logical
                    .lines
                    .iter()
                    .rposition(|line| line.has_content || line.time.is_some());
                let blocks = &logical.lines[..=last.unwrap_or(0).max(logical.cursor)];
                let reading = last.map_or(blocks, |index| {
                    &blocks[..(index + 1 + usize::from(blocks[index].linefeed)).min(blocks.len())]
                });
                (blocks, reading)
            }
            None => (&[][..], &[][..]),
        };
        Self {
            block_lines,
            lines,
            top,
            positions: Default::default(),
        }
    }

    pub fn position(&self, address: &usize) -> Option<(Line, usize)> {
        self.positions
            .get_or_init(|| {
                self.block_lines
                    .iter()
                    .enumerate()
                    .flat_map(|(index, line)| {
                        line.rows.iter().map(move |&address| (address, index))
                    })
                    .enumerate()
                    .map(|(n, (address, index))| (address, (Line(self.top.0 + n as i32), index)))
                    .collect()
            })
            .get(address)
            .copied()
    }
    fn points<'b>(
        &'b self,
        terminal: &'b Terminal,
        logical: &'b LogicalTime,
    ) -> impl Iterator<Item = Point> + 'b {
        logical
            .rows
            .iter()
            .filter_map(|address| self.position(address).map(|p| p.0))
            .flat_map(move |line| {
                (0..terminal.content_columns(line).max(1))
                    .map(move |column| Point::new(line, Column(column)))
            })
            .filter(|point| {
                !terminal.term.grid()[*point]
                    .flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            })
    }

    fn coordinate(&self, terminal: &Terminal, mut point: Point) -> Option<Coordinate> {
        if terminal.term.grid()[point]
            .flags
            .contains(Flags::WIDE_CHAR_SPACER)
        {
            point.column = Column(point.column.0.checked_sub(1)?);
        }
        let address = timestamps::address(terminal.term.grid(), point.line);
        let (_, index) = self.position(&address)?;
        let line = self.lines.get(index)?;
        let mut suffix = self
            .points(terminal, line)
            .skip_while(|&p| p != point)
            .peekable();
        suffix.peek()?;
        let (characters_from_end, suffix_fingerprint) = Self::fingerprint(terminal, suffix);
        Some(Coordinate {
            from_end: self.lines.len() - index - 1,
            fingerprint: line.fingerprint,
            characters_from_end,
            suffix_fingerprint,
        })
    }

    fn point(&self, terminal: &Terminal, coordinate: Coordinate) -> Option<Point> {
        let index = self.lines.len().checked_sub(coordinate.from_end + 1)?;
        let line = self.lines.get(index)?;
        let character = self
            .points(terminal, line)
            .count()
            .checked_sub(coordinate.characters_from_end)?;
        let mut suffix = self.points(terminal, line).skip(character);
        let point = suffix.next()?;
        if line.fingerprint != coordinate.fingerprint {
            // Only the oldest logical line can lose a prefix during column reflow.
            // Keep a retained suffix by position, never by finding equal text elsewhere.
            if index != 0
                || Self::fingerprint(terminal, std::iter::once(point).chain(suffix)).1
                    != coordinate.suffix_fingerprint
            {
                return None;
            }
        }
        Some(point)
    }

    fn fingerprint(terminal: &Terminal, points: impl Iterator<Item = Point>) -> (usize, u64) {
        let mut hash = DefaultHasher::new();
        let mut count = 0;
        for point in points {
            let cell = &terminal.term.grid()[point];
            cell.c.hash(&mut hash);
            cell.zerowidth().unwrap_or_default().hash(&mut hash);
            count += 1;
        }
        (count, hash.finish())
    }
}

impl Terminal {
    pub(super) fn reading_before_reflow(&self, map: &LogicalMap<'_>) -> ReflowReading {
        if self.is_alternate() {
            return ReflowReading::default();
        }
        let viewport = self.viewport();
        let reading = self.viewport_anchor.is_some() || viewport.offset > 0;
        let current = self.current_search_points();
        if !reading && current.is_none() && !self.selection_valid(0) {
            return ReflowReading::default();
        }
        let anchor = if reading {
            viewport
                .lines
                .first()
                .and_then(|&line| map.coordinate(self, Point::new(line, Column(0))))
                .map_or(ReadingAnchor::Unavailable, ReadingAnchor::Retained)
        } else {
            ReadingAnchor::Following
        };
        let current = current.and_then(|(start, end)| {
            Some(ReflowMatch {
                start: map.coordinate(self, start)?,
                end: map.coordinate(self, end)?,
                visible: viewport
                    .lines
                    .iter()
                    .any(|&line| line >= start.line && line <= end.line),
            })
        });
        ReflowReading {
            viewport: anchor,
            selection: self.selection_before_reflow(map),
            current,
        }
    }

    pub(super) fn reading_after_reflow(
        &mut self,
        map: &LogicalMap<'_>,
        saved: ReflowReading,
        folded_anchor: Option<u64>,
    ) {
        if !self.is_alternate() {
            self.reading_anchor_after_reflow(saved.viewport, map, folded_anchor);
        }
        let had_selection = self.selection_anchor.is_some();
        self.clear_selection();
        self.selection_invalidated =
            self.selection_after_reflow(saved.selection, map).is_none() && had_selection;
        let current = saved.current.and_then(|saved| {
            let start = map.point(self, saved.start)?;
            let mut end = map.point(self, saved.end)?;
            // Search ranges include the continuation cell of their last wide glyph.
            if self.term.grid()[end].flags.contains(Flags::WIDE_CHAR) {
                end.column = Column((end.column.0 + 1).min(usize::from(self.size.columns) - 1));
            }
            (start <= end).then_some((start, end, saved.visible))
        });
        self.search_after_reflow(current);
    }

    fn reading_anchor_after_reflow(
        &mut self,
        saved: ReadingAnchor,
        map: &LogicalMap,
        folded_anchor: Option<u64>,
    ) {
        let point = match saved {
            ReadingAnchor::Following => {
                if let Some(id) = folded_anchor {
                    // Folding supplies a header to retain; reading restoration
                    // owns the final viewport instead of two restorers competing.
                    self.viewport_anchor = Some(id);
                    self.restore_viewport();
                } else {
                    self.scroll_projected(0);
                    self.anchor_viewport();
                }
                return;
            }
            ReadingAnchor::Retained(coordinate) => map.point(self, coordinate),
            ReadingAnchor::Unavailable => None,
        };
        if let Some(point) = point {
            self.viewport_anchor = self
                .main_tracker
                .id_at((point.line.0 + self.term.grid().history_size() as i32) as usize);
            self.restore_viewport();
        } else {
            self.term
                .scroll_display(alacritty_terminal::grid::Scroll::Top);
            self.viewport_anchor = self.main_tracker.id_at(0);
            self.viewport_evicted = true;
        }
    }

    fn selection_before_reflow(&self, map: &LogicalMap) -> Option<ReflowSelection> {
        if self.is_alternate() || !self.selection_valid(0) {
            return None;
        }
        let (start, end) = self.selection_points()?;
        let fixed = match &self.selection_origin {
            Some(SelectionOrigin::Boundary { pressed, side, .. }) => Some((*pressed, *side)),
            Some(SelectionOrigin::Character { pressed, .. }) => Some((*pressed, Side::Left)),
            Some(SelectionOrigin::Range { start, .. }) => Some((*start, Side::Left)),
            None => None,
        }
        .and_then(|(pressed, side)| {
            Some((
                map.coordinate(self, self.point_for(pressed.0, pressed.1)?)?,
                side,
            ))
        });
        let visible = self
            .viewport()
            .lines
            .iter()
            .copied()
            .find(|&line| line >= start.line && line <= end.line)
            .and_then(|line| {
                map.coordinate(
                    self,
                    Point::new(
                        line,
                        if line == start.line {
                            start.column
                        } else {
                            Column(0)
                        },
                    ),
                )
            });
        Some(ReflowSelection {
            start: map.coordinate(self, start)?,
            end: map.coordinate(self, end)?,
            kind: self.selection_kind?,
            fixed,
            visible,
        })
    }

    fn selection_after_reflow(
        &mut self,
        saved: Option<ReflowSelection>,
        map: &LogicalMap,
    ) -> Option<()> {
        let saved = saved?;
        let start = map.point(self, saved.start)?;
        let end = map.point(self, saved.end)?;
        if start > end {
            return None;
        }
        let mut selection = Selection::new(SelectionType::Simple, start, Side::Left);
        selection.update(end, Side::Right);
        let anchor = self.content_selection_anchor(&selection)?;
        let mut origin = SelectionOrigin::from(&anchor);
        if let Some((coordinate, side)) = saved.fixed
            && let Some(mut point) = map.point(self, coordinate)
        {
            if side == Side::Right && self.term.grid()[point].flags.contains(Flags::WIDE_CHAR) {
                point.column = Column((point.column.0 + 1).min(usize::from(self.size.columns) - 1));
            }
            if let Some(fixed) = self.boundary_origin(point, side) {
                origin = fixed;
            }
        }
        self.selection_anchor = Some(anchor);
        self.selection_origin = Some(origin);
        self.selection_kind = Some(saved.kind);
        self.term.selection = Some(selection);
        if let Some(coordinate) = saved.visible
            && let Some(point) = map.point(self, coordinate)
            && self.viewport().row(point.line).is_none()
        {
            self.viewport_anchor = self
                .main_tracker
                .id_at((point.line.0 + self.term.grid().history_size() as i32) as usize);
            self.restore_viewport();
        }
        Some(())
    }
}
