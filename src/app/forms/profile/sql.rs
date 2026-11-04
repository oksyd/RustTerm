use super::super::{column, hint, parse_value, row};
use super::{ProfileForm, ProfileKind};
use crate::app::ui::{self, Icon};
use gpui::{AnyElement, App, Context, div, prelude::*, px, rgb};
use rustterm::i18n::Message;
use rustterm::i18n::messages as i18n_key;
use rustterm::{
    config::{Connection, Profile, SqlConfig, SqlEngine, Uuid},
    i18n::tr,
};

pub(super) const FIELDS: &[&str] = &[
    i18n_key::WORKSPACE_HOST,
    i18n_key::WORKSPACE_PORT,
    i18n_key::DATABASE_DATABASE_USERNAME,
    i18n_key::DATABASE_DATABASE_NAME,
    i18n_key::WORKSPACE_CONNECTION_TIMEOUT_SECONDS,
    i18n_key::DATABASE_QUERY_TIMEOUT_SECONDS,
];

impl ProfileForm {
    pub(super) fn sql_config(
        &self,
        engine: SqlEngine,
        current_profiles: &[Profile],
        cx: &App,
    ) -> anyhow::Result<SqlConfig> {
        let get = |label| self.fields.get(label, cx);
        let mut config = SqlConfig {
            host: get(i18n_key::WORKSPACE_HOST),
            port: parse_value(i18n_key::WORKSPACE_PORT, &get(i18n_key::WORKSPACE_PORT))?,
            username: get(i18n_key::DATABASE_DATABASE_USERNAME),
            database: get(i18n_key::DATABASE_DATABASE_NAME),
            tls: self.database_tls,
            timeout_secs: parse_value(
                i18n_key::WORKSPACE_CONNECTION_TIMEOUT_SECONDS,
                &get(i18n_key::WORKSPACE_CONNECTION_TIMEOUT_SECONDS),
            )?,
            query_timeout_secs: parse_value(
                i18n_key::DATABASE_QUERY_TIMEOUT_SECONDS,
                &get(i18n_key::DATABASE_QUERY_TIMEOUT_SECONDS),
            )?,
            credential: None,
            ssh_profile: self.ssh_profile,
        };
        if config.ssh_profile.is_some_and(|id| {
            !current_profiles
                .iter()
                .any(|profile| profile.id == id && matches!(profile.connection, Connection::Ssh(_)))
        }) {
            anyhow::bail!(Message::new(
                i18n_key::REDIS_THE_SELECTED_SSH_PROFILE_IS_NO_LONGER_AVAILABLE_CHOOSE_ANOTHER
            ));
        }
        preserve_sql_credential_reference(
            &mut config,
            engine,
            self.original.as_ref().map(|profile| profile.id),
            current_profiles,
        );
        config.validate()?;
        Ok(config)
    }

    pub(super) fn sql_body(&self, cx: &mut Context<Self>) -> AnyElement {
        column()
            .child(self.tunnel_picker(cx))
            .child(self.fields.selected_rows(FIELDS))
            .child(hint(if self.kind == ProfileKind::Mysql {
                i18n_key::DATABASE_LEAVE_THE_DATABASE_NAME_BLANK_TO_CHOOSE_A_DATABASE_AFTER
            } else {
                i18n_key::DATABASE_POSTGRESQL_OPENS_ONE_DATABASE_PER_CONNECTION_LEAVE_BLANK_TO_USE
            }))
            .child(self.tls_picker(cx))
            .when(self.database_tls, |body| {
                body.child(hint(
                    i18n_key::REDIS_TLS_VERIFIES_THE_SERVER_CERTIFICATE_AND_HOSTNAME,
                ))
            })
            .child(self.credential_editor(cx))
            .into_any_element()
    }

    pub(super) fn tls_picker(&self, cx: &mut Context<Self>) -> AnyElement {
        row(
            i18n_key::REDIS_TLS,
            div()
                .id("database-tls")
                .flex()
                .items_center()
                .gap_2()
                .py_1()
                .px_2()
                .cursor_pointer()
                .tab_index(0)
                .border_1()
                .border_color(rgb(ui::BORDER))
                .focus(|style| style.border_color(rgb(ui::ACCENT)))
                .child(
                    div()
                        .size(px(14.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .border_1()
                        .rounded(px(2.))
                        .border_color(rgb(if self.database_tls {
                            ui::ACCENT
                        } else {
                            ui::BORDER
                        }))
                        .when(self.database_tls, |checkbox| {
                            checkbox.child(ui::icon(Icon::Check).text_color(rgb(ui::ACCENT)))
                        }),
                )
                .child(tr(i18n_key::REDIS_USE_TLS))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.database_tls = !this.database_tls;
                    cx.notify();
                })),
        )
        .into_any_element()
    }
}

fn preserve_sql_credential_reference(
    current: &mut SqlConfig,
    engine: SqlEngine,
    profile_id: Option<Uuid>,
    profiles: &[Profile],
) {
    let previous = profile_id
        .and_then(|id| profiles.iter().find(|profile| profile.id == id))
        .and_then(|profile| match (&profile.connection, engine) {
            (Connection::Mysql(config), SqlEngine::Mysql)
            | (Connection::Postgres(config), SqlEngine::Postgres) => Some(config),
            _ => None,
        });
    if let Some(previous) = previous
        && previous.same_auth_identity(current)
    {
        current.credential = previous.credential;
    }
}

#[cfg(test)]
mod tests {
    use super::preserve_sql_credential_reference;
    use rustterm::config::{CredentialRef, Profile, SqlConfig, SqlEngine, Uuid};

    #[test]
    fn sql_credentials_follow_latest_matching_engine_and_identity() {
        let reference = CredentialRef::new();
        let mut saved = SqlConfig::mysql();
        saved.credential = Some(reference);
        let profile = Profile::mysql("Database", saved.clone());
        let profiles = [profile.clone()];
        let mut draft = saved.clone();
        draft.credential = None;
        draft.timeout_secs = 25;
        draft.query_timeout_secs = 90;
        preserve_sql_credential_reference(
            &mut draft,
            SqlEngine::Mysql,
            Some(profile.id),
            &profiles,
        );
        assert_eq!(draft.credential, Some(reference));
        for field in 0..7 {
            let mut draft = saved.clone();
            draft.credential = None;
            let engine = if field == 6 {
                SqlEngine::Postgres
            } else {
                SqlEngine::Mysql
            };
            match field {
                0 => draft.host = "another.example.invalid".into(),
                1 => draft.port = 3307,
                2 => draft.username = "other".into(),
                3 => draft.database = "production".into(),
                4 => draft.tls = true,
                5 => draft.ssh_profile = Some(Uuid::new_v4()),
                _ => {}
            }
            preserve_sql_credential_reference(&mut draft, engine, Some(profile.id), &profiles);
            assert!(draft.credential.is_none());
        }
    }
}
