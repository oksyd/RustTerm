use super::*;
use gpui::{AnyElement, MouseButton, Pixels, Point, div, prelude::*, px, rgb, uniform_list};
use std::collections::{BTreeMap, BTreeSet};
use ui::{ButtonStyle, Icon};

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum GroupKey {
    Schema(String),
    Kind(String, bool),
}
#[derive(Clone)]
pub(super) enum ObjectRow {
    Group {
        key: GroupKey,
        label: String,
        count: usize,
        depth: usize,
        expanded: bool,
    },
    Object {
        index: usize,
        depth: usize,
    },
}
pub(super) struct ObjectMenu {
    table: TableInfo,
    position: Point<Pixels>,
    selected: usize,
}

fn object_rows(
    tables: &[TableInfo],
    visible: &[usize],
    grouped: bool,
    collapsed: &BTreeSet<GroupKey>,
    filtering: bool,
) -> Vec<ObjectRow> {
    let mut schemas = BTreeMap::<&str, [Vec<usize>; 2]>::new();
    for &i in visible {
        schemas
            .entry(if grouped { &tables[i].schema } else { "" })
            .or_default()[usize::from(dialect::is_view(&tables[i]))]
        .push(i);
    }
    let mut rows = Vec::new();
    for (schema, mut kinds) in schemas {
        if grouped {
            let key = GroupKey::Schema(schema.into());
            let expanded = filtering || !collapsed.contains(&key);
            rows.push(ObjectRow::Group {
                key,
                label: schema.into(),
                count: kinds.iter().map(Vec::len).sum(),
                depth: 0,
                expanded,
            });
            if !expanded {
                continue;
            }
        }
        for (kind, items) in kinds.iter_mut().enumerate() {
            if items.is_empty() {
                continue;
            }
            items.sort_by(|a, b| tables[*a].name.cmp(&tables[*b].name));
            let key = GroupKey::Kind(schema.into(), kind == 1);
            let expanded = filtering || !collapsed.contains(&key);
            rows.push(ObjectRow::Group {
                key,
                label: tr(if kind == 1 {
                    i18n_key::DATABASE_VIEWS
                } else {
                    i18n_key::DATABASE_TABLES
                })
                .into(),
                count: items.len(),
                depth: usize::from(grouped),
                expanded,
            });
            if expanded {
                rows.extend(items.iter().map(|&index| ObjectRow::Object {
                    index,
                    depth: usize::from(grouped) + 1,
                }));
            }
        }
    }
    rows
}

impl DatabaseView {
    pub(super) fn rebuild_object_tree(&mut self, cx: &App) {
        self.object_rows = object_rows(
            &self.tables,
            &self.visible_tables,
            matches!(self.engine(), SqlEngine::Postgres | SqlEngine::Sqlite),
            &self.collapsed_objects,
            !self.table_filter.read(cx).value().is_empty(),
        );
    }
    pub(super) fn render_browser(&self, compact: bool, cx: &Context<Self>) -> AnyElement {
        let mut browser = div()
            .min_h_0()
            .min_w_0()
            .flex()
            .flex_col()
            .bg(rgb(ui::colors().surface))
            .when(compact, |b| b.flex_1())
            .when(!compact, |b| b.w(px(self.browser_width())).flex_shrink_0())
            .child(
                div()
                    .h(px(38.))
                    .px_3()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .text_color(rgb(ui::colors().muted))
                            .text_size(px(11.))
                            .flex_1()
                            .child(tr(i18n_key::DATABASE_OBJECTS)),
                    )
                    .child(
                        ui::icon_button_enabled(
                            "database-refresh",
                            Icon::Refresh,
                            i18n_key::DATABASE_REFRESH_TABLE,
                            self.available(),
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.refresh_browser(cx))),
                    ),
            )
            .child(
                div().px_2().pb_2().child(
                    div()
                        .id("database-selector")
                        .h(px(32.))
                        .px_2()
                        .flex()
                        .items_center()
                        .gap_2()
                        .rounded(px(5.))
                        .border_1()
                        .border_color(rgb(ui::colors().border))
                        .bg(rgb(ui::colors().bg))
                        .when(self.engine() != SqlEngine::Sqlite, |d| {
                            d.cursor_pointer()
                                .tab_index(0)
                                .hover(|s| s.bg(rgb(ui::colors().hover)))
                        })
                        .child(ui::icon(Icon::Database).text_color(rgb(self.engine_color())))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .child(self.database().to_owned()),
                        )
                        .when(self.engine() != SqlEngine::Sqlite, |d| {
                            d.child(ui::icon(Icon::ChevronDown))
                        })
                        .on_click(cx.listener(|this, _, _, cx| {
                            if this.available() && this.engine() != SqlEngine::Sqlite {
                                this.database_open = !this.database_open;
                                cx.notify();
                            }
                        })),
                ),
            );
        if self.database_open {
            browser = browser
                .child(
                    div()
                        .px_2()
                        .pb_2()
                        .flex()
                        .gap_1()
                        .child(div().flex_1().min_w_0().child(self.database_input.clone()))
                        .child(
                            render::action(
                                "database-go",
                                "Go",
                                self.available(),
                                ButtonStyle::Neutral,
                            )
                            .on_click(cx.listener(|this, _, _, cx| this.change_database(cx))),
                        ),
                )
                .child(
                    uniform_list(
                        "database-choices",
                        self.databases.len(),
                        cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                            range
                                .map(|index| this.render_database_choice(index, cx))
                                .collect::<Vec<_>>()
                        }),
                    )
                    .h(px((self.databases.len() as f32 * 28.).min(140.)))
                    .flex_shrink_0()
                    .track_scroll(self.database_scroll.clone()),
                );
        }
        browser = browser.child(div().px_2().pb_2().child(self.table_filter.clone()));
        if self.object_rows.is_empty() {
            browser = browser.child(
                div()
                    .flex_1()
                    .p_3()
                    .text_color(rgb(ui::colors().muted))
                    .child(tr(if self.busy.is_some() {
                        i18n_key::DATABASE_LOADING_DATABASE_OBJECTS
                    } else if self.state == State::Connected {
                        i18n_key::DATABASE_NO_MATCHING_TABLES_OR_VIEWS
                    } else {
                        i18n_key::DATABASE_CONNECT_TO_BROWSE_DATABASE_OBJECTS
                    })),
            );
        } else {
            browser = browser.child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .pr(px(ui::SCROLLBAR_WIDTH))
                    .child(
                        uniform_list(
                            "database-object-tree",
                            self.object_rows.len(),
                            cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                                range
                                    .map(|i| this.render_object_row(i, cx))
                                    .collect::<Vec<_>>()
                            }),
                        )
                        .map(ui::preserve_scroll_axes)
                        .track_scroll(self.table_scroll.clone())
                        .flex_1()
                        .min_h_0()
                        .w_full(),
                    )
                    .child(ui::scrollbars(
                        "database-tree-bars",
                        None,
                        Some(self.table_scroll.0.borrow().base_handle.clone()),
                    )),
            );
        }
        browser
            .child(
                div()
                    .px_3()
                    .py_2()
                    .flex_shrink_0()
                    .border_t_1()
                    .border_color(rgb(ui::colors().border))
                    .text_size(px(10.))
                    .text_color(rgb(ui::colors().muted))
                    .child(tr(i18n_key::DATABASE_OBJECT_HINT)),
            )
            .into_any_element()
    }

    fn render_object_row(&self, index: usize, cx: &Context<Self>) -> AnyElement {
        let base = div()
            .id(("object-row", index))
            .h(px(30.))
            .w_full()
            .min_w_0()
            .pr_2()
            .flex()
            .items_center()
            .gap_2()
            .cursor_pointer()
            .tab_index(0)
            .hover(|s| s.bg(rgb(ui::colors().hover)))
            .focus(|s| s.bg(rgb(ui::colors().selected)));
        match &self.object_rows[index] {
            ObjectRow::Group {
                key,
                label,
                count,
                depth,
                expanded,
            } => {
                let key = key.clone();
                base.pl(px(10. + *depth as f32 * 14.))
                    .text_color(rgb(ui::colors().muted))
                    .child(ui::icon(if *expanded {
                        Icon::ChevronDown
                    } else {
                        Icon::ChevronRight
                    }))
                    .child(div().flex_1().truncate().child(label.clone()))
                    .child(div().text_size(px(10.)).child(count.to_string()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.interactive() {
                            return;
                        }
                        if !this.collapsed_objects.remove(&key) {
                            this.collapsed_objects.insert(key.clone());
                        }
                        this.rebuild_object_tree(cx);
                        cx.notify();
                    }))
                    .into_any_element()
            }
            ObjectRow::Object {
                index: table_index,
                depth,
            } => {
                let table = self.tables[*table_index].clone();
                let context_table = table.clone();
                let hint = format!("{}.{}\n{}", table.schema, table.name, table.kind);
                base.pl(px(10. + *depth as f32 * 14.))
                    .when(self.selected_table.as_ref() == Some(&table), |r| {
                        r.bg(rgb(ui::colors().selected))
                    })
                    .child(
                        ui::icon(if dialect::is_view(&table) {
                            Icon::View
                        } else {
                            Icon::Table
                        })
                        .text_color(rgb(self.engine_color())),
                    )
                    .child(div().flex_1().truncate().child(table.name.clone()))
                    .tooltip(move |_, cx| cx.new(|_| ui::Tooltip(hint.clone())).into())
                    .on_click(cx.listener(move |this, event: &gpui::ClickEvent, _, cx| {
                        if !this.interactive() {
                            return;
                        }
                        this.selected_table = Some(table.clone());
                        this.object_menu = None;
                        if event.click_count() == 2 {
                            this.table_action(TableAction::Data, cx);
                        }
                        cx.notify();
                    }))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                            if !this.interactive() {
                                return;
                            }
                            this.selected_table = Some(context_table.clone());
                            if this.object_menu.is_none() && !this.menu_open {
                                this.menu_return_focus = window.focused(cx);
                            }
                            this.menu_scroll.scroll_to_item(0);
                            this.object_menu = Some(ObjectMenu {
                                table: context_table.clone(),
                                position: event.position - this.pane_origin,
                                selected: 0,
                            });
                            this.menu_open = false;
                            this.focus.focus(window);
                            window.prevent_default();
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    )
                    .into_any_element()
            }
        }
    }

    fn render_database_choice(&self, index: usize, cx: &Context<Self>) -> AnyElement {
        let name = self.databases[index].clone();
        div()
            .id(("database-choice", index))
            .h(px(28.))
            .px_3()
            .flex()
            .items_center()
            .cursor_pointer()
            .tab_index(0)
            .hover(|s| s.bg(rgb(ui::colors().hover)))
            .when(name == self.database(), |r| {
                r.bg(rgb(ui::colors().selected))
            })
            .child(div().truncate().child(name.clone()))
            .on_click(cx.listener(move |this, _, _, cx| {
                if this.available() {
                    this.database_input
                        .update(cx, |input, cx| input.set_value(name.clone(), cx));
                    this.change_database(cx);
                }
            }))
            .into_any_element()
    }

    fn activate_object_menu(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if !self.interactive() {
            return;
        }
        let Some(menu) = self.object_menu.as_ref() else {
            return;
        };
        let actions = dialect::sections(self.engine(), &menu.table);
        if index >= actions.len() + 2 || (index < actions.len() && !self.available()) {
            return;
        }
        let menu = self.object_menu.take().expect("menu checked above");
        self.menu_return_focus
            .take()
            .unwrap_or_else(|| self.focus.clone())
            .focus(window);
        if let Some(&section) = actions.get(index) {
            if self.available() {
                self.selected_table = Some(menu.table);
                self.table_action(section, cx);
            }
        } else {
            let qualified = format!(
                "{}.{}",
                quote_identifier(self.engine(), &menu.table.schema),
                quote_identifier(self.engine(), &menu.table.name)
            );
            if index == actions.len() {
                self.open_query_text(
                    format!("SELECT *\nFROM {qualified}\nLIMIT 100;"),
                    window,
                    cx,
                );
            } else {
                cx.write_to_clipboard(ClipboardItem::new_string(qualified));
            }
        }
        cx.notify();
    }

    pub(super) fn object_menu_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let engine = self.engine();
        let available = self.available();
        let interactive = self.interactive();
        let Some(menu) = &mut self.object_menu else {
            return;
        };
        let count = dialect::sections(engine, &menu.table).len() + 2;
        match event.keystroke.key.as_str() {
            "escape" => self.close_object_menu(window, cx),
            "up" | "down" | "tab" | "home" | "end" => {
                let enabled: Vec<_> = (0..count)
                    .map(|i| {
                        if i + 2 < count {
                            available
                        } else {
                            interactive
                        }
                    })
                    .collect();
                if let Some(index) = ui::popup::next_index(event, menu.selected, &enabled) {
                    menu.selected = index;
                    self.menu_scroll.scroll_to_item(index);
                }
            }
            "enter" | "return" | "space" => {
                let selected = menu.selected;
                self.activate_object_menu(selected, window, cx);
            }
            _ => {}
        }
        window.prevent_default();
        cx.stop_propagation();
        cx.notify();
    }

    fn close_object_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.object_menu = None;
        self.menu_return_focus
            .take()
            .unwrap_or_else(|| self.focus.clone())
            .focus(window);
        cx.notify();
    }

    pub(super) fn render_object_menu(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let menu = self.object_menu.as_ref()?;
        let mut actions: Vec<_> = dialect::sections(self.engine(), &menu.table)
            .into_iter()
            .map(|section| {
                (
                    section.icon(),
                    section.label(self.engine()),
                    self.available(),
                )
            })
            .collect();
        actions.push((
            Icon::Terminal,
            i18n_key::DATABASE_QUERY_OBJECT,
            self.interactive(),
        ));
        actions.push((
            Icon::Copy,
            i18n_key::DATABASE_COPY_OBJECT_NAME,
            self.interactive(),
        ));
        let bounds = ui::popup::fit(
            menu.position,
            gpui::size(
                px(240.),
                px(actions.len() as f32 * ui::metrics::ROW + 2. * ui::popup::PADDING),
            ),
            gpui::Bounds::new(
                gpui::point(px(0.), px(0.)),
                gpui::size(px(self.viewport_size.0), px(self.viewport_size.1)),
            ),
        );
        let popup = ui::popup::surface("database-object-menu")
            .absolute()
            .left(bounds.left())
            .top(bounds.top())
            .w(bounds.size.width)
            .max_h(bounds.size.height)
            .overflow_y_scroll()
            .map(ui::preserve_scroll_axes)
            .track_scroll(&self.menu_scroll)
            .children(
                actions
                    .into_iter()
                    .enumerate()
                    .map(|(i, (icon, label, enabled))| {
                        ui::popup::row(("object-menu", i), enabled, i == menu.selected)
                            .child(ui::icon(icon))
                            .child(div().flex_1().min_w_0().truncate().child(tr(label)))
                            .on_mouse_move(cx.listener(move |this, _, _, cx| {
                                if enabled
                                    && let Some(menu) = &mut this.object_menu
                                    && menu.selected != i
                                {
                                    menu.selected = i;
                                    cx.notify();
                                }
                            }))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                if enabled {
                                    this.activate_object_menu(i, window, cx);
                                }
                                cx.stop_propagation();
                            }))
                    }),
            );
        Some(
            div()
                .absolute()
                .inset_0()
                .occlude()
                .on_any_mouse_down(cx.listener(|this, _, window, cx| {
                    this.close_object_menu(window, cx);
                    window.prevent_default();
                    cx.stop_propagation();
                }))
                .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                .child(ui::scrollable(popup, Some(self.menu_scroll.clone())))
                .into_any_element(),
        )
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn filters_reveal_matches_in_collapsed_schema_and_view_groups() {
        let tables = vec![
            TableInfo {
                schema: "app".into(),
                name: "events".into(),
                kind: "TABLE".into(),
            },
            TableInfo {
                schema: "report".into(),
                name: "daily".into(),
                kind: "VIEW".into(),
            },
        ];
        let collapsed = BTreeSet::from([
            GroupKey::Schema("report".into()),
            GroupKey::Kind("app".into(), false),
        ]);
        let rows = object_rows(&tables, &[0, 1], true, &collapsed, false);
        assert!(!rows.iter().any(|r| matches!(r, ObjectRow::Object { .. })));
        let rows = object_rows(&tables, &[1], true, &collapsed, true);
        assert!(matches!(
            rows.last(),
            Some(ObjectRow::Object { index: 1, depth: 2 })
        ));
        assert_eq!(rows.len(), 3);
    }
}
