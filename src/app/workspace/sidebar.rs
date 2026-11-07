use super::Workspace;
use super::menus::Menu;
use crate::app::forms::ProfileKind;
use crate::app::ui;
use crate::app::ui::{Icon, icon, icon_button};
use gpui::prelude::*;
use gpui::{AnyElement, App, Context, FontWeight, KeyDownEvent, MouseButton, Window, div, px, rgb};
use rustterm::config::{AppConfig, Connection, Group, Profile, Uuid};
use rustterm::i18n::messages as i18n_key;
use rustterm::i18n::tr;
use std::collections::{HashMap, HashSet};

use super::menus::MenuAction;

#[derive(Clone)]
struct LibraryDrag {
    id: Uuid,
    name: String,
    group: bool,
}

impl gpui::Render for LibraryDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_3()
            .py_2()
            .rounded(px(4.))
            .bg(rgb(ui::colors().selected))
            .text_color(rgb(ui::colors().text))
            .child(self.name.clone())
    }
}

fn profile_matches(profile: &Profile, group: &str, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let target = match &profile.connection {
        Connection::Local(s) => s.shell.clone(),
        Connection::Ssh(s) => format!("{} {} {}", s.host, s.username, s.port),
        Connection::Telnet(s) => format!("Telnet {} {}", s.host, s.port),
        Connection::Elasticsearch(config) => format!("Elasticsearch {}", config.endpoint),
        Connection::Redis(s) => format!(
            "Redis {} {} {}",
            s.host,
            s.port,
            s.username.as_deref().unwrap_or_default()
        ),
        Connection::Mysql(s) | Connection::Postgres(s) | Connection::ClickHouse(s) => format!(
            "{} {} {} {} {}",
            profile.connection.sql().unwrap().0.label(),
            s.host,
            s.port,
            s.username,
            s.database
        ),
        Connection::Sqlite(s) => format!("SQLite {}", s.path.display()),
        Connection::Rdp(s) => format!(
            "RDP {} {} {} {}",
            s.host,
            s.port,
            s.username,
            s.domain.as_deref().unwrap_or_default()
        ),
    };
    [profile.name.as_str(), group, &target]
        .iter()
        .any(|value| value.to_lowercase().contains(query))
}

struct LibrarySection<'a> {
    id: Option<Uuid>,
    parent: Option<Uuid>,
    name: String,
    path: String,
    depth: usize,
    profiles: Vec<(usize, &'a Profile)>,
    count: usize,
    matched: bool,
    collapsed: bool,
}

// Rendering and keyboard navigation share the same visible tree and row order.
fn library_sections<'a>(
    config: &'a AppConfig,
    collapsed: &HashSet<Uuid>,
    query: &str,
) -> Vec<LibrarySection<'a>> {
    let mut profiles: HashMap<_, Vec<_>> = HashMap::new();
    for (index, profile) in config.profiles.iter().enumerate() {
        profiles
            .entry(profile.group)
            .or_default()
            .push((index, profile));
    }
    let root = (
        None,
        None,
        tr(i18n_key::WORKSPACE_UNGROUPED).to_owned(),
        tr(i18n_key::WORKSPACE_UNGROUPED).to_owned(),
        0,
    );
    let mut sections: Vec<_> = std::iter::once(root)
        .chain(Group::tree(&config.groups).into_iter().map(|node| {
            (
                Some(node.group.id),
                node.group.parent,
                node.group.name.clone(),
                node.path,
                node.depth,
            )
        }))
        .map(|(id, parent, name, path, depth)| {
            let profiles: Vec<_> = profiles
                .remove(&id)
                .unwrap_or_default()
                .into_iter()
                .filter(|(_, profile)| profile_matches(profile, &path, query))
                .collect();
            LibrarySection {
                id,
                parent,
                name,
                depth,
                count: profiles.len(),
                matched: query.is_empty()
                    || !profiles.is_empty()
                    || path.to_lowercase().contains(query),
                collapsed: query.is_empty() && id.is_some_and(|id| collapsed.contains(&id)),
                path,
                profiles,
            }
        })
        .collect();
    let indices: HashMap<_, _> = sections
        .iter()
        .enumerate()
        .filter_map(|(i, section)| section.id.map(|id| (id, i)))
        .collect();
    for index in (0..sections.len()).rev() {
        if let Some(parent) = sections[index]
            .parent
            .and_then(|id| indices.get(&id).copied())
        {
            sections[parent].count += sections[index].count;
            sections[parent].matched |= sections[index].matched;
        }
    }
    let mut hidden_depth = None;
    sections.retain(|section| {
        if hidden_depth.is_some_and(|depth| section.depth > depth) {
            return false;
        }
        hidden_depth = None;
        if !section.matched {
            return false;
        }
        if section.collapsed {
            hidden_depth = Some(section.depth);
        }
        true
    });
    sections
}

impl Workspace {
    pub(super) fn expand_library_group(&mut self, group: Uuid) {
        let mut current = Some(group);
        for _ in 0..self.config.groups.len() {
            let Some(id) = current else { break };
            self.collapsed.remove(&id);
            current = self
                .config
                .groups
                .iter()
                .find(|g| g.id == id)
                .and_then(|g| g.parent);
        }
    }

    fn filtered_profile_rows(&self, cx: &App) -> Vec<(&Profile, usize)> {
        let query = self.session_filter.read(cx).value().trim().to_lowercase();
        let mut rows = Vec::new();
        let mut row = 0;
        for section in library_sections(&self.config, &self.collapsed, &query) {
            row += 1;
            if section.collapsed {
                continue;
            }
            for (_, profile) in section.profiles {
                rows.push((profile, row));
                row += 1;
            }
        }
        rows
    }
    pub(super) fn selected_filtered_profile(&self, cx: &App) -> Option<Profile> {
        let rows = self.filtered_profile_rows(cx);
        rows.iter()
            .find(|(profile, _)| Some(profile.id) == self.selected_profile)
            .or_else(|| rows.first())
            .map(|(profile, _)| (*profile).clone())
    }
    pub(super) fn sidebar_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let filter = self.session_filter.read(cx);
        let navigating_filter = filter.focus_handle().is_focused(window)
            && !filter.is_composing()
            && matches!(event.keystroke.key.as_str(), "up" | "down");
        let context_menu = event.keystroke.key == "f10"
            && event.keystroke.modifiers
                == gpui::Modifiers {
                    shift: true,
                    ..Default::default()
                };
        if !self.sidebar_visible
            || (!self.sidebar_focus.is_focused(window) && !navigating_filter)
            || self.modal.is_some()
            || (event.keystroke.modifiers != gpui::Modifiers::default() && !context_menu)
        {
            return false;
        }
        if context_menu || event.keystroke.key == "menu" {
            if let Some((id, row)) = self
                .filtered_profile_rows(cx)
                .into_iter()
                .find(|(profile, _)| Some(profile.id) == self.selected_profile)
                .map(|(profile, row)| (profile.id, row))
                && let Some(bounds) = self.sidebar_scroll.bounds_for_item(row)
            {
                let viewport = self.sidebar_scroll.bounds();
                let position = gpui::point(
                    viewport.left() + px(16.),
                    (bounds.bottom() + self.sidebar_scroll.offset().y)
                        .clamp(viewport.top(), viewport.bottom()),
                );
                self.open_profile_menu(id, position, window, cx);
            }
            cx.stop_propagation();
            window.prevent_default();
            return true;
        }
        match event.keystroke.key.as_str() {
            "up" | "down" | "home" | "end" => {
                let profiles = self.filtered_profile_rows(cx);
                if !profiles.is_empty() {
                    let current = profiles
                        .iter()
                        .position(|(p, _)| Some(p.id) == self.selected_profile);
                    let next = match event.keystroke.key.as_str() {
                        "home" => 0,
                        "end" => profiles.len() - 1,
                        "up" => current
                            .map(|i| i.saturating_sub(1))
                            .unwrap_or(profiles.len() - 1),
                        _ => current
                            .map(|i| (i + 1).min(profiles.len() - 1))
                            .unwrap_or(0),
                    };
                    let (id, row) = (profiles[next].0.id, profiles[next].1);
                    self.selected_profile = Some(id);
                    self.sidebar_scroll.scroll_to_item(row);
                    if navigating_filter {
                        self.sidebar_focus.focus(window);
                    }
                }
            }
            "enter" => {
                if let Some(profile) = self.selected_filtered_profile(cx) {
                    self.open_or_focus_profile(profile, window, cx);
                }
            }
            "left" | "right" => {
                if self.session_filter.read(cx).value().trim().is_empty()
                    && let Some(group) = self
                        .config
                        .profiles
                        .iter()
                        .find(|profile| Some(profile.id) == self.selected_profile)
                        .and_then(|profile| profile.group)
                {
                    if event.keystroke.key == "left" {
                        self.collapsed.insert(group);
                    } else {
                        self.expand_library_group(group);
                    }
                }
            }
            "f2" | "delete" => {
                if let Some(profile) = self
                    .filtered_profile_rows(cx)
                    .into_iter()
                    .find(|(profile, _)| Some(profile.id) == self.selected_profile)
                    .map(|(profile, _)| profile.clone())
                {
                    if event.keystroke.key == "delete" {
                        self.delete_profile(profile.id, window, cx);
                    } else {
                        let kind = match &profile.connection {
                            Connection::Local(_) => ProfileKind::Local,
                            Connection::Ssh(_) => ProfileKind::Ssh,
                            Connection::Telnet(_) => ProfileKind::Telnet,
                            Connection::Redis(_) => ProfileKind::Redis,
                            Connection::Elasticsearch(_) => ProfileKind::Elasticsearch,
                            Connection::Mysql(_) => ProfileKind::Mysql,
                            Connection::ClickHouse(_) => ProfileKind::ClickHouse,
                            Connection::Postgres(_) => ProfileKind::Postgres,
                            Connection::Sqlite(_) => ProfileKind::Sqlite,
                            Connection::Rdp(_) => ProfileKind::Rdp,
                        };
                        self.edit_profile(Some(profile), kind, window, cx);
                    }
                }
            }
            "escape" => self.restore_focus(window, cx),
            _ => return false,
        }
        cx.stop_propagation();
        window.prevent_default();
        cx.notify();
        true
    }
    fn move_library_item(
        &mut self,
        drag: &LibraryDrag,
        group: Option<Uuid>,
        before: Option<Uuid>,
        cx: &mut Context<Self>,
    ) {
        if self.closing
            || self.lock_screen.locked
            || self.modal.is_some()
            || self.window_state.prompts.is_busy()
        {
            return;
        }
        if drag.group {
            if self.config.move_group(drag.id, group, None).is_err() {
                return;
            }
            if let Some(group) = group {
                self.expand_library_group(group);
            }
        } else {
            if before == Some(drag.id) {
                return;
            }
            let Some(from) = self
                .config
                .profiles
                .iter()
                .position(|item| item.id == drag.id)
            else {
                return;
            };
            let mut item = self.config.profiles.remove(from);
            item.group = group;
            let to = before
                .and_then(|id| self.config.profiles.iter().position(|item| item.id == id))
                .or_else(|| {
                    self.config
                        .profiles
                        .iter()
                        .rposition(|item| item.group == group)
                        .map(|i| i + 1)
                })
                .unwrap_or(self.config.profiles.len());
            self.config.profiles.insert(to, item);
            self.selected_profile = Some(drag.id);
            if let Some(group) = group {
                self.expand_library_group(group);
            }
        }
        self.save(cx);
        cx.notify();
    }
    pub(super) fn render_sidebar(&self, cx: &mut Context<Self>) -> AnyElement {
        let query = self.session_filter.read(cx).value().trim().to_lowercase();
        let selected = self.selected_profile;
        let sidebar = div()
            .id("connection-sidebar")
            .track_focus(&self.sidebar_focus)
            .w(px(self.sidebar_width))
            .h_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .bg(rgb(ui::colors().surface))
            .border_r_1()
            .border_color(rgb(ui::colors().border))
            .child(
                div()
                    .h(px(44.))
                    .px_3()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(
                        div()
                            .flex_1()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(tr(i18n_key::WORKSPACE_CONNECTIONS)),
                    )
                    .child(
                        icon_button(
                            "hide-connection-sidebar",
                            Icon::Sidebar,
                            i18n_key::WORKSPACE_HIDE_CONNECTION_LIBRARY_CTRL_SHIFT_B,
                        )
                        .tab_index(0)
                        .on_click(
                            cx.listener(|this, _, window, cx| this.toggle_sidebar(window, cx)),
                        ),
                    ),
            )
            .child(
                div()
                    .px_3()
                    .pb_2()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(div().flex_1().min_w_0().child(self.session_filter.clone()))
                    .when(!query.is_empty(), |d| {
                        d.child(
                            icon_button(
                                "clear-filter",
                                Icon::Close,
                                tr(i18n_key::WORKSPACE_CLEAR_FILTER),
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.session_filter.update(cx, |field, cx| field.clear(cx))
                            })),
                        )
                    }),
            )
            .child(
                div()
                    .id("add-connection")
                    .tab_index(0)
                    .h(px(32.))
                    .mx_3()
                    .mb_3()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .rounded(px(4.))
                    .border_1()
                    .border_color(rgb(ui::colors().border))
                    .text_size(px(12.))
                    .text_color(rgb(ui::colors().text))
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(ui::colors().hover)))
                    .child(icon(Icon::Plus))
                    .child(tr(i18n_key::COMMON_NEW_CONNECTION))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.toggle_menu(Menu::Create, window, cx)
                    })),
            );
        let sections = library_sections(&self.config, &self.collapsed, &query);
        let has_matches = !sections.is_empty();
        let mut tree = div()
            .id("session-tree")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .map(ui::preserve_scroll_axes)
            .track_scroll(&self.sidebar_scroll)
            .pb_2();
        let mut matched = 0;
        for (group_index, section) in sections.into_iter().enumerate() {
            let LibrarySection {
                id: group,
                parent,
                name,
                path,
                depth,
                profiles,
                count,
                collapsed,
                ..
            } = section;
            matched += count;
            let indent = if query.is_empty() {
                depth.min(12) as f32 * 14.
            } else {
                0.
            };
            let heading_path = path.clone();
            let hover_group: gpui::SharedString = format!("group-{group_index}").into();
            let drag_name = name.clone();
            let mut heading = div()
                .id(("library-group", group_index))
                .relative()
                .h(px(32.))
                .pl(px(12. + indent))
                .pr_3()
                .tooltip(move |_, cx| cx.new(|_| ui::Tooltip(heading_path.clone())).into())
                .when_some(group, |d, id| {
                    d.on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                            this.open_group_menu(id, event.position, window, cx);
                            window.prevent_default();
                            cx.stop_propagation();
                        }),
                    )
                })
                .flex()
                .items_center()
                .group(hover_group.clone())
                .drag_over::<LibraryDrag>(|style, _, _, _| style.bg(rgb(ui::colors().selected)))
                .on_drop(cx.listener(move |this, drag: &LibraryDrag, _, cx| {
                    cx.stop_propagation();
                    this.move_library_item(drag, group, None, cx);
                }))
                .child(
                    div()
                        .id(("group", group_index))
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .flex()
                        .items_center()
                        .gap_2()
                        .cursor_pointer()
                        .text_size(px(12.))
                        .text_color(rgb(ui::colors().muted))
                        .font_weight(FontWeight::MEDIUM)
                        .child(
                            icon(if collapsed {
                                Icon::ChevronRight
                            } else {
                                Icon::ChevronDown
                            })
                            .size(px(12.)),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .whitespace_nowrap()
                                .overflow_hidden()
                                .text_ellipsis()
                                .child(if query.is_empty() {
                                    name.clone()
                                } else {
                                    path.clone()
                                }),
                        )
                        .child(
                            div()
                                .text_size(px(10.))
                                .text_color(rgb(ui::colors().faint))
                                .child(count.to_string()),
                        )
                        .when_some(group, |d, id| {
                            d.on_drag(
                                LibraryDrag {
                                    id,
                                    name: drag_name,
                                    group: true,
                                },
                                |drag, _, _, cx| cx.new(|_| drag.clone()),
                            )
                        })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(id) = group
                                && !this.collapsed.remove(&id)
                            {
                                this.collapsed.insert(id);
                            }
                            cx.notify();
                        })),
                );
            if let Some(id) = group {
                // A separate upper-edge drop target retains sibling ordering;
                // dropping on the rest of the heading reparents the subtree.
                heading = heading.child(
                    div()
                        .id(("group-insert-before", group_index))
                        .absolute()
                        .top_0()
                        .left_0()
                        .right_0()
                        .h(px(5.))
                        .drag_over::<LibraryDrag>(|style, drag, _, _| {
                            if drag.group {
                                style.bg(rgb(ui::colors().accent))
                            } else {
                                style
                            }
                        })
                        .on_drop(cx.listener(move |this, drag: &LibraryDrag, _, cx| {
                            if !drag.group {
                                return;
                            }
                            cx.stop_propagation();
                            if this.closing
                                || this.lock_screen.locked
                                || this.modal.is_some()
                                || this.window_state.prompts.is_busy()
                            {
                                return;
                            }
                            if this.config.move_group(drag.id, parent, Some(id)).is_ok() {
                                if let Some(parent) = parent {
                                    this.expand_library_group(parent);
                                }
                                this.save(cx);
                                cx.notify();
                            }
                        })),
                );
                heading = heading.child(
                    div()
                        .flex()
                        .opacity(0.)
                        .group_hover(hover_group, |s| s.opacity(1.))
                        .child(
                            icon_button(
                                ("edit-group", group_index),
                                Icon::Edit,
                                tr(i18n_key::WORKSPACE_EDIT_GROUP),
                            )
                            .size(px(20.))
                            .on_click(cx.listener(
                                move |this, _, window, cx| {
                                    this.edit_group(Some(id), None, window, cx);
                                },
                            )),
                        )
                        .child(
                            icon_button(
                                ("delete-group", group_index),
                                Icon::Trash,
                                tr(i18n_key::WORKSPACE_DELETE_GROUP),
                            )
                            .size(px(20.))
                            .on_click(cx.listener(
                                move |this, _, window, cx| this.delete_group(id, window, cx),
                            )),
                        ),
                );
            }
            tree = tree.child(heading);
            if collapsed {
                continue;
            }
            for (index, profile) in profiles {
                let connect = profile.clone();
                let id = profile.id;
                let glyph = Icon::for_connection(&profile.connection);
                let selected = selected == Some(id);
                let open: Vec<_> = self
                    .tabs
                    .iter()
                    .flat_map(|tab| tab.layout.leaves())
                    .filter(|pane| {
                        self.pane_profiles
                            .get(pane)
                            .is_some_and(|profile| profile.id == id)
                    })
                    .filter_map(|pane| self.panes.get(&pane))
                    .collect();
                let open_count = open.len();
                let status = open
                    .iter()
                    .find(|pane| pane.is_running(cx))
                    .or_else(|| open.first())
                    .map(|pane| pane.status_color(cx))
                    .unwrap_or(ui::colors().faint);
                let detail = match &profile.connection {
                    Connection::Local(local) => local.shell.clone(),
                    Connection::Ssh(ssh) => {
                        format!("SSH  {}@{}:{}", ssh.username, ssh.host, ssh.port)
                    }
                    Connection::Elasticsearch(config) => {
                        format!("Elasticsearch {}", config.endpoint)
                    }
                    Connection::Redis(redis) => format!("Redis  {}:{}", redis.host, redis.port),
                    Connection::ClickHouse(sql) => {
                        format!("ClickHouse  {}:{} / {}", sql.host, sql.port, sql.database)
                    }
                    Connection::Mysql(sql) => {
                        format!("MySQL  {}:{} / {}", sql.host, sql.port, sql.database)
                    }
                    Connection::Postgres(sql) => {
                        format!("PostgreSQL  {}:{} / {}", sql.host, sql.port, sql.database)
                    }
                    Connection::Sqlite(sql) => format!("SQLite  {}", sql.path.display()),
                    Connection::Telnet(telnet) => telnet_target(telnet),
                    Connection::Rdp(rdp) => rdp_target(rdp),
                };
                let hint = if open_count == 0 {
                    tr(i18n_key::WORKSPACE_DOUBLE_CLICK_OR_PRESS_ENTER_TO_OPEN_CONNECTION)
                        .to_owned()
                } else if !matches!(profile.connection, Connection::Ssh(_)) {
                    rustterm::trf!(
                        i18n_key::WORKSPACE_OPEN_PANE_COUNT_FOCUS_HELP,
                        count = open_count
                    )
                } else if open_count == 1 {
                    tr(i18n_key::WORKSPACE_CONNECTION_OPEN_DOUBLE_CLICK_OR_PRESS_ENTER_TO_OPEN_ANOTHER)
                        .to_owned()
                } else {
                    rustterm::trf!(i18n_key::WORKSPACE_OPEN_PANE_COUNT_HELP, count = open_count)
                };
                let tooltip = format!("{path} / {}\n{detail}\n{hint}", profile.name);
                tree = tree.child(
                    div()
                        .id(("profile", index))
                        .h(px(36.))
                        .mx_2()
                        .pl(px(24. + indent))
                        .pr_2()
                        .rounded(px(4.))
                        .flex()
                        .items_center()
                        .gap_2()
                        .cursor_pointer()
                        .text_color(rgb(if selected || open_count > 0 {
                            ui::colors().text
                        } else {
                            ui::colors().muted
                        }))
                        .when(selected, |d| d.bg(rgb(ui::colors().selected)))
                        .hover(|s| s.bg(rgb(ui::colors().hover)))
                        .tooltip(move |_, cx| cx.new(|_| ui::Tooltip(tooltip.clone())).into())
                        .child(
                            icon(glyph).when(glyph == Icon::Terminal && open_count > 0, |icon| {
                                icon.text_color(rgb(ui::colors().blue))
                            }),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .whitespace_nowrap()
                                .overflow_hidden()
                                .text_ellipsis()
                                .child(profile.name.clone()),
                        )
                        .when(open_count > 1, |d| {
                            d.child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(ui::colors().muted))
                                    .child(open_count.to_string()),
                            )
                        })
                        .when(open_count > 0, |d| {
                            d.child(
                                div()
                                    .size(px(6.))
                                    .flex_shrink_0()
                                    .rounded_full()
                                    .bg(rgb(status)),
                            )
                        })
                        .on_mouse_down(
                            MouseButton::Right,
                            cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                                this.open_profile_menu(id, event.position, window, cx);
                                window.prevent_default();
                                cx.stop_propagation();
                            }),
                        )
                        .on_click(
                            cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                                this.selected_profile = Some(id);
                                this.sidebar_focus.focus(window);
                                if event.click_count() == 2 {
                                    this.open_or_focus_profile(connect.clone(), window, cx);
                                }
                                cx.notify();
                            }),
                        )
                        .on_drag(
                            LibraryDrag {
                                id,
                                name: profile.name.clone(),
                                group: false,
                            },
                            |drag, _, _, cx| cx.new(|_| drag.clone()),
                        )
                        .on_drop(cx.listener(move |this, drag: &LibraryDrag, _, cx| {
                            cx.stop_propagation();
                            this.move_library_item(drag, group, Some(id), cx);
                        })),
                );
            }
        }
        if matched == 0 && (query.is_empty() || !has_matches) {
            tree = tree.child(
                div()
                    .px_3()
                    .py_4()
                    .text_size(px(11.))
                    .text_color(rgb(ui::colors().faint))
                    .child(if query.is_empty() {
                        tr(i18n_key::WORKSPACE_NO_SAVED_CONNECTIONS_YET_CHOOSE_NEW_CONNECTION_TO_ADD_ONE)
                    } else {
                        tr(i18n_key::WORKSPACE_NO_MATCHING_CONNECTION_PROFILES)
                    }),
            );
        }
        sidebar
            .child(ui::scrollable(tree, Some(self.sidebar_scroll.clone())))
            .child(
                div()
                    .h(px(44.))
                    .px_3()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .border_t_1()
                    .border_color(rgb(ui::colors().border))
                    .child(
                        div()
                            .id("settings")
                            .tab_index(0)
                            .h_full()
                            .flex_1()
                            .flex()
                            .items_center()
                            .gap_2()
                            .cursor_pointer()
                            .text_size(px(12.))
                            .text_color(rgb(ui::colors().muted))
                            .hover(|s| s.text_color(rgb(ui::colors().text)))
                            .child(icon(Icon::Settings))
                            .child(tr(i18n_key::COMMON_SETTINGS))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.menu_action(MenuAction::Settings, window, cx)
                            })),
                    ),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustterm::config::LocalConfig;

    fn library() -> AppConfig {
        let company = Group::new("Company A");
        let other = Group::new("Company B");
        let mut project = Group::new("Project");
        project.parent = Some(company.id);
        let mut second = Group::new("Project");
        second.parent = Some(other.id);
        let mut first_profile = Profile::local("App server", LocalConfig::default());
        first_profile.group = Some(project.id);
        let mut second_profile = Profile::local("Database", LocalConfig::default());
        second_profile.group = Some(second.id);
        AppConfig {
            groups: vec![company, project, other, second],
            profiles: vec![first_profile, second_profile],
            ..AppConfig::default()
        }
    }

    #[test]
    fn collapsed_company_hides_its_subtree_but_keeps_recursive_count() {
        let config = library();
        let collapsed = HashSet::from([config.groups[0].id]);
        let sections = library_sections(&config, &collapsed, "");
        assert_eq!(sections.len(), 4); // Ungrouped, Company A, Company B, its project.
        assert_eq!(sections[1].count, 1);
        assert!(sections[1].collapsed);
        assert_eq!(sections[3].path, "Company B / Project");
        assert_eq!(sections[3].depth, 1);
        assert_eq!(sections[3].profiles[0].1.name, "Database");
    }

    #[test]
    fn search_uses_ancestor_paths_and_reveals_matches_in_collapsed_groups() {
        let config = library();
        let collapsed = HashSet::from([config.groups[0].id, config.groups[1].id]);
        for query in ["company a", "app server", "company a / project"] {
            let sections = library_sections(&config, &collapsed, query);
            assert_eq!(sections.len(), 2);
            assert_eq!(sections[0].name, "Company A");
            assert_eq!(sections[1].path, "Company A / Project");
            assert!(!sections[0].collapsed && !sections[1].collapsed);
            assert_eq!(sections[1].profiles[0].1.id, config.profiles[0].id);
        }
        let both = library_sections(&config, &collapsed, "project");
        assert_eq!(both.len(), 4);
        assert_ne!(both[1].path, both[3].path);
        assert_eq!(collapsed.len(), 2); // Searching never changes saved collapse choices.
        assert!(library_sections(&config, &collapsed, "missing").is_empty());
    }
}

pub(super) fn rdp_target(settings: &rustterm::config::RdpConfig) -> String {
    let identity = settings.domain.as_ref().map_or_else(
        || settings.username.clone(),
        |domain| format!("{domain}\\{}", settings.username),
    );
    format!("RDP  {identity}@{}:{}", settings.host, settings.port)
}

pub(super) fn telnet_target(settings: &rustterm::config::TelnetConfig) -> String {
    let host = if settings.host.contains(':') && !settings.host.starts_with('[') {
        format!("[{}]", settings.host)
    } else {
        settings.host.clone()
    };
    format!("Telnet  {host}:{}", settings.port)
}
