use super::*;
use crate::app::text_input::TextInput;
use gpui::{Entity, FocusHandle, ScrollHandle};

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(in crate::app::forms::profile) enum RdpPage {
    #[default]
    Connection,
    Display,
    Resources,
    Advanced,
}

impl RdpPage {
    fn label(self) -> &'static str {
        match self {
            Self::Connection => i18n_key::RDP_CONNECTION,
            Self::Display => i18n_key::CONNECTIONS_DISPLAY_TAB,
            Self::Resources => i18n_key::RDP_LOCAL_RESOURCES,
            Self::Advanced => i18n_key::CONNECTIONS_ADVANCED_TAB,
        }
    }
}

pub(in crate::app::forms::profile) struct RdpFormView {
    pub page: RdpPage,
    pub custom_dimensions: bool,
    pub custom_quality: bool,
    pub launch_options: bool,
    scroll: ScrollHandle,
    pub(super) tab_focus: [FocusHandle; 4],
}

impl RdpFormView {
    pub fn new(state: &RdpFormState, cx: &App) -> Self {
        Self {
            page: RdpPage::Connection,
            custom_dimensions: !display::RESOLUTIONS
                .contains(&(state.settings.width, state.settings.height)),
            custom_quality: state.settings.quality() == RdpQuality::Custom,
            launch_options: false,
            scroll: ScrollHandle::new(),
            tab_focus: std::array::from_fn(|_| cx.focus_handle()),
        }
    }
}

impl ProfileForm {
    pub fn scroll_handle(&self) -> Option<ScrollHandle> {
        Some(if self.kind == super::super::ProfileKind::Rdp {
            self.rdp_view.scroll.clone()
        } else {
            self.page_scroll[usize::from(self.advanced)].clone()
        })
    }

    pub(in crate::app::forms::profile) fn rdp_inputs(&self, cx: &App) -> Vec<Entity<TextInput>> {
        match self.rdp_view.page {
            RdpPage::Connection => {
                let mut inputs = self.fields.selected_inputs(FIELDS);
                if self.credential_input_visible(cx) {
                    inputs.push(self.credentials.input.clone());
                }
                if self.rdp.settings.remote_app.is_some() {
                    inputs.push(self.fields.field(REMOTE_APP[0]).clone());
                    if self.rdp_view.launch_options {
                        inputs.extend(self.fields.selected_inputs(&REMOTE_APP[1..]));
                    }
                }
                inputs.push(self.fields.field(i18n_key::COMMON_NAME).clone());
                inputs
            }
            RdpPage::Display
                if self.rdp_view.custom_dimensions
                    && self.rdp.display_mode() != DisplayMode::FollowWindow =>
            {
                self.fields.selected_inputs(DIMENSIONS)
            }
            RdpPage::Advanced => self.fields.selected_inputs(ADVANCED),
            _ => Vec::new(),
        }
    }

    fn select_rdp_page(&mut self, page: RdpPage, window: &mut Window, cx: &mut Context<Self>) {
        if self.native_pending || self.prompts.is_blocked() {
            return;
        }
        self.rdp_view.page = page;
        self.rdp_view.scroll.set_offset(gpui::Point::default());
        self.group_open = false;
        self.rdp_view.tab_focus[page as usize].focus(window);
        cx.notify();
    }

    pub(in crate::app::forms::profile) fn rdp_body(
        &self,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let pages = [
            RdpPage::Connection,
            RdpPage::Display,
            RdpPage::Resources,
            RdpPage::Advanced,
        ];
        let navigation =
            ui::preferences_navigation(compact).children(pages.into_iter().map(|page| {
                let glyph = match page {
                    RdpPage::Connection => Icon::Connections,
                    RdpPage::Display => Icon::Desktop,
                    RdpPage::Resources => Icon::Folder,
                    RdpPage::Advanced => Icon::Settings,
                };
                ui::preferences_item(
                    ("rdp-section", page as usize),
                    tr(page.label()),
                    glyph,
                    self.rdp_view.page == page,
                    compact,
                )
                .track_focus(
                    &self.rdp_view.tab_focus[page as usize]
                        .clone()
                        .tab_stop(self.rdp_view.page == page),
                )
                .on_key_down(
                    cx.listener(move |this, event: &gpui::KeyDownEvent, window, cx| {
                        if event.keystroke.modifiers.modified()
                            || !this.rdp_view.tab_focus[page as usize].is_focused(window)
                        {
                            return;
                        }
                        let next = match event.keystroke.key.as_str() {
                            "left" | "up" => (page as usize + 3) % 4,
                            "right" | "down" => (page as usize + 1) % 4,
                            "home" => 0,
                            "end" => 3,
                            _ => return,
                        };
                        this.select_rdp_page(pages[next], window, cx);
                        window.prevent_default();
                        cx.stop_propagation();
                    }),
                )
                .on_click(
                    cx.listener(move |this, _, window, cx| this.select_rdp_page(page, window, cx)),
                )
            }));
        let body = column()
            .gap(px(16.))
            .child(match self.rdp_view.page {
                RdpPage::Connection => self.rdp_connection(cx),
                RdpPage::Display => self.rdp_display(cx),
                RdpPage::Resources => self.rdp_resources(cx),
                RdpPage::Advanced => self.rdp_advanced(cx),
            })
            .when(self.original.is_some(), |body| {
                body.child(hint(
                    i18n_key::RDP_SAVED_CHANGES_APPLY_TO_THE_NEXT_CONNECTION_THE_CURRENT_SESSION,
                ))
            });
        ui::preferences_layout(
            navigation,
            ("rdp-profile-page", self.rdp_view.page as usize),
            tr(self.rdp_view.page.label()),
            body,
            self.rdp_view.scroll.clone(),
            compact,
        )
        .into_any_element()
    }

    fn rdp_connection(&self, cx: &mut Context<Self>) -> AnyElement {
        column()
            .child(self.host_port())
            .child(self.fields.selected_rows(&[i18n_key::WORKSPACE_USERNAME]))
            .child(self.credential_editor(cx))
            .child(self.fields.selected_rows(&[i18n_key::RDP_DOMAIN_OPTIONAL]))
            .child(self.rdp_choices(
                "rdp-session-mode",
                i18n_key::RDP_REMOTE_APP_MODE,
                self.rdp.settings.remote_app.is_some(),
                [
                    (false, i18n_key::RDP_REMOTE_APP_DESKTOP),
                    (true, i18n_key::RDP_REMOTE_APP),
                ],
                |this, enabled, window, cx| {
                    if enabled != this.rdp.settings.remote_app.is_some() {
                        this.rdp.settings.remote_app = enabled.then(RdpRemoteApp::default);
                    }
                    this.fields
                        .field(if enabled {
                            REMOTE_APP[0]
                        } else {
                            i18n_key::WORKSPACE_USERNAME
                        })
                        .read(cx)
                        .focus(window);
                },
                cx,
            ))
            .when(self.rdp.settings.remote_app.is_some(), |body| {
                body.child(self.fields.selected_rows(&REMOTE_APP[..1]))
                    .child(hint(i18n_key::RDP_REMOTE_APP_HINT))
                    .child(self.remote_app_options(cx))
            })
            .child(self.profile_details(cx))
            .into_any_element()
    }

    fn remote_app_options(&self, cx: &mut Context<Self>) -> AnyElement {
        let configured = REMOTE_APP[1..]
            .iter()
            .any(|key| !self.fields.get(key, cx).is_empty());
        column()
            .child(
                ui::text_button(
                    "rdp-launch-options",
                    i18n_key::CONNECTIONS_LAUNCH_OPTIONS,
                    ButtonStyle::Neutral,
                )
                .tab_index(0)
                .flex()
                .items_center()
                .gap_2()
                .child(ui::icon(if self.rdp_view.launch_options {
                    Icon::ChevronDown
                } else {
                    Icon::ChevronRight
                }))
                .when(configured, |button| {
                    button.child(
                        div()
                            .text_color(rgb(ui::colors().accent))
                            .child(tr(i18n_key::CONNECTIONS_CONFIGURED)),
                    )
                })
                .on_click(cx.listener(|this, _, window, cx| {
                    if this.native_pending || this.prompts.is_blocked() {
                        return;
                    }
                    this.fields.field(REMOTE_APP[0]).read(cx).focus(window);
                    this.rdp_view.launch_options = !this.rdp_view.launch_options;
                    cx.notify();
                })),
            )
            .when(self.rdp_view.launch_options, |body| {
                body.child(self.fields.selected_rows(&REMOTE_APP[1..]))
            })
            .into_any_element()
    }

    pub(in crate::app::forms::profile) fn reveal_rdp_field(&mut self, field: &str) {
        if DIMENSIONS.contains(&field) {
            self.rdp_view.page = RdpPage::Display;
            self.rdp_view.custom_dimensions = true;
        } else if ADVANCED.contains(&field) {
            self.rdp_view.page = RdpPage::Advanced;
        } else {
            self.rdp_view.page = RdpPage::Connection;
            if REMOTE_APP[1..].contains(&field) {
                self.rdp_view.launch_options = true;
            }
        }
    }
}
