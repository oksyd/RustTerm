use super::Workspace;
use gpui::{Context, Window};
use rustterm::config::TabSwitchOrder;
use std::collections::HashSet;

#[derive(Default)]
pub(super) struct TabHistory {
    recent: Vec<usize>,
    // While Ctrl is held, keep the list stable and remember only the final choice.
    pending: Option<usize>,
}

impl TabHistory {
    pub(super) fn visit(&mut self, id: usize) {
        self.finish();
        self.promote(id);
    }

    fn promote(&mut self, id: usize) {
        if let Some(index) = self.recent.iter().position(|&tab| tab == id) {
            self.recent[..=index].rotate_right(1);
        } else {
            self.recent.insert(0, id);
        }
    }

    pub(super) fn finish(&mut self) {
        if let Some(id) = self.pending.take() {
            self.promote(id);
        }
    }

    pub(super) fn sync_open(&mut self, ids: impl Iterator<Item = usize> + Clone) {
        self.finish();
        let mut remaining: HashSet<_> = ids.clone().collect();
        self.recent.retain(|id| remaining.remove(id));
        self.recent.extend(ids.filter(|id| remaining.contains(id)));
    }

    pub(super) fn after_close(
        &mut self,
        order: TabSwitchOrder,
        previous: &[usize],
        active: usize,
        remaining: impl Iterator<Item = usize> + Clone,
    ) -> Option<usize> {
        self.sync_open(remaining.clone());
        let (&current, right) = previous.get(active..)?.split_first()?;
        if remaining.clone().any(|id| id == current) {
            return Some(current);
        }
        match order {
            TabSwitchOrder::RecentlyUsed => self.recent.first().copied(),
            TabSwitchOrder::TabBar => right
                .iter()
                .chain(previous[..active].iter().rev())
                .copied()
                .find(|candidate| remaining.clone().any(|id| id == *candidate)),
        }
    }

    fn step(&mut self, current: usize, reverse: bool) -> Option<usize> {
        let index = self.recent.iter().position(|&id| id == current)?;
        let next = adjacent(index, self.recent.len(), reverse)?;
        self.pending = Some(self.recent[next]);
        self.pending
    }
}

fn adjacent(index: usize, count: usize, reverse: bool) -> Option<usize> {
    if count < 2 || index >= count {
        return None;
    }
    Some(if reverse {
        (index + count - 1) % count
    } else {
        (index + 1) % count
    })
}

impl Workspace {
    pub(super) fn switch_tab(
        &mut self,
        reverse: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.config.settings.tab_switch_order {
            TabSwitchOrder::TabBar => {
                if let Some(next) = adjacent(self.active, self.tabs.len(), reverse) {
                    self.set_active(next, window, cx);
                }
            }
            TabSwitchOrder::RecentlyUsed => {
                if let Some(current) = self.tabs.get(self.active)
                    && let Some(id) = self.tab_history.step(current.id, reverse)
                    && let Some(next) = self.tabs.iter().position(|tab| tab.id == id)
                {
                    self.show_tab(next, window, cx);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closing_tabs_respects_the_selected_order_and_keeps_surviving_focus() {
        let tabs = [10, 20, 30, 40, 50];
        let mut history = TabHistory::default();
        for id in [20, 40, 50, 10, 30] {
            history.visit(id);
        }
        let mut after = |order, active, remaining: &[usize]| {
            history.after_close(order, &tabs, active, remaining.iter().copied())
        };
        // From A to C, close C. B was also closed in this batch.
        assert_eq!(
            after(TabSwitchOrder::RecentlyUsed, 2, &[10, 40, 50]),
            Some(10)
        );
        assert_eq!(after(TabSwitchOrder::TabBar, 2, &[10, 40, 50]), Some(40));
        for order in [TabSwitchOrder::RecentlyUsed, TabSwitchOrder::TabBar] {
            assert_eq!(after(order, 2, &[20, 30]), Some(30));
            assert_eq!(after(order, 4, &[20]), Some(20));
            assert_eq!(after(order, 1, &[]), None);
        }
    }

    #[test]
    fn holding_control_cycles_both_ways_and_only_commits_the_final_tab() {
        let mut history = TabHistory::default();
        for id in [4, 1, 7, 2] {
            history.visit(id);
        }
        assert_eq!(history.step(2, false), Some(7));
        assert_eq!(history.step(7, false), Some(1));
        assert_eq!(history.step(1, true), Some(7));
        assert_eq!(history.step(7, false), Some(1));
        history.finish();
        assert_eq!(history.step(1, false), Some(2));
        history.finish();
        assert_eq!(history.step(2, false), Some(1));
    }

    #[test]
    fn restored_tabs_follow_visited_tabs_and_closed_ids_are_forgotten() {
        let mut history = TabHistory::default();
        history.visit(90);
        history.visit(80);
        history.sync_open([80, 10, 90, 30].into_iter());
        assert_eq!(history.step(80, true), Some(30));
        history.finish();
        assert_eq!(history.step(30, false), Some(80));
        history.sync_open([30, 10].into_iter());
        history.visit(30);
        assert_eq!(history.step(30, false), Some(10));
        assert_eq!(history.step(10, false), Some(30));
        history.sync_open([10].into_iter());
        assert_eq!(history.step(10, false), None);
        history.sync_open([].into_iter());
        assert_eq!(history.step(10, true), None);
    }
}
