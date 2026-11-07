mod render;
use crate::app::{
    interaction::WindowState,
    resource_monitor::{ResourceMonitor, ResourceState, memory_percent},
};
use gpui::{App, Context, Entity, EventEmitter, FocusHandle, Focusable, Subscription, Window};
pub(crate) use render::{bytes, percent, rate};

#[derive(Clone, Copy, Debug)]
pub enum ResourceEvent {
    Close,
}

pub struct ResourceView {
    monitor: Entity<ResourceMonitor>,
    window_state: WindowState,
    focus: FocusHandle,
    _subscription: Subscription,
}
impl EventEmitter<ResourceEvent> for ResourceView {}
impl Focusable for ResourceView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}
impl ResourceView {
    pub fn new(
        monitor: Entity<ResourceMonitor>,
        window_state: WindowState,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.observe(&monitor, |_, _, cx| cx.notify());
        Self {
            monitor,
            window_state,
            focus: cx.focus_handle(),
            _subscription: subscription,
        }
    }
    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }
    pub fn owns_focus(&self, window: &Window, cx: &App) -> bool {
        self.focus.contains_focused(window, cx)
    }
    fn retry(&mut self, cx: &mut Context<Self>) {
        self.monitor.update(cx, |monitor, cx| monitor.retry(cx));
    }
    fn select_network(&mut self, name: String, cx: &mut Context<Self>) {
        self.monitor
            .update(cx, |monitor, cx| monitor.select_network(name, cx));
    }
}
