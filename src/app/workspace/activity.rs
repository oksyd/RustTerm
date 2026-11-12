use super::Workspace;
use crate::app::{
    pane::{Pane, SessionTool},
    ui::{self, Icon},
};
use gpui::{AnyElement, Context, MouseButton, Window, div, prelude::*, px, rgb, rgba};
use rustterm::i18n::messages as key;
use rustterm::i18n::tr;

impl Workspace {
    pub(super) fn select_tool(&mut self, tool: SessionTool, cx: &mut Context<Self>) {
        self.release_rdp_control(cx);
        self.left_tool = Some(tool);
        self.sidebar_overlay_open = false;
        self.tools.clear();
        self.tools.insert(self.focused, tool);
    }

    pub(super) fn follow_left_tool(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tool) = self.left_tool else {
            return;
        };
        let changed = self.tools.get(&self.focused) != Some(&tool);
        if !changed
            && !(tool == SessionTool::Files
                && !self.files.contains_key(&self.focused)
                && matches!(self.panes.get(&self.focused), Some(Pane::Terminal(view)) if view.read(cx).sftp_client().is_some()))
        {
            return;
        }
        match tool {
            SessionTool::Files => self.open_files(window, cx),
            SessionTool::Commands => self.open_commands(window, cx),
            SessionTool::Resources => self.open_resources(window, cx),
            SessionTool::Docker => self.open_docker(window, cx),
            SessionTool::Ports => {
                self.select_tool(tool, cx);
            }
        }
    }

    pub(super) fn render_panel_buttons(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut buttons = div()
            .id("panel-buttons")
            .h_full()
            .flex_shrink_0()
            .flex()
            .items_center()
            .px_1()
            .gap(px(2.))
            .border_r_1()
            .border_color(rgb(ui::colors().border));
        let terminal = self.panes.get(&self.focused).and_then(|pane| match pane {
            Pane::Terminal(view) => Some(view.read(cx)),
            _ => None,
        });
        let files_available = self.files.contains_key(&self.focused)
            || terminal.is_some_and(|view| view.sftp_client().is_some());
        let docker_available = terminal.is_some_and(|view| view.resource_session().is_some());
        for (id, tool, icon, label, available) in [
            (
                "dock-connections",
                None,
                Icon::Connections,
                key::WORKSPACE_CONNECTIONS,
                true,
            ),
            (
                "dock-files",
                Some(SessionTool::Files),
                Icon::Folder,
                key::SFTP_FILE_MANAGER_DIALOG,
                files_available,
            ),
            (
                "dock-ports",
                Some(SessionTool::Ports),
                Icon::Link,
                key::PORTS_TITLE,
                true,
            ),
            (
                "dock-docker",
                Some(SessionTool::Docker),
                Icon::Container,
                key::DOCKER_TITLE,
                docker_available,
            ),
        ] {
            let selected = tool.map_or(self.sidebar_visible, |tool| self.left_tool == Some(tool));
            // An already open tool can always be closed, including after moving
            // to a session that cannot provide that tool. Its position is stable.
            let enabled = available || selected;
            let color = if selected {
                ui::colors().accent
            } else {
                ui::colors().muted
            };
            let mut tooltip = tr(if tool.is_none() {
                key::WORKSPACE_SHOW_HIDE_SIDEBAR_CTRL_SHIFT_B
            } else {
                label
            })
            .to_owned();
            if !available {
                let reason = if self
                    .pane_profiles
                    .get(&self.focused)
                    .is_some_and(|profile| {
                        matches!(profile.connection, rustterm::config::Connection::Ssh(_))
                    }) {
                    key::WORKSPACE_WAITING_FOR_SSH_CONNECTION
                } else {
                    key::WORKSPACE_TOOL_NEEDS_SSH
                };
                tooltip.push('\n');
                tooltip.push_str(tr(reason));
            }
            let forwards = if tool == Some(SessionTool::Ports) {
                self.background_port_count()
            } else {
                0
            };
            if forwards > 0 {
                tooltip.push('\n');
                tooltip.push_str(&rustterm::trf!(
                    key::WORKSPACE_ENABLED_PORT_COUNT,
                    count = forwards
                ));
            }
            buttons = buttons.child(
                div()
                    .id(id)
                    .relative()
                    .group("panel-button")
                    .h(px(28.))
                    .min_w(px(32.))
                    .flex_shrink_0()
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .gap(px(6.))
                    .rounded(px(4.))
                    .border_1()
                    .border_color(rgba(0))
                    .tab_index(0)
                    .tab_stop(enabled)
                    .text_color(rgb(color))
                    .focus(|s| {
                        s.bg(rgb(ui::colors().selected))
                            .border_color(rgb(ui::colors().accent))
                    })
                    .when(selected, |button| button.bg(rgb(ui::colors().selected)))
                    .when(!enabled, |button| button.opacity(0.4))
                    .when(enabled, |button| {
                        button
                            .cursor_pointer()
                            .hover(|s| {
                                s.bg(rgb(ui::colors().hover))
                                    .text_color(rgb(ui::colors().text))
                            })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                if let Some(tool) = tool {
                                    this.toggle_tool(tool, window, cx);
                                } else {
                                    this.toggle_sidebar(window, cx);
                                }
                            }))
                    })
                    .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                        if !enabled {
                            window.prevent_default();
                        }
                        cx.stop_propagation();
                    })
                    .tooltip(move |_, cx| cx.new(|_| ui::Tooltip(tooltip.clone())).into())
                    .child(ui::icon(icon).text_color(rgb(color)).when(enabled, |icon| {
                        icon.group_hover("panel-button", |s| s.text_color(rgb(ui::colors().text)))
                    }))
                    .when(tool.is_none(), |button| button.child(tr(label)))
                    .when(forwards > 0, |button| {
                        button.child(
                            div()
                                .absolute()
                                .right(px(3.))
                                .top(px(3.))
                                .size(px(4.))
                                .rounded_full()
                                .bg(rgb(ui::colors().accent)),
                        )
                    }),
            );
        }
        buttons.into_any_element()
    }
}
