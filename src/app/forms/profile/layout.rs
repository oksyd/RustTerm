//! Presentation state stays separate from the draft that is saved and tested.
use super::super::{column, row};
use super::{ProfileForm, ProfileKind, SshAuthentication, ssh::SSH_ADVANCED};
use crate::app::{
    text_input::TextInput,
    ui::{self, Icon},
};
use gpui::{AnyElement, App, Context, Entity, Window, div, prelude::*, px, rgb};
use rustterm::{
    config::{ElasticsearchAuth, RedisTopology},
    i18n::{messages as key, tr},
};

impl ProfileForm {
    pub fn inputs(&self, cx: &App) -> Vec<Entity<TextInput>> {
        if self.kind == ProfileKind::Rdp {
            return self.rdp_inputs(cx);
        }
        let mut keys = match self.kind {
            ProfileKind::Local => vec![key::WORKSPACE_SHELL, key::COMMON_WORKING_DIRECTORY],
            ProfileKind::Sqlite => vec![key::DATABASE_DATABASE_FILE],
            ProfileKind::Elasticsearch => {
                let mut keys = vec![key::ES_ENDPOINT];
                if self.es_auth == ElasticsearchAuth::Basic {
                    keys.push(key::WORKSPACE_USERNAME);
                }
                keys
            }
            _ => vec![key::WORKSPACE_HOST, key::WORKSPACE_PORT],
        };
        match self.kind {
            ProfileKind::Ssh => {
                keys.push(key::WORKSPACE_USERNAME);
                match self.ssh_auth {
                    SshAuthentication::PrivateKey => keys.push(key::CONNECTIONS_PRIVATE_KEY_PATH),
                    SshAuthentication::Agent => {
                        keys.push(key::CONNECTIONS_SSH_AGENT_SOCKET_OPTIONAL)
                    }
                    _ => {}
                }
            }
            ProfileKind::Mysql | ProfileKind::Postgres | ProfileKind::ClickHouse => {
                keys.extend([key::DATABASE_DATABASE_USERNAME, key::DATABASE_DATABASE_NAME])
            }
            ProfileKind::Redis => {
                if self.redis_topology != RedisTopology::Standalone {
                    keys.push(key::REDIS_ADDITIONAL_REDIS_NODES);
                }
                if self.redis_topology == RedisTopology::Sentinel {
                    keys.extend([
                        key::REDIS_SENTINEL_SERVICE_NAME,
                        key::REDIS_SENTINEL_USERNAME,
                    ]);
                }
                keys.push(key::REDIS_ACL_USERNAME_OPTIONAL);
                if self.redis_topology != RedisTopology::Cluster {
                    keys.push(key::REDIS_DATABASE_INDEX);
                }
            }
            _ => {}
        }
        let mut inputs = self.fields.selected_inputs(&keys);
        if self.credential_input_visible(cx) {
            inputs.push(self.credentials.input.clone());
        }
        if self.advanced {
            if self.kind == ProfileKind::Local {
                inputs.extend(self.arguments.iter().map(|row| row.value.clone()));
                for row in &self.environment {
                    inputs.extend([row.key.clone(), row.value.clone()]);
                }
            } else {
                inputs.extend(self.fields.selected_inputs(&self.advanced_fields()));
            }
        }
        inputs.push(self.fields.field(key::COMMON_NAME).clone());
        if self.advanced
            && let Some(editor) = &self.forward_editor
        {
            inputs.extend(editor.read(cx).inputs());
        }
        inputs
    }

    pub fn focus(&self, window: &mut Window, cx: &App) {
        if let Some(input) = self.inputs(cx).first() {
            input.read(cx).focus(window);
        }
    }

    pub(super) fn advanced_fields(&self) -> Vec<&'static str> {
        match self.kind {
            ProfileKind::Ssh => SSH_ADVANCED.to_vec(),
            ProfileKind::Sqlite => vec![key::DATABASE_QUERY_TIMEOUT_SECONDS],
            ProfileKind::Elasticsearch => vec![
                key::ES_CA_FILE,
                key::WORKSPACE_CONNECTION_TIMEOUT_SECONDS,
                key::DATABASE_QUERY_TIMEOUT_SECONDS,
            ],
            ProfileKind::Mysql | ProfileKind::Postgres | ProfileKind::ClickHouse => vec![
                key::WORKSPACE_CONNECTION_TIMEOUT_SECONDS,
                key::DATABASE_QUERY_TIMEOUT_SECONDS,
            ],
            ProfileKind::Redis | ProfileKind::Telnet => {
                vec![key::WORKSPACE_CONNECTION_TIMEOUT_SECONDS]
            }
            _ => Vec::new(),
        }
    }

    pub(super) fn host_port(&self) -> AnyElement {
        row(
            key::WORKSPACE_HOST,
            div()
                .flex()
                .items_start()
                .gap_3()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(self.fields.field(key::WORKSPACE_HOST).clone()),
                )
                .child(
                    div()
                        .h(px(ui::metrics::INPUT))
                        .flex()
                        .items_center()
                        .text_color(rgb(ui::colors().muted))
                        .child(tr(key::WORKSPACE_PORT)),
                )
                .child(
                    div()
                        .w(px(80.))
                        .flex_shrink_0()
                        .child(self.fields.field(key::WORKSPACE_PORT).clone()),
                ),
        )
        .into_any_element()
    }

    pub(super) fn advanced_toggle(
        &self,
        label: &'static str,
        summary: String,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .id("profile-advanced")
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_2()
            .mt_2()
            .border_1()
            .border_color(rgb(ui::colors().border))
            .rounded(px(4.))
            .cursor_pointer()
            .tab_index(0)
            .focus(|style| style.border_color(rgb(ui::colors().accent)))
            .hover(|style| style.bg(rgb(ui::colors().hover)))
            .child(ui::icon(if self.advanced {
                Icon::ChevronDown
            } else {
                Icon::ChevronRight
            }))
            .child(div().flex_shrink_0().child(tr(label)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(11.))
                    .text_color(rgb(ui::colors().muted))
                    .child(summary),
            )
            .on_click(cx.listener(|this, _, window, cx| {
                if this.native_pending || this.prompts.is_blocked() {
                    return;
                }
                // Do not leave an input method attached to a hidden field.
                this.focus(window, cx);
                this.advanced = !this.advanced;
                this.tunnel_open = false;
                cx.notify();
            }))
            .into_any_element()
    }

    pub(super) fn network_options(&self, cx: &mut Context<Self>) -> AnyElement {
        let database = matches!(
            self.kind,
            ProfileKind::Mysql
                | ProfileKind::Postgres
                | ProfileKind::ClickHouse
                | ProfileKind::Redis
                | ProfileKind::Elasticsearch
        );
        let mut summary = Vec::new();
        if database {
            summary.push(match self.ssh_profile {
                None => tr(key::REDIS_DIRECT_CONNECTION).to_owned(),
                Some(id) => self
                    .ssh_profiles
                    .iter()
                    .find(|p| p.id == id)
                    .map(|p| format!("SSH: {}", p.name))
                    .unwrap_or_else(|| tr(key::REDIS_UNAVAILABLE_SSH_PROFILE).to_owned()),
            });
        }
        if self.kind == ProfileKind::Elasticsearch
            && !self.fields.get(key::ES_CA_FILE, cx).is_empty()
        {
            summary.push(tr(key::CONNECTIONS_CUSTOM_CA).to_owned());
        }
        let timeout = if self.kind == ProfileKind::Sqlite {
            key::DATABASE_QUERY_TIMEOUT_SECONDS
        } else {
            key::WORKSPACE_CONNECTION_TIMEOUT_SECONDS
        };
        summary.push(rustterm::trf!(
            if self.kind == ProfileKind::Sqlite {
                key::CONNECTIONS_QUERY_TIMEOUT_SUMMARY
            } else {
                key::CONNECTIONS_TIMEOUT_SUMMARY
            },
            seconds = self.fields.get(timeout, cx)
        ));
        if matches!(
            self.kind,
            ProfileKind::Mysql
                | ProfileKind::Postgres
                | ProfileKind::ClickHouse
                | ProfileKind::Elasticsearch
        ) {
            summary.push(rustterm::trf!(
                key::CONNECTIONS_QUERY_TIMEOUT_SUMMARY,
                seconds = self.fields.get(key::DATABASE_QUERY_TIMEOUT_SECONDS, cx)
            ));
        }
        column()
            .child(self.advanced_toggle(
                if database {
                    key::CONNECTIONS_NETWORK_ADVANCED
                } else {
                    key::CONNECTIONS_ADVANCED_TAB
                },
                summary.join(" · "),
                cx,
            ))
            .when(self.advanced, |body| {
                body.child(
                    column()
                        .pl_3()
                        .when(database, |body| body.child(self.tunnel_picker(cx)))
                        .child(self.fields.selected_rows(&self.advanced_fields())),
                )
            })
            .into_any_element()
    }

    pub(super) fn profile_details(&self, cx: &mut Context<Self>) -> AnyElement {
        column()
            .mt_2()
            .pt_3()
            .border_t_1()
            .border_color(rgb(ui::colors().border))
            .child(row(
                key::CONNECTIONS_DISPLAY_NAME_OPTIONAL,
                self.fields.field(key::COMMON_NAME).clone(),
            ))
            .when(!self.groups.is_empty(), |body| {
                body.child(self.group_picker(cx))
            })
            .into_any_element()
    }

    fn suggested_name(&self, cx: &App) -> String {
        let field = match self.kind {
            ProfileKind::Local => key::WORKSPACE_SHELL,
            ProfileKind::Sqlite => key::DATABASE_DATABASE_FILE,
            ProfileKind::Elasticsearch => key::ES_ENDPOINT,
            _ => key::WORKSPACE_HOST,
        };
        suggested_name(self.kind, &self.fields.get(field, cx))
    }

    pub(super) fn profile_name(&self, cx: &App) -> String {
        let name = self.fields.get(key::COMMON_NAME, cx);
        if name.trim().is_empty() {
            self.suggested_name(cx)
        } else {
            name
        }
    }

    pub(super) fn refresh_name_suggestion(&mut self, cx: &mut Context<Self>) {
        let suggestion = self.suggested_name(cx);
        if suggestion != self.name_suggestion {
            self.name_suggestion = suggestion.clone();
            self.fields
                .field(key::COMMON_NAME)
                .update(cx, |input, cx| input.set_placeholder(suggestion, cx));
        }
    }
}

fn suggested_name(kind: ProfileKind, value: &str) -> String {
    let value = value.trim();
    let suggestion = match kind {
        ProfileKind::Elasticsearch => url::Url::parse(value)
            .ok()
            .and_then(|url| {
                url.host_str().map(|host| match url.port() {
                    Some(port) => format!("{host}:{port}"),
                    None => host.to_owned(),
                })
            })
            .unwrap_or_default(),
        ProfileKind::Local | ProfileKind::Sqlite => value
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or_default()
            .to_owned(),
        _ => value.to_owned(),
    };
    let mut suggestion = suggestion
        .chars()
        .filter(|ch| !ch.is_control())
        .collect::<String>();
    let mut end = suggestion.len().min(128);
    while !suggestion.is_char_boundary(end) {
        end -= 1;
    }
    suggestion.truncate(end);
    if suggestion.is_empty() {
        tr(key::COMMON_NEW_CONNECTION).to_owned()
    } else {
        suggestion
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_names_are_bounded_and_exclude_url_credentials_and_paths() {
        assert_eq!(
            suggested_name(ProfileKind::Ssh, " server.example "),
            "server.example"
        );
        assert_eq!(
            suggested_name(
                ProfileKind::Elasticsearch,
                "https://user:secret@search.example:9200/private?q=secret"
            ),
            "search.example:9200"
        );
        assert_eq!(
            suggested_name(ProfileKind::Sqlite, r"C:\data\catalog.sqlite"),
            "catalog.sqlite"
        );
        assert_eq!(suggested_name(ProfileKind::Local, "/bin/bash"), "bash");
        let long = suggested_name(ProfileKind::Ssh, &"界".repeat(80));
        assert!(long.len() <= 128);
        assert!(!long.is_empty());
    }
}
