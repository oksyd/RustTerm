use rustterm::i18n::Message;
use rustterm::i18n::messages as i18n_key;
mod credentials;
mod group;
mod local;
mod rdp;
mod redis;
mod sql;
mod sqlite;
mod ssh;
#[cfg(test)]
mod tests;
mod tunnel;
mod validation;

pub use credentials::CredentialChange;

use self::{
    redis::REDIS_FIELDS,
    ssh::{SSH_ADVANCED, SshAuthentication},
};
use super::{Fields, column, hint, input, parse_value};
use crate::app::{interaction::PromptGate, text_input::TextInput, ui};
use gpui::{App, Context, Entity, FocusHandle, ScrollHandle, Window, prelude::*, px, rgb};
use rustterm::config::{
    Connection, Group, Profile, SqlConfig, SqlEngine, SshConfig, TelnetConfig, Uuid,
};
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProfileKind {
    Local,
    Ssh,
    Telnet,
    Redis,
    Mysql,
    Postgres,
    Sqlite,
    Rdp,
}

const TELNET_FIELDS: &[&str] = &[
    i18n_key::WORKSPACE_HOST,
    i18n_key::WORKSPACE_PORT,
    i18n_key::WORKSPACE_CONNECTION_TIMEOUT_SECONDS,
];

struct ArgumentRow {
    id: usize,
    value: Entity<TextInput>,
    original: String,
}
struct EnvironmentRow {
    id: usize,
    key: Entity<TextInput>,
    value: Entity<TextInput>,
    original_key: String,
    original_value: String,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ProfileDraft {
    fields: Vec<String>,
    arguments: Vec<String>,
    environment: Vec<(String, String)>,
    group: Option<Uuid>,
    ssh_auth: SshAuthentication,
    shell_integration: bool,
    database_tls: bool,
    sqlite_read_only: bool,
    redis_topology: rustterm::config::RedisTopology,
    ssh_profile: Option<Uuid>,
    rdp: rdp::RdpFormState,
}

pub struct ProfileForm {
    id: Uuid,
    pub original: Option<Profile>,
    pub kind: ProfileKind,
    fields: Fields,
    groups: Vec<Group>,
    group: Option<Uuid>,
    group_open: bool,
    suppress_group_key_up: bool,
    group_cursor: usize,
    group_focus: FocusHandle,
    group_scroll: ScrollHandle,
    ssh_auth: SshAuthentication,
    shell_integration: bool,
    database_tls: bool,
    sqlite_read_only: bool,
    sqlite_error: Option<Message>,
    sqlite_notice: Option<Message>,
    sqlite_creating: bool,
    redis_topology: rustterm::config::RedisTopology,
    rdp: rdp::RdpFormState,
    ssh_profiles: Vec<Profile>,
    ssh_profile: Option<Uuid>,
    tunnel_open: bool,
    suppress_tunnel_key_up: bool,
    tunnel_cursor: usize,
    tunnel_focus: FocusHandle,
    tunnel_scroll: ScrollHandle,
    advanced: bool,
    arguments: Vec<ArgumentRow>,
    environment: Vec<EnvironmentRow>,
    next_row: usize,
    initial: ProfileDraft,
    prompts: PromptGate,
    native_pending: bool,
    picker_error: Option<&'static str>,
    ssh_import_error: Option<Message>,
    imported_ssh: Option<SshConfig>,
    credentials: credentials::CredentialEditor,
}
impl ProfileForm {
    pub fn new(
        profile: Option<Profile>,
        kind: ProfileKind,
        groups: Vec<Group>,
        profiles: Vec<Profile>,
        prompts: PromptGate,
        cx: &mut Context<Self>,
    ) -> Self {
        let group = profile.as_ref().and_then(|p| p.group);
        let mut values = vec![(
            i18n_key::COMMON_NAME,
            profile.as_ref().map(|p| p.name.clone()).unwrap_or_default(),
        )];
        let mut arguments = Vec::new();
        let mut environment = Vec::new();
        let mut next_row = 0;
        let mut ssh_auth = SshAuthentication::Password;
        let shell_integration = profile
            .as_ref()
            .and_then(|profile| match &profile.connection {
                Connection::Ssh(config) => Some(config.shell_integration),
                _ => None,
            })
            .unwrap_or(true);
        let mut database_tls = false;
        let mut sqlite_read_only = true;
        let mut redis_topology = rustterm::config::RedisTopology::Standalone;
        let mut ssh_profile = None;
        let mut rdp = rdp::RdpFormState::default();
        match kind {
            ProfileKind::Ssh => {
                ssh_auth = ssh::initial_fields(profile.as_ref(), &mut values);
            }
            ProfileKind::Telnet => {
                let settings = profile
                    .as_ref()
                    .and_then(|profile| match &profile.connection {
                        Connection::Telnet(settings) => Some(settings.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                values.extend([
                    (i18n_key::WORKSPACE_HOST, settings.host),
                    (i18n_key::WORKSPACE_PORT, settings.port.to_string()),
                    (
                        i18n_key::WORKSPACE_CONNECTION_TIMEOUT_SECONDS,
                        settings.timeout_secs.to_string(),
                    ),
                ]);
            }
            ProfileKind::Local => {
                let settings = profile
                    .as_ref()
                    .and_then(|p| match &p.connection {
                        Connection::Local(s) => Some(s.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                values.extend([
                    (i18n_key::WORKSPACE_SHELL, settings.shell),
                    (
                        i18n_key::COMMON_WORKING_DIRECTORY,
                        settings
                            .cwd
                            .map(|p| p.display().to_string())
                            .unwrap_or_default(),
                    ),
                ]);
                for value in settings.args {
                    arguments.push(ArgumentRow {
                        id: next_row,
                        value: input(i18n_key::CONNECTIONS_ARGUMENT, &value, cx),
                        original: value,
                    });
                    next_row += 1;
                }
                for (key, value) in settings.env {
                    environment.push(EnvironmentRow {
                        id: next_row,
                        key: input(i18n_key::CONNECTIONS_VARIABLE_NAME, &key, cx),
                        value: input(i18n_key::CONNECTIONS_VALUE, &value, cx),
                        original_key: key,
                        original_value: value,
                    });
                    next_row += 1;
                }
            }
            ProfileKind::Redis => {
                let settings = profile
                    .as_ref()
                    .and_then(|profile| match &profile.connection {
                        Connection::Redis(settings) => Some(settings.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                database_tls = settings.tls;
                redis_topology = settings.topology;
                ssh_profile = settings.ssh_profile;
                values.extend([
                    (i18n_key::WORKSPACE_HOST, settings.host),
                    (
                        i18n_key::REDIS_ADDITIONAL_REDIS_NODES,
                        settings
                            .nodes
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join(", "),
                    ),
                    (
                        i18n_key::REDIS_SENTINEL_SERVICE_NAME,
                        settings.sentinel_service.unwrap_or_default(),
                    ),
                    (
                        i18n_key::REDIS_SENTINEL_USERNAME,
                        settings.sentinel_username.unwrap_or_default(),
                    ),
                    (i18n_key::WORKSPACE_PORT, settings.port.to_string()),
                    (
                        i18n_key::REDIS_ACL_USERNAME_OPTIONAL,
                        settings.username.unwrap_or_default(),
                    ),
                    (
                        i18n_key::REDIS_DATABASE_INDEX,
                        settings.database.to_string(),
                    ),
                    (
                        i18n_key::WORKSPACE_CONNECTION_TIMEOUT_SECONDS,
                        settings.timeout_secs.to_string(),
                    ),
                ]);
            }
            ProfileKind::Mysql | ProfileKind::Postgres => {
                let engine = if kind == ProfileKind::Mysql {
                    SqlEngine::Mysql
                } else {
                    SqlEngine::Postgres
                };
                let settings = profile
                    .as_ref()
                    .and_then(|profile| match (&profile.connection, engine) {
                        (Connection::Mysql(settings), SqlEngine::Mysql)
                        | (Connection::Postgres(settings), SqlEngine::Postgres) => {
                            Some(settings.clone())
                        }
                        _ => None,
                    })
                    .unwrap_or_else(|| {
                        if kind == ProfileKind::Mysql {
                            SqlConfig::mysql()
                        } else {
                            SqlConfig::postgres()
                        }
                    });
                database_tls = settings.tls;
                ssh_profile = settings.ssh_profile;
                values.extend([
                    (i18n_key::WORKSPACE_HOST, settings.host),
                    (i18n_key::WORKSPACE_PORT, settings.port.to_string()),
                    (i18n_key::DATABASE_DATABASE_USERNAME, settings.username),
                    (i18n_key::DATABASE_DATABASE_NAME, settings.database),
                    (
                        i18n_key::WORKSPACE_CONNECTION_TIMEOUT_SECONDS,
                        settings.timeout_secs.to_string(),
                    ),
                    (
                        i18n_key::DATABASE_QUERY_TIMEOUT_SECONDS,
                        settings.query_timeout_secs.to_string(),
                    ),
                ]);
            }
            ProfileKind::Rdp => {
                rdp = rdp::initial_fields(profile.as_ref(), &mut values);
            }
            ProfileKind::Sqlite => {
                let settings = profile
                    .as_ref()
                    .and_then(|profile| match &profile.connection {
                        Connection::Sqlite(settings) => Some(settings.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                sqlite_read_only = settings.read_only;
                values.extend([
                    (
                        i18n_key::DATABASE_DATABASE_FILE,
                        settings.path.display().to_string(),
                    ),
                    (
                        i18n_key::DATABASE_QUERY_TIMEOUT_SECONDS,
                        settings.query_timeout_secs.to_string(),
                    ),
                ]);
            }
        }
        let mut form = Self {
            id: profile
                .as_ref()
                .map_or_else(Uuid::new_v4, |profile| profile.id),
            original: profile,
            kind,
            fields: Fields::new(values, cx),
            groups,
            group,
            group_open: false,
            suppress_group_key_up: false,
            group_cursor: 0,
            group_focus: cx.focus_handle(),
            group_scroll: ScrollHandle::new(),
            ssh_auth,
            shell_integration,
            database_tls,
            sqlite_read_only,
            sqlite_error: None,
            sqlite_notice: None,
            sqlite_creating: false,
            redis_topology,
            rdp,
            ssh_profiles: profiles
                .into_iter()
                .filter(|profile| matches!(profile.connection, Connection::Ssh(_)))
                .collect(),
            ssh_profile,
            tunnel_open: false,
            suppress_tunnel_key_up: false,
            tunnel_cursor: 0,
            tunnel_focus: cx.focus_handle(),
            tunnel_scroll: ScrollHandle::new(),
            advanced: false,
            arguments,
            environment,
            next_row,
            initial: ProfileDraft::default(),
            prompts,
            native_pending: false,
            picker_error: None,
            ssh_import_error: None,
            imported_ssh: None,
            credentials: credentials::CredentialEditor::new(cx),
        };
        form.initial = form.draft(cx);
        form
    }

    fn draft(&self, cx: &App) -> ProfileDraft {
        ProfileDraft {
            fields: self.fields.values(cx),
            arguments: self
                .arguments
                .iter()
                .map(|row| row.value.read(cx).value().to_owned())
                .collect(),
            environment: self
                .environment
                .iter()
                .map(|row| {
                    (
                        row.key.read(cx).value().to_owned(),
                        row.value.read(cx).value().to_owned(),
                    )
                })
                .collect(),
            group: self.group,
            ssh_auth: self.ssh_auth,
            shell_integration: self.shell_integration,
            database_tls: self.database_tls,
            sqlite_read_only: self.sqlite_read_only,
            redis_topology: self.redis_topology,
            ssh_profile: self.ssh_profile,
            rdp: self.rdp.clone(),
        }
    }

    pub fn is_dirty(&self, cx: &App) -> bool {
        self.draft(cx) != self.initial || self.credential_dirty(cx)
    }

    pub fn id(&self) -> Uuid {
        self.id
    }

    pub fn inputs(&self, cx: &App) -> Vec<Entity<TextInput>> {
        let mut inputs = self.fields.selected_inputs(&[i18n_key::COMMON_NAME]);
        match self.kind {
            ProfileKind::Ssh => {
                inputs.extend(self.fields.selected_inputs(&[
                    i18n_key::WORKSPACE_HOST,
                    i18n_key::WORKSPACE_PORT,
                    i18n_key::WORKSPACE_USERNAME,
                ]));
                if self.ssh_auth == SshAuthentication::PrivateKey {
                    inputs.push(
                        self.fields
                            .field(i18n_key::CONNECTIONS_PRIVATE_KEY_PATH)
                            .clone(),
                    );
                }
                if self.ssh_auth == SshAuthentication::Agent {
                    inputs.push(
                        self.fields
                            .field(i18n_key::CONNECTIONS_SSH_AGENT_SOCKET_OPTIONAL)
                            .clone(),
                    );
                }
                if self.credential_input_visible(cx) {
                    inputs.push(self.credentials.input.clone());
                }
                if self.advanced {
                    inputs.extend(self.fields.selected_inputs(SSH_ADVANCED));
                }
            }
            ProfileKind::Local => {
                inputs.push(self.fields.field(i18n_key::WORKSPACE_SHELL).clone());
                inputs.extend(self.arguments.iter().map(|row| row.value.clone()));
                inputs.push(
                    self.fields
                        .field(i18n_key::COMMON_WORKING_DIRECTORY)
                        .clone(),
                );
                for row in &self.environment {
                    inputs.push(row.key.clone());
                    inputs.push(row.value.clone());
                }
            }
            ProfileKind::Redis => {
                inputs.extend(self.fields.selected_inputs(REDIS_FIELDS));
                if self.redis_topology != rustterm::config::RedisTopology::Standalone {
                    inputs.extend(
                        self.fields
                            .selected_inputs(&[i18n_key::REDIS_ADDITIONAL_REDIS_NODES]),
                    );
                }
                if self.redis_topology == rustterm::config::RedisTopology::Sentinel {
                    inputs.extend(self.fields.selected_inputs(&[
                        i18n_key::REDIS_SENTINEL_SERVICE_NAME,
                        i18n_key::REDIS_SENTINEL_USERNAME,
                    ]));
                }
            }
            ProfileKind::Mysql | ProfileKind::Postgres => {
                inputs.extend(self.fields.selected_inputs(sql::FIELDS))
            }
            ProfileKind::Sqlite => inputs.extend(self.fields.selected_inputs(sqlite::FIELDS)),
            ProfileKind::Telnet => inputs.extend(self.fields.selected_inputs(TELNET_FIELDS)),
            ProfileKind::Rdp => inputs.extend(self.rdp_inputs(cx)),
        }
        if !matches!(self.kind, ProfileKind::Ssh | ProfileKind::Rdp)
            && self.credential_input_visible(cx)
        {
            inputs.push(self.credentials.input.clone());
        }
        inputs
    }

    pub fn focus(&self, window: &mut Window, cx: &App) {
        self.fields.focus(window, cx);
    }

    pub fn profile(&self, current_profiles: &[Profile], cx: &App) -> anyhow::Result<Profile> {
        let get = |key| self.fields.get(key, cx);
        let name = get(i18n_key::COMMON_NAME);
        let connection = match self.kind {
            ProfileKind::Ssh => Connection::Ssh(self.ssh_config(current_profiles, cx)?),
            ProfileKind::Telnet => {
                let config = TelnetConfig {
                    host: get(i18n_key::WORKSPACE_HOST),
                    port: parse_value(i18n_key::WORKSPACE_PORT, &get(i18n_key::WORKSPACE_PORT))?,
                    timeout_secs: parse_value(
                        i18n_key::WORKSPACE_CONNECTION_TIMEOUT_SECONDS,
                        &get(i18n_key::WORKSPACE_CONNECTION_TIMEOUT_SECONDS),
                    )?,
                };
                config.validate()?;
                Connection::Telnet(config)
            }
            ProfileKind::Local => Connection::Local(self.local_config(cx)?),
            ProfileKind::Redis => Connection::Redis(self.redis_config(current_profiles, cx)?),
            ProfileKind::Mysql => {
                Connection::Mysql(self.sql_config(SqlEngine::Mysql, current_profiles, cx)?)
            }
            ProfileKind::Postgres => {
                Connection::Postgres(self.sql_config(SqlEngine::Postgres, current_profiles, cx)?)
            }
            ProfileKind::Sqlite => Connection::Sqlite(self.sqlite_config(cx)?),
            ProfileKind::Rdp => Connection::Rdp(self.rdp_config(current_profiles, cx)?),
        };
        Ok(Profile {
            id: self.id,
            name,
            group: self.group,
            connection,
        })
    }
}

impl Render for ProfileForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        column().text_size(px(12.)).text_color(rgb(ui::TEXT))
            .child(self.fields.selected_rows(&[i18n_key::COMMON_NAME]))
            .child(self.group_picker(cx))
            .child(match self.kind {
                ProfileKind::Local => self.local_body(cx),
                ProfileKind::Ssh => self.ssh_body(cx),
                ProfileKind::Telnet => column().child(self.fields.selected_rows(TELNET_FIELDS))
                    .child(hint(i18n_key::CONNECTIONS_TELNET_IS_UNENCRYPTED_ENTER_LOGIN_CREDENTIALS_IN_THE_TERMINAL_WHEN))
                    .into_any_element(),
                ProfileKind::Redis => self.redis_body(cx),
                ProfileKind::Mysql | ProfileKind::Postgres => self.sql_body(cx),
                ProfileKind::Sqlite => self.sqlite_body(cx),
                ProfileKind::Rdp => self.rdp_body(cx),
            })
            .when(self.kind == ProfileKind::Local, |form| form.child(hint(i18n_key::CONNECTIONS_ENVIRONMENT_VARIABLES_AND_STARTUP_ARGUMENTS_ARE_SAVED_AS_PLAIN_CONFIGURATION)))
    }
}

// Match the non-password TextInput's single-line presentation. Keep imported
// bytes if that presentation is unchanged; editing a row replaces its value.
fn display_text(text: &str) -> String {
    text.chars()
        .filter_map(|ch| match ch {
            '\r' | '\n' | '\t' => Some(' '),
            ch if ch.is_control() => None,
            ch => Some(ch),
        })
        .collect()
}

fn preserve_text(text: &str, original: &str) -> String {
    if text == display_text(original) {
        original.to_owned()
    } else {
        text.to_owned()
    }
}

fn preserved_path(text: &str, original: Option<&PathBuf>) -> PathBuf {
    original
        .filter(|path| display_text(&path.display().to_string()) == text)
        .cloned()
        .unwrap_or_else(|| text.into())
}
