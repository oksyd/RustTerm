use super::menus::{Menu, MenuAction};
use super::sidebar::{rdp_target, telnet_target};
use super::{Tab, Workspace};
use crate::app::pane::{Pane, PaneEvent};
use crate::app::ui;
use crate::app::ui::{Icon, icon, icon_button};
use crate::app::window_controls;
use crate::app::window_frame::WindowFrame;
use gpui::prelude::*;
use gpui::{
    AnyElement, App, Context, Decorations, Div, MouseButton, MouseDownEvent, Stateful, Window,
    WindowControlArea, canvas, div, px, relative, rgb, rgba,
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
        Some(Pane::Terminal(view)) if view.read(cx).is_container() => Icon::Container,
        Some(Pane::Terminal(_)) | None => Icon::Terminal,
    }
}

fn native_window_control(button: Stateful<Div>, area: WindowControlArea) -> Stateful<Div> {
    button.when(cfg!(target_os = "windows"), |button| {
        button
            .window_control_area(area)
            .on_mouse_down(MouseButton::Left, |_, window, cx| {
                // Keep native hover hit testing (including Snap Layouts), while
                // our click handler performs the action exactly once.
                window.prevent_default();
                cx.stop_propagation();
            })
    })
}

const TITLEBAR_BUTTON_WIDTH: f32 = 38.;
const TITLEBAR_BUTTON_VISUAL_SIZE: f32 = 28.;
const WINDOW_CONTROL_GAP: f32 = 12.;
const TITLEBAR_RIGHT_PADDING: f32 = 8.;

fn titlebar_button(
    id: &'static str,
    glyph: Icon,
    label: &'static str,
    danger: bool,
) -> Stateful<Div> {
    let hover_background = if danger {
        ui::colors().danger_bg
    } else {
        ui::colors().hover
    };
    let hover_foreground = if danger {
        ui::colors().danger
    } else {
        ui::colors().text
    };
    // Keep the full caption hit area, including Windows' native Max hit test.
    // Only the visual background is circular; corner clicks still work.
    div()
        .id(id)
        .group("titlebar-button")
        .relative()
        .occlude()
        .w(px(TITLEBAR_BUTTON_WIDTH))
        .h_full()
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .tab_index(0)
        .cursor_pointer()
        .rounded(px(6.))
        .border_1()
        .border_color(rgba(0))
        .focus(|style| style.border_color(rgb(ui::colors().accent)))
        .tooltip(move |_, cx| cx.new(|_| ui::Tooltip(tr(label).to_owned())).into())
        .child(
            div()
                .size(px(TITLEBAR_BUTTON_VISUAL_SIZE))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .bg(rgb(ui::colors().raised))
                .group_hover("titlebar-button", move |style| {
                    style.bg(rgb(hover_background))
                })
                .child(
                    icon(glyph)
                        .text_color(rgb(ui::colors().muted))
                        .group_hover("titlebar-button", move |style| {
                            style.text_color(rgb(hover_foreground))
                        }),
                ),
        )
}

impl Workspace {
    fn caption_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.lock_screen.locked && self.menu.take().is_some() {
            self.restore_popup_focus(window, cx);
            cx.notify();
        }
        // Ancestor focus handlers prevent Windows' default caption action.
        // Own the gesture once, then hand movement to the native move loop.
        window.prevent_default();
        cx.stop_propagation();
        let accepted = if event.click_count == 2 {
            window_controls::toggle_size(window)
        } else {
            window_controls::start_move(window)
        };
        if !accepted {
            self.window_state.record("window-caption-request-failed", 0);
        }
    }

    pub(super) fn render_titlebar(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        // GPUI's Windows backend reports Server even with its caption hidden.
        // Keep its native resize frame/shadow, but draw our own title and controls.
        let custom_titlebar = cfg!(target_os = "windows")
            || matches!(window.window_decorations(), Decorations::Client { .. });
        let corners = WindowFrame::new(window).corners;
        let application_menu = self.application_menu().filter(|_| !self.lock_screen.locked);
        let controls = window.window_controls();
        let show_settings = !self.lock_screen.locked;
        let show_minimize = custom_titlebar && controls.minimize;
        let show_maximize = custom_titlebar && (controls.maximize || window.is_fullscreen());
        let actions_width = TITLEBAR_RIGHT_PADDING
            + TITLEBAR_BUTTON_WIDTH
                * (usize::from(show_settings)
                    + usize::from(show_minimize)
                    + usize::from(show_maximize)
                    + usize::from(custom_titlebar)) as f32
            + if show_settings && custom_titlebar {
                WINDOW_CONTROL_GAP
            } else {
                0.
            };
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
                    .pl(px(52.))
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
                    .when(cfg!(target_os = "windows"), |title| {
                        title.child(
                            div()
                                .id("window-drag-region")
                                .absolute()
                                .left_0()
                                .right_0()
                                .bottom_0()
                                // Leave the top sizing band to Windows while floating.
                                .top(px(if window.is_maximized() || window.is_fullscreen() {
                                    0.
                                } else {
                                    8.
                                }))
                                .window_control_area(WindowControlArea::Drag)
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(Self::caption_mouse_down),
                                ),
                        )
                    })
                    .when(custom_titlebar && !cfg!(target_os = "windows"), |title| {
                        title
                            .on_mouse_down(MouseButton::Left, cx.listener(Self::caption_mouse_down))
                    })
                    .on_mouse_down(MouseButton::Right, move |event, window, _| {
                        if custom_titlebar && !cfg!(target_os = "windows") {
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
        if self.lock_screen.locked || application_menu.is_some() {
            bar = bar.child(div().flex_1());
        } else {
            // Reserve a stable brand/drag area aligned with the library at normal widths.
            let width = f32::from(
                WindowFrame::new(window)
                    .content_size(window.viewport_size())
                    .width,
            );
            let brand = if width >= 1100. {
                self.sidebar_width + 5.
            } else {
                172.
            };
            bar = bar
                .child(div().w(px(brand - 36.)).flex_shrink_0())
                .child(self.render_tabs((width - brand - actions_width - 24.).max(0.), cx))
                .child(div().w(px(24.)).h_full().flex_shrink_0());
        }
        let mut actions = div()
            .id("titlebar-actions")
            .relative()
            .h_full()
            .flex_shrink_0()
            .flex()
            .items_center()
            .pr(px(TITLEBAR_RIGHT_PADDING));
        if show_settings {
            actions = actions.child(
                ui::icon_button_with_size(
                    "titlebar-settings",
                    Icon::Settings,
                    i18n_key::WORKSPACE_SETTINGS_CTRL_COMMA,
                    18.,
                )
                .relative()
                .occlude()
                .w(px(TITLEBAR_BUTTON_WIDTH))
                .h_full()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .on_click(cx.listener(|this, _, window, cx| {
                    cx.stop_propagation();
                    this.menu_action(MenuAction::Settings, window, cx);
                })),
            );
        }
        if custom_titlebar {
            if show_settings {
                actions = actions.child(div().w(px(WINDOW_CONTROL_GAP)).h_full().flex_shrink_0());
            }
            if show_minimize {
                actions = actions.child(
                    titlebar_button(
                        "window-minimize",
                        Icon::Minimize,
                        i18n_key::WORKSPACE_MINIMIZE,
                        false,
                    )
                    .map(|button| native_window_control(button, WindowControlArea::Min))
                    .on_click(|_, window, cx| {
                        cx.stop_propagation();
                        window.minimize_window();
                    }),
                );
            }
            if show_maximize {
                let (glyph, label) = if window.is_fullscreen() {
                    (Icon::ExitFullscreen, i18n_key::WORKSPACE_EXIT_FULLSCREEN)
                } else if window.is_maximized() {
                    (Icon::Restore, i18n_key::WORKSPACE_RESTORE_WINDOW)
                } else {
                    (Icon::Maximize, i18n_key::WORKSPACE_MAXIMIZE_WINDOW)
                };
                actions = actions.child(
                    titlebar_button("window-maximize", glyph, label, false)
                        .map(|button| native_window_control(button, WindowControlArea::Max))
                        .on_click(cx.listener(|this, _, window, cx| {
                            cx.stop_propagation();
                            if !window_controls::toggle_size(window) {
                                this.window_state.record("window-size-request-failed", 0);
                            }
                        })),
                );
            }
            actions = actions.child(
                titlebar_button(
                    "window-close",
                    Icon::Close,
                    i18n_key::WORKSPACE_CLOSE_WINDOW,
                    true,
                )
                .map(|button| native_window_control(button, WindowControlArea::Close))
                .on_click(cx.listener(|this, _, window, cx| {
                    cx.stop_propagation();
                    this.request_exit(window, cx);
                })),
            );
        }
        bar.child(actions).into_any_element()
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
            leaves
                .first()
                .and_then(|id| self.pane_profiles.get(id))
                .map(|profile| Icon::for_connection(&profile.connection))
                .unwrap_or_else(|| pane_icon(leaves.first().and_then(|id| self.panes.get(id)), cx))
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
            .group("workspace-tab")
            .relative()
            .occlude()
            .h_full()
            .w(px(TAB_WIDTH))
            .min_w(px(TAB_WIDTH))
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
                active && matches!(glyph, Icon::Terminal | Icon::Ssh | Icon::SplitHorizontal),
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
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                    this.open_tab_menu(id, event.position, window, cx);
                    window.prevent_default();
                    cx.stop_propagation();
                }),
            )
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
                .size(px(24.))
                .when(!active, |button| {
                    button
                        .opacity(0.)
                        .group_hover("workspace-tab", |style| style.opacity(1.))
                })
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
        let short_name = members
            .iter()
            .flat_map(|tab| tab.layout.leaves())
            .find_map(|id| self.pane_profiles.get(&id))
            .and_then(|profile| match &profile.connection {
                Connection::Ssh(config) => Some(short_group_name(&config.host)),
                _ => None,
            })
            .unwrap_or_else(|| name.clone());
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
            .w(px(GROUP_WIDTH))
            .occlude()
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
                    .child(div().flex_1().min_w_0().truncate().child(short_name))
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
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                    this.open_tab_group_menu(profile, event.position, window, cx);
                    window.prevent_default();
                    cx.stop_propagation();
                }),
            )
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

    pub(super) fn render_tabs(&self, available: f32, cx: &mut Context<Self>) -> AnyElement {
        let groups = self.tab_groups();
        let entries: Vec<_> = groups
            .iter()
            .flat_map(|group| {
                let folded = group
                    .profile
                    .is_some_and(|profile| self.collapsed_tab_groups.contains(&profile));
                group
                    .tabs
                    .iter()
                    .take(if folded { 1 } else { usize::MAX })
                    .map(move |id| StripEntry {
                        id: *id,
                        profile: group.profile,
                        folded,
                    })
            })
            .collect();
        let active = self.tabs.get(self.active).map(|tab| {
            groups
                .iter()
                .find(|group| {
                    group.tabs.contains(&tab.id)
                        && group
                            .profile
                            .is_some_and(|id| self.collapsed_tab_groups.contains(&id))
                })
                .map_or(tab.id, |group| group.tabs[0])
        });
        let visible = visible_strip(&entries, active, (available - 76.).max(0.));
        let mut strip = div()
            .id("tab-strip")
            .relative()
            .h_full()
            .flex_1()
            .min_w_0()
            .flex()
            .items_center();
        let mut previous = None;
        for entry in &entries[visible.clone()] {
            if let Some(profile) = entry.profile
                && previous != Some(profile)
                && let Some(group) = groups.iter().find(|group| group.profile == Some(profile))
            {
                strip = strip.child(self.render_tab_group(profile, group, cx));
            }
            previous = entry.profile;
            if !entry.folded
                && let Some(tab) = self.tabs.iter().find(|tab| tab.id == entry.id)
            {
                strip = strip.child(self.render_workspace_tab(tab, entry.profile, cx));
            }
        }
        strip
            .child(
                icon_button(
                    "new-workspace-tab",
                    Icon::Plus,
                    i18n_key::WORKSPACE_NEW_LOCAL_WORKSPACE_TAB_CTRL_SHIFT_T,
                )
                .relative()
                .occlude()
                .size(px(32.))
                .mx_1()
                .tab_index(0)
                .on_click(cx.listener(|this, _, window, cx| {
                    this.pane_event(&PaneEvent::NewTab, window, cx)
                })),
            )
            .when(
                visible.len() < entries.len() || entries.iter().any(|entry| entry.folded),
                |strip| {
                    strip.child(
                        icon_button(
                            "all-workspace-tabs",
                            Icon::ChevronDown,
                            i18n_key::WORKSPACE_ALL_TABS,
                        )
                        .relative()
                        .occlude()
                        .size(px(28.))
                        .tab_index(0)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.toggle_menu(Menu::Tabs, window, cx)
                        })),
                    )
                },
            )
            .into_any_element()
    }
    pub(super) fn render_statusbar(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let corners = WindowFrame::new(window).corners;
        let compact = f32::from(window.viewport_size().width) < 760.;
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
            })
            .unwrap_or_default();
        let name = self
            .pane_display_name(self.focused)
            .or_else(|| match pane {
                Some(Pane::Terminal(view)) => Some(view.read(cx).display_name().to_owned()),
                _ => None,
            })
            .unwrap_or_default();
        let tooltip = format!("{name} · {state}\n{target}\n{detail}");
        let protocol = profile.map_or(
            if matches!(pane, Some(Pane::Terminal(view)) if view.read(cx).is_container()) {
                "Docker"
            } else {
                ""
            },
            |profile| match &profile.connection {
                Connection::Ssh(_) => "SSH",
                Connection::Telnet(_) => "Telnet",
                Connection::Local(_) => tr(i18n_key::TERMINAL_LOCAL),
                Connection::Redis(_) => "Redis",
                Connection::Elasticsearch(_) => "Elasticsearch",
                Connection::Mysql(_) => "MySQL",
                Connection::ClickHouse(_) => "ClickHouse",
                Connection::Postgres(_) => "PostgreSQL",
                Connection::Sqlite(_) => "SQLite",
                Connection::Rdp(_) => "RDP",
            },
        );
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
            .child(self.render_panel_buttons(cx))
            .child(match pane {
                Some(Pane::Rdp(view)) => {
                    view.update(cx, |view, cx| view.render_status(compact, cx))
                }
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
                    .child(
                        div()
                            .flex_shrink_0()
                            .max_w(relative(0.75))
                            .truncate()
                            .child(label),
                    )
                    .child(div().min_w_0().truncate().child(name))
                    .into_any_element(),
            })
            .when_some(self.resource_summary(), |bar, summary| bar.child(summary))
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

const TAB_WIDTH: f32 = 180.;
const GROUP_WIDTH: f32 = 104.;
#[derive(Clone, Copy)]
struct StripEntry {
    id: usize,
    profile: Option<Uuid>,
    folded: bool,
}

fn strip_width(entries: &[StripEntry]) -> f32 {
    let mut previous = None;
    entries
        .iter()
        .map(|entry| {
            let group = if entry.profile.is_some() && previous != entry.profile {
                GROUP_WIDTH
            } else {
                0.
            };
            previous = entry.profile;
            group + if entry.folded { 0. } else { TAB_WIDTH }
        })
        .sum()
}

/// Keep whole tabs and their group labels visible; reserve the active tab first.
fn visible_strip(
    entries: &[StripEntry],
    active: Option<usize>,
    width: f32,
) -> std::ops::Range<usize> {
    if entries.is_empty() {
        return 0..0;
    }
    let center = entries
        .iter()
        .position(|entry| Some(entry.id) == active)
        .unwrap_or(0);
    let mut range = center..center + 1;
    // Very small windows use the tab menu instead of painting a clipped close target.
    if strip_width(&entries[range.clone()]) > width {
        return center..center;
    }
    while range.start > 0 && strip_width(&entries[range.start - 1..range.end]) <= width {
        range.start -= 1;
    }
    while range.end < entries.len() && strip_width(&entries[range.start..range.end + 1]) <= width {
        range.end += 1;
    }
    range
}

fn short_group_name(host: &str) -> String {
    if host.parse::<std::net::IpAddr>().is_ok() {
        host.to_owned()
    } else {
        host.split('.').next().unwrap_or(host).to_owned()
    }
}

#[cfg(test)]
mod strip_tests {
    use super::*;
    #[test]
    fn overflow_keeps_active_tab_and_its_group_with_whole_close_targets() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let entries = [
            StripEntry {
                id: 1,
                profile: Some(a),
                folded: false,
            },
            StripEntry {
                id: 2,
                profile: Some(a),
                folded: false,
            },
            StripEntry {
                id: 3,
                profile: Some(b),
                folded: false,
            },
            StripEntry {
                id: 4,
                profile: None,
                folded: false,
            },
        ];
        for active in 1..=4 {
            for width in [284., 464., 748., 1000.] {
                let visible = visible_strip(&entries, Some(active), width);
                assert!(
                    entries[visible.clone()]
                        .iter()
                        .any(|entry| entry.id == active)
                );
                assert!(strip_width(&entries[visible]) <= width);
            }
        }
        assert!(visible_strip(&entries, Some(2), 100.).is_empty());
        assert!(visible_strip(&[], None, 1000.).is_empty());
    }
    #[test]
    fn folded_groups_and_unavailable_active_ids_remain_accessible() {
        let entries = [
            StripEntry {
                id: 1,
                profile: Some(Uuid::new_v4()),
                folded: true,
            },
            StripEntry {
                id: 2,
                profile: None,
                folded: false,
            },
        ];
        assert_eq!(visible_strip(&entries, Some(99), GROUP_WIDTH), 0..1);
        assert_eq!(visible_strip(&entries, Some(2), TAB_WIDTH), 1..2);
        assert_eq!(short_group_name("app-01.example.test"), "app-01");
        assert_eq!(short_group_name("192.0.2.10"), "192.0.2.10");
        assert_eq!(short_group_name("2001:db8::1"), "2001:db8::1");
    }
}
