use super::super::{column, field_error, hint, parse_value, row};
use super::ProfileForm;
use crate::app::ui::{self, ButtonStyle};
use gpui::{AnyElement, App, Context, div, prelude::*};
use rustterm::config::{Connection, Profile, RedisConfig, Uuid};
use rustterm::i18n::Message;
use rustterm::i18n::messages as i18n_key;

pub(super) const REDIS_FIELDS: &[&str] = &[
    i18n_key::WORKSPACE_HOST,
    i18n_key::WORKSPACE_PORT,
    i18n_key::REDIS_ACL_USERNAME_OPTIONAL,
    i18n_key::REDIS_DATABASE_INDEX,
    i18n_key::WORKSPACE_CONNECTION_TIMEOUT_SECONDS,
];

impl ProfileForm {
    pub(super) fn redis_config(
        &self,
        current_profiles: &[Profile],
        cx: &App,
    ) -> anyhow::Result<RedisConfig> {
        let get = |key| self.fields.get(key, cx);
        let username = get(i18n_key::REDIS_ACL_USERNAME_OPTIONAL);
        let mut config = RedisConfig {
            host: get(i18n_key::WORKSPACE_HOST),
            port: parse_value(i18n_key::WORKSPACE_PORT, &get(i18n_key::WORKSPACE_PORT))?,
            username: (!username.is_empty()).then_some(username),
            credential: None,
            topology: self.redis_topology,
            nodes: get(i18n_key::REDIS_ADDITIONAL_REDIS_NODES)
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::parse)
                .collect::<rustterm::config::Result<Vec<_>>>()
                .map_err(|error| {
                    field_error(
                        i18n_key::REDIS_ADDITIONAL_REDIS_NODES,
                        Message::from_error(error),
                    )
                })?,
            sentinel_service: (!get(i18n_key::REDIS_SENTINEL_SERVICE_NAME).is_empty())
                .then(|| get(i18n_key::REDIS_SENTINEL_SERVICE_NAME)),
            sentinel_username: (!get(i18n_key::REDIS_SENTINEL_USERNAME).is_empty())
                .then(|| get(i18n_key::REDIS_SENTINEL_USERNAME)),
            database: parse_value(
                i18n_key::REDIS_DATABASE_INDEX,
                &get(i18n_key::REDIS_DATABASE_INDEX),
            )?,
            tls: self.database_tls,
            ssh_profile: self.ssh_profile,
            timeout_secs: parse_value(
                i18n_key::WORKSPACE_CONNECTION_TIMEOUT_SECONDS,
                &get(i18n_key::WORKSPACE_CONNECTION_TIMEOUT_SECONDS),
            )?,
        };
        if self.ssh_profile.is_some_and(|id| {
            !current_profiles
                .iter()
                .any(|profile| profile.id == id && matches!(profile.connection, Connection::Ssh(_)))
        }) {
            anyhow::bail!(Message::new(
                i18n_key::REDIS_THE_SELECTED_SSH_PROFILE_IS_NO_LONGER_AVAILABLE_CHOOSE_ANOTHER
            ));
        }
        preserve_redis_credential_reference(
            &mut config,
            self.original.as_ref().map(|profile| profile.id),
            current_profiles,
        );
        config.validate()?;
        Ok(config)
    }

    pub(super) fn redis_body(&self, cx: &mut Context<Self>) -> AnyElement {
        use rustterm::config::RedisTopology;
        column()
            .child(row(i18n_key::REDIS_DEPLOYMENT, div().flex().gap_2().children([
                (RedisTopology::Standalone, i18n_key::REDIS_STANDALONE), (RedisTopology::Cluster, i18n_key::REDIS_CLUSTER), (RedisTopology::Sentinel, i18n_key::REDIS_SENTINEL)
            ].into_iter().map(|(topology, label)| ui::text_button(label, label, if self.redis_topology == topology { ButtonStyle::Primary } else { ButtonStyle::Neutral })
                .tab_index(0).on_click(cx.listener(move |this, _, _, cx| { if this.native_pending || this.prompts.is_blocked() {return;} this.redis_topology = topology; if topology == RedisTopology::Cluster { this.fields.field(i18n_key::REDIS_DATABASE_INDEX).update(cx, |input, cx| input.set_value("0", cx)); } cx.notify(); }))))))
            .child(self.tunnel_picker(cx))
            .child(self.fields.selected_rows(REDIS_FIELDS))
            .when(self.redis_topology != RedisTopology::Standalone, |body| body.child(self.fields.selected_rows(&[i18n_key::REDIS_ADDITIONAL_REDIS_NODES])).child(hint(i18n_key::REDIS_ADDITIONAL_SEED_NODES_HOST_PORT_SEPARATED_BY_COMMAS)))
            .when(self.redis_topology == RedisTopology::Sentinel, |body| body.child(self.fields.selected_rows(&[i18n_key::REDIS_SENTINEL_SERVICE_NAME, i18n_key::REDIS_SENTINEL_USERNAME])).child(hint(i18n_key::REDIS_HOST_AND_PORT_IDENTIFY_SENTINEL_SENTINEL_CREDENTIALS_ARE_ENTERED_SEPARATELY)))
            .child(self.tls_picker(cx))
            .when(self.database_tls, |body| body.child(hint(i18n_key::REDIS_TLS_VERIFIES_THE_SERVER_CERTIFICATE_AND_HOSTNAME)))
            .child(hint(i18n_key::REDIS_LEAVE_THE_ACL_USERNAME_BLANK_TO_USE_THE_DEFAULT_REDIS))
            .child(self.credential_editor(cx))
            .into_any_element()
    }
}

fn preserve_redis_credential_reference(
    current: &mut RedisConfig,
    profile_id: Option<Uuid>,
    profiles: &[Profile],
) {
    if let Some(Profile {
        connection: Connection::Redis(previous),
        ..
    }) = profile_id.and_then(|id| profiles.iter().find(|profile| profile.id == id))
        && previous.host == current.host
        && previous.port == current.port
        && previous.username == current.username
        && previous.tls == current.tls
        && previous.ssh_profile == current.ssh_profile
        && previous.topology == current.topology
        && previous.nodes == current.nodes
        && previous.sentinel_service == current.sentinel_service
    {
        // Logical database and timeouts do not change the login identity. TLS and
        // SSH routing do: never send an old credential through a changed route.
        current.credential = previous.credential;
    }
}

#[cfg(test)]
mod tests {
    use super::preserve_redis_credential_reference;
    use rustterm::config::{Connection, CredentialRef, Profile, RedisConfig, Uuid};

    #[test]
    fn redis_credentials_follow_the_latest_login_identity_and_tls_mode() {
        let id = Uuid::from_u128(21);
        let reference = Some(CredentialRef(Uuid::from_u128(22)));
        let previous = RedisConfig {
            host: "cache.example.invalid".into(),
            username: Some("reader".into()),
            credential: reference,
            tls: true,
            ..RedisConfig::default()
        };
        let profiles = [Profile {
            id,
            name: "Cache".into(),
            group: None,
            connection: Connection::Redis(previous.clone()),
        }];
        let mut draft = RedisConfig {
            credential: None,
            database: 2,
            timeout_secs: 42,
            ..previous.clone()
        };
        preserve_redis_credential_reference(&mut draft, Some(id), &profiles);
        assert_eq!(draft.credential, reference);

        for field in 0..5 {
            let mut changed = RedisConfig {
                credential: None,
                ..previous.clone()
            };
            match field {
                0 => changed.host = "other.example.invalid".into(),
                1 => changed.port = 6380,
                2 => changed.username = None,
                3 => changed.tls = false,
                _ => changed.ssh_profile = Some(Uuid::from_u128(23)),
            }
            preserve_redis_credential_reference(&mut changed, Some(id), &profiles);
            assert_eq!(changed.credential, None);
        }
        draft.credential = None;
        preserve_redis_credential_reference(&mut draft, None, &profiles);
        assert_eq!(draft.credential, None);
        preserve_redis_credential_reference(&mut draft, Some(id), &[]);
        assert_eq!(draft.credential, None);
        let cleared = [Profile {
            connection: Connection::Redis(RedisConfig {
                credential: None,
                ..previous
            }),
            ..profiles[0].clone()
        }];
        preserve_redis_credential_reference(&mut draft, Some(id), &cleared);
        assert_eq!(draft.credential, None);
    }
}
