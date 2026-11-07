//! Window prompts are exclusive: GPUI otherwise replaces the previous prompt.
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

const PROMPT_BUSY: u64 = 1;
const WINDOW_LOCKED: u64 = 2;
const NEXT_GENERATION: u64 = 4;
const FLAGS: u64 = PROMPT_BUSY | WINDOW_LOCKED;

#[derive(Clone, Default)]
pub struct PromptGate(Arc<AtomicU64>);
pub struct PromptPermit {
    state: Arc<AtomicU64>,
    generation: u64,
}
impl PromptGate {
    pub fn acquire(&self) -> Option<PromptPermit> {
        let state = self.0.load(Ordering::Acquire);
        if state & FLAGS != 0 {
            return None;
        }
        self.0
            .compare_exchange(
                state,
                state | PROMPT_BUSY,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .ok()
            .map(|_| PromptPermit {
                state: self.0.clone(),
                generation: state,
            })
    }

    pub fn is_busy(&self) -> bool {
        self.0.load(Ordering::Acquire) & PROMPT_BUSY != 0
    }

    pub fn is_blocked(&self) -> bool {
        self.0.load(Ordering::Acquire) & WINDOW_LOCKED != 0
    }

    fn set_locked(&self, locked: bool) {
        if locked {
            let _ = self
                .0
                .try_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                    (state & WINDOW_LOCKED == 0)
                        .then(|| state.wrapping_add(NEXT_GENERATION) | WINDOW_LOCKED)
                });
        } else {
            self.0.fetch_and(!WINDOW_LOCKED, Ordering::AcqRel);
        }
    }
}

impl PromptPermit {
    /// A result obtained before locking cannot authorize work after unlocking.
    pub fn is_valid(&self) -> bool {
        self.state.load(Ordering::Acquire) & !PROMPT_BUSY == self.generation
    }
}
impl Drop for PromptPermit {
    fn drop(&mut self) {
        self.state.fetch_and(!PROMPT_BUSY, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn prompts_cannot_replace_each_other_and_cancellation_releases_the_gate() {
        let gate = PromptGate::default();
        let other_pane = gate.clone();
        let pending = gate.acquire().unwrap();
        assert!(other_pane.acquire().is_none());
        drop(pending);
        assert!(other_pane.acquire().is_some());
        assert!(!gate.is_busy());
    }

    #[test]
    fn locking_invalidates_pending_prompts_even_after_unlocking() {
        let gate = PromptGate::default();
        let permit = gate.acquire().unwrap();
        assert!(permit.is_valid());
        gate.set_locked(true);
        assert!(gate.is_blocked());
        assert!(gate.is_busy());
        assert!(!permit.is_valid());
        assert!(gate.acquire().is_none());
        gate.set_locked(false);
        assert!(!gate.is_blocked());
        assert!(!permit.is_valid());
        assert!(gate.acquire().is_none());
        drop(permit);
        assert!(gate.acquire().unwrap().is_valid());
    }

    #[test]
    fn releasing_a_prompt_preserves_the_window_lock() {
        let gate = PromptGate::default();
        let permit = gate.acquire().unwrap();
        gate.set_locked(true);
        drop(permit);
        assert!(!gate.is_busy());
        assert!(gate.is_blocked());
        assert!(gate.acquire().is_none());
        gate.set_locked(false);
        assert!(gate.acquire().unwrap().is_valid());
    }
}

#[derive(Clone)]
pub struct WindowState {
    pub prompts: PromptGate,
    pub visibility: crate::app::window_visibility::WindowVisibility,
    diagnostics_dir: Arc<std::path::Path>,
    pub cache_dir: Arc<std::path::Path>,
    pub authentication: tokio::sync::mpsc::Sender<rustterm::session::AuthenticationChallenge>,
    authentication_receiver: Arc<
        std::sync::Mutex<
            Option<tokio::sync::mpsc::Receiver<rustterm::session::AuthenticationChallenge>>,
        >,
    >,
}
impl WindowState {
    pub fn new(window: &gpui::Window, paths: &rustterm::config::AppPaths) -> Self {
        let (authentication, receiver) = tokio::sync::mpsc::channel(8);
        Self {
            prompts: PromptGate::default(),
            visibility: crate::app::window_visibility::WindowVisibility::new(window),
            diagnostics_dir: paths.logs().into(),
            cache_dir: paths.cache.clone().into(),
            authentication,
            authentication_receiver: Arc::new(std::sync::Mutex::new(Some(receiver))),
        }
    }

    pub fn take_authentication_receiver(
        &self,
    ) -> Option<tokio::sync::mpsc::Receiver<rustterm::session::AuthenticationChallenge>> {
        self.authentication_receiver
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    pub fn record(&self, event: &'static str, pane: usize) {
        super::diagnostics::record(&self.diagnostics_dir, event, pane);
    }

    pub fn record_values(&self, event: &'static str, values: impl Into<Vec<u64>>) {
        super::diagnostics::record_values(&self.diagnostics_dir, event, values);
    }

    pub fn record_with_context(
        &self,
        event: &'static str,
        context: Option<&'static str>,
        pane: usize,
    ) {
        super::diagnostics::record_with_context(&self.diagnostics_dir, event, context, pane);
    }

    pub fn set_locked(&self, locked: bool) {
        self.prompts.set_locked(locked);
    }

    pub fn is_locked(&self) -> bool {
        self.prompts.is_blocked()
    }
}
