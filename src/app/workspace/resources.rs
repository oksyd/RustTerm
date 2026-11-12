use super::Workspace;
use crate::app::{
    pane::{Pane, SessionTool},
    resource_monitor::{MetricsView, MonitorInterest, ResourceMonitor, ResourceRegistry},
    resource_view::{ResourceEvent, ResourceView},
};
use gpui::{App, AppContext, Context, Entity, Subscription, Window};
use rustterm::{
    config::Uuid,
    docker::DockerClient,
    session::{ResourceClient, SessionState},
};
use std::collections::HashMap;

struct Monitor {
    session: Uuid,
    model: Entity<ResourceMonitor>,
    interest: MonitorInterest,
    summary: Entity<MetricsView>,
    view: Option<Entity<ResourceView>>,
    _summary_subscription: Subscription,
    view_subscription: Option<Subscription>,
}
#[derive(Default)]
pub(super) struct Resources {
    pub(super) registry: ResourceRegistry,
    monitors: HashMap<usize, Monitor>,
}
enum Target {
    Host(ResourceClient),
    Container(DockerClient, String),
}
impl Workspace {
    fn resource_target(&self, pane: usize, cx: &App) -> Option<(Uuid, Target)> {
        let Pane::Terminal(terminal) = self.panes.get(&pane)? else {
            return None;
        };
        let terminal = terminal.read(cx);
        let session = terminal.monitor_session()?;
        if session.state() != SessionState::Connected {
            return None;
        }
        let target = if terminal.is_container() {
            let (client, id) = self.container_resource_source(pane)?;
            Target::Container(client, id)
        } else {
            Target::Host(session.resources()?)
        };
        Some((session.id(), target))
    }
    fn resource_view(&self, pane: usize) -> Option<&Entity<ResourceView>> {
        self.resources.monitors.get(&pane)?.view.as_ref()
    }
    pub(super) fn resource_summary(&self) -> Option<Entity<MetricsView>> {
        self.config
            .settings
            .status_bar_monitoring
            .then(|| {
                self.resources
                    .monitors
                    .get(&self.focused)
                    .map(|m| m.summary.clone())
            })
            .flatten()
    }
    fn create_resource_monitor(
        &mut self,
        pane: usize,
        session: Uuid,
        target: Target,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let model = match target {
            Target::Host(client) => {
                self.resources
                    .registry
                    .host(session, client, self.window_state.clone(), cx)
            }
            Target::Container(client, id) => {
                self.resources
                    .registry
                    .container(client, id, self.window_state.clone(), cx)
            }
        };
        let interest = model.read(cx).subscribe();
        let summary = cx.new(|cx| MetricsView::new(model.clone(), cx));
        let subscription = cx.subscribe_in(&summary, window, move |this, entity, _, window, cx| {
            if this.focused != pane
                || this
                    .resources
                    .monitors
                    .get(&pane)
                    .is_none_or(|m| &m.summary != entity)
            {
                return;
            }
            if this.lock_screen.locked
                || this.closing
                || this.modal.is_some()
                || this.palette.open
                || this.window_state.prompts.is_busy()
                || this.input_composing(window, cx)
            {
                return;
            }
            this.open_resources(window, cx);
        });
        self.resources.monitors.insert(
            pane,
            Monitor {
                session,
                model,
                interest,
                summary,
                view: None,
                _summary_subscription: subscription,
                view_subscription: None,
            },
        );
    }
    fn create_resource_view(&mut self, pane: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(monitor) = self.resources.monitors.get(&pane) else {
            return;
        };
        let view =
            cx.new(|cx| ResourceView::new(monitor.model.clone(), self.window_state.clone(), cx));
        let subscription =
            cx.subscribe_in(&view, window, move |this, entity, event, window, cx| {
                if this.resource_view(pane) != Some(entity) {
                    return;
                }
                if matches!(event, ResourceEvent::Close) && pane == this.focused {
                    this.close_utility_panel(window, cx);
                }
            });
        let monitor = self.resources.monitors.get_mut(&pane).unwrap();
        monitor.view = Some(view);
        monitor.view_subscription = Some(subscription);
    }
    pub(super) fn sync_resources(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut panes: Vec<_> = self.resources.monitors.keys().copied().collect();
        if !panes.contains(&self.focused) {
            panes.push(self.focused);
        }
        for pane in panes {
            let target = self.resource_target(pane, cx);
            let selected = self.tools.get(&pane) == Some(&SessionTool::Resources);
            let wanted = pane == self.focused
                && (selected || self.config.settings.status_bar_monitoring)
                && !self.lock_screen.locked
                && !self.closing;
            let replace = self.resources.monitors.get(&pane).is_some_and(|m| {
                target
                    .as_ref()
                    .is_none_or(|(session, _)| *session != m.session)
            });
            let restore = replace
                && selected
                && pane == self.focused
                && self
                    .resource_view(pane)
                    .is_some_and(|view| view.read(cx).owns_focus(window, cx));
            if replace {
                self.resources.monitors.remove(&pane);
            }
            if wanted
                && !self.resources.monitors.contains_key(&pane)
                && let Some((session, target)) = target
            {
                self.create_resource_monitor(pane, session, target, window, cx);
            }
            if selected && wanted && self.resource_view(pane).is_none() {
                self.create_resource_view(pane, window, cx);
            }
            if let Some(monitor) = self.resources.monitors.get(&pane) {
                monitor.interest.set(wanted, selected);
            }
            if restore
                && !self.lock_screen.locked
                && self.modal.is_none()
                && !self.palette.open
                && !self.window_state.prompts.is_busy()
            {
                self.restore_focus(window, cx);
            }
        }
    }
    pub(super) fn open_resources(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.select_tool(SessionTool::Resources, cx);
        if self.resource_target(self.focused, cx).is_none() {
            return;
        }
        self.tools.insert(self.focused, SessionTool::Resources);
        self.sync_resources(window, cx);
        if let Some(view) = self.resource_view(self.focused) {
            view.read(cx).focus_handle().focus(window);
        }
        cx.notify();
    }
    pub(super) fn active_resources(&self) -> Option<Entity<ResourceView>> {
        (self.tools.get(&self.focused) == Some(&SessionTool::Resources))
            .then(|| self.resource_view(self.focused).cloned())
            .flatten()
    }
}
