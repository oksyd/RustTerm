use super::*;
use crate::app::tunnel_panel::TunnelPanel;
use gpui::{App, Task, Timer};
use rustterm::{
    config::Connection,
    session::{ForwardRegistry, ForwardSettings},
};

#[derive(Default)]
pub(super) struct Ports {
    pub registry: ForwardRegistry,
    panels: HashMap<Uuid, Entity<TunnelPanel>>,
    synchronized: HashMap<Uuid, ForwardSettings>,
    poll: Option<Task<()>>,
    status: (usize, usize),
}

impl Workspace {
    pub(super) fn capture_port_settings(&mut self) {
        for (id, controller) in self.ports.registry.connections() {
            let Some(profile) = self.config.profiles.iter_mut().find(|p| p.id == id) else {
                continue;
            };
            let Connection::Ssh(config) = &mut profile.connection else {
                continue;
            };
            if !self.ports.registry.matches_target(id, config) {
                continue;
            }
            let saved = ForwardSettings {
                rules: config.forwards.clone(),
                automatic: config.auto_forward.clone(),
            };
            if self
                .ports
                .synchronized
                .get(&id)
                .is_none_or(|previous| *previous == saved)
            {
                let runtime = controller.settings();
                config.forwards = runtime.rules;
                config.auto_forward = runtime.automatic;
            }
        }
    }

    pub(super) fn start_port_sync(&mut self, cx: &mut Context<Self>) {
        self.ports.poll = Some(cx.spawn(async move |this, cx| {
            loop {
                Timer::after(Duration::from_secs(1)).await;
                if this
                    .update(cx, |this, cx| {
                        if this.closing {
                            return false;
                        }
                        // Keep a form's commit snapshot stable. Discovery keeps
                        // running; its next snapshot is merged after the save.
                        if this.modal_saving {
                            return true;
                        }
                        let mut changed = false;
                        let mut visible = false;
                        for (id, controller) in this.ports.registry.connections() {
                            let Some(profile) =
                                this.config.profiles.iter_mut().find(|p| p.id == id)
                            else {
                                this.ports.registry.remove(id);
                                this.ports.panels.remove(&id);
                                this.ports.synchronized.remove(&id);
                                continue;
                            };
                            let Connection::Ssh(config) = &mut profile.connection else {
                                this.ports.registry.remove(id);
                                this.ports.panels.remove(&id);
                                this.ports.synchronized.remove(&id);
                                continue;
                            };
                            if !this.ports.registry.matches_target(id, config) {
                                this.ports.registry.remove(id);
                                this.ports.panels.remove(&id);
                                this.ports.synchronized.remove(&id);
                                visible = true;
                                continue;
                            }
                            let saved = ForwardSettings {
                                rules: config.forwards.clone(),
                                automatic: config.auto_forward.clone(),
                            };
                            let runtime = controller.settings();
                            let previous = this
                                .ports
                                .synchronized
                                .entry(id)
                                .or_insert_with(|| saved.clone());
                            if saved != *previous {
                                // An explicit connection edit wins over an older
                                // runtime snapshot; do not overwrite a form commit.
                                let merged = merge_edits(previous, &saved, runtime);
                                if controller.configure(merged.clone()).is_ok() {
                                    config.forwards = merged.rules.clone();
                                    config.auto_forward = merged.automatic.clone();
                                    *previous = merged;
                                    changed = true;
                                }
                            } else if runtime != *previous {
                                config.forwards = runtime.rules.clone();
                                config.auto_forward = runtime.automatic.clone();
                                *previous = runtime;
                                changed = true;
                            }
                            let open = this.pane_profiles.iter().any(|(pane, profile)| {
                                profile.id == id
                                    && this.tools.get(pane) == Some(&SessionTool::Ports)
                            });
                            if open
                                && this
                                    .ports
                                    .panels
                                    .get(&id)
                                    .is_none_or(|panel| !panel.read(cx).uses(&controller))
                            {
                                let state = this.window_state.clone();
                                this.ports.panels.insert(
                                    id,
                                    cx.new(|cx| TunnelPanel::new(controller, state, cx)),
                                );
                                visible = true;
                            } else if !open {
                                this.ports.panels.remove(&id);
                            }
                        }
                        if changed {
                            this.save(cx);
                        }
                        // Refresh the shared status count without tying discovery
                        // or listener lifetimes to visibility of this panel.
                        let status = (
                            this.ports.registry.connections().len(),
                            this.background_port_count(),
                        );
                        if status != this.ports.status || visible {
                            this.ports.status = status;
                            cx.notify();
                        }
                        true
                    })
                    .ok()
                    != Some(true)
                {
                    break;
                }
            }
        }));
    }

    pub(super) fn active_ports(&self) -> Option<Entity<TunnelPanel>> {
        if self.tools.get(&self.focused) != Some(&SessionTool::Ports) {
            return None;
        }
        self.pane_profiles
            .get(&self.focused)
            .and_then(|p| self.ports.panels.get(&p.id))
            .cloned()
    }

    pub(super) fn open_ports(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(profile) = self.pane_profiles.get(&self.focused) else {
            return;
        };
        let id = profile.id;
        let Some(controller) = self.ports.registry.get(id) else {
            self.tools.insert(self.focused, SessionTool::Ports);
            cx.notify();
            return;
        };
        let panel = self
            .ports
            .panels
            .entry(id)
            .or_insert_with(|| {
                cx.new(|cx| TunnelPanel::new(controller, self.window_state.clone(), cx))
            })
            .clone();
        self.tools.insert(self.focused, SessionTool::Ports);
        panel.read(cx).focus(window, cx);
        cx.notify();
    }

    pub(super) fn background_port_count(&self) -> usize {
        self.ports
            .registry
            .connections()
            .iter()
            .map(|(_, controller)| {
                controller
                    .statuses()
                    .iter()
                    .filter(|status| status.enabled)
                    .count()
            })
            .sum()
    }

    pub(super) fn open_all_ports(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.lock_screen.locked || self.modal.is_some() {
            return;
        }
        let mut connections = self
            .ports
            .registry
            .connections()
            .into_iter()
            .map(|(id, controller)| {
                let name = self
                    .config
                    .profiles
                    .iter()
                    .find(|p| p.id == id)
                    .map(|p| p.name.clone())
                    .unwrap_or_else(|| "SSH".into());
                (name, controller)
            })
            .collect::<Vec<_>>();
        connections.sort_by(|a, b| a.0.cmp(&b.0));
        if connections.is_empty() {
            return;
        }
        let state = self.window_state.clone();
        self.modal = Some(Modal::Tunnels(
            cx.new(|cx| TunnelPanel::connections(connections, state, cx)),
        ));
        self.modal_focus.focus(window);
        cx.notify();
    }

    pub(super) fn ports_own_focus(&self, window: &Window, cx: &App) -> bool {
        self.active_ports()
            .is_some_and(|panel| panel.read(cx).focus_handle().contains_focused(window, cx))
    }
}

fn merge_edits(
    previous: &ForwardSettings,
    saved: &ForwardSettings,
    mut runtime: ForwardSettings,
) -> ForwardSettings {
    if saved.rules != previous.rules {
        runtime.rules = saved.rules.clone();
    }
    if saved.automatic.enabled != previous.automatic.enabled {
        runtime.automatic.enabled = saved.automatic.enabled;
    }
    if saved.automatic.projects != previous.automatic.projects {
        runtime.automatic.projects = saved.automatic.projects.clone();
    }
    if saved.automatic.services != previous.automatic.services {
        runtime.automatic.services = saved.automatic.services.clone();
    }
    runtime
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn editing_manual_rules_does_not_discard_newly_discovered_policies() {
        let previous = ForwardSettings {
            rules: Vec::new(),
            automatic: Default::default(),
        };
        let mut saved = previous.clone();
        saved.rules =
            rustterm::config::ssh::parse_forwards("L 127.0.0.1:3000 target:3000").unwrap();
        let mut runtime = previous.clone();
        runtime.automatic.projects.push("new-project".into());
        let merged = merge_edits(&previous, &saved, runtime);
        assert_eq!(merged.rules, saved.rules);
        assert_eq!(merged.automatic.projects, ["new-project"]);
    }
}
