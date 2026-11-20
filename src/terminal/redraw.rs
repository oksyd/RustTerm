//! Repainted screen rows are presentation metadata, never extra scrollback.
use super::*;
use alacritty_terminal::{grid::Grid, term::cell::Cell as GridCell};
use std::collections::{HashMap, HashSet};

#[derive(Default)]
pub(super) struct Redraws {
    pending: HashSet<usize>,
    passes: HashMap<usize, u8>,
}

impl Redraws {
    pub fn clear(&mut self) {
        self.pending.clear();
        self.passes.clear();
    }

    pub fn retire(&mut self, address: usize) {
        self.pending.remove(&address);
        self.passes.remove(&address);
    }

    pub fn observe(
        &mut self,
        grid: &Grid<GridCell>,
        previous: Point,
        event: &timestamps::Observation,
    ) {
        let cursor = grid.cursor.point;
        if !event.printed && !event.bulk && cursor < previous {
            if self.pending.len() + self.passes.len() >= usize::from(MAX_ROWS) {
                self.retain_screen(grid);
            }
            let end = if previous.line > cursor.line && previous.column.0 == 0 {
                previous.line.0 - 1
            } else {
                previous.line.0
            };
            for row in cursor.line.0..=end {
                self.pending.insert(timestamps::address(grid, Line(row)));
            }
        }
        if event.printed || event.repeat {
            let address = timestamps::address(grid, cursor.line);
            if self.pending.remove(&address) {
                let passes = self.passes.entry(address).or_default();
                *passes = passes.saturating_add(1);
            }
        }
    }

    pub fn contains(&self, address: usize) -> bool {
        self.passes.get(&address).is_some_and(|passes| *passes >= 2)
    }

    pub fn retain_screen(&mut self, grid: &Grid<GridCell>) {
        if self.pending.is_empty() && self.passes.is_empty() {
            return;
        }
        let live: HashSet<_> = (0..grid.screen_lines())
            .map(|row| timestamps::address(grid, Line(row as i32)))
            .collect();
        self.pending.retain(|address| live.contains(address));
        self.passes.retain(|address, _| live.contains(address));
    }
}
