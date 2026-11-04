//! Active panes receive history reservations before hidden panes. Frozen
//! instances include their retained screen cost, and whole old archives are
//! evicted when a grid itself no longer fits its budget.
//! Reservations are estimated capacity targets, not hard allocation limits:
//! upstream grid caches may survive history reduction until reuse or Term drop.
use super::state::Shared;
use crate::terminal::Terminal;
use std::sync::{Arc, Mutex, OnceLock, Weak, atomic::Ordering};
struct Registry {
    sessions: Vec<Weak<Shared>>,
    bytes: usize,
}
fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        Mutex::new(Registry {
            sessions: Vec::new(),
            bytes: 512 * 1024 * 1024,
        })
    })
}
pub(super) fn register(session: &Arc<Shared>) {
    registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .sessions
        .push(Arc::downgrade(session));
    rebalance();
}
pub(super) fn set_global(bytes: usize) {
    registry().lock().unwrap_or_else(|e| e.into_inner()).bytes = bytes;
    rebalance();
}
fn evict(session: &Shared) {
    session.evicted.store(true, Ordering::Release);
    session.allocated_budget.store(0, Ordering::Release);
    session
        .final_alternate
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    session.actor_stop.cancel();
    session.io_stop.cancel();
    *session
        .command_history
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Arc::from([]);
    session.command_completion.send_replace(None);
    *session
        .shell_state
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = super::state::ShellState::default();
    // Empty the shared cached view as well; the UI observes the new publication
    // and drops any retained Arc when it removes the evicted run.
    session
        .snapshot
        .send_replace(Arc::new(Terminal::new(2, 1, 0).snapshot()));
    session.snapshot_count.fetch_add(1, Ordering::Release);
    session.changed();
}
pub(super) fn rebalance() {
    let mut registry = registry().lock().unwrap_or_else(|e| e.into_inner());
    registry
        .sessions
        .retain(|s| s.upgrade().is_some_and(|s| !s.actor_stop.is_cancelled()));
    let sessions: Vec<_> = registry.sessions.iter().filter_map(Weak::upgrade).collect();
    let reservations: Vec<_> = sessions
        .iter()
        .map(|session| {
            (
                session,
                session.visible.load(Ordering::Acquire),
                session.requested_budget.load(Ordering::Acquire),
            )
        })
        .collect();
    let mut available = registry.bytes;
    for foreground in [true, false] {
        // Capture each request exactly once. Another actor can change its
        // requested limit while waiting for this registry lock; rereading it
        // for both the denominator and share could exceed the global envelope.
        let mut group: Vec<_> = reservations
            .iter()
            .filter(|(_, visible, _)| *visible == foreground)
            .map(|(session, _, requested)| (*session, *requested))
            .collect();
        loop {
            let requested: u128 = group.iter().map(|(_, bytes)| *bytes as u128).sum();
            let allocation = (available as u128).min(requested);
            let share = |bytes: usize| {
                (allocation * bytes as u128)
                    .checked_div(requested)
                    .unwrap_or(0) as usize
            };
            if let Some(index) = group.iter().position(|(session, bytes)| {
                session.read_only.load(Ordering::Acquire)
                    && session.retained_base.load(Ordering::Acquire) > share(*bytes)
            }) {
                evict(group.remove(index).0);
                continue;
            }
            for (session, request) in group {
                let bytes = share(request);
                if session.allocated_budget.swap(bytes, Ordering::AcqRel) != bytes {
                    session.budget_notify.notify_one();
                }
            }
            available -= allocation as usize;
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Session;
    #[test]
    fn global_budget_reclaims_hidden_history_then_oldest_archive_base() {
        let (active, _, _) = Session::create(80, 24, 100).unwrap();
        let (older, _, _) = Session::create(80, 24, 100).unwrap();
        let (newer, _, _) = Session::create(80, 24, 100).unwrap();
        older.set_visible(false);
        newer.set_visible(false);
        Session::set_global_history_budget(128 * 1024 * 1024);
        assert_eq!(active.allocated_history_budget(), 64 * 1024 * 1024);
        assert_eq!(older.allocated_history_budget(), 32 * 1024 * 1024);
        assert_eq!(newer.allocated_history_budget(), 32 * 1024 * 1024);
        for session in [&older, &newer] {
            session.shared.read_only.store(true, Ordering::Release);
            session
                .shared
                .retained_base
                .store(40 * 1024 * 1024, Ordering::Release);
        }
        rebalance();
        assert!(older.is_evicted());
        assert!(!newer.is_evicted());
        assert_eq!(newer.allocated_history_budget(), 64 * 1024 * 1024);
        assert_eq!(older.allocated_history_budget(), 0);
        newer.shared.read_only.store(false, Ordering::Release);
        newer.shared.retained_base.store(0, Ordering::Release);
        // Keep both live sessions in the same priority group for the overflow case.
        newer.set_visible(true);
        active
            .shared
            .requested_budget
            .store(usize::MAX, Ordering::Release);
        newer
            .shared
            .requested_budget
            .store(usize::MAX, Ordering::Release);
        Session::set_global_history_budget(1024);
        assert_eq!(active.allocated_history_budget(), 512);
        assert_eq!(newer.allocated_history_budget(), 512);
        Session::set_global_history_budget(512 * 1024 * 1024);
    }
}
