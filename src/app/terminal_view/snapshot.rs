use super::TerminalView;
use crate::app::pane::PaneEvent;
use gpui::Context;
use rustterm::terminal::Snapshot;
use std::sync::Arc;

impl TerminalView {
    pub(super) fn watch_snapshot(&mut self, cx: &mut Context<Self>) {
        self.snapshot_task = None;
        let Some(session) = self.runs.get(self.viewed_run) else {
            return;
        };
        let id = session.id();
        let generation = self.generation;
        let mut snapshots = session.subscribe_snapshots();
        self.snapshot_task = Some(cx.spawn(async move |this, cx| {
            while snapshots.changed().await.is_ok() {
                let snapshot = snapshots.borrow_and_update().clone();
                let current = this.update(cx, |this, cx| {
                    if this.generation != generation
                        || this
                            .runs
                            .get(this.viewed_run)
                            .is_none_or(|run| run.id() != id)
                    {
                        return false;
                    }
                    if this.visible
                        && this.window_state.visibility.is_visible()
                        && !this.window_state.is_locked()
                        && !this.show_final_alternate
                        && this.apply_snapshot(snapshot, cx)
                    {
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

    pub(super) fn apply_snapshot(
        &mut self,
        snapshot: Arc<Snapshot>,
        cx: &mut Context<Self>,
    ) -> bool {
        if self
            .snapshot
            .as_ref()
            .is_some_and(|old| Arc::ptr_eq(old, &snapshot))
        {
            return false;
        }
        let previous_status = self.status_text();
        let previous_command = self.command_status();
        let previous_dimensions = self
            .snapshot
            .as_ref()
            .map(|snapshot| (snapshot.columns, snapshot.rows));
        self.snapshot = Some(snapshot);
        if previous_status != self.status_text()
            || previous_command != self.command_status()
            || previous_dimensions
                != self
                    .snapshot
                    .as_ref()
                    .map(|snapshot| (snapshot.columns, snapshot.rows))
        {
            cx.emit(PaneEvent::Activity);
        }
        true
    }
}
