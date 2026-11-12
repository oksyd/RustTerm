use rustterm::i18n::Message;
use rustterm::i18n::messages as i18n_key;
mod pages;
mod security;
mod updates;
use crate::app::updates::Updates;

use super::{Fields, column, hint, parse_value};
use crate::app::{
    text_input::{InputEvent, TextInput},
    ui::{self, Icon},
};
use gpui::{
    App, Context, Div, Entity, FocusHandle, ScrollHandle, Subscription, Window, div, prelude::*,
    px, rgb,
};
use rustterm::{
    app_lock,
    config::{AppLockConfig, AppTheme, Secret, Settings},
    i18n::{Language, tr},
};

const FONT_FIELDS: &[&str] = &[
    i18n_key::WORKSPACE_MONOSPACE_FONT,
    i18n_key::WORKSPACE_FONT_SIZE,
    i18n_key::WORKSPACE_LINE_HEIGHT_MULTIPLIER,
];
const COLOR_FIELDS: &[&str] = &[
    i18n_key::WORKSPACE_BACKGROUND_COLOR_RGB,
    i18n_key::WORKSPACE_TEXT_COLOR_RGB,
    i18n_key::WORKSPACE_WARN_COLOR_RGB,
    i18n_key::WORKSPACE_ERROR_COLOR_RGB,
];
const HISTORY_FIELDS: &[&str] = &[
    i18n_key::WORKSPACE_HISTORY_LINE_LIMIT,
    i18n_key::WORKSPACE_HISTORY_PER_PANE_MIB,
    i18n_key::WORKSPACE_GLOBAL_HISTORY_MIB,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsSection {
    General,
    Appearance,
    Terminal,
    Security,
    Updates,
}
impl SettingsSection {
    const ALL: [Self; 5] = [
        Self::General,
        Self::Appearance,
        Self::Terminal,
        Self::Security,
        Self::Updates,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::General => i18n_key::CONNECTIONS_GENERAL,
            Self::Appearance => i18n_key::CONNECTIONS_APPEARANCE,
            Self::Terminal => i18n_key::CONNECTIONS_TERMINAL,
            Self::Security => i18n_key::CONNECTIONS_SECURITY,
            Self::Updates => i18n_key::UPDATES_TITLE,
        }
    }
}

pub struct LockChange {
    pub current: Secret,
    pub password: Secret,
    pub enabled: bool,
    pub idle_minutes: u32,
}

#[derive(PartialEq, Eq)]
struct SettingsDraft {
    fields: Vec<String>,
    language: Language,
    app_theme: AppTheme,
    line_numbers: bool,
    line_timestamps: bool,
    cache_command_output: bool,
    highlight: bool,
    highlight_selection_matches: bool,
    confirm_multiline_paste: bool,
    restore_workspace: bool,
    auto_reconnect: bool,
    status_bar_monitoring: bool,
    check_updates: bool,
}

struct LockFields {
    configured: bool,
    initial_idle: u32,
    enabled: bool,
    automatic: bool,
    password_pending: bool,
    password_editing: bool,
    current: Entity<TextInput>,
    password: Entity<TextInput>,
    confirm: Entity<TextInput>,
    idle: Entity<TextInput>,
}

pub struct SettingsForm {
    fields: Fields,
    values: Settings,
    initial: SettingsDraft,
    section: SettingsSection,
    section_focus: [FocusHandle; 5],
    scroll_handles: [ScrollHandle; 5],
    updates: Entity<Updates>,
    _subscriptions: Vec<Subscription>,
    lock: LockFields,
}
impl SettingsForm {
    fn section_count() -> usize {
        SettingsSection::ALL.len()
    }
    pub fn new(
        settings: &Settings,
        lock: &AppLockConfig,
        updates: Entity<Updates>,
        cx: &mut Context<Self>,
    ) -> Self {
        let fields = Fields::new(
            vec![
                (i18n_key::UPDATES_PROXY, settings.update_proxy.clone()),
                (
                    i18n_key::WORKSPACE_MONOSPACE_FONT,
                    settings.font_family.clone(),
                ),
                (
                    i18n_key::WORKSPACE_FONT_SIZE,
                    settings.font_size.to_string(),
                ),
                (
                    i18n_key::WORKSPACE_LINE_HEIGHT_MULTIPLIER,
                    settings.line_height.to_string(),
                ),
                (
                    i18n_key::WORKSPACE_HISTORY_LINE_LIMIT,
                    settings.history_lines.to_string(),
                ),
                (
                    i18n_key::WORKSPACE_HISTORY_PER_PANE_MIB,
                    format_mib(settings.history_bytes),
                ),
                (
                    i18n_key::WORKSPACE_GLOBAL_HISTORY_MIB,
                    format_mib(settings.global_history_bytes),
                ),
                (
                    i18n_key::WORKSPACE_BACKGROUND_COLOR_RGB,
                    format!("{:06x}", settings.theme.background),
                ),
                (
                    i18n_key::WORKSPACE_TEXT_COLOR_RGB,
                    format!("{:06x}", settings.theme.foreground),
                ),
                (
                    i18n_key::WORKSPACE_WARN_COLOR_RGB,
                    format!("{:06x}", settings.theme.warn),
                ),
                (
                    i18n_key::WORKSPACE_ERROR_COLOR_RGB,
                    format!("{:06x}", settings.theme.error),
                ),
            ],
            cx,
        );
        let initial = SettingsDraft {
            fields: fields.values(cx),
            language: settings.language,
            app_theme: settings.app_theme,
            line_numbers: settings.line_numbers,
            line_timestamps: settings.line_timestamps,
            cache_command_output: settings.cache_command_output,
            highlight: settings.highlight,
            highlight_selection_matches: settings.highlight_selection_matches,
            confirm_multiline_paste: settings.confirm_multiline_paste,
            restore_workspace: settings.restore_workspace,
            auto_reconnect: settings.auto_reconnect,
            status_bar_monitoring: settings.status_bar_monitoring,
            check_updates: settings.check_updates,
        };
        let configured = lock.password_hash.is_some();
        let mut form = Self {
            fields,
            updates,
            values: settings.clone(),
            initial,
            section: SettingsSection::General,
            section_focus: std::array::from_fn(|_| cx.focus_handle()),
            scroll_handles: std::array::from_fn(|_| ScrollHandle::new()),
            _subscriptions: Vec::new(),
            lock: LockFields {
                configured,
                initial_idle: lock.idle_minutes,
                enabled: configured,
                automatic: !configured || lock.idle_minutes > 0,
                password_pending: false,
                password_editing: false,
                current: cx
                    .new(|cx| TextInput::new(i18n_key::SETTINGS_CURRENT_LOCK_PASSWORD, true, cx)),
                password: cx
                    .new(|cx| TextInput::new(i18n_key::SETTINGS_NEW_LOCK_PASSWORD, true, cx)),
                confirm: cx
                    .new(|cx| TextInput::new(i18n_key::SETTINGS_CONFIRM_LOCK_PASSWORD, true, cx)),
                idle: cx.new(|cx| {
                    let mut input = TextInput::new(i18n_key::CONNECTIONS_IDLE_MINUTES, false, cx);
                    input.set_value(default_idle_minutes(lock.idle_minutes).to_string(), cx);
                    input
                }),
            },
        };
        let mut inputs = form.fields.inputs();
        inputs.extend([
            form.lock.current.clone(),
            form.lock.password.clone(),
            form.lock.confirm.clone(),
            form.lock.idle.clone(),
        ]);
        form._subscriptions = inputs
            .iter()
            .map(|input| {
                cx.subscribe(input, |this, _, event, cx| {
                    if matches!(event, InputEvent::Changed) {
                        this.refresh_lock_inputs(cx);
                        cx.notify();
                    }
                })
            })
            .collect();
        form._subscriptions
            .push(cx.observe(&form.updates, |_, _, cx| cx.notify()));
        form
    }

    pub fn is_dirty(&self, cx: &App) -> bool {
        self.initial
            != SettingsDraft {
                fields: self.fields.values(cx),
                language: self.values.language,
                app_theme: self.values.app_theme,
                line_numbers: self.values.line_numbers,
                line_timestamps: self.values.line_timestamps,
                cache_command_output: self.values.cache_command_output,
                highlight: self.values.highlight,
                highlight_selection_matches: self.values.highlight_selection_matches,
                confirm_multiline_paste: self.values.confirm_multiline_paste,
                restore_workspace: self.values.restore_workspace,
                auto_reconnect: self.values.auto_reconnect,
                status_bar_monitoring: self.values.status_bar_monitoring,
                check_updates: self.values.check_updates,
            }
            || self.lock_is_dirty(cx)
    }

    pub fn inputs(&self, cx: &App) -> Vec<Entity<TextInput>> {
        match self.section {
            SettingsSection::General => Vec::new(),
            SettingsSection::Appearance => {
                let mut inputs = self.fields.selected_inputs(FONT_FIELDS);
                inputs.extend(self.fields.selected_inputs(COLOR_FIELDS));
                inputs
            }
            SettingsSection::Terminal => self.fields.selected_inputs(HISTORY_FIELDS),
            SettingsSection::Security => self.security_inputs(cx),
            SettingsSection::Updates => self.fields.selected_inputs(&[i18n_key::UPDATES_PROXY]),
        }
    }

    pub fn focus(&self, window: &mut Window, cx: &App) {
        if let Some(input) = self.inputs(cx).first() {
            input.read(cx).focus(window);
        } else {
            self.section_focus[self.section as usize].focus(window);
        }
    }

    pub fn select_section(
        &mut self,
        section: SettingsSection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self
            .inputs(cx)
            .iter()
            .any(|input| input.read(cx).is_composing())
        {
            return;
        }
        self.section = section;
        self.section_focus[section as usize].focus(window);
        cx.notify();
    }

    pub fn scroll_handle(&self) -> ScrollHandle {
        self.scroll_handles[self.section as usize].clone()
    }

    pub fn validation_section(&self, cx: &App) -> Option<SettingsSection> {
        self.parsed_settings(cx)
            .err()
            .map(|(section, _)| section)
            .or_else(|| {
                self.lock_change(cx)
                    .err()
                    .map(|_| SettingsSection::Security)
            })
    }

    pub fn settings(&self, cx: &App) -> anyhow::Result<Settings> {
        self.parsed_settings(cx).map_err(|(_, error)| error)
    }

    fn parsed_settings(&self, cx: &App) -> Result<Settings, (SettingsSection, anyhow::Error)> {
        let get = |key| self.fields.get(key, cx);
        let mut settings = self.values.clone();
        let appearance = || -> anyhow::Result<Settings> {
            let mut settings = settings.clone();
            settings.font_family = get(i18n_key::WORKSPACE_MONOSPACE_FONT);
            settings.font_size = parse_value(
                i18n_key::WORKSPACE_FONT_SIZE,
                &get(i18n_key::WORKSPACE_FONT_SIZE),
            )?;
            settings.line_height = parse_value(
                i18n_key::WORKSPACE_LINE_HEIGHT_MULTIPLIER,
                &get(i18n_key::WORKSPACE_LINE_HEIGHT_MULTIPLIER),
            )?;
            let color = |label| -> anyhow::Result<u32> {
                let value = get(label);
                let value = value.trim().strip_prefix('#').unwrap_or(value.trim());
                if value.len() != 6 {
                    anyhow::bail!(rustterm::msg!(
                        i18n_key::CONNECTIONS_ENTER_SIX_HEXADECIMAL_DIGITS_FOR_FIELD,
                        field = Message::new(label)
                    ));
                }
                u32::from_str_radix(value, 16).map_err(|_| {
                    anyhow::anyhow!(rustterm::msg!(
                        i18n_key::CONNECTIONS_ENTER_SIX_HEXADECIMAL_DIGITS_FOR_FIELD,
                        field = Message::new(label)
                    ))
                })
            };
            settings.theme.background = color(i18n_key::WORKSPACE_BACKGROUND_COLOR_RGB)?;
            settings.theme.foreground = color(i18n_key::WORKSPACE_TEXT_COLOR_RGB)?;
            settings.theme.warn = color(i18n_key::WORKSPACE_WARN_COLOR_RGB)?;
            settings.theme.error = color(i18n_key::WORKSPACE_ERROR_COLOR_RGB)?;
            settings.validate()?;
            Ok(settings)
        };
        settings = appearance().map_err(|error| (SettingsSection::Appearance, error))?;
        let history = || -> anyhow::Result<Settings> {
            settings.history_lines = parse_value(
                i18n_key::WORKSPACE_HISTORY_LINE_LIMIT,
                &get(i18n_key::WORKSPACE_HISTORY_LINE_LIMIT),
            )?;
            settings.history_bytes = parse_mib(
                i18n_key::WORKSPACE_HISTORY_PER_PANE_MIB,
                &get(i18n_key::WORKSPACE_HISTORY_PER_PANE_MIB),
            )?;
            settings.global_history_bytes = parse_mib(
                i18n_key::WORKSPACE_GLOBAL_HISTORY_MIB,
                &get(i18n_key::WORKSPACE_GLOBAL_HISTORY_MIB),
            )?;
            settings.validate()?;
            Ok(settings)
        };
        let mut settings = history().map_err(|error| (SettingsSection::Terminal, error))?;
        settings.update_proxy = get(i18n_key::UPDATES_PROXY).trim().to_owned();
        rustterm::update::validate_proxy(&settings.update_proxy)
            .map_err(|error| (SettingsSection::Updates, error))?;
        Ok(settings)
    }
}

impl Render for SettingsForm {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let compact = crate::app::window_frame::WindowFrame::new(window)
            .content_size(window.viewport_size())
            .width
            < px(720.);
        let body = match self.section {
            SettingsSection::General => self.general(cx),
            SettingsSection::Appearance => self.appearance(cx),
            SettingsSection::Terminal => self.terminal(cx),
            SettingsSection::Security => self.security(cx),
            SettingsSection::Updates => self.updates_page(cx),
        };
        let navigation = div()
            .flex()
            .flex_shrink_0()
            .gap_1()
            .p_2()
            .bg(rgb(ui::colors().bg))
            .when(compact, |nav| {
                nav.w_full()
                    .border_b_1()
                    .border_color(rgb(ui::colors().border))
            })
            .when(!compact, |nav| {
                nav.w(px(156.))
                    .flex_col()
                    .border_r_1()
                    .border_color(rgb(ui::colors().border))
            })
            .children(SettingsSection::ALL.into_iter().map(|section| {
                let selected = section == self.section;
                let icon = match section {
                    SettingsSection::General => Icon::Settings,
                    SettingsSection::Appearance => Icon::Eye,
                    SettingsSection::Terminal => Icon::Terminal,
                    SettingsSection::Security => Icon::Shield,
                    SettingsSection::Updates => Icon::Refresh,
                };
                div()
                    .id(("settings-section", section as usize))
                    .track_focus(&self.section_focus[section as usize])
                    .tab_index(0)
                    .tab_stop(selected)
                    .flex()
                    .items_center()
                    .gap_2()
                    .min_w_0()
                    .h(px(38.))
                    .px_2()
                    .when(compact, |item| item.flex_1().justify_center())
                    .rounded(px(4.))
                    .border_1()
                    .border_color(gpui::transparent_black())
                    .cursor_pointer()
                    .bg(rgb(if selected {
                        ui::colors().selected
                    } else {
                        ui::colors().bg
                    }))
                    .text_color(rgb(if selected {
                        ui::colors().text
                    } else {
                        ui::colors().muted
                    }))
                    .hover(|style| style.bg(rgb(ui::colors().hover)))
                    .focus(|style| style.border_color(rgb(ui::colors().accent)))
                    .child(ui::icon(icon).text_color(rgb(if selected {
                        ui::colors().accent
                    } else {
                        ui::colors().muted
                    })))
                    .child(div().truncate().child(tr(section.label())))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.select_section(section, window, cx)
                    }))
                    .on_key_down(cx.listener(
                        move |this, event: &gpui::KeyDownEvent, window, cx| {
                            if event.keystroke.modifiers.modified() {
                                return;
                            }
                            let index = match event.keystroke.key.as_str() {
                                "left" | "up" => {
                                    (section as usize + Self::section_count() - 1)
                                        % Self::section_count()
                                }
                                "right" | "down" => (section as usize + 1) % Self::section_count(),
                                "home" => 0,
                                "end" => Self::section_count() - 1,
                                _ => return,
                            };
                            this.select_section(SettingsSection::ALL[index], window, cx);
                            window.prevent_default();
                            cx.stop_propagation();
                        },
                    ))
            }));
        let scroll = self.scroll_handle();
        let content = div()
            .id(("settings-content", self.section as usize))
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_y_scroll()
            .map(ui::preserve_scroll_axes)
            .track_scroll(&scroll)
            .px(px(24.))
            .pb(px(24.))
            .child(body);
        div()
            .size_full()
            .min_w_0()
            .min_h_0()
            .flex()
            .when(compact, |layout| layout.flex_col())
            .text_size(px(12.))
            .text_color(rgb(ui::colors().text))
            .child(navigation)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .flex_shrink_0()
                            .px(px(24.))
                            .pt(px(20.))
                            .pb(px(8.))
                            .text_size(px(18.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(tr(self.section.label())),
                    )
                    .child(ui::scrollable(content, Some(scroll))),
            )
    }
}

fn default_idle_minutes(minutes: u32) -> u32 {
    if minutes == 0 { 10 } else { minutes }
}

fn section_heading(label: &'static str) -> Div {
    div()
        .pt(px(20.))
        .pb(px(4.))
        .text_size(px(11.))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(rgb(ui::colors().muted))
        .child(tr(label))
}

fn setting_row(
    label: &'static str,
    description: Option<&'static str>,
    control: impl IntoElement,
) -> Div {
    ui::setting_row(tr(label), description.map(tr), control)
}

fn toggle(
    id: &'static str,
    label: &'static str,
    description: Option<&'static str>,
    selected: bool,
) -> gpui::Stateful<Div> {
    setting_row(
        label,
        description,
        div()
            .w(px(34.))
            .h(px(20.))
            .flex()
            .items_center()
            .px(px(3.))
            .rounded(px(10.))
            .bg(rgb(if selected {
                ui::colors().accent
            } else {
                ui::colors().border
            }))
            .when(selected, |rail| rail.justify_end())
            .child(div().size(px(14.)).rounded(px(7.)).bg(rgb(if selected {
                ui::colors().on_accent
            } else {
                ui::colors().text
            }))),
    )
    .id(id)
    .cursor_pointer()
    .tab_index(0)
    .hover(|style| style.bg(rgb(ui::colors().hover)))
    .focus(|style| style.border_color(rgb(ui::colors().accent)))
}
// Valid history budgets fit exactly in f64 (maximum 64 GiB). Dividing by a
// power of two preserves every byte, unlike the old integer-MiB display.
fn format_mib(bytes: usize) -> String {
    (bytes as f64 / 1_048_576.).to_string()
}
fn parse_mib(label: &'static str, text: &str) -> anyhow::Result<usize> {
    let bytes = parse_value::<f64>(label, text)? * 1_048_576.;
    if !bytes.is_finite() || !(0. ..=68_719_476_736.).contains(&bytes) || bytes.fract() != 0. {
        anyhow::bail!(rustterm::msg!(
            i18n_key::CONNECTIONS_INVALID_VALUE_FOR_FIELD,
            field = Message::new(label)
        ));
    }
    usize::try_from(bytes as u64).map_err(|_| {
        anyhow::anyhow!(Message::new(
            i18n_key::WORKSPACE_HISTORY_BUDGET_IS_TOO_LARGE
        ))
    })
}

pub struct RestartForUpdate;
impl gpui::EventEmitter<RestartForUpdate> for SettingsForm {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_budget_preserves_sub_mib_values() {
        for bytes in [1024, 1_048_577, 67_108_865, 68_719_476_736] {
            assert_eq!(
                parse_mib(i18n_key::WORKSPACE_GLOBAL_HISTORY_MIB, &format_mib(bytes)).unwrap(),
                bytes
            );
        }
        assert!(parse_mib(i18n_key::WORKSPACE_GLOBAL_HISTORY_MIB, "NaN").is_err());
        assert!(parse_mib(i18n_key::WORKSPACE_GLOBAL_HISTORY_MIB, "-1").is_err());
        assert!(parse_mib(i18n_key::WORKSPACE_GLOBAL_HISTORY_MIB, "65537").is_err());
    }
}
