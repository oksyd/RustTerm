use super::super::{column, hint, parse_value, row};
use super::ProfileForm;
use crate::app::ui::{self, ButtonStyle};
use gpui::{AnyElement, App, Context, div, prelude::*};
use rustterm::{
    config::{Connection, ElasticsearchAuth, ElasticsearchConfig, Profile, Uuid},
    i18n::messages as msg,
};

pub(super) fn initial_fields(
    profile: Option<&Profile>,
    fields: &mut Vec<(&'static str, String)>,
) -> (ElasticsearchAuth, Option<Uuid>) {
    let config = profile
        .and_then(|profile| match &profile.connection {
            Connection::Elasticsearch(config) => Some(config.clone()),
            _ => None,
        })
        .unwrap_or_default();
    fields.extend([
        (msg::ES_ENDPOINT, config.endpoint),
        (msg::WORKSPACE_USERNAME, config.username),
        (
            msg::ES_CA_FILE,
            config
                .ca_file
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
        ),
        (
            msg::WORKSPACE_CONNECTION_TIMEOUT_SECONDS,
            config.timeout_secs.to_string(),
        ),
        (
            msg::DATABASE_QUERY_TIMEOUT_SECONDS,
            config.query_timeout_secs.to_string(),
        ),
    ]);
    (config.auth, config.ssh_profile)
}

impl ProfileForm {
    pub(super) fn elasticsearch_config(
        &self,
        profiles: &[Profile],
        cx: &App,
    ) -> anyhow::Result<ElasticsearchConfig> {
        let get = |key| self.fields.get(key, cx);
        let mut config = ElasticsearchConfig {
            endpoint: get(msg::ES_ENDPOINT),
            auth: self.es_auth,
            username: if self.es_auth == ElasticsearchAuth::Basic {
                get(msg::WORKSPACE_USERNAME)
            } else {
                String::new()
            },
            ca_file: (!get(msg::ES_CA_FILE).is_empty()).then(|| get(msg::ES_CA_FILE).into()),
            timeout_secs: parse_value(
                msg::WORKSPACE_CONNECTION_TIMEOUT_SECONDS,
                &get(msg::WORKSPACE_CONNECTION_TIMEOUT_SECONDS),
            )?,
            query_timeout_secs: parse_value(
                msg::DATABASE_QUERY_TIMEOUT_SECONDS,
                &get(msg::DATABASE_QUERY_TIMEOUT_SECONDS),
            )?,
            credential: None,
            ssh_profile: self.ssh_profile,
        };
        if config.ssh_profile.is_some_and(|id| {
            !profiles
                .iter()
                .any(|profile| profile.id == id && matches!(profile.connection, Connection::Ssh(_)))
        }) {
            anyhow::bail!(rustterm::i18n::Message::new(
                msg::REDIS_THE_SELECTED_SSH_PROFILE_IS_NO_LONGER_AVAILABLE_CHOOSE_ANOTHER
            ));
        }
        if config.auth != ElasticsearchAuth::None
            && let Some(previous) = self
                .original
                .as_ref()
                .and_then(|original| profiles.iter().find(|profile| profile.id == original.id))
            && let Connection::Elasticsearch(previous) = &previous.connection
            && previous.same_auth_identity(&config)
        {
            config.credential = previous.credential;
        }
        config.validate()?;
        Ok(config)
    }

    pub(super) fn elasticsearch_body(&self, cx: &mut Context<Self>) -> AnyElement {
        let choices = div().flex().flex_wrap().gap_2().children(
            [
                ("es-auth-none", msg::ES_AUTH_NONE, ElasticsearchAuth::None),
                (
                    "es-auth-basic",
                    msg::ES_AUTH_BASIC,
                    ElasticsearchAuth::Basic,
                ),
                (
                    "es-auth-api-key",
                    msg::ES_AUTH_API_KEY,
                    ElasticsearchAuth::ApiKey,
                ),
            ]
            .map(|(id, label, auth)| {
                ui::text_button(
                    id,
                    label,
                    if self.es_auth == auth {
                        ButtonStyle::Primary
                    } else {
                        ButtonStyle::Neutral
                    },
                )
                .tab_index(0)
                .on_click(cx.listener(move |this, _, _, cx| {
                    if this.native_pending || this.prompts.is_blocked() {
                        return;
                    }
                    if this.es_auth != auth {
                        this.credentials.reset(cx);
                        this.es_auth = auth;
                        cx.notify();
                    }
                }))
            }),
        );
        column()
            .child(self.fields.selected_rows(&[msg::ES_ENDPOINT]))
            .child(hint(msg::ES_ENDPOINT_HELP))
            .child(row(msg::ES_AUTH, choices))
            .when(self.es_auth == ElasticsearchAuth::Basic, |body| {
                body.child(self.fields.selected_rows(&[msg::WORKSPACE_USERNAME]))
            })
            .child(self.credential_editor(cx))
            .child(self.network_options(cx))
            .into_any_element()
    }
}
