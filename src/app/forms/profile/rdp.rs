//! Remote desktop preferences retain credentials and certificate trust by identity.
use rustterm::i18n::messages as i18n_key;
mod display;
mod resources;

use super::super::{column, hint, parse_value, row};
use super::ProfileForm;
use crate::app::{
    text_input::TextInput,
    ui::{self, ButtonStyle, Icon},
};
use gpui::{AnyElement, App, Context, Div, Entity, Window, div, prelude::*, px, rgb};
use rustterm::{
    config::{Connection, Profile, RdpConfig, RdpQuality, RdpSharedFolder, Uuid},
    i18n::tr,
};

const FIELDS: &[&str] = &[
    i18n_key::WORKSPACE_HOST,
    i18n_key::WORKSPACE_PORT,
    i18n_key::WORKSPACE_USERNAME,
    i18n_key::RDP_DOMAIN_OPTIONAL,
];
const DIMENSIONS: &[&str] = &[
    i18n_key::RDP_DESKTOP_WIDTH_PIXELS,
    i18n_key::RDP_DESKTOP_HEIGHT_PIXELS,
];
pub(super) const ADVANCED: &[&str] = &[i18n_key::WORKSPACE_CONNECTION_TIMEOUT_SECONDS];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DisplayMode {
    FollowWindow,
    Fixed,
    VirtualMonitors,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ClipboardMode {
    Off,
    Text,
    Files,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct RdpFormState {
    settings: RdpConfig,
    clear_certificate: bool,
}

impl RdpFormState {
    fn display_mode(&self) -> DisplayMode {
        if self.settings.monitor_count > 1 {
            DisplayMode::VirtualMonitors
        } else if self.settings.dynamic_resolution {
            DisplayMode::FollowWindow
        } else {
            DisplayMode::Fixed
        }
    }

    fn clipboard_mode(&self) -> ClipboardMode {
        match (self.settings.clipboard, self.settings.clipboard_files) {
            (false, _) => ClipboardMode::Off,
            (true, false) => ClipboardMode::Text,
            (true, true) => ClipboardMode::Files,
        }
    }
}

pub(super) fn initial_fields(
    profile: Option<&Profile>,
    values: &mut Vec<(&'static str, String)>,
) -> RdpFormState {
    let settings = profile
        .and_then(|profile| match &profile.connection {
            Connection::Rdp(settings) => Some(settings.clone()),
            _ => None,
        })
        .unwrap_or_default();
    values.extend([
        (i18n_key::WORKSPACE_HOST, settings.host.clone()),
        (i18n_key::WORKSPACE_PORT, settings.port.to_string()),
        (i18n_key::WORKSPACE_USERNAME, settings.username.clone()),
        (
            i18n_key::RDP_DOMAIN_OPTIONAL,
            settings.domain.clone().unwrap_or_default(),
        ),
        (
            i18n_key::RDP_DESKTOP_WIDTH_PIXELS,
            settings.width.to_string(),
        ),
        (
            i18n_key::RDP_DESKTOP_HEIGHT_PIXELS,
            settings.height.to_string(),
        ),
        (
            i18n_key::WORKSPACE_CONNECTION_TIMEOUT_SECONDS,
            settings.timeout_secs.to_string(),
        ),
    ]);
    RdpFormState {
        settings,
        clear_certificate: false,
    }
}

impl ProfileForm {
    pub(super) fn rdp_inputs(&self, cx: &App) -> Vec<Entity<TextInput>> {
        let mut inputs = self.fields.selected_inputs(FIELDS);
        if self.credential_input_visible(cx) {
            inputs.push(self.credentials.input.clone());
        }
        if self.rdp.display_mode() != DisplayMode::FollowWindow {
            inputs.extend(self.fields.selected_inputs(DIMENSIONS));
        }
        if self.advanced {
            inputs.extend(self.fields.selected_inputs(ADVANCED));
        }
        inputs
    }

    pub(super) fn rdp_config(
        &self,
        current_profiles: &[Profile],
        cx: &App,
    ) -> anyhow::Result<RdpConfig> {
        let get = |key| self.fields.get(key, cx);
        let domain = get(i18n_key::RDP_DOMAIN_OPTIONAL);
        let mut settings = self.rdp.settings.clone();
        settings.host = get(i18n_key::WORKSPACE_HOST);
        settings.port = parse_value(i18n_key::WORKSPACE_PORT, &get(i18n_key::WORKSPACE_PORT))?;
        settings.username = get(i18n_key::WORKSPACE_USERNAME);
        settings.domain = (!domain.is_empty()).then_some(domain);
        settings.dynamic_resolution = self.rdp.display_mode() == DisplayMode::FollowWindow;
        if !settings.dynamic_resolution {
            settings.width = parse_value(
                i18n_key::RDP_DESKTOP_WIDTH_PIXELS,
                &get(i18n_key::RDP_DESKTOP_WIDTH_PIXELS),
            )?;
            settings.height = parse_value(
                i18n_key::RDP_DESKTOP_HEIGHT_PIXELS,
                &get(i18n_key::RDP_DESKTOP_HEIGHT_PIXELS),
            )?;
        }
        settings.timeout_secs = parse_value(
            i18n_key::WORKSPACE_CONNECTION_TIMEOUT_SECONDS,
            &get(i18n_key::WORKSPACE_CONNECTION_TIMEOUT_SECONDS),
        )?;
        preserve_references(
            &mut settings,
            self.original.as_ref().map(|profile| profile.id),
            current_profiles,
            self.rdp.clear_certificate,
        );
        settings.validate()?;
        Ok(settings)
    }

    pub(super) fn rdp_body(&self, cx: &mut Context<Self>) -> AnyElement {
        column()
            .child(
                section(i18n_key::RDP_CONNECTION)
                    .child(self.fields.selected_rows(FIELDS))
                    .child(self.credential_editor(cx)),
            )
            .child(self.rdp_display(cx))
            .child(self.rdp_resources(cx))
            .child(
                ui::text_button(
                    "rdp-advanced",
                    i18n_key::RDP_ADVANCED_RDP_SETTINGS,
                    ButtonStyle::Neutral,
                )
                .flex()
                .items_center()
                .gap_1()
                .tab_index(0)
                .border_1()
                .border_color(rgb(ui::colors().border))
                .focus(|style| style.border_color(rgb(ui::colors().accent)))
                .child(ui::icon(if self.advanced {
                    Icon::ChevronDown
                } else {
                    Icon::ChevronRight
                }))
                .on_click(cx.listener(|this, _, window, cx| {
                    if this.native_pending || this.prompts.is_blocked() {
                        return;
                    }
                    if this.advanced
                        && this
                            .fields
                            .selected_inputs(ADVANCED)
                            .iter()
                            .any(|input| input.read(cx).focus_handle().is_focused(window))
                    {
                        this.fields
                            .field(i18n_key::WORKSPACE_USERNAME)
                            .read(cx)
                            .focus(window);
                    }
                    this.advanced = !this.advanced;
                    cx.notify();
                })),
            )
            .when(self.advanced, |body| body.child(self.rdp_advanced(cx)))
            .when(self.original.is_some(), |body| {
                body.child(hint(
                    i18n_key::RDP_SAVED_CHANGES_APPLY_TO_THE_NEXT_CONNECTION_THE_CURRENT_SESSION,
                ))
            })
            .into_any_element()
    }

    fn rdp_choices<T: Copy + PartialEq + 'static>(
        &self,
        id: &'static str,
        label: &'static str,
        selected: T,
        choices: impl IntoIterator<Item = (T, &'static str)>,
        select: fn(&mut Self, T, &mut Window, &mut Context<Self>),
        cx: &mut Context<Self>,
    ) -> AnyElement {
        row(
            label,
            div()
                .flex()
                .flex_wrap()
                .gap_1()
                .children(
                    choices
                        .into_iter()
                        .enumerate()
                        .map(|(index, (value, label))| {
                            ui::text_button(
                                (id, index),
                                label,
                                if selected == value {
                                    ButtonStyle::Primary
                                } else {
                                    ButtonStyle::Neutral
                                },
                            )
                            .tab_index(0)
                            .whitespace_nowrap()
                            .border_1()
                            .border_color(rgb(if selected == value {
                                ui::colors().accent
                            } else {
                                ui::colors().border
                            }))
                            .focus(|style| style.border_color(rgb(ui::colors().text)))
                            .on_click(cx.listener(
                                move |this, _, window, cx| {
                                    if this.native_pending || this.prompts.is_blocked() {
                                        return;
                                    }
                                    select(this, value, window, cx);
                                    cx.notify();
                                },
                            ))
                        }),
                ),
        )
        .into_any_element()
    }

    fn rdp_toggle(
        &self,
        id: &'static str,
        label: &'static str,
        checked: bool,
        toggle: fn(&mut RdpConfig),
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .id(id)
            .flex()
            .items_center()
            .gap_2()
            .py_1()
            .px_2()
            .border_1()
            .border_color(rgb(ui::colors().border))
            .cursor_pointer()
            .tab_index(0)
            .focus(|style| style.border_color(rgb(ui::colors().accent)))
            .child(
                div()
                    .size(px(14.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .border_1()
                    .rounded(px(2.))
                    .border_color(rgb(if checked {
                        ui::colors().accent
                    } else {
                        ui::colors().border
                    }))
                    .when(checked, |checkbox| {
                        checkbox.child(ui::icon(Icon::Check).text_color(rgb(ui::colors().accent)))
                    }),
            )
            .child(div().flex_1().min_w_0().child(tr(label)))
            .on_click(cx.listener(move |this, _, _, cx| {
                if this.native_pending || this.prompts.is_blocked() {
                    return;
                }
                toggle(&mut this.rdp.settings);
                cx.notify();
            }))
            .into_any_element()
    }
}

fn section(label: &'static str) -> Div {
    column().pt_3().child(
        div()
            .pb_1()
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .child(tr(label)),
    )
}

fn preserve_references(
    current: &mut RdpConfig,
    profile_id: Option<Uuid>,
    profiles: &[Profile],
    clear_certificate: bool,
) {
    // Use the latest persisted references: vault operations can finish while the form is open.
    current.credential = None;
    current.certificate_sha256 = None;
    if let Some(Profile {
        connection: Connection::Rdp(previous),
        ..
    }) = profile_id.and_then(|id| profiles.iter().find(|profile| profile.id == id))
    {
        if previous.same_auth_identity(current) {
            current.credential = previous.credential;
        }
        if previous.same_server(current) && !clear_certificate {
            current.certificate_sha256 = previous
                .certificate_sha256
                .as_ref()
                .map(|pin| pin.to_ascii_lowercase());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::preserve_references;
    use rustterm::config::{Connection, CredentialRef, Profile, RdpConfig, Uuid};

    #[test]
    fn identity_changes_clear_only_the_relevant_rdp_references() {
        let saved = RdpConfig {
            username: "operator".into(),
            domain: Some("WORK".into()),
            credential: Some(CredentialRef(Uuid::from_u128(80))),
            certificate_sha256: Some("ab".repeat(32)),
            ..RdpConfig::default()
        };
        let profiles = [Profile::rdp("Desktop", saved.clone())];
        let id = Some(profiles[0].id);
        let mut preferences = RdpConfig {
            width: 1920,
            height: 1080,
            dynamic_resolution: false,
            clipboard: false,
            timeout_secs: 30,
            ..saved.clone()
        };
        preserve_references(&mut preferences, id, &profiles, false);
        assert_eq!(preferences.credential, saved.credential);
        assert_eq!(preferences.certificate_sha256, saved.certificate_sha256);

        for field in 0..4 {
            let mut changed = saved.clone();
            match field {
                0 => changed.host = "another.example.invalid".into(),
                1 => changed.port = 3390,
                2 => changed.username = "another-user".into(),
                _ => changed.domain = None,
            }
            preserve_references(&mut changed, id, &profiles, false);
            assert_eq!(changed.credential, None);
            assert_eq!(
                changed.certificate_sha256,
                if field < 2 {
                    None
                } else {
                    saved.certificate_sha256.clone()
                }
            );
        }
        preserve_references(&mut preferences, id, &profiles, true);
        assert_eq!(preferences.certificate_sha256, None);
        assert_eq!(preferences.credential, saved.credential);

        let cleared = [Profile {
            connection: Connection::Rdp(RdpConfig {
                credential: None,
                certificate_sha256: None,
                ..saved
            }),
            ..profiles[0].clone()
        }];
        preserve_references(&mut preferences, id, &cleared, false);
        assert_eq!(preferences.credential, None);
        assert_eq!(preferences.certificate_sha256, None);
    }
}
