use super::Workspace;
use crate::app::{
    pane::{Pane, SessionTool},
    resource_view::{ResourceEvent, ResourceView},
};
use gpui::{App, AppContext, Context, Entity, Subscription, Window};
use rustterm::{config::Uuid, session::ResourceClient};
use std::collections::HashMap;

struct Monitor {
    view: Entity<ResourceView>,
    _subscription: Subscription,
}

#[derive(Default)]
pub(super) struct Resources {
    monitors: HashMap<usize, Option<Monitor>>,
}

impl Workspace {
    fn resource_target(&self, pane: usize, cx: &App) -> Option<(Uuid, Option<ResourceClient>)> {
        let Pane::Terminal(terminal) = self.panes.get(&pane)? else {
            return None;
        };
        let session = terminal.read(cx).resource_session()?;
        Some((session.id(), session.resources()))
    }

    fn resource_view(&self, pane: usize) -> Option<&Entity<ResourceView>> {
        self.resources
            .monitors
            .get(&pane)?
            .as_ref()
            .map(|monitor| &monitor.view)
    }

    fn create_resource_view(
        &mut self,
        pane: usize,
        session: Uuid,
        client: ResourceClient,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let view = cx.new(|cx| ResourceView::new(session, client, self.window_state.clone(), cx));
        let subscription =
            cx.subscribe_in(&view, window, move |this, entity, event, window, cx| {
                if this.resource_view(pane) != Some(entity) {
                    return;
                }
                if matches!(event, ResourceEvent::Close) && pane == this.focused {
                    this.close_utility_panel(window, cx);
                }
            });
        self.resources.monitors.insert(
            pane,
            Some(Monitor {
                view,
                _subscription: subscription,
            }),
        );
    }

    pub(super) fn sync_resources(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let panes: Vec<_> = self.resources.monitors.keys().copied().collect();
        for pane in panes {
            if !self.panes.contains_key(&pane)
                || !self.pane_profiles.get(&pane).is_some_and(|profile| {
                    matches!(profile.connection, rustterm::config::Connection::Ssh(_))
                })
            {
                if let Some(Some(monitor)) = self.resources.monitors.remove(&pane) {
                    monitor.view.update(cx, |view, cx| view.close(cx));
                }
                continue;
            }
            let target = self.resource_target(pane, cx);
            let selected = self.tools.get(&pane) == Some(&SessionTool::Resources);
            let replacing_focused = target.as_ref().is_some_and(|(session, _)| {
                pane == self.focused
                    && selected
                    && self.resource_view(pane).is_some_and(|view| {
                        view.read(cx).session_id() != *session
                            && view.read(cx).owns_focus(window, cx)
                    })
            });
            if let Some((session, client)) = target.as_ref() {
                if self
                    .resource_view(pane)
                    .is_some_and(|view| view.read(cx).session_id() != *session)
                    && let Some(Some(monitor)) = self.resources.monitors.insert(pane, None)
                {
                    monitor.view.update(cx, |view, cx| view.close(cx));
                }
                if selected
                    && self.resource_view(pane).is_none()
                    && let Some(client) = client
                {
                    self.create_resource_view(pane, *session, client.clone(), window, cx);
                }
            }
            if let Some(view) = self.resource_view(pane) {
                let active = pane == self.focused
                    && selected
                    && target.is_some()
                    && !self.lock_screen.locked
                    && !self.closing;
                view.update(cx, |view, cx| view.set_active(active, cx));
            }
            if replacing_focused
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
        let Some((session, client)) = self.resource_target(self.focused, cx) else {
            return;
        };
        self.resources.monitors.entry(self.focused).or_insert(None);
        if self.resource_view(self.focused).is_none()
            && let Some(client) = client
        {
            self.create_resource_view(self.focused, session, client, window, cx);
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
