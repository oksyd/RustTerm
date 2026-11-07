use super::*;
use rustterm::i18n::messages as i18n_key;

impl SettingsForm {
    pub(super) fn general(&self, cx: &Context<Self>) -> Div {
        let language = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_1()
            .p_1()
            .rounded(px(4.))
            .bg(rgb(ui::colors().input))
            .children(Language::available().map(|language| {
                let selected = self.values.language == language;
                div()
                    .id(language.code())
                    .px_2()
                    .py_1()
                    .rounded(px(3.))
                    .cursor_pointer()
                    .tab_index(0)
                    .whitespace_nowrap()
                    .border_1()
                    .border_color(rgb(if selected {
                        ui::colors().accent
                    } else {
                        ui::colors().input
                    }))
                    .bg(rgb(if selected {
                        ui::colors().selected
                    } else {
                        ui::colors().input
                    }))
                    .text_color(rgb(if selected {
                        ui::colors().accent
                    } else {
                        ui::colors().muted
                    }))
                    .hover(|style| style.bg(rgb(ui::colors().hover)))
                    .focus(|style| style.border_color(rgb(ui::colors().text)))
                    .child(language.native_name())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.values.language = language;
                        cx.notify();
                    }))
            }));
        column()
            .child(setting_row(i18n_key::COMMON_LANGUAGE, None, language))
            .child(section_heading(
                i18n_key::CONNECTIONS_STARTUP_AND_CONNECTIONS,
            ))
            .child(
                toggle(
                    "restore-workspace",
                    i18n_key::WORKSPACE_RESTORE_TABS_AND_SPLIT_LAYOUT_ON_STARTUP,
                    None,
                    self.values.restore_workspace,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.values.restore_workspace = !this.values.restore_workspace;
                    cx.notify();
                })),
            )
            .child(
                toggle(
                    "auto-reconnect",
                    i18n_key::WORKSPACE_RECONNECT_AFTER_NETWORK_INTERRUPTIONS,
                    None,
                    self.values.auto_reconnect,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.values.auto_reconnect = !this.values.auto_reconnect;
                    cx.notify();
                })),
            )
            .child(
                toggle(
                    "status-bar-monitoring",
                    i18n_key::RESOURCES_STATUS_MONITORING,
                    Some(i18n_key::RESOURCES_STATUS_MONITORING_HELP),
                    self.values.status_bar_monitoring,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.values.status_bar_monitoring = !this.values.status_bar_monitoring;
                    cx.notify();
                })),
            )
    }

    pub(super) fn appearance(&self, cx: &Context<Self>) -> Div {
        let mut body = column()
            .child(section_heading(i18n_key::SETTINGS_APPLICATION_THEME))
            .child(hint(i18n_key::SETTINGS_APPLICATION_THEME_HELP))
            .child(self.theme_picker(cx))
            .child(section_heading(i18n_key::CONNECTIONS_TERMINAL_FONT))
            .child(setting_row(
                i18n_key::WORKSPACE_MONOSPACE_FONT,
                None,
                div().w(px(220.)).child(
                    self.fields
                        .field(i18n_key::WORKSPACE_MONOSPACE_FONT)
                        .clone(),
                ),
            ))
            .child(setting_row(
                i18n_key::WORKSPACE_FONT_SIZE,
                None,
                self.number_control(i18n_key::WORKSPACE_FONT_SIZE, "px"),
            ))
            .child(setting_row(
                i18n_key::SETTINGS_LINE_HEIGHT,
                None,
                self.number_control(i18n_key::WORKSPACE_LINE_HEIGHT_MULTIPLIER, "×"),
            ))
            .child(section_heading(i18n_key::SETTINGS_TERMINAL_PREVIEW))
            .child(self.terminal_preview(cx))
            .child(section_heading(i18n_key::CONNECTIONS_TERMINAL_COLORS));
        for (field, label, saved) in [
            (
                i18n_key::WORKSPACE_BACKGROUND_COLOR_RGB,
                i18n_key::SETTINGS_BACKGROUND,
                self.values.theme.background,
            ),
            (
                i18n_key::WORKSPACE_TEXT_COLOR_RGB,
                i18n_key::REDIS_TEXT,
                self.values.theme.foreground,
            ),
            (
                i18n_key::WORKSPACE_WARN_COLOR_RGB,
                i18n_key::SETTINGS_WARNINGS,
                self.values.theme.warn,
            ),
            (
                i18n_key::WORKSPACE_ERROR_COLOR_RGB,
                i18n_key::SETTINGS_ERRORS,
                self.values.theme.error,
            ),
        ] {
            let color = parse_color(&self.fields.get(field, cx)).unwrap_or(saved);
            let control = div()
                .w(px(150.))
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .size(px(24.))
                        .flex_shrink_0()
                        .rounded(px(3.))
                        .border_1()
                        .border_color(rgb(ui::colors().border))
                        .bg(rgb(color)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(self.fields.field(field).clone()),
                );
            body = body.child(setting_row(label, None, control));
        }
        body
    }

    fn theme_picker(&self, cx: &Context<Self>) -> Div {
        div()
            .flex()
            .flex_wrap()
            .gap_2()
            .children(AppTheme::ALL.into_iter().enumerate().map(|(index, theme)| {
                let palette = ui::Colors::for_theme(theme);
                let selected = self.values.app_theme == theme;
                div()
                    .id(("application-theme", index))
                    .w(px(136.))
                    .p_2()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .rounded(px(5.))
                    .border_1()
                    .border_color(rgb(if selected {
                        ui::colors().accent
                    } else {
                        ui::colors().border
                    }))
                    .bg(rgb(if selected {
                        ui::colors().selected
                    } else {
                        ui::colors().surface
                    }))
                    .cursor_pointer()
                    .tab_index(0)
                    .hover(|s| s.border_color(rgb(ui::colors().accent)))
                    .focus(|s| s.border_color(rgb(ui::colors().text)))
                    .child(
                        div()
                            .h(px(64.))
                            .w_full()
                            .rounded(px(3.))
                            .overflow_hidden()
                            .bg(rgb(palette.bg))
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .h(px(13.))
                                    .bg(rgb(palette.chrome))
                                    .border_b_1()
                                    .border_color(rgb(palette.border)),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .flex()
                                    .child(
                                        div()
                                            .w(px(26.))
                                            .bg(rgb(palette.surface))
                                            .border_r_1()
                                            .border_color(rgb(palette.border)),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .p_2()
                                            .flex()
                                            .flex_col()
                                            .gap_1()
                                            .child(
                                                div().h(px(4.)).w(px(24.)).bg(rgb(palette.accent)),
                                            )
                                            .child(div().h(px(3.)).w(px(42.)).bg(rgb(palette.text)))
                                            .child(
                                                div().h(px(3.)).w(px(30.)).bg(rgb(palette.muted)),
                                            ),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(tr(theme.label()))
                            .when(selected, |d| {
                                d.child(ui::icon(Icon::Check).text_color(rgb(ui::colors().accent)))
                            }),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.values.app_theme = theme;
                        ui::set_theme(theme, cx);
                        cx.notify();
                    }))
            }))
    }

    pub(super) fn terminal(&self, cx: &Context<Self>) -> Div {
        column()
            .child(section_heading(i18n_key::CONNECTIONS_OUTPUT_DISPLAY))
            .child(
                toggle(
                    "line-toggle",
                    i18n_key::CONNECTIONS_SHOW_SEPARATE_LINE_NUMBERS,
                    None,
                    self.values.line_numbers,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.values.line_numbers = !this.values.line_numbers;
                    cx.notify();
                })),
            )
            .child(
                toggle(
                    "timestamp-toggle",
                    i18n_key::SETTINGS_SHOW_OUTPUT_TIMESTAMPS,
                    Some(i18n_key::SETTINGS_OUTPUT_TIMESTAMPS_HELP),
                    self.values.line_timestamps,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.values.line_timestamps = !this.values.line_timestamps;
                    cx.notify();
                })),
            )
            .child(
                toggle(
                    "highlight-toggle",
                    i18n_key::WORKSPACE_TERMINAL_HIGHLIGHTING,
                    None,
                    self.values.highlight,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.values.highlight = !this.values.highlight;
                    cx.notify();
                })),
            )
            .child(section_heading(i18n_key::CONNECTIONS_SCROLLBACK_HISTORY))
            .child(
                toggle(
                    "command-output-cache-toggle",
                    i18n_key::SETTINGS_CACHE_COMMAND_OUTPUT,
                    Some(i18n_key::SETTINGS_CACHE_COMMAND_OUTPUT_HELP),
                    self.values.cache_command_output,
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.values.cache_command_output = !this.values.cache_command_output;
                    cx.notify();
                })),
            )
            .child(setting_row(
                i18n_key::WORKSPACE_HISTORY_LINE_LIMIT,
                None,
                self.number_control(
                    i18n_key::WORKSPACE_HISTORY_LINE_LIMIT,
                    i18n_key::SETTINGS_LINES,
                ),
            ))
            .child(setting_row(
                i18n_key::SETTINGS_HISTORY_PER_PANE,
                None,
                self.number_control(i18n_key::WORKSPACE_HISTORY_PER_PANE_MIB, "MiB"),
            ))
            .child(setting_row(
                i18n_key::SETTINGS_GLOBAL_HISTORY,
                Some(i18n_key::CONNECTIONS_THE_GLOBAL_HISTORY_BUDGET_MUST_BE_AT_LEAST_THE_PER),
                self.number_control(i18n_key::WORKSPACE_GLOBAL_HISTORY_MIB, "MiB"),
            ))
    }

    fn number_control(&self, field: &'static str, unit: &'static str) -> Div {
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(div().w(px(96.)).child(self.fields.field(field).clone()))
            .child(
                div()
                    .w(px(32.))
                    .text_size(px(11.))
                    .text_color(rgb(ui::colors().muted))
                    .child(tr(unit)),
            )
    }

    fn terminal_preview(&self, cx: &App) -> Div {
        let font = self.fields.get(i18n_key::WORKSPACE_MONOSPACE_FONT, cx);
        let font =
            if font.trim().is_empty() || font.len() > 256 || font.chars().any(char::is_control) {
                self.values.font_family.clone()
            } else {
                font
            };
        // Partial edits remain in the inputs. Only valid display values reach GPUI.
        let font_size = self
            .fields
            .get(i18n_key::WORKSPACE_FONT_SIZE, cx)
            .trim()
            .parse::<f32>()
            .ok()
            .filter(|size| (8. ..=40.).contains(size))
            .unwrap_or(self.values.font_size);
        let line_height = self
            .fields
            .get(i18n_key::WORKSPACE_LINE_HEIGHT_MULTIPLIER, cx)
            .trim()
            .parse::<f32>()
            .ok()
            .filter(|height| (1. ..=3.).contains(height))
            .unwrap_or(self.values.line_height);
        let color = |field, saved| parse_color(&self.fields.get(field, cx)).unwrap_or(saved);
        div()
            .min_w_0()
            .p_3()
            .rounded(px(4.))
            .border_1()
            .border_color(rgb(ui::colors().border))
            .bg(rgb(color(
                i18n_key::WORKSPACE_BACKGROUND_COLOR_RGB,
                self.values.theme.background,
            )))
            .font_family(font)
            .text_size(px(font_size))
            .line_height(px(font_size * line_height))
            .text_color(rgb(color(
                i18n_key::WORKSPACE_TEXT_COLOR_RGB,
                self.values.theme.foreground,
            )))
            .overflow_hidden()
            .child(
                div()
                    .whitespace_nowrap()
                    .child("rustterm@ubuntu:~$ cargo run"),
            )
            .child(
                div()
                    .whitespace_nowrap()
                    .text_color(rgb(color(
                        i18n_key::WORKSPACE_WARN_COLOR_RGB,
                        self.values.theme.warn,
                    )))
                    .child("WARN  connection is slow"),
            )
            .child(
                div()
                    .whitespace_nowrap()
                    .text_color(rgb(color(
                        i18n_key::WORKSPACE_ERROR_COLOR_RGB,
                        self.values.theme.error,
                    )))
                    .child("ERROR connection refused"),
            )
    }
}

fn parse_color(value: &str) -> Option<u32> {
    let value = value.trim();
    let value = value.strip_prefix('#').unwrap_or(value);
    (value.len() == 6)
        .then(|| u32::from_str_radix(value, 16).ok())
        .flatten()
}
