use super::Workspace;
use crate::app::{
    docker_view::{DockerEvent, DockerView},
    pane::{Pane, SessionTool},
};
use gpui::{App, AppContext, Context, Entity, Subscription, Window};
use rustterm::{config::Uuid, docker::DockerClient};
use std::collections::HashMap;

struct Monitor {
    view: Entity<DockerView>,
    _subscription: Subscription,
}

#[derive(Default)]
pub(super) struct Dockers {
    monitors: HashMap<usize, Option<Monitor>>,
    consoles: HashMap<usize, ConsoleLaunch>,
}

impl Workspace {
    pub(super) fn container_resource_source(&self, pane: usize) -> Option<(DockerClient, String)> {
        let launch = self.docker.consoles.get(&pane)?;
        Some((launch.client.clone(), launch.id.clone()))
    }
    fn docker_target(&self, pane: usize, cx: &App) -> Option<(Uuid, Option<DockerClient>)> {
        let Pane::Terminal(terminal) = self.panes.get(&pane)? else {
            return None;
        };
        let session = terminal.read(cx).resource_session()?;
        Some((session.id(), session.docker()))
    }

    fn docker_view(&self, pane: usize) -> Option<&Entity<DockerView>> {
        self.docker
            .monitors
            .get(&pane)?
            .as_ref()
            .map(|monitor| &monitor.view)
    }

    fn create_docker_view(
        &mut self,
        pane: usize,
        session: Uuid,
        client: DockerClient,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let view = cx.new(|cx| {
            DockerView::new(
                session,
                client,
                self.resources.registry.clone(),
                self.window_state.clone(),
                cx,
            )
        });
        let subscription =
            cx.subscribe_in(&view, window, move |this, entity, event, window, cx| {
                if this.docker_view(pane) != Some(entity) {
                    return;
                }
                if let DockerEvent::Console {
                    client,
                    id,
                    name,
                    shell,
                } = event
                {
                    this.open_container(
                        client.clone(),
                        id.clone(),
                        name.clone(),
                        *shell,
                        pane,
                        window,
                        cx,
                    );
                }
                if matches!(event, DockerEvent::Close) && pane == this.focused {
                    this.close_utility_panel(window, cx);
                }
            });
        self.docker.monitors.insert(
            pane,
            Some(Monitor {
                view,
                _subscription: subscription,
            }),
        );
    }

    pub(super) fn sync_docker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.docker
            .consoles
            .retain(|id, _| self.panes.contains_key(id));
        let panes: Vec<_> = self.docker.monitors.keys().copied().collect();
        for pane in panes {
            if !self.panes.contains_key(&pane)
                || !self.pane_profiles.get(&pane).is_some_and(|profile| {
                    matches!(profile.connection, rustterm::config::Connection::Ssh(_))
                })
            {
                if let Some(Some(monitor)) = self.docker.monitors.remove(&pane) {
                    monitor.view.update(cx, |view, cx| view.close(cx));
                }
                continue;
            }
            let target = self.docker_target(pane, cx);
            let selected = self.tools.get(&pane) == Some(&SessionTool::Docker);
            let replacing_focused = target.as_ref().is_some_and(|(session, _)| {
                pane == self.focused
                    && selected
                    && self.docker_view(pane).is_some_and(|view| {
                        view.read(cx).session_id() != *session
                            && view.read(cx).owns_focus(window, cx)
                    })
            });
            if let Some((session, client)) = target.as_ref() {
                if self
                    .docker_view(pane)
                    .is_some_and(|view| view.read(cx).session_id() != *session)
                    && let Some(Some(monitor)) = self.docker.monitors.insert(pane, None)
                {
                    monitor.view.update(cx, |view, cx| view.close(cx));
                }
                if selected
                    && self.docker_view(pane).is_none()
                    && let Some(client) = client
                {
                    self.create_docker_view(pane, *session, client.clone(), window, cx);
                }
            }
            if let Some(view) = self.docker_view(pane) {
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

    pub(super) fn open_docker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.select_tool(SessionTool::Docker, cx);
        let Some((session, client)) = self.docker_target(self.focused, cx) else {
            return;
        };
        self.docker.monitors.entry(self.focused).or_insert(None);
        if self.docker_view(self.focused).is_none()
            && let Some(client) = client
        {
            self.create_docker_view(self.focused, session, client, window, cx);
        }
        self.tools.insert(self.focused, SessionTool::Docker);
        self.sync_docker(window, cx);
        if let Some(view) = self.docker_view(self.focused) {
            view.read(cx).focus_handle().focus(window);
        }
        cx.notify();
    }

    pub(super) fn active_docker(&self) -> Option<Entity<DockerView>> {
        (self.tools.get(&self.focused) == Some(&SessionTool::Docker))
            .then(|| self.docker_view(self.focused).cloned())
            .flatten()
    }
}

#[derive(Clone)]
struct ConsoleLaunch {
    client: DockerClient,
    id: String,
    shell: rustterm::docker::Shell,
}
impl Workspace {
    #[allow(clippy::too_many_arguments)]
    fn open_container(
        &mut self,
        client: DockerClient,
        container: String,
        name: String,
        shell: rustterm::docker::Shell,
        parent: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.lock_screen.locked
            || self.closing
            || self.modal.is_some()
            || self.window_state.prompts.is_busy()
        {
            return;
        }
        let launch = ConsoleLaunch {
            client,
            id: container,
            shell,
        };
        let session = rustterm::session::Session::spawn_container(
            launch.client.clone(),
            launch.id.clone(),
            shell,
            80,
            24,
            self.config.settings.history_lines,
        );
        let id = self.id();
        let host = self
            .pane_profiles
            .get(&parent)
            .map(|p| p.name.as_str())
            .unwrap_or("SSH");
        let title = format!("{name} · {host}");
        let view = cx.new(|cx| {
            let mut view = crate::app::terminal_view::TerminalView::new(
                id,
                title.clone(),
                session,
                &self.config.settings,
                self.window_state.clone(),
                window,
                cx,
            );
            view.set_container(title.clone(), cx);
            view
        });
        let subscription = cx.subscribe_in(&view, window, |this, _, event, window, cx| {
            this.pane_event(event, window, cx)
        });
        self.panes.insert(id, Pane::Terminal(view));
        self.subscriptions.insert(id, subscription);
        self.docker.consoles.insert(id, launch);
        let tab_id = self.id();
        self.tabs.push(super::Tab {
            id: tab_id,
            title,
            layout: crate::app::layout::PaneNode::Leaf(id),
            focused: id,
        });
        self.zoomed = None;
        self.focused = id;
        self.set_active(self.tabs.len() - 1, window, cx);
        cx.notify();
        // No saved connection profile: recovery must never reopen a container console as a host shell.
    }
    pub(super) fn reconnect_container(&mut self, id: usize, cx: &mut Context<Self>) -> bool {
        let Some(launch) = self.docker.consoles.get(&id) else {
            return false;
        };
        match rustterm::session::Session::spawn_container(
            launch.client.clone(),
            launch.id.clone(),
            launch.shell,
            80,
            24,
            self.config.settings.history_lines,
        ) {
            Ok(session) => {
                if let Some(Pane::Terminal(view)) = self.panes.get(&id) {
                    view.update(cx, |view, cx| view.reconnect(session, cx));
                }
            }
            Err(error) => self.error = Some(rustterm::i18n::Message::from_error(error)),
        }
        cx.notify();
        true
    }
}
