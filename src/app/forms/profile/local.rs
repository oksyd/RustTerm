use super::super::{column, hint, input, row};
use super::{ArgumentRow, EnvironmentRow, ProfileForm, preserve_text, preserved_path};
use crate::app::ui::{self, ButtonStyle, Icon};
use gpui::{AnyElement, App, Context, Window, div, prelude::*, rgb};
use rustterm::config::{Connection, LocalConfig};
use rustterm::i18n::Message;
use rustterm::i18n::messages as i18n_key;
use std::{collections::BTreeMap, path::PathBuf};

const MAX_ARGUMENTS: usize = 256;
const MAX_ENVIRONMENT: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum StartupField {
    Argument(usize),
    EnvironmentKey(usize),
    EnvironmentValue(usize),
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub(super) struct StartupError {
    pub field: StartupField,
    pub message: Message,
}

impl ProfileForm {
    pub(super) fn local_config(&self, cx: &App) -> anyhow::Result<LocalConfig> {
        let get = |key| self.fields.get(key, cx);
        let cwd = get(i18n_key::COMMON_WORKING_DIRECTORY);
        let original_cwd = self.original.as_ref().and_then(|p| match &p.connection {
            Connection::Local(local) => local.cwd.as_ref(),
            _ => None,
        });
        let config = local_config(
            get(i18n_key::WORKSPACE_SHELL),
            self.arguments
                .iter()
                .map(|row| preserve_text(row.value.read(cx).value(), &row.original))
                .collect(),
            (!cwd.is_empty()).then(|| preserved_path(&cwd, original_cwd)),
            self.environment.iter().map(|row| {
                (
                    preserve_text(row.key.read(cx).value(), &row.original_key),
                    preserve_text(row.value.read(cx).value(), &row.original_value),
                )
            }),
        )?;
        Ok(config)
    }

    fn add_argument(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.arguments.len() >= MAX_ARGUMENTS {
            return;
        }
        let value = input(i18n_key::CONNECTIONS_ARGUMENT, "", cx);
        value.read(cx).focus(window);
        self.arguments.push(ArgumentRow {
            id: self.next_row,
            value,
            original: String::new(),
        });
        self.next_row += 1;
        cx.notify();
    }

    fn add_environment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.environment.len() >= MAX_ENVIRONMENT {
            return;
        }
        let key = input(i18n_key::CONNECTIONS_VARIABLE_NAME, "", cx);
        key.read(cx).focus(window);
        self.environment.push(EnvironmentRow {
            id: self.next_row,
            key,
            value: input(i18n_key::CONNECTIONS_VALUE, "", cx),
            original_key: String::new(),
            original_value: String::new(),
        });
        self.next_row += 1;
        cx.notify();
    }

    pub(super) fn local_body(&self, cx: &mut Context<Self>) -> AnyElement {
        let imported_controls = self
            .arguments
            .iter()
            .any(|row| row.original.chars().any(char::is_control))
            || self.environment.iter().any(|row| {
                row.original_key
                    .chars()
                    .chain(row.original_value.chars())
                    .any(char::is_control)
            });
        column()
            .child(self.fields.selected_rows(&[i18n_key::WORKSPACE_SHELL, i18n_key::COMMON_WORKING_DIRECTORY]))
            .child(self.advanced_toggle(i18n_key::CONNECTIONS_STARTUP_OPTIONS,
                rustterm::trf!(i18n_key::CONNECTIONS_STARTUP_SUMMARY, arguments = self.arguments.len(), variables = self.environment.len()), cx))
            .when(self.advanced, |body| body.child(column()
            .child(row(
                i18n_key::CONNECTIONS_ARGUMENTS,
                column()
                    .children(self.arguments.iter().map(|argument| {
                        let id = argument.id;
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(div().flex_1().min_w_0().child(argument.value.clone()))
                            .child(
                                ui::icon_button(
                                    ("remove-argument", id),
                                    Icon::Close,
                                    i18n_key::CONNECTIONS_REMOVE_ARGUMENT,
                                )
                                .tab_index(0)
                                .border_1()
                                .border_color(rgb(ui::colors().border))
                                .focus(|style| style.border_color(rgb(ui::colors().accent)))
                                .on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        let index = this
                                            .arguments
                                            .iter()
                                            .position(|row| row.id == id)
                                            .unwrap_or(0);
                                        this.arguments.retain(|row| row.id != id);
                                        if let Some(row) = this
                                            .arguments
                                            .get(index.min(this.arguments.len().saturating_sub(1)))
                                        {
                                            row.value.read(cx).focus(window);
                                        } else {
                                            this.fields.field(i18n_key::WORKSPACE_SHELL).read(cx).focus(window);
                                        }
                                        cx.notify();
                                    },
                                )),
                            )
                    }))
                    .child(
                        div().child(
                            ui::text_button_enabled(
                                "add-argument",
                                i18n_key::CONNECTIONS_ADD_ARGUMENT,
                                ButtonStyle::Neutral,
                                self.arguments.len() < MAX_ARGUMENTS,
                            )
                            .border_1()
                            .border_color(rgb(ui::colors().border))
                            .focus(|style| style.border_color(rgb(ui::colors().accent)))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.add_argument(window, cx)),
                            ),
                        ),
                    )
                    .child(hint(
                        i18n_key::CONNECTIONS_EACH_ROW_IS_ONE_ARGUMENT_QUOTES_AND_SPACES_ARE_KEPT,
                    )),
            ))
            .child(row(
                i18n_key::CONNECTIONS_ENVIRONMENT_VARIABLES,
                column()
                    .children(self.environment.iter().map(|entry| {
                        let id = entry.id;
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(div().flex_1().min_w_0().child(entry.key.clone()))
                            .child(div().flex_1().min_w_0().child(entry.value.clone()))
                            .child(
                                ui::icon_button(
                                    ("remove-environment", id),
                                    Icon::Close,
                                    i18n_key::CONNECTIONS_REMOVE_ENVIRONMENT_VARIABLE,
                                )
                                .tab_index(0)
                                .border_1()
                                .border_color(rgb(ui::colors().border))
                                .focus(|style| style.border_color(rgb(ui::colors().accent)))
                                .on_click(cx.listener(
                                    move |this, _, window, cx| {
                                        let index = this
                                            .environment
                                            .iter()
                                            .position(|row| row.id == id)
                                            .unwrap_or(0);
                                        this.environment.retain(|row| row.id != id);
                                        if let Some(row) = this.environment.get(
                                            index.min(this.environment.len().saturating_sub(1)),
                                        ) {
                                            row.key.read(cx).focus(window);
                                        } else {
                                            this.fields
                                                .field(i18n_key::COMMON_WORKING_DIRECTORY)
                                                .read(cx)
                                                .focus(window);
                                        }
                                        cx.notify();
                                    },
                                )),
                            )
                    }))
                    .child(
                        div().child(
                            ui::text_button_enabled(
                                "add-environment",
                                i18n_key::CONNECTIONS_ADD_VARIABLE,
                                ButtonStyle::Neutral,
                                self.environment.len() < MAX_ENVIRONMENT,
                            )
                            .border_1()
                            .border_color(rgb(ui::colors().border))
                            .focus(|style| style.border_color(rgb(ui::colors().accent)))
                            .on_click(
                                cx.listener(|this, _, window, cx| this.add_environment(window, cx)),
                            ),
                        ),
                    ),
            ))
            .when(imported_controls, |body| {
                body.child(hint(
                    i18n_key::CONNECTIONS_IMPORTED_CONTROL_CHARACTERS_ARE_PRESERVED_UNLESS_YOU_EDIT_THEIR_ROW,
                ))
            })
            .child(hint(i18n_key::CONNECTIONS_ENVIRONMENT_VARIABLES_AND_STARTUP_ARGUMENTS_ARE_SAVED_AS_PLAIN_CONFIGURATION))
            ))
            .into_any_element()
    }
}

fn local_config(
    shell: String,
    args: Vec<String>,
    cwd: Option<PathBuf>,
    environment: impl IntoIterator<Item = (String, String)>,
) -> anyhow::Result<LocalConfig> {
    for (index, argument) in args.iter().enumerate() {
        if argument.len() > 65536 || argument.contains('\0') {
            return Err(StartupError {
                field: StartupField::Argument(index),
                message: Message::new(
                    i18n_key::CONNECTIONS_A_STARTUP_ARGUMENT_IS_TOO_LONG_OR_CONTAINS_NUL,
                ),
            }
            .into());
        }
    }
    let mut env = BTreeMap::new();
    for (index, (key, value)) in environment.into_iter().enumerate() {
        let invalid_key = key.is_empty() || key.contains(['=', '\0']) || key.len() > 256;
        if invalid_key || value.contains('\0') || value.len() > 65536 {
            return Err(StartupError {
                field: if invalid_key {
                    StartupField::EnvironmentKey(index)
                } else {
                    StartupField::EnvironmentValue(index)
                },
                message: Message::new(
                    i18n_key::CONNECTIONS_AN_ENVIRONMENT_VARIABLE_IS_INVALID_OR_TOO_LONG,
                ),
            }
            .into());
        }
        if env.insert(key.clone(), value).is_some() {
            return Err(StartupError {
                field: StartupField::EnvironmentKey(index),
                message: rustterm::msg!(
                    i18n_key::CONNECTIONS_DUPLICATE_ENVIRONMENT_VARIABLE_NAME,
                    name = key
                ),
            }
            .into());
        }
    }
    let config = LocalConfig {
        shell,
        args,
        cwd,
        env,
    };
    config.validate()?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::{StartupError, StartupField, local_config};
    use rustterm::i18n::messages as i18n_key;

    #[test]
    fn local_rows_keep_empty_arguments_and_reject_duplicate_environment_keys() {
        let args = vec![String::new(), "one two".into(), "\"literal quotes\"".into()];
        let local = local_config(
            "/bin/sh".into(),
            args.clone(),
            None,
            [("EMPTY".into(), String::new())],
        )
        .unwrap();
        assert_eq!(local.args, args);
        assert_eq!(local.env.get("EMPTY"), Some(&String::new()));
        let duplicate = local_config(
            "/bin/sh".into(),
            Vec::new(),
            None,
            [
                ("PATH".into(), "first".into()),
                ("PATH".into(), "second".into()),
            ],
        )
        .unwrap_err();
        assert_eq!(
            duplicate.downcast_ref::<StartupError>().unwrap().field,
            StartupField::EnvironmentKey(1)
        );
        assert_eq!(
            duplicate.to_string(),
            rustterm::trf!(
                i18n_key::CONNECTIONS_DUPLICATE_ENVIRONMENT_VARIABLE_NAME,
                name = "PATH"
            )
        );
        assert!(
            local_config(
                "/bin/sh".into(),
                Vec::new(),
                None,
                [(String::new(), "value".into())]
            )
            .is_err()
        );
    }

    #[test]
    fn startup_validation_identifies_the_row_to_reveal_after_collapsing() {
        let error = local_config(
            "/bin/sh".into(),
            vec!["valid".into(), "bad\0argument".into()],
            None,
            [],
        )
        .unwrap_err();
        assert_eq!(
            error.downcast_ref::<StartupError>().unwrap().field,
            StartupField::Argument(1)
        );
        let error = local_config(
            "/bin/sh".into(),
            vec![],
            None,
            [("OK".into(), "bad\0value".into())],
        )
        .unwrap_err();
        assert_eq!(
            error.downcast_ref::<StartupError>().unwrap().field,
            StartupField::EnvironmentValue(0)
        );
    }
}
