use super::menus::Menu;
use super::sidebar::{rdp_target, telnet_target};
use super::{Tab, Workspace};
use crate::app::pane::{Pane, PaneEvent};
use crate::app::ui;
use crate::app::ui::{Icon, icon, icon_button};
use crate::app::window_frame::WindowFrame;
use gpui::prelude::*;
use gpui::{
    AnyElement, App, Context, Decorations, MouseButton, Window, canvas, div, px, rgb, rgba,
};
use rustterm::config::{Connection, Uuid};
use rustterm::i18n::messages as i18n_key;
use rustterm::i18n::tr;

use super::menus::APPLICATION_MENUS;
fn pane_icon(pane: Option<&Pane>, cx: &App) -> Icon {
    match pane {
        Some(Pane::Redis(_)) => Icon::Redis,
        Some(Pane::Elasticsearch(_)) => Icon::Elasticsearch,
        Some(Pane::Database(view)) => Icon::for_sql_engine(view.read(cx).engine()),
        Some(Pane::Rdp(_)) => Icon::Desktop,
        Some(Pane::Terminal(_)) | None => Icon::Terminal,
    }
}

fn toggle_window_size(window: &Window) {
    if window.is_fullscreen() {
        window.toggle_fullscreen();
    } else {
        window.zoom_window();
    }
}

impl Workspace {
    pub(super) fn render_titlebar(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let client = matches!(window.window_decorations(), Decorations::Client { .. });
        let corners = WindowFrame::new(window).corners;
        let application_menu = self.application_menu().filter(|_| !self.lock_screen.locked);
        let mut bar = div()
            .relative()
            .h(px(ui::TITLE_HEIGHT))
            .flex_shrink_0()
            .flex()
            .items_center()
            .bg(rgb(ui::colors().chrome))
            .rounded_tl(corners.top_left)
            .rounded_tr(corners.top_right)
            .border_b_1()
            .border_color(rgb(ui::colors().border))
            .text_size(px(12.))
            .text_color(rgb(ui::colors().muted))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    if !this.lock_screen.locked && this.menu.take().is_some() {
                        this.restore_popup_focus(window, cx);
                        window.prevent_default();
                        cx.notify();
                    }
                }),
            )
            .child(
                div()
                    .id("window-title")
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .gap(px(7.))
                    .overflow_hidden()
                    .when(application_menu.is_none(), |title| {
                        title
                            .child(
                                icon(Icon::Logo)
                                    .size(px(16.))
                                    .text_color(rgb(ui::colors().brand)),
                            )
                            .child("RustTerm")
                    })
                    .on_mouse_down(MouseButton::Left, move |event, window, _| {
                        if client {
                            if event.click_count == 2 {
                                toggle_window_size(window);
                            } else {
                                window.start_window_move();
                            }
                        }
                    })
                    .on_mouse_down(MouseButton::Right, move |event, window, _| {
                        if client {
                            window.show_window_menu(event.position);
                        }
                    }),
            );
        if !self.lock_screen.locked {
            if let Some(selected) = application_menu {
                let mut categories = div()
                    .id("application-menu-bar")
                    .relative()
                    .h_full()
                    .flex()
                    .flex_shrink_0()
                    .occlude();
                for (index, (category, label)) in APPLICATION_MENUS.into_iter().enumerate() {
                    let entity = cx.entity();
                    categories = categories.child(
                        div()
                            .id(("titlebar-menu-category", index))
                            .relative()
                            .h_full()
                            .flex_shrink_0()
                            .px(px(12.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .when(index == 0, |button| button.rounded_tl(corners.top_left))
                            .text_color(rgb(if selected == category {
                                ui::colors().text
                            } else {
                                ui::colors().muted
                            }))
                            .when(selected == category, |button| {
                                button.bg(rgb(ui::colors().selected))
                            })
                            .hover(|style| {
                                style
                                    .bg(rgb(ui::colors().hover))
                                    .text_color(rgb(ui::colors().text))
                            })
                            .on_hover(cx.listener(move |this, hovered, window, cx| {
                                if *hovered && this.application_menu().is_some() {
                                    this.switch_application_menu(category, window, cx);
                                }
                            }))
                            .on_mouse_down(MouseButton::Left, |_, window, cx| {
                                window.prevent_default();
                                cx.stop_propagation();
                            })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.toggle_menu(category, window, cx);
                            }))
                            .child(tr(label))
                            .child(
                                canvas(
                                    move |bounds, _, cx| {
                                        entity.update(cx, |this, cx| {
                                            this.record_application_menu_bounds(
                                                category, bounds, cx,
                                            )
                                        });
                                    },
                                    |_, _, _, _| {},
                                )
                                .absolute()
                                .inset_0(),
                            ),
                    );
                }
                bar = bar.child(categories);
            } else {
                bar = bar.child(
                    icon_button(
                        "application-menu",
                        Icon::Menu,
                        i18n_key::WORKSPACE_APPLICATION_MENU,
                    )
                    .relative()
                    .occlude()
                    .tab_index(0)
                    .w(px(36.))
                    .h_full()
                    .rounded_tl(corners.top_left)
                    .on_mouse_down(MouseButton::Left, |_, window, cx| {
                        window.prevent_default();
                        cx.stop_propagation();
                    })
                    .on_click(cx.listener(|this, _, window, cx| {
                        cx.stop_propagation();
                        this.toggle_menu(Menu::File, window, cx);
                    })),
                );
            }
        }
        bar = bar.child(div().flex_1());
        if client {
            let controls = window.window_controls();
            if controls.minimize {
                bar = bar.child(
                    icon_button(
                        "window-minimize",
                        Icon::Minimize,
                        i18n_key::WORKSPACE_MINIMIZE,
                    )
                    .relative()
                    .occlude()
                    .w(px(38.))
                    .h_full()
                    .on_click(|_, window, _| window.minimize_window()),
                );
            }
            if controls.maximize || window.is_fullscreen() {
                let (glyph, label) = if window.is_fullscreen() {
                    (Icon::ExitFullscreen, i18n_key::WORKSPACE_EXIT_FULLSCREEN)
                } else if window.is_maximized() {
                    (Icon::Restore, i18n_key::WORKSPACE_RESTORE_WINDOW)
                } else {
                    (Icon::Maximize, i18n_key::WORKSPACE_MAXIMIZE_WINDOW)
                };
                bar = bar.child(
                    icon_button("window-maximize", glyph, label)
                        .relative()
                        .occlude()
                        .w(px(38.))
                        .h_full()
                        .on_click(|_, window, _| toggle_window_size(window)),
                );
            }
            bar = bar.child(
                ui::styled_icon_button(
                    "window-close",
                    Icon::Close,
                    i18n_key::WORKSPACE_CLOSE_WINDOW,
                    ui::ButtonStyle::Danger,
                )
                .relative()
                .occlude()
                .w(px(38.))
                .h_full()
                .rounded_tr(corners.top_right)
                .on_click(cx.listener(|this, _, window, cx| {
                    this.request_exit(window, cx);
                })),
            );
        }
        bar.into_any_element()
    }
    pub(super) fn workspace_tab_title(&self, tab: &Tab) -> String {
        let names: Vec<_> = tab
            .layout
            .leaves()
            .into_iter()
            .map(|id| {
                self.pane_display_name(id)
                    .unwrap_or_else(|| tab.title.clone())
            })
            .collect();
        if names.is_empty() {
            return tab.title.clone();
        }
        let title = names
            .iter()
            .take(2)
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(" / ");
        if names.len() > 2 {
            rustterm::trf!(
                i18n_key::WORKSPACE_ADDITIONAL_TAB_COUNT,
                title = title,
                count = names.len() - 2
            )
        } else {
            title
        }
    }
    fn workspace_tab_label(&self, tab: &Tab) -> AnyElement {
        let leaves = tab.layout.leaves();
        let mut label = div()
            .flex_1()
            .min_w_0()
            .flex()
            .items_center()
            .overflow_hidden();
        for (index, id) in leaves.iter().take(2).enumerate() {
            if index > 0 {
                label = label.child(div().flex_shrink_0().child(" / "));
            }
            let name = self
                .pane_profiles
                .get(id)
                .map(|profile| &profile.name)
                .unwrap_or(&tab.title)
                .clone();
            let suffix = self.session_names.suffix(*id);
            label = label.child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .overflow_hidden()
                    .child(
                        div()
                            .min_w_0()
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(name),
                    )
                    // Keep the distinguishing number visible when the name is clipped.
                    .when(!suffix.is_empty(), |label| {
                        label.child(div().flex_shrink_0().child(suffix))
                    }),
            );
        }
        if leaves.len() > 2 {
            label = label.child(div().flex_shrink_0().child(rustterm::trf!(
                i18n_key::WORKSPACE_ADDITIONAL_TAB_COUNT,
                title = "",
                count = leaves.len() - 2
            )));
        }
        label.into_any_element()
    }

    fn scroll_tabs(&self, right: bool, cx: &mut Context<Self>) {
        let mut offset = self.tab_scroll.offset();
        let distance = (self.tab_scroll.bounds().size.width * 0.7).max(px(150.));
        offset.x = if right {
            offset.x - distance
        } else {
            offset.x + distance
        }
        .clamp(-self.tab_scroll.max_offset().width, px(0.));
        self.tab_scroll.set_offset(offset);
        cx.notify();
    }

    fn render_workspace_tab(
        &self,
        tab: &Tab,
        group: Option<rustterm::config::Uuid>,
        cx: &Context<Self>,
    ) -> AnyElement {
        let id = tab.id;
        let active = self.tabs.get(self.active).is_some_and(|tab| tab.id == id);
        let leaves = tab.layout.leaves();
        let close_ids = leaves.clone();
        let unread = leaves
            .iter()
            .any(|id| self.panes.get(id).is_some_and(|pane| pane.has_unread(cx)));
        let glyph = if leaves.len() > 1 {
            Icon::SplitHorizontal
        } else {
            pane_icon(leaves.first().and_then(|id| self.panes.get(id)), cx)
        };
        let title = self.workspace_tab_title(tab);
        let full_title = leaves
            .iter()
            .map(|id| {
                self.pane_display_name(*id)
                    .unwrap_or_else(|| tab.title.clone())
            })
            .collect::<Vec<_>>()
            .join(" / ");
        let tooltip = if leaves.len() == 1 {
            rustterm::trf!(i18n_key::WORKSPACE_WORKSPACE_NAME, name = full_title)
        } else {
            rustterm::trf!(
                i18n_key::WORKSPACE_NAMED_PANE_COUNT,
                name = full_title,
                count = leaves.len()
            )
        };
        div()
            .id(("tab", id))
            .relative()
            .h_full()
            .min_w(px(150.))
            .max_w(px(if leaves.len() > 1 { 360. } else { 240. }))
            .flex_shrink_0()
            .px(px(12.))
            .flex()
            .items_center()
            .gap(px(8.))
            .cursor_pointer()
            .bg(rgb(if active {
                ui::colors().bg
            } else {
                ui::colors().chrome
            }))
            .border_b_2()
            .border_color(if active {
                rgb(ui::colors().accent)
            } else {
                rgba(0)
            })
            .text_color(rgb(if active {
                ui::colors().text
            } else {
                ui::colors().muted
            }))
            .hover(|s| s.bg(rgb(ui::colors().hover)))
            .tooltip(move |_, cx| cx.new(|_| ui::Tooltip(tooltip.clone())).into())
            .child(icon(glyph).when(
                active && matches!(glyph, Icon::Terminal | Icon::SplitHorizontal),
                |icon| icon.text_color(rgb(ui::colors().accent)),
            ))
            .child(self.workspace_tab_label(tab))
            .when(unread, |d| {
                d.child(
                    div()
                        .size(px(5.))
                        .flex_shrink_0()
                        .rounded_full()
                        .bg(rgb(ui::colors().accent)),
                )
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                if let Some(index) = this.tabs.iter().position(|tab| tab.id == id) {
                    this.set_active(index, window, cx);
                }
            }))
            .on_drag(
                TabDrag {
                    id,
                    title,
                    whole_group: false,
                },
                |drag, _, _, cx| cx.new(|_| drag.clone()),
            )
            .on_drop(cx.listener(move |this, drag: &TabDrag, _, cx| {
                this.reorder_tab(drag.id, id, drag.whole_group, cx);
            }))
            .child(
                icon_button(
                    ("tab-close", id),
                    Icon::Close,
                    i18n_key::WORKSPACE_CLOSE_WORKSPACE_TAB_AND_ALL_ITS_PANES,
                )
                .size(px(20.))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| {
                        if this.menu.take().is_some() {
                            this.restore_popup_focus(window, cx);
                            cx.notify();
                        }
                        cx.stop_propagation();
                    }),
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.confirm_close(close_ids.clone(), window, cx);
                })),
            )
            .child(
                div()
                    .absolute()
                    .right_0()
                    .top(px(7.))
                    .bottom(px(7.))
                    .w(px(1.))
                    .bg(rgb(ui::colors().border)),
            )
            .when_some(group, |tab, profile| {
                tab.child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .right_0()
                        .h(px(2.))
                        .bg(rgb(self.tab_group_colors.color(profile))),
                )
            })
            .into_any_element()
    }

    fn render_tab_group(
        &self,
        profile: Uuid,
        group: &super::tab_groups::TabGroup,
        cx: &Context<Self>,
    ) -> AnyElement {
        let first = group.tabs[0];
        let members: Vec<_> = self
            .tabs
            .iter()
            .filter(|tab| group.tabs.contains(&tab.id))
            .collect();
        let name = members
            .iter()
            .flat_map(|tab| tab.layout.leaves())
            .find_map(|id| self.pane_profiles.get(&id))
            .map(|profile| profile.name.clone())
            .unwrap_or_default();
        let collapsed = self.collapsed_tab_groups.contains(&profile);
        let active = self
            .tabs
            .get(self.active)
            .is_some_and(|tab| group.tabs.contains(&tab.id));
        let unread = members
            .iter()
            .flat_map(|tab| tab.layout.leaves())
            .any(|id| self.panes.get(&id).is_some_and(|pane| pane.has_unread(cx)));
        let tooltip = rustterm::trf!(
            if collapsed {
                i18n_key::WORKSPACE_EXPAND_SSH_TAB_GROUP
            } else {
                i18n_key::WORKSPACE_COLLAPSE_SSH_TAB_GROUP
            },
            name = name.clone(),
            count = group.tabs.len()
        );
        let color = self.tab_group_colors.color(profile);
        div()
            .id(gpui::SharedString::from(format!("ssh-tab-group-{profile}")))
            .relative()
            .h_full()
            // Member tabs carry the names while expanded; only folded groups
            // need a name in the header to remain identifiable.
            .min_w(px(if collapsed { 64. } else { 44. }))
            .max_w(px(if collapsed { 176. } else { 72. }))
            .flex_shrink_0()
            .px_1()
            .flex()
            .items_center()
            .cursor_pointer()
            .tab_index(0)
            .bg(rgb(if active {
                ui::colors().bg
            } else {
                ui::colors().chrome
            }))
            .hover(|s| s.bg(rgb(ui::colors().hover)))
            .tooltip(move |_, cx| cx.new(|_| ui::Tooltip(tooltip.clone())).into())
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .gap(px(3.))
                    .px(px(6.))
                    .py_1()
                    .rounded(px(4.))
                    .bg(rgb(if active {
                        ui::colors().selected
                    } else {
                        ui::colors().raised
                    }))
                    .text_size(px(11.))
                    .text_color(rgb(color))
                    .child(
                        icon(if collapsed {
                            Icon::ChevronRight
                        } else {
                            Icon::ChevronDown
                        })
                        .text_color(rgb(color)),
                    )
                    .when(collapsed, |header| {
                        header.child(div().flex_1().min_w_0().truncate().child(name.clone()))
                    })
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_size(px(if collapsed { 10. } else { 12. }))
                            .child(group.tabs.len().to_string()),
                    )
                    .when(unread, |d| {
                        d.child(
                            div()
                                .size(px(5.))
                                .flex_shrink_0()
                                .rounded_full()
                                .bg(rgb(color)),
                        )
                    }),
            )
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .right_0()
                    .h(px(2.))
                    .bg(rgb(color)),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                this.toggle_tab_group(profile, cx);
                this.restore_focus(window, cx);
                cx.stop_propagation();
            }))
            .on_drag(
                TabDrag {
                    id: first,
                    title: name,
                    whole_group: true,
                },
                |drag, _, _, cx| cx.new(|_| drag.clone()),
            )
            .on_drop(cx.listener(move |this, drag: &TabDrag, _, cx| {
                this.reorder_tab(drag.id, first, drag.whole_group, cx);
            }))
            .into_any_element()
    }

    pub(super) fn render_tabs(&self, cx: &mut Context<Self>) -> AnyElement {
        let workspace = cx.entity().downgrade();
        let scroll = self.tab_scroll.clone();
        let previous_width = scroll.bounds().size.width;
        let mut tabs = div()
            .on_children_prepainted(move |_, _, cx| {
                let edges = (
                    scroll.offset().x < px(-1.),
                    scroll.max_offset().width + scroll.offset().x > px(1.),
                );
                let resized = (f32::from(scroll.bounds().size.width - previous_width)).abs() > 0.5;
                let workspace = workspace.clone();
                cx.defer(move |cx| {
                    let _ = workspace.update(cx, |this, cx| {
                        if resized {
                            this.scroll_active_tab_into_view();
                        }
                        if resized || this.tab_scroll_edges != edges {
                            this.tab_scroll_edges = edges;
                            cx.notify();
                        }
                    });
                });
            })
            .id("workspace-tab-list")
            .track_scroll(&self.tab_scroll)
            .h_full()
            .flex_initial()
            .min_w_0()
            .flex()
            .items_center()
            .overflow_x_scroll()
            // A tab strip uses arrows instead of a scrollbar. Keep GPUI's
            // single-axis fallback so an ordinary wheel also scrolls the tabs.
            .scrollbar_width(px(0.));
        for group in self.tab_groups() {
            if let Some(profile) = group.profile {
                tabs = tabs.child(self.render_tab_group(profile, &group, cx));
                if self.collapsed_tab_groups.contains(&profile) {
                    continue;
                }
            }
            for id in group.tabs {
                if let Some(tab) = self.tabs.iter().find(|tab| tab.id == id) {
                    tabs = tabs.child(self.render_workspace_tab(tab, group.profile, cx));
                }
            }
        }
        div()
            .id("tab-strip")
            .h(px(ui::TAB_HEIGHT))
            .flex_shrink_0()
            .min_w_0()
            .flex()
            .items_center()
            .bg(rgb(ui::colors().chrome))
            .border_b_1()
            .border_color(rgb(ui::colors().border))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    if this.menu.take().is_some() {
                        this.restore_popup_focus(window, cx);
                        window.prevent_default();
                        cx.notify();
                    }
                }),
            )
            .when(!self.sidebar_visible, |d| {
                d.child(
                    icon_button(
                        "show-connection-sidebar",
                        Icon::Sidebar,
                        i18n_key::WORKSPACE_SHOW_CONNECTION_LIBRARY_CTRL_SHIFT_B,
                    )
                    .mx_1()
                    .tab_index(0)
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_sidebar(window, cx))),
                )
            })
            .when(self.tab_scroll_edges.0 || self.tab_scroll_edges.1, |bar| {
                bar.child(
                    ui::icon_button_enabled(
                        "scroll-tabs-left",
                        Icon::ChevronLeft,
                        i18n_key::COMMON_SCROLL_TABS_LEFT,
                        self.tab_scroll_edges.0,
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.scroll_tabs(false, cx))),
                )
            })
            .child(tabs)
            .when(self.tab_scroll_edges.0 || self.tab_scroll_edges.1, |bar| {
                bar.child(
                    ui::icon_button_enabled(
                        "scroll-tabs-right",
                        Icon::ChevronRight,
                        i18n_key::COMMON_SCROLL_TABS_RIGHT,
                        self.tab_scroll_edges.1,
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.scroll_tabs(true, cx))),
                )
            })
            .child(
                icon_button(
                    "new-workspace-tab",
                    Icon::Plus,
                    i18n_key::WORKSPACE_NEW_LOCAL_WORKSPACE_TAB_CTRL_SHIFT_T,
                )
                .mx_2()
                .tab_index(0)
                .on_click(cx.listener(|this, _, window, cx| {
                    this.pane_event(&PaneEvent::NewTab, window, cx);
                })),
            )
            .into_any_element()
    }
    pub(super) fn render_statusbar(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let corners = WindowFrame::new(window).corners;
        let pane = self.panes.get(&self.focused);
        let profile = self.pane_profiles.get(&self.focused);
        let state = pane
            .map(|pane| pane.status_text(cx))
            .unwrap_or_else(|| tr(i18n_key::WORKSPACE_READY).into());
        let color = pane
            .map(|pane| pane.status_color(cx))
            .unwrap_or(ui::colors().muted);
        let detail = pane.map(|pane| pane.status_details(cx)).unwrap_or_default();
        let target = profile
            .map(|profile| match &profile.connection {
                Connection::Ssh(ssh) => format!("SSH  {}@{}:{}", ssh.username, ssh.host, ssh.port),
                Connection::Local(_) => tr(i18n_key::COMMON_LOCAL_TERMINAL).into(),
                Connection::Elasticsearch(config) => format!("Elasticsearch {}", config.endpoint),
                Connection::Redis(redis) => format!("Redis  {}:{}", redis.host, redis.port),
                Connection::Mysql(sql) => {
                    format!("MySQL  {}:{} / {}", sql.host, sql.port, sql.database)
                }
                Connection::Postgres(sql) => {
                    format!("PostgreSQL  {}:{} / {}", sql.host, sql.port, sql.database)
                }
                Connection::Sqlite(sql) => format!("SQLite  {}", sql.path.display()),
                Connection::Telnet(telnet) => telnet_target(telnet),
                Connection::Rdp(rdp) => rdp_target(rdp),
            })
            .unwrap_or_default();
        let name = profile
            .map(|profile| profile.name.clone())
            .unwrap_or_default();
        let tooltip = format!("{name} · {state}\n{target}\n{detail}");
        let protocol = profile.map_or("", |profile| match &profile.connection {
            Connection::Ssh(_) => "SSH",
            Connection::Telnet(_) => "Telnet",
            Connection::Local(_) => tr(i18n_key::TERMINAL_LOCAL),
            Connection::Redis(_) => "Redis",
            Connection::Elasticsearch(_) => "Elasticsearch",
            Connection::Mysql(_) => "MySQL",
            Connection::Postgres(_) => "PostgreSQL",
            Connection::Sqlite(_) => "SQLite",
            Connection::Rdp(_) => "RDP",
        });
        let label = if protocol.is_empty() {
            state
        } else {
            format!("{protocol} · {state}")
        };
        div()
            .h(px(ui::STATUS_HEIGHT))
            .flex_shrink_0()
            .flex()
            .items_center()
            .overflow_hidden()
            .bg(rgb(ui::colors().status))
            .rounded_bl(corners.bottom_left)
            .rounded_br(corners.bottom_right)
            .pl(corners.bottom_left)
            .pr(corners.bottom_right)
            .border_t_1()
            .border_color(rgb(ui::colors().border))
            .text_size(px(11.))
            .text_color(rgb(ui::colors().muted))
            .child(match pane {
                Some(Pane::Rdp(view)) => view.update(cx, |view, cx| view.render_status(cx)),
                _ => div()
                    .id("focused-pane-status")
                    .px_3()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap_2()
                    .tooltip(move |_, cx| cx.new(|_| ui::Tooltip(tooltip.clone())).into())
                    .child(
                        div()
                            .size(px(6.))
                            .flex_shrink_0()
                            .rounded_full()
                            .bg(rgb(color)),
                    )
                    .child(div().min_w_0().truncate().child(label))
                    .into_any_element(),
            })
            .when(self.saving || self.pending_credentials > 0, |bar| {
                bar.child(
                    div()
                        .px_2()
                        .text_size(px(10.))
                        .child(tr(i18n_key::WORKSPACE_SAVING)),
                )
            })
            .when(
                self.save_confirmation.is_some() && !self.saving && !self.save_failed,
                |bar| {
                    bar.child(
                        div()
                            .px_2()
                            .text_size(px(10.))
                            .text_color(rgb(ui::colors().success))
                            .child(tr(i18n_key::COMMON_SAVED)),
                    )
                },
            )
            .when(self.save_failed, |bar| {
                bar.child(
                    ui::text_button(
                        "retry-save",
                        i18n_key::WORKSPACE_RETRY_SAVING,
                        ui::ButtonStyle::Neutral,
                    )
                    .flex_shrink_0()
                    .text_size(px(10.))
                    .text_color(rgb(ui::colors().warning))
                    .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                )
            })
            .when(self.updates.read(cx).attention(), |bar| {
                bar.child(
                    ui::text_button(
                        "update-available",
                        i18n_key::UPDATES_TITLE,
                        ui::ButtonStyle::Neutral,
                    )
                    .text_size(px(10.))
                    .text_color(rgb(ui::colors().accent))
                    .tooltip({
                        let text = self.updates.read(cx).message().render();
                        move |_, cx| cx.new(|_| ui::Tooltip(text.clone())).into()
                    })
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_settings(crate::app::forms::SettingsSection::Updates, window, cx)
                    })),
                )
            })
            .children(self.render_transfer_status(cx))
            .into_any_element()
    }
}

#[derive(Clone)]
struct TabDrag {
    id: usize,
    title: String,
    whole_group: bool,
}
impl gpui::Render for TabDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_3()
            .py_2()
            .bg(rgb(ui::colors().selected))
            .child(self.title.clone())
    }
}
