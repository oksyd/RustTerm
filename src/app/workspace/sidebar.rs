use super::menus::Menu;
use super::{
    Workspace,
    connection_kind::{ConnectionKind, target},
};
use crate::app::forms::ProfileKind;
use crate::app::ui;
use crate::app::ui::{Icon, icon, icon_button};
use gpui::prelude::*;
use gpui::{AnyElement, App, Context, FontWeight, KeyDownEvent, MouseButton, Window, div, px, rgb};
use rustterm::config::{AppConfig, Connection, Group, Profile, Uuid};
use rustterm::i18n::messages as i18n_key;
use rustterm::i18n::tr;
use std::collections::{HashMap, HashSet};

mod search;
pub(super) use search::LibrarySearch;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LibraryItem {
    Group(Option<Uuid>),
    Profile(Uuid),
}

struct LibraryRow {
    item: LibraryItem,
    parent: Option<Uuid>,
    name: String,
}

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
    if query.is_empty()
        || ConnectionKind::of(&profile.connection)
            .label()
            .to_lowercase()
            .contains(query)
    {
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
    kind: Option<ConnectionKind>,
) -> Vec<LibrarySection<'a>> {
    let mut profiles: HashMap<_, Vec<_>> = HashMap::new();
    for (index, profile) in config
        .profiles
        .iter()
        .enumerate()
        .filter(|(_, p)| kind.is_none_or(|kind| ConnectionKind::of(&p.connection) == kind))
    {
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
                matched: !profiles.is_empty()
                    || (kind.is_none()
                        && (query.is_empty() || path.to_lowercase().contains(query))),
                collapsed: query.is_empty()
                    && kind.is_none()
                    && id.is_some_and(|id| collapsed.contains(&id)),
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
        if section.id.is_none() && section.profiles.is_empty() {
            return false;
        }
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
        for section in library_sections(&self.config, &self.collapsed, &query, self.connection_type)
        {
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
            .find(|(profile, _)| Some(profile.id) == self.selected_library_profile())
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
        let input = self.library_search.query.read(cx);
        let typing = input.focus_handle().is_focused(window);
        if !self.sidebar_visible
            || (!self.sidebar_focus.is_focused(window) && !typing && !navigating_filter)
            || self.modal.is_some()
            || self.menu.is_some()
            || self.lock_screen.locked
            || self.closing
            || self.window_state.prompts.is_busy()
            || self.authentication.read(cx).is_active()
            || input.is_composing()
        {
            return false;
        }
        let query = input.value().trim().to_owned();
        let searching = !input.value().is_empty();
        let key = event.keystroke.key.as_str();
        let modifiers = event.keystroke.modifiers;
        let context_menu = key == "menu"
            || (key == "f10"
                && modifiers
                    == gpui::Modifiers {
                        shift: true,
                        ..Default::default()
                    });
        if (modifiers.control || modifiers.alt || modifiers.platform || modifiers.shift)
            && !context_menu
        {
            return false; // Text input handles layouts, AltGr and editing shortcuts.
        }
        let rows = self.library_rows(cx);
        let current = rows
            .iter()
            .position(|row| Some(row.item) == self.library_selection);
        let matches = search::matching_rows(&rows, &query);
        if context_menu {
            if let Some(index) = current
                && let Some(bounds) = self.sidebar_scroll.bounds_for_item(index)
            {
                let viewport = self.sidebar_scroll.bounds();
                let position = gpui::point(
                    viewport.left() + px(16.),
                    (bounds.bottom() + self.sidebar_scroll.offset().y)
                        .clamp(viewport.top(), viewport.bottom()),
                );
                match rows[index].item {
                    LibraryItem::Group(Some(id)) => self.open_group_menu(id, position, window, cx),
                    LibraryItem::Profile(id) => self.open_profile_menu(id, position, window, cx),
                    LibraryItem::Group(None) => {}
                }
            }
        } else {
            match key {
                "up" | "down" | "home" | "end" => {
                    if searching && matches!(key, "home" | "end") {
                        return false;
                    }
                    let next = if searching {
                        search::next_match(&matches, current.unwrap_or(usize::MAX), key == "up")
                    } else if rows.is_empty() {
                        None
                    } else {
                        Some(match key {
                            "home" => 0,
                            "end" => rows.len() - 1,
                            "up" => current
                                .map(|i| i.saturating_sub(1))
                                .unwrap_or(rows.len() - 1),
                            _ => current.map(|i| (i + 1).min(rows.len() - 1)).unwrap_or(0),
                        })
                    };
                    if let Some(index) = next {
                        self.library_selection = Some(rows[index].item);
                        self.sidebar_scroll.scroll_to_item(index);
                        if navigating_filter {
                            self.focus_library(window, cx);
                        }
                    }
                }
                "enter" => {
                    if !event.is_held
                        && let Some(index) =
                            current.filter(|index| !searching || matches.contains(index))
                    {
                        let item = rows[index].item;
                        self.clear_library_search(cx);
                        match item {
                            LibraryItem::Group(Some(id)) => {
                                if !self.collapsed.remove(&id) && !searching {
                                    self.collapsed.insert(id);
                                }
                            }
                            LibraryItem::Profile(id) => {
                                if let Some(profile) =
                                    self.config.profiles.iter().find(|p| p.id == id).cloned()
                                {
                                    self.open_or_focus_profile(profile, window, cx);
                                }
                            }
                            LibraryItem::Group(None) => {}
                        }
                    }
                }
                "left" | "right" if !searching => {
                    if let Some(index) = current {
                        let row = &rows[index];
                        let mut next = None;
                        match (key, row.item) {
                            ("left", LibraryItem::Group(Some(id)))
                                if !self.collapsed.contains(&id)
                                    && self.session_filter.read(cx).value().trim().is_empty()
                                    && self.connection_type.is_none() =>
                            {
                                self.collapsed.insert(id);
                            }
                            ("right", LibraryItem::Group(Some(id)))
                                if self.collapsed.remove(&id) => {}
                            ("right", LibraryItem::Group(group)) => {
                                next = rows
                                    .get(index + 1)
                                    .filter(|child| child.parent == group)
                                    .map(|_| index + 1);
                            }
                            ("left", item)
                                if row.parent.is_some()
                                    || matches!(item, LibraryItem::Profile(_)) =>
                            {
                                next = rows.iter().position(|candidate| {
                                    candidate.item == LibraryItem::Group(row.parent)
                                });
                            }
                            _ => {}
                        }
                        if let Some(next) = next {
                            self.library_selection = Some(rows[next].item);
                            self.sidebar_scroll.scroll_to_item(next);
                        }
                    }
                }
                "f2" | "delete" if !searching => match self.library_selection {
                    Some(LibraryItem::Group(Some(id))) => {
                        if key == "delete" {
                            self.delete_group(id, window, cx);
                        } else {
                            self.edit_group(Some(id), None, window, cx);
                        }
                    }
                    Some(LibraryItem::Profile(id)) => {
                        if let Some(profile) =
                            self.config.profiles.iter().find(|p| p.id == id).cloned()
                        {
                            if key == "delete" {
                                self.delete_profile(id, window, cx);
                            } else {
                                let kind = ConnectionKind::of(&profile.connection).profile_kind();
                                self.edit_profile(Some(profile), kind, window, cx);
                            }
                        }
                    }
                    _ => {}
                },
                "escape" => {
                    if !self.library_search.query.read(cx).value().is_empty() {
                        self.clear_library_search(cx);
                    } else {
                        self.restore_focus(window, cx);
                    }
                }
                _ => return false,
            }
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
            self.library_selection = Some(LibraryItem::Profile(drag.id));
            if let Some(group) = group {
                self.expand_library_group(group);
            }
        }
        self.save(cx);
        cx.notify();
    }
    pub(super) fn render_sidebar(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let query = self.session_filter.read(cx).value().trim().to_lowercase();
        let speed_query = self.library_search.query.read(cx).value().trim();
        let selected = self.selected_library_profile();
        let show_details = self.config.settings.sidebar_connection_details;
        let sidebar = div()
            .id("connection-sidebar")
            .track_focus(&self.sidebar_focus)
            .relative()
            .capture_key_down(cx.listener(|this, event, window, cx| {
                this.sidebar_key(event, window, cx);
            }))
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
                        div()
                            .text_size(px(11.))
                            .text_color(rgb(ui::colors().faint))
                            .child(self.config.profiles.len().to_string()),
                    )
                    .child(
                        icon_button(
                            "add-connection",
                            Icon::Plus,
                            i18n_key::COMMON_NEW_CONNECTION,
                        )
                        .tab_index(0)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.edit_profile(None, ProfileKind::Ssh, window, cx)
                        })),
                    )
                    .child(
                        icon_button(
                            "library-actions",
                            Icon::More,
                            i18n_key::WORKSPACE_CONNECTIONS,
                        )
                        .tab_index(0)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.toggle_menu(Menu::Create, window, cx)
                        })),
                    ),
            )
            .child(
                div()
                    .mx_3()
                    .mb_2()
                    .h(px(32.))
                    .rounded(px(4.))
                    .border_1()
                    .border_color(rgb(
                        if self
                            .session_filter
                            .read(cx)
                            .focus_handle()
                            .is_focused(window)
                        {
                            ui::colors().accent
                        } else {
                            ui::colors().border
                        },
                    ))
                    .bg(rgb(ui::colors().input))
                    .overflow_hidden()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .child(div().pl_2().child(icon(Icon::Search).size(px(12.))))
                    .child(div().flex_1().min_w_0().child(self.session_filter.clone()))
                    .child(
                        div()
                            .id("connection-type-filter")
                            .tab_index(0)
                            .h(px(30.))
                            .max_w(px(108.))
                            .px_2()
                            .flex()
                            .items_center()
                            .gap_1()
                            .border_l_1()
                            .border_color(rgb(ui::colors().border))
                            .cursor_pointer()
                            .text_size(px(11.))
                            .text_color(rgb(if self.connection_type.is_some() {
                                ui::colors().accent
                            } else {
                                ui::colors().muted
                            }))
                            .hover(|style| style.bg(rgb(ui::colors().hover)))
                            .child(div().truncate().child(
                                self.connection_type.map_or(
                                    tr(i18n_key::WORKSPACE_ALL_TYPES),
                                    ConnectionKind::label,
                                ),
                            ))
                            .child(icon(Icon::ChevronDown).size(px(10.)))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.toggle_menu(Menu::ConnectionType, window, cx)
                            })),
                    )
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
            );
        let sections =
            library_sections(&self.config, &self.collapsed, &query, self.connection_type);
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
                .when(
                    self.library_selection == Some(LibraryItem::Group(group)),
                    |d| d.bg(rgb(ui::colors().selected)),
                )
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
                            div()
                                .id(("group-disclosure", group_index))
                                .size(px(16.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .hover(|d| d.bg(rgb(ui::colors().hover)))
                                .child(
                                    icon(if collapsed {
                                        Icon::ChevronRight
                                    } else {
                                        Icon::ChevronDown
                                    })
                                    .size(px(12.)),
                                )
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.clear_library_search(cx);
                                    this.library_selection = Some(LibraryItem::Group(group));
                                    this.release_rdp_control(cx);
                                    this.focus_library(window, cx);
                                    if let Some(id) = group
                                        && !this.collapsed.remove(&id)
                                    {
                                        this.collapsed.insert(id);
                                    }
                                    cx.stop_propagation();
                                    cx.notify();
                                })),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .whitespace_nowrap()
                                .overflow_hidden()
                                .text_ellipsis()
                                .child(search::highlighted(
                                    if query.is_empty() {
                                        name.clone()
                                    } else {
                                        path.clone()
                                    },
                                    speed_query,
                                )),
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
                        .on_click(cx.listener(
                            move |this, event: &gpui::ClickEvent, window, cx| {
                                this.clear_library_search(cx);
                                this.library_selection = Some(LibraryItem::Group(group));
                                this.release_rdp_control(cx);
                                this.focus_library(window, cx);
                                if event.click_count() == 2
                                    && let Some(id) = group
                                    && !this.collapsed.remove(&id)
                                {
                                    this.collapsed.insert(id);
                                }
                                cx.notify();
                            },
                        )),
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
                        .h(px(if show_details { 44. } else { 32. }))
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
                            icon(glyph).when(glyph == Icon::Ssh && open_count > 0, |icon| {
                                icon.text_color(rgb(ui::colors().blue))
                            }),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .gap(px(3.))
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .min_w_0()
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .truncate()
                                                .text_color(rgb(ui::colors().text))
                                                .child(search::highlighted(
                                                    profile.name.clone(),
                                                    speed_query,
                                                )),
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
                                        }),
                                )
                                .when(show_details, |row| {
                                    row.child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap(px(6.))
                                            .text_size(px(11.))
                                            .text_color(rgb(ui::colors().muted))
                                            .child(div().w(px(84.)).flex_shrink_0().child(
                                                ConnectionKind::of(&profile.connection).label(),
                                            ))
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .truncate()
                                                    .child(target(&profile.connection)),
                                            ),
                                    )
                                }),
                        )
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
                                this.clear_library_search(cx);
                                this.library_selection = Some(LibraryItem::Profile(id));
                                this.release_rdp_control(cx);
                                this.focus_library(window, cx);
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
                    .child(if query.is_empty() && self.connection_type.is_none() {
                        tr(i18n_key::WORKSPACE_NO_SAVED_CONNECTIONS_YET_CHOOSE_NEW_CONNECTION_TO_ADD_ONE)
                    } else {
                        tr(i18n_key::WORKSPACE_NO_MATCHING_CONNECTION_PROFILES)
                    }),
            );
        }
        sidebar
            .child(ui::scrollable(tree, Some(self.sidebar_scroll.clone())))
            .child(self.render_library_search(window, cx))
            .into_any_element()
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

#[cfg(test)]
mod tests {
    use super::*;
    use rustterm::config::LocalConfig;

    pub(super) fn library() -> AppConfig {
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
        let sections = library_sections(&config, &collapsed, "", None);
        assert_eq!(sections.len(), 3); // Company A, Company B, its project.
        assert_eq!(sections[0].count, 1);
        assert!(sections[0].collapsed);
        assert_eq!(sections[2].path, "Company B / Project");
        assert_eq!(sections[2].depth, 1);
        assert_eq!(sections[2].profiles[0].1.name, "Database");
    }

    #[test]
    fn search_uses_ancestor_paths_and_reveals_matches_in_collapsed_groups() {
        let config = library();
        let collapsed = HashSet::from([config.groups[0].id, config.groups[1].id]);
        for query in ["company a", "app server", "company a / project"] {
            let sections = library_sections(&config, &collapsed, query, None);
            assert_eq!(sections.len(), 2);
            assert_eq!(sections[0].name, "Company A");
            assert_eq!(sections[1].path, "Company A / Project");
            assert!(!sections[0].collapsed && !sections[1].collapsed);
            assert_eq!(sections[1].profiles[0].1.id, config.profiles[0].id);
        }
        let both = library_sections(&config, &collapsed, "project", None);
        assert_eq!(both.len(), 4);
        assert_ne!(both[1].path, both[3].path);
        assert_eq!(collapsed.len(), 2); // Searching never changes saved collapse choices.
        assert!(library_sections(&config, &collapsed, "missing", None).is_empty());
    }
    #[test]
    fn type_filter_preserves_group_paths_and_reveals_collapsed_matches() {
        let mut config = library();
        config.profiles[0].connection = Connection::Ssh(Default::default());
        let collapsed = HashSet::from([config.groups[0].id, config.groups[1].id]);
        let sections = library_sections(&config, &collapsed, "", Some(ConnectionKind::Ssh));
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].count, 1);
        assert!(!sections[0].collapsed && !sections[1].collapsed);
        assert_eq!(sections[1].path, "Company A / Project");
        assert_eq!(sections[1].profiles[0].1.id, config.profiles[0].id);
        assert!(
            library_sections(&config, &collapsed, "database", Some(ConnectionKind::Ssh)).is_empty()
        );
        assert!(library_sections(&config, &collapsed, "", Some(ConnectionKind::Rdp)).is_empty());
        assert!(profile_matches(&config.profiles[0], "", "ssh"));
        assert!(profile_matches(
            &config.profiles[1],
            "",
            &ConnectionKind::Local.label().to_lowercase()
        ));
    }
}
