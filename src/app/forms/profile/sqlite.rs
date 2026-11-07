use super::super::{column, hint, parse_value, row};
use super::{ProfileForm, ProfileKind, preserved_path};
use crate::app::ui::{self, ButtonStyle};
use futures_util::future::Either;
use gpui::{AnyElement, App, Context, PathPromptOptions, div, prelude::*, rgb};
use rustterm::i18n::Message;
use rustterm::i18n::messages as i18n_key;
use rustterm::{
    config::{Connection, SqliteConfig},
    i18n::tr,
};
use std::path::PathBuf;

impl ProfileForm {
    pub(super) fn sqlite_config(&self, cx: &App) -> anyhow::Result<SqliteConfig> {
        let original = self.original.as_ref().and_then(|profile| {
            if let Connection::Sqlite(config) = &profile.connection {
                Some(&config.path)
            } else {
                None
            }
        });
        let config = SqliteConfig {
            path: preserved_path(
                &self.fields.get(i18n_key::DATABASE_DATABASE_FILE, cx),
                original,
            ),
            read_only: self.sqlite_read_only,
            query_timeout_secs: parse_value(
                i18n_key::DATABASE_QUERY_TIMEOUT_SECONDS,
                &self
                    .fields
                    .get(i18n_key::DATABASE_QUERY_TIMEOUT_SECONDS, cx),
            )?,
        };
        config.validate()?;
        Ok(config)
    }

    fn choose_sqlite(&mut self, create: bool, cx: &mut Context<Self>) {
        if self.kind != ProfileKind::Sqlite || self.native_pending {
            return;
        }
        let Some(permit) = self.prompts.acquire() else {
            return;
        };
        let previous = self.draft(cx);
        let response = if create {
            let current = PathBuf::from(self.fields.get(i18n_key::DATABASE_DATABASE_FILE, cx));
            let directory = current
                .parent()
                .filter(|path| path.is_absolute())
                .map(ToOwned::to_owned)
                .or_else(|| {
                    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
                        .map(PathBuf::from)
                })
                .unwrap_or_else(|| PathBuf::from("/"));
            let response = cx.prompt_for_new_path(&directory, Some("database.sqlite3"));
            Either::Left(async move { response.await.map_err(|_| ())?.map_err(|_| ()) })
        } else {
            let response = cx.prompt_for_paths(PathPromptOptions {
                files: true,
                directories: false,
                multiple: false,
                prompt: Some(tr(i18n_key::DATABASE_OPEN_DATABASE).into()),
            });
            Either::Right(async move {
                response
                    .await
                    .map_err(|_| ())?
                    .map(|paths| paths.and_then(|paths| paths.into_iter().next()))
                    .map_err(|_| ())
            })
        };
        self.native_pending = true;
        self.sqlite_error = None;
        self.sqlite_notice = None;
        cx.spawn(async move |this, cx| {
            let response = response.await;
            let request = this.update(cx, |this, cx| {
                this.native_pending = false;
                cx.notify();
                if !permit.is_valid() {
                    return None;
                }
                let path = match response {
                    Ok(Some(path)) => path,
                    Ok(None) => return None,
                    Err(()) => {
                        this.sqlite_error = Some(
                            Message::new(if create {
                                i18n_key::TERMINAL_COULD_NOT_OPEN_THE_SAVE_DIALOG_PLEASE_TRY_AGAIN
                            } else {
                                i18n_key::COMMON_COULD_NOT_OPEN_THE_FILE_PICKER_PLEASE_TRY_AGAIN
                            }),
                        );
                        return None;
                    }
                };
                if this.draft(cx) != previous {
                    this.sqlite_error = Some(Message::new(i18n_key::DATABASE_THE_CONNECTION_SETTINGS_CHANGED_WHILE_THE_PICKER_WAS_OPEN_SELECT));
                    return None;
                }
                let Some(text) = path
                    .to_str()
                    .filter(|text| !text.chars().any(char::is_control))
                else {
                    this.sqlite_error = Some(Message::new(i18n_key::CONNECTIONS_THE_SELECTED_PATH_CANNOT_BE_ENTERED_AS_TEXT_CHOOSE_A));
                    return None;
                };
                if !create {
                    this.fields
                        .field(i18n_key::DATABASE_DATABASE_FILE)
                        .update(cx, |input, cx| input.set_value(text, cx));
                    return None;
                }
                let config = parse_value(
                    i18n_key::DATABASE_QUERY_TIMEOUT_SECONDS,
                    &this.fields.get(i18n_key::DATABASE_QUERY_TIMEOUT_SECONDS, cx),
                )
                .and_then(|query_timeout_secs| {
                    let config = SqliteConfig {
                        path,
                        read_only: false,
                        query_timeout_secs,
                    };
                    config.validate()?;
                    Ok(config)
                });
                match config {
                    Ok(config) => {
                        this.native_pending = true;
                        this.sqlite_creating = true;
                        Some(config)
                    }
                    Err(error) => {
                        this.sqlite_error = Some(Message::from_error(error));
                        None
                    }
                }
            }).ok().flatten();
            let Some(config) = request else {
                return;
            };
            let path = config.path.clone();
            let (permit, result) = cx
                .background_executor()
                .spawn(async move {
                    let result = if permit.is_valid() {
                        Some(rustterm::database::create_sqlite(config).await)
                    } else {
                        None
                    };
                    (permit, result)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.native_pending = false;
                this.sqlite_creating = false;
                match result {
                    Some(Ok(())) if permit.is_valid() && this.draft(cx) == previous => {
                        this.fields
                            .field(i18n_key::DATABASE_DATABASE_FILE)
                            .update(cx, |input, cx| input.set_value(path.to_string_lossy(), cx));
                        this.sqlite_read_only = false;
                        this.sqlite_notice = Some(Message::new(i18n_key::DATABASE_DATABASE_CREATED_CANCELLING_THIS_CONNECTION_DOES_NOT_DELETE_THE_FILE));
                    }
                    Some(Ok(())) => {
                        // A completed create remains a real file even if locking
                        // invalidated its permission to replace the form draft.
                        this.sqlite_notice = Some(rustterm::msg!(
                            i18n_key::DATABASE_DATABASE_CREATED_AT_PATH_OPEN_IT_TO_USE_THIS_FILE,
                            path = path.display().to_string()
                        ));
                    }
                    Some(Err(error)) if permit.is_valid() => {
                        this.sqlite_error = Some(Message::from_error(error));
                    }
                    _ => {}
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn sqlite_body(&self, cx: &mut Context<Self>) -> AnyElement {
        let files = div().flex().flex_wrap().gap_2().children(
            [
                (
                    "open-sqlite-database",
                    i18n_key::DATABASE_OPEN_DATABASE,
                    false,
                ),
                (
                    "create-sqlite-database",
                    i18n_key::DATABASE_NEW_DATABASE,
                    true,
                ),
            ]
            .map(|(id, label, create)| {
                ui::text_button_enabled(id, label, ButtonStyle::Neutral, !self.native_pending)
                    .on_click(cx.listener(move |this, _, _, cx| this.choose_sqlite(create, cx)))
            }),
        );
        let access = div().flex().flex_wrap().gap_2().children(
            [
                ("sqlite-read-only", i18n_key::COMMON_READ_ONLY_BADGE, true),
                ("sqlite-read-write", i18n_key::DATABASE_READ_WRITE, false),
            ]
            .map(|(id, label, read_only)| {
                ui::text_button_enabled(
                    id,
                    label,
                    if self.sqlite_read_only == read_only {
                        ButtonStyle::Primary
                    } else {
                        ButtonStyle::Neutral
                    },
                    !self.native_pending,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    if this.native_pending || this.prompts.is_blocked() {
                        return;
                    }
                    this.sqlite_read_only = read_only;
                    cx.notify();
                }))
            }),
        );
        column()
            .child(row(
                i18n_key::DATABASE_DATABASE_FILE,
                column()
                    .child(self.fields.field(i18n_key::DATABASE_DATABASE_FILE).clone())
                    .child(files),
            ))
            .child(hint(i18n_key::DATABASE_OPENING_A_DATABASE_REQUIRES_AN_EXISTING_FILE_NEW_DATABASE_CREATES))
            .child(row(i18n_key::DATABASE_ACCESS_MODE, access))
            .child(self.network_options(cx))
            .when(self.native_pending, |body| {
                body.child(hint(if self.sqlite_creating {
                    i18n_key::DATABASE_CREATING_DATABASE
                } else {
                    i18n_key::CONNECTIONS_SELECTING
                }))
            })
            .when_some(self.sqlite_error.clone(), |body, error| {
                body.child(div().text_color(rgb(ui::colors().danger)).child(error.render()))
            })
            .when_some(self.sqlite_notice.clone(), |body, notice| {
                body.child(div().text_color(rgb(ui::colors().muted)).child(notice.render()))
            })
            .into_any_element()
    }
}
