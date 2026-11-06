use super::{Modal, Tab, Workspace};
use crate::app::layout::{Axis, PaneNode};
use crate::app::pane::Pane;
use crate::app::text_input::TextInput;
use gpui::{Context, Entity, Window};
use rustterm::config::{
    AuthMethod, Connection, CredentialRef, CredentialStore, ProcessCredentialStore, Profile,
    RedisTopology, Secret, SshConfig,
};
use rustterm::i18n::messages as i18n_key;
use rustterm::i18n::{Message, tr};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use rustterm::config::{PanePosition, SavedLayout, SavedTab, SavedWorkspace, WorkspaceSnapshot};
use std::{collections::VecDeque, time::Instant};

#[derive(Default)]
pub(super) struct Recovery {
    restoring: Option<Restore>,
    task: Option<gpui::Task<()>>,
    save_task: Option<gpui::Task<()>>,
    pending_save: bool,
    retry_task: Option<gpui::Task<()>>,
    retries: HashMap<usize, Retry>,
    stopped: HashSet<usize>,
    loaded_credentials: HashSet<gpui::EntityId>,
}
struct Restore {
    snapshot: WorkspaceSnapshot,
    pending: VecDeque<usize>,
    current: Option<usize>,
    restored: HashMap<usize, usize>,
}
struct Retry {
    attempts: u32,
    due: Instant,
    connecting: bool,
    credentials: Option<gpui::Task<()>>,
}

enum ReconnectCredential {
    Empty,
    Stored(CredentialRef),
}

impl ReconnectCredential {
    fn load(self) -> Result<Secret, rustterm::config::credentials::CredentialError> {
        match self {
            Self::Empty => Ok(Secret::new(String::new())),
            Self::Stored(reference) => ProcessCredentialStore::new().get(reference),
        }
    }
}

fn reconnect_credential(connection: &Connection) -> Option<ReconnectCredential> {
    match connection {
        Connection::Ssh(SshConfig {
            auth: AuthMethod::KeyboardInteractive,
            ..
        }) => None,
        Connection::Redis(config) if config.topology == RedisTopology::Sentinel => None,
        _ if connection.credential_ref().is_some() => {
            connection.credential_ref().map(ReconnectCredential::Stored)
        }
        Connection::Ssh(SshConfig {
            auth: AuthMethod::Agent { .. } | AuthMethod::PrivateKey { .. },
            ..
        })
        | Connection::Telnet(_)
        | Connection::Sqlite(_) => Some(ReconnectCredential::Empty),
        Connection::Elasticsearch(config)
            if config.auth == rustterm::config::ElasticsearchAuth::None =>
        {
            Some(ReconnectCredential::Empty)
        }
        Connection::Redis(config) if config.username.is_none() => Some(ReconnectCredential::Empty),
        _ => None,
    }
}

impl Recovery {
    fn stop_reconnect(&mut self, id: usize) {
        self.retries.remove(&id);
        self.stopped.insert(id);
    }

    fn retry_ready(
        &mut self,
        id: usize,
        connected: bool,
        running: bool,
        retryable: bool,
        now: Instant,
    ) -> bool {
        if connected {
            self.retries.remove(&id);
            self.stopped.remove(&id);
            return false;
        }
        if running || self.stopped.contains(&id) {
            return false;
        }
        if !retryable {
            if self.retries.remove(&id).is_some() {
                self.stopped.insert(id);
            }
            return false;
        }
        let retry = self.retries.entry(id).or_insert(Retry {
            attempts: 0,
            due: now + Duration::from_secs(2),
            connecting: false,
            credentials: None,
        });
        if retry.credentials.is_some() {
            return false;
        }
        if retry.connecting {
            retry.connecting = false;
            retry.due = now + retry_delay(retry.attempts);
        }
        if retry.attempts >= 5 {
            self.stop_reconnect(id);
            return false;
        }
        now >= retry.due
    }
}

impl Workspace {
    fn pause_reconnect_for_authentication(
        &mut self,
        id: usize,
        name: &str,
        cx: &mut Context<Self>,
    ) {
        self.recovery.stop_reconnect(id);
        self.error = Some(rustterm::msg!(
            i18n_key::WORKSPACE_AUTOMATIC_RECONNECT_PAUSED_FOR_NAME_RECONNECT_MANUALLY_TO_AUTHENTICATE,
            name = name
        ));
        cx.notify();
    }

    fn reconnect_in_background(
        &mut self,
        profile: Profile,
        id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(credential) = reconnect_credential(&profile.connection) else {
            self.pause_reconnect_for_authentication(id, &profile.name, cx);
            return;
        };
        let tunnel = if let Some(route) = profile.connection.ssh_profile() {
            let Some(tunnel) = self
                .config
                .profiles
                .iter()
                .find(|candidate| {
                    candidate.id == route && matches!(candidate.connection, Connection::Ssh(_))
                })
                .cloned()
            else {
                self.recovery.stop_reconnect(id);
                self.error = Some(i18n_key::DATABASE_THE_SSH_PROFILE_FOR_THIS_DATABASE_CONNECTION_IS_UNAVAILABLE_EDIT.into());
                cx.notify();
                return;
            };
            let Some(credential) = reconnect_credential(&tunnel.connection) else {
                self.pause_reconnect_for_authentication(id, &profile.name, cx);
                return;
            };
            Some((tunnel, credential))
        } else {
            None
        };
        let route = tunnel.as_ref().map(|(profile, _)| profile.clone());
        let load = cx.background_executor().spawn(async move {
            let secret = credential.load()?;
            let tunnel = tunnel
                .map(|(profile, credential)| credential.load().map(|secret| (profile, secret)))
                .transpose()?;
            Ok::<_, rustterm::config::credentials::CredentialError>((secret, tunnel))
        });
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = load.await;
            let _ = this.update_in(cx, |this, window, cx| {
                let Some(retry) = this.recovery.retries.get_mut(&id) else {
                    return;
                };
                retry.credentials = None;
                if this.closing {
                    return;
                }
                if this.window_state.is_locked()
                    || this.modal.is_some()
                    || this.palette.open
                    || this.input_composing(window, cx)
                    || this.authentication.read(cx).is_active()
                    || this.window_state.prompts.is_busy()
                {
                    this.recovery.retries.get_mut(&id).unwrap().due =
                        Instant::now() + Duration::from_secs(2);
                    return;
                }
                let current = this.profile_for_pane(id, cx).filter(|current| {
                    current.id == profile.id && current.connection == profile.connection
                });
                let route_matches = route.as_ref().is_none_or(|route| {
                    this.config.profiles.iter().any(|current| {
                        current.id == route.id && current.connection == route.connection
                    })
                });
                let Some(current) = current.filter(|_| route_matches) else {
                    this.recovery.retries.remove(&id);
                    return;
                };
                match result {
                    Ok((secret, tunnel)) => {
                        let retry = this.recovery.retries.get_mut(&id).unwrap();
                        retry.attempts += 1;
                        retry.connecting = true;
                        this.reconnect_with_credentials(current, id, secret, tunnel, window, cx);
                    }
                    Err(error) => {
                        this.recovery.stop_reconnect(id);
                        this.error = Some(rustterm::msg!(
                            i18n_key::WORKSPACE_COULD_NOT_LOAD_SAVED_CREDENTIALS_FOR_NAME_RECONNECT_MANUALLY_ERROR,
                            name = current.name,
                            error = Message::from_error(error)
                        ));
                    }
                }
                cx.notify();
            });
        });
        if let Some(retry) = self.recovery.retries.get_mut(&id) {
            retry.credentials = Some(task);
        }
    }

    pub(super) fn note_recovery_credential(&mut self, field: gpui::EntityId) {
        self.recovery.loaded_credentials.insert(field);
    }
    pub(super) fn revoke_recovery_credential(&mut self, field: gpui::EntityId) {
        self.recovery.loaded_credentials.remove(&field);
    }
    pub(super) fn clear_recovery_credentials(&mut self) {
        self.recovery.loaded_credentials.clear();
    }
    pub(super) fn reset_reconnect(&mut self, id: usize) {
        self.recovery.retries.remove(&id);
        self.recovery.stopped.remove(&id);
    }
    pub(super) fn try_saved_authentication(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.closing
            || self.lock_screen.locked
            || self.palette.open
            || self.window_state.prompts.is_busy()
            || self.modal_composing(cx)
            || self.authentication.read(cx).is_active()
            || self.auth_credentials_loading()
        {
            return;
        }
        let Some(Modal::Auth {
            profile,
            reconnect,
            secret,
            sentinel_secret,
            tunnel,
            auto_connect,
            ..
        }) = &self.modal
        else {
            return;
        };
        let recovering = self
            .recovery
            .restoring
            .as_ref()
            .is_some_and(|restore| restore.current.is_some())
            || reconnect.is_some_and(|id| {
                self.recovery
                    .retries
                    .get(&id)
                    .is_some_and(|retry| retry.connecting)
            });
        if (!auto_connect && !recovering) || sentinel_secret.is_some() {
            return;
        }
        let ready = |profile: &Profile, input: &Entity<TextInput>| {
            (self
                .recovery
                .loaded_credentials
                .contains(&input.entity_id())
                || super::connections::credential_optional(profile))
                && (!super::connections::needs_password(profile)
                    || !input.read(cx).value().is_empty())
        };
        if ready(profile, secret)
            && tunnel
                .as_ref()
                .is_none_or(|tunnel| ready(&tunnel.profile, &tunnel.secret))
        {
            self.authenticate(window, cx);
        }
    }
    pub(super) fn restore_saved_workspace(
        &mut self,
        requested: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.start_reconnect_worker(window, cx);
        if !requested
            || !self.config.settings.restore_workspace
            || self.config.workspace.layout.tabs.is_empty()
        {
            return false;
        }
        let snapshot = self.config.workspace.clone();
        self.recovery.restoring = Some(Restore {
            pending: (0..snapshot.panes.len()).collect(),
            snapshot,
            current: None,
            restored: HashMap::new(),
        });
        self.recovery.task = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                gpui::Timer::after(Duration::from_millis(80)).await;
                let keep = this
                    .update_in(cx, |this, window, cx| this.advance_restore(window, cx))
                    .unwrap_or(false);
                if !keep {
                    break;
                }
            }
        }));
        true
    }

    fn advance_restore(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.closing {
            self.recovery.restoring = None;
            return false;
        }
        self.try_saved_authentication(window, cx);
        if self.lock_screen.locked
            || self.palette.open
            || self.modal.is_some()
            || self.authentication.read(cx).is_active()
            || self.window_state.prompts.is_busy()
        {
            return true;
        }
        let Some(restore) = self.recovery.restoring.as_mut() else {
            return false;
        };
        // A dismissed authentication form skips that pane. Successfully created
        // panes clear current in place_pane, before the next queue iteration.
        restore.current = None;
        let Some(index) = restore.pending.pop_front() else {
            let saved_active = restore.snapshot.layout.active;
            let saved_focus = restore
                .restored
                .get(&restore.snapshot.layout.focused)
                .copied()
                .or_else(|| {
                    let tab = restore.snapshot.layout.tabs.get(saved_active)?;
                    restore
                        .restored
                        .iter()
                        .filter(|(saved, _)| saved_contains(&tab.layout, **saved))
                        .min_by_key(|(saved, _)| **saved)
                        .map(|(_, pane)| *pane)
                });
            let collapsed = restore
                .snapshot
                .layout
                .collapsed_ssh_tab_groups
                .iter()
                .copied()
                .collect();
            self.recovery.restoring = None;
            if let Some(focused) = saved_focus {
                self.focus_pane(focused, window, cx);
            } else {
                self.set_active(saved_active, window, cx);
            }
            self.collapsed_tab_groups = collapsed;
            self.regroup_tabs();
            self.scroll_active_tab_into_view();
            cx.notify();
            return false;
        };
        let position = restore.snapshot.panes[index].clone();
        let Some(profile) = profile_at_position(&position, &self.config.profiles) else {
            return true;
        };
        restore.current = Some(index);
        self.open_profile(profile, None, window, cx);
        cx.notify();
        true
    }

    pub(super) fn restoring_session_number(
        &self,
        profile: rustterm::config::Uuid,
    ) -> Option<rustterm::config::SshSessionPosition> {
        let restore = self.recovery.restoring.as_ref()?;
        let position = restore.snapshot.panes.get(restore.current?)?;
        (position.profile == profile)
            .then_some(position.ssh_session)
            .flatten()
    }

    pub(super) fn capture_restored_pane(
        &mut self,
        id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(restore) = &mut self.recovery.restoring else {
            return;
        };
        let Some(index) = restore.current.take() else {
            return;
        };
        restore.restored.retain(|_, id| self.panes.contains_key(id));
        restore.restored.insert(index, id);
        let position = restore.snapshot.panes[index].clone();
        if let Some(Pane::Redis(view)) = self.panes.get(&id) {
            view.update(cx, |view, cx| view.restore_position(&position, cx));
        }
        if let Some(Pane::Terminal(view)) = self.panes.get(&id)
            && self.pane_profiles.get(&id).is_some_and(|profile| {
                matches!(
                    profile.connection,
                    Connection::Ssh(_) | Connection::Telnet(_)
                )
            })
            && let Some(directory) = &position.directory
        {
            view.update(cx, |view, cx| {
                view.offer_restore_directory(Some(directory.clone()), cx)
            });
        }
        let existing: HashSet<_> = restore.restored.values().copied().collect();
        self.tabs
            .retain(|tab| !tab.layout.leaves().iter().any(|id| existing.contains(id)));
        let layouts = restore.snapshot.layout.tabs.clone();
        let mapping = restore.restored.clone();
        let mut restored_tabs = Vec::new();
        for tab in layouts {
            if let Some(layout) = rebuild_layout(&tab.layout, &mapping, &mut self.next_id) {
                let tab_id = self.id();
                restored_tabs.push(Tab {
                    id: tab_id,
                    title: tab.title,
                    focused: layout.leaves()[0],
                    layout,
                });
            }
        }
        restored_tabs.append(&mut self.tabs);
        self.tabs = restored_tabs;
        self.focus_pane(id, window, cx);
    }

    pub(super) fn interrupt_restore_for_open(&mut self) {
        if self
            .recovery
            .restoring
            .as_ref()
            .is_some_and(|restore| restore.current.is_none())
        {
            self.recovery.restoring = None;
            self.recovery.task = None;
        }
    }

    pub(super) fn cancel_reconnect_for_modal(&mut self, cx: &mut Context<Self>) {
        if let Some(Modal::Auth {
            reconnect: Some(id),
            ..
        }) = &self.modal
        {
            self.recovery.stop_reconnect(*id);
        }
        if let Some(restore) = &mut self.recovery.restoring {
            // A new pane opened after dismissal must not inherit the skipped slot.
            restore.current = None;
        }
        cx.notify();
    }

    pub(super) fn cancel_workspace_recovery(&mut self, cx: &mut Context<Self>) {
        self.recovery.restoring = None;
        self.recovery.task = None;
        self.recovery.stopped.extend(self.panes.keys().copied());
        self.recovery.retries.clear();
        cx.notify();
    }

    pub(super) fn recovery_status(&self) -> Option<String> {
        if let Some(restore) = &self.recovery.restoring {
            return Some(rustterm::trf!(
                i18n_key::WORKSPACE_RESTORING_WORKSPACE_DONE_TOTAL,
                done = restore.restored.len(),
                total = restore.snapshot.panes.len()
            ));
        }
        (!self.recovery.retries.is_empty()).then(|| {
            rustterm::trf!(
                i18n_key::WORKSPACE_PENDING_RECONNECT_COUNT,
                count = self.recovery.retries.len()
            )
        })
    }

    pub(super) fn flush_pending_workspace(&mut self, cx: &mut Context<Self>) -> bool {
        if !self.recovery.pending_save || self.modal_saving || self.save_failed {
            return false;
        }
        self.recovery.save_task = None;
        self.recovery.pending_save = false;
        self.save(cx);
        true
    }

    pub(super) fn persist_workspace(&mut self, cx: &mut Context<Self>) {
        self.capture_workspace(cx);
        if self.recovery.pending_save {
            self.schedule_workspace_persistence(cx);
        }
    }

    pub(super) fn schedule_workspace_persistence(&mut self, cx: &mut Context<Self>) {
        if self.recovery.save_task.is_some()
            || self.closing
            || self.recovery.restoring.is_some()
            || self.modal_saving
            || self.save_failed
        {
            return;
        }
        // Child views can repaint at video rates. Coalesce snapshot construction
        // as well as disk writes, while capturing the latest state when due.
        self.recovery.save_task = Some(cx.spawn(async move |this, cx| {
            gpui::Timer::after(Duration::from_secs(1)).await;
            let _ = this.update(cx, |this, cx| {
                this.recovery.save_task = None;
                // A failed form save can already have committed on disk. Keep
                // its error and let the user retry before saving another snapshot.
                if this.modal_saving || this.save_failed || this.closing {
                    return;
                }
                this.capture_workspace(cx);
                this.flush_pending_workspace(cx);
            });
        }));
    }

    fn capture_workspace(&mut self, cx: &mut Context<Self>) {
        if self.lock_screen.locked
            || self.closing
            || self.recovery.restoring.is_some()
            || self.modal_saving
            || self.save_failed
        {
            return;
        }
        let mut panes = Vec::new();
        let mut positions = HashMap::new();
        for tab in &self.tabs {
            for id in tab.layout.leaves() {
                let Some(profile) = self.pane_profiles.get(&id) else {
                    continue;
                };
                let known = self
                    .config
                    .profiles
                    .iter()
                    .any(|known| known.id == profile.id);
                if !known
                    && matches!(&profile.connection, Connection::Local(local) if !local.args.is_empty() || !local.env.is_empty())
                {
                    continue;
                }
                let mut position = PanePosition {
                    profile: profile.id,
                    ssh_session: self.session_names.position(id),
                    ..Default::default()
                };
                match self.panes.get(&id) {
                    Some(Pane::Terminal(view)) => {
                        position.directory = view.read(cx).current_directory();
                        if !known && let Connection::Local(local) = &profile.connection {
                            position.local = Some(local.clone());
                        }
                    }
                    Some(Pane::Redis(view)) => {
                        view.read(cx).save_position(&mut position);
                    }
                    Some(Pane::Database(view)) if profile.connection.sql().is_some() => {
                        position.sql_database = Some(view.read(cx).database().to_owned());
                    }
                    _ => {}
                }
                positions.insert(id, panes.len());
                panes.push(position);
            }
        }
        let tabs: Vec<_> = self
            .tabs
            .iter()
            .filter_map(|tab| {
                save_layout(&tab.layout, &positions).map(|layout| SavedTab {
                    title: tab.title.clone(),
                    layout,
                })
            })
            .collect();
        let focused = positions.get(&self.focused).copied().unwrap_or(0);
        let active = tabs
            .iter()
            .position(|tab| saved_contains(&tab.layout, focused))
            .unwrap_or(0);
        let snapshot = WorkspaceSnapshot {
            layout: SavedWorkspace {
                tabs,
                active,
                focused,
                collapsed_ssh_tab_groups: {
                    let mut groups: Vec<_> = self
                        .collapsed_tab_groups
                        .iter()
                        .copied()
                        .filter(|profile| panes.iter().any(|pane| pane.profile == *profile))
                        .collect();
                    groups.sort_unstable();
                    groups
                },
            },
            panes,
        };
        if snapshot != self.config.workspace && snapshot.validate().is_ok() {
            self.config.workspace = snapshot;
            self.recovery.pending_save = true;
        }
    }

    fn start_reconnect_worker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.recovery.retry_task.is_some() {
            return;
        }
        self.recovery.retry_task = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                gpui::Timer::after(Duration::from_secs(1)).await;
                let keep = this
                    .update_in(cx, |this, window, cx| this.reconnect_tick(window, cx))
                    .unwrap_or(false);
                if !keep {
                    break;
                }
            }
        }));
    }

    fn reconnect_tick(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.closing {
            return false;
        }
        // Manual opens also finish here after a lock/native prompt is dismissed,
        // independently of whether automatic reconnect is enabled.
        self.try_saved_authentication(window, cx);
        if !self.config.settings.auto_reconnect {
            if !self.recovery.retries.is_empty() {
                self.recovery.retries.clear();
                cx.notify();
            }
            return true;
        }
        if self.lock_screen.locked
            || self.palette.open
            || self.modal.is_some()
            || self.input_composing(window, cx)
            || self.authentication.read(cx).is_active()
            || self.recovery.restoring.is_some()
            || self.window_state.prompts.is_busy()
            || self
                .recovery
                .retries
                .values()
                .any(|retry| retry.credentials.is_some())
        {
            return true;
        }
        let previous_count = self.recovery.retries.len();
        self.recovery
            .retries
            .retain(|id, _| self.panes.contains_key(id));
        self.recovery
            .stopped
            .retain(|id| self.panes.contains_key(id));
        let now = Instant::now();
        for (&id, pane) in &self.panes {
            let retryable = match pane {
                Pane::Terminal(view) => view.read(cx).may_auto_reconnect(),
                Pane::Redis(view) => view.read(cx).may_auto_reconnect(),
                Pane::Elasticsearch(_) => false,
                Pane::Database(view) => view.read(cx).may_auto_reconnect(),
                Pane::Rdp(view) => view.read(cx).may_auto_reconnect(),
            };
            if !self.recovery.retry_ready(
                id,
                pane.diagnostic_state(cx) == "connected",
                pane.is_running(cx),
                retryable,
                now,
            ) {
                continue;
            }
            let Some(profile) = self.profile_for_pane(id, cx) else {
                self.recovery.stop_reconnect(id);
                continue;
            };
            self.reconnect_in_background(profile, id, window, cx);
            cx.notify();
            break;
        }
        if previous_count != self.recovery.retries.len() {
            cx.notify();
        }

        true
    }
}

fn profile_at_position(position: &PanePosition, profiles: &[Profile]) -> Option<Profile> {
    let mut profile = profiles
        .iter()
        .find(|profile| profile.id == position.profile)
        .cloned()
        .or_else(|| {
            position
                .local
                .as_ref()
                .map(|local| Profile::local(tr(i18n_key::COMMON_LOCAL_TERMINAL), local.clone()))
        })?;
    match &mut profile.connection {
        Connection::Local(local) => {
            if let Some(directory) = &position.directory {
                local.cwd = Some(PathBuf::from(directory));
            }
        }
        Connection::Redis(redis) => {
            redis.database = if redis.topology == rustterm::config::RedisTopology::Cluster {
                0
            } else {
                position
                    .database
                    .filter(|database| *database <= i32::MAX as u32)
                    .unwrap_or(redis.database)
            };
        }
        Connection::Mysql(config) | Connection::Postgres(config) => {
            if let Some(database) = &position.sql_database {
                config.database = database.clone();
            }
        }
        _ => {}
    }
    Some(profile)
}

fn saved_contains(layout: &SavedLayout, pane: usize) -> bool {
    match layout {
        SavedLayout::Pane { index } => *index == pane,
        SavedLayout::Split { first, second, .. } => {
            saved_contains(first, pane) || saved_contains(second, pane)
        }
    }
}

fn retry_delay(attempt: u32) -> Duration {
    Duration::from_secs((2_u64 << attempt.min(4)).min(30))
}
fn save_layout(node: &PaneNode, positions: &HashMap<usize, usize>) -> Option<SavedLayout> {
    match node {
        PaneNode::Leaf(id) => positions
            .get(id)
            .map(|index| SavedLayout::Pane { index: *index }),
        PaneNode::Split {
            axis,
            ratio,
            first,
            second,
            ..
        } => match (
            save_layout(first, positions),
            save_layout(second, positions),
        ) {
            (Some(first), Some(second)) => Some(SavedLayout::Split {
                vertical: matches!(axis, Axis::Vertical),
                ratio: *ratio,
                first: Box::new(first),
                second: Box::new(second),
            }),
            (first, second) => first.or(second),
        },
    }
}
fn rebuild_layout(
    node: &SavedLayout,
    mapping: &HashMap<usize, usize>,
    next_id: &mut usize,
) -> Option<PaneNode> {
    match node {
        SavedLayout::Pane { index } => mapping.get(index).copied().map(PaneNode::Leaf),
        SavedLayout::Split {
            vertical,
            ratio,
            first,
            second,
        } => match (
            rebuild_layout(first, mapping, next_id),
            rebuild_layout(second, mapping, next_id),
        ) {
            (Some(first), Some(second)) => {
                let id = *next_id;
                *next_id += 1;
                Some(PaneNode::Split {
                    id,
                    axis: if *vertical {
                        Axis::Vertical
                    } else {
                        Axis::Horizontal
                    },
                    ratio: *ratio,
                    first: Box::new(first),
                    second: Box::new(second),
                })
            }
            (first, second) => first.or(second),
        },
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use rustterm::config::LocalConfig;

    #[test]
    fn automatic_reconnect_requires_saved_or_noninteractive_credentials() {
        let mut ssh = SshConfig {
            auth: AuthMethod::Password { credential: None },
            ..Default::default()
        };
        assert!(reconnect_credential(&Connection::Ssh(ssh.clone())).is_none());
        let reference = CredentialRef::new();
        ssh.auth = AuthMethod::Password {
            credential: Some(reference),
        };
        assert!(matches!(
            reconnect_credential(&Connection::Ssh(ssh.clone())),
            Some(ReconnectCredential::Stored(saved)) if saved == reference
        ));
        ssh.auth = AuthMethod::Agent { socket: None };
        assert!(matches!(
            reconnect_credential(&Connection::Ssh(ssh.clone())),
            Some(ReconnectCredential::Empty)
        ));
        ssh.auth = AuthMethod::KeyboardInteractive;
        assert!(reconnect_credential(&Connection::Ssh(ssh)).is_none());

        let mut redis = rustterm::config::RedisConfig::default();
        assert!(matches!(
            reconnect_credential(&Connection::Redis(redis.clone())),
            Some(ReconnectCredential::Empty)
        ));
        redis.credential = Some(reference);
        redis.topology = RedisTopology::Sentinel;
        assert!(reconnect_credential(&Connection::Redis(redis)).is_none());
    }

    #[test]
    fn layout_restore_preserves_ratios_and_prunes_missing_profiles() {
        let node = SavedLayout::Split {
            vertical: true,
            ratio: 0.4,
            first: Box::new(SavedLayout::Pane { index: 0 }),
            second: Box::new(SavedLayout::Pane { index: 1 }),
        };
        let mapping = HashMap::from([(0, 10), (1, 20)]);
        let mut next = 30;
        let restored = rebuild_layout(&node, &mapping, &mut next).unwrap();
        assert_eq!(
            save_layout(&restored, &HashMap::from([(10, 0), (20, 1)])),
            Some(node)
        );
        assert!(rebuild_layout(&SavedLayout::Pane { index: 5 }, &mapping, &mut next).is_none());
        assert_eq!(retry_delay(100), Duration::from_secs(30));
    }
    #[test]
    fn retries_stop_after_five_attempts_and_resume_after_a_successful_manual_connection() {
        let mut recovery = Recovery::default();
        let mut now = Instant::now();
        assert!(!recovery.retry_ready(7, false, false, true, now));
        for attempt in 1..=5 {
            now += Duration::from_secs(31);
            assert!(recovery.retry_ready(7, false, false, true, now));
            let retry = recovery.retries.get_mut(&7).unwrap();
            retry.attempts = attempt;
            retry.connecting = true;
            // An in-flight connection must not start another attempt.
            assert!(!recovery.retry_ready(7, false, true, false, now));
            now += Duration::from_secs(1);
            assert!(!recovery.retry_ready(7, false, false, true, now));
        }
        assert!(recovery.retries.is_empty());
        assert!(recovery.stopped.contains(&7));
        assert!(!recovery.retry_ready(7, true, true, false, now));
        assert!(!recovery.stopped.contains(&7));
        assert!(!recovery.retry_ready(7, false, false, true, now));
        assert_eq!(recovery.retries[&7].attempts, 0);
    }

    #[test]
    fn cancelled_and_non_network_attempts_do_not_reopen_authentication() {
        let mut recovery = Recovery::default();
        let now = Instant::now();
        recovery.retry_ready(1, false, false, true, now);
        recovery.stop_reconnect(1);
        assert!(!recovery.retry_ready(1, false, false, true, now + Duration::from_secs(60)));
        assert!(recovery.retries.is_empty());
        recovery.retry_ready(2, false, false, true, now);
        recovery.retries.get_mut(&2).unwrap().connecting = true;
        assert!(!recovery.retry_ready(2, false, false, false, now));
        assert!(recovery.stopped.contains(&2));
        assert!(recovery.retries.is_empty());
    }

    #[test]
    fn sqlite_restore_preserves_file_mode_and_never_requests_credentials() {
        let profile = Profile::sqlite(
            "Local database",
            rustterm::config::SqliteConfig {
                path: std::env::temp_dir().join("work.sqlite"),
                read_only: true,
                query_timeout_secs: 30,
            },
        );
        let position = PanePosition {
            profile: profile.id,
            sql_database: Some("stale_server_database".into()),
            ..Default::default()
        };
        let restored = profile_at_position(&position, std::slice::from_ref(&profile)).unwrap();
        assert_eq!(restored.connection, profile.connection);
        assert!(matches!(
            reconnect_credential(&restored.connection),
            Some(ReconnectCredential::Empty)
        ));
    }

    #[test]
    fn saved_sql_database_overrides_only_the_restored_instance() {
        use rustterm::config::{SqlConfig, SqlEngine};

        for (engine, profile) in [
            (
                SqlEngine::Mysql,
                Profile::mysql("MySQL", SqlConfig::mysql()),
            ),
            (
                SqlEngine::Postgres,
                Profile::postgres("PostgreSQL", SqlConfig::postgres()),
            ),
        ] {
            let mut position = PanePosition {
                profile: profile.id,
                ..Default::default()
            };
            let restored = profile_at_position(&position, std::slice::from_ref(&profile)).unwrap();
            assert_eq!(restored.connection, profile.connection);

            position.sql_database = Some("selected_database".into());
            let restored = profile_at_position(&position, std::slice::from_ref(&profile)).unwrap();
            let (restored_engine, config) = restored.connection.sql().unwrap();
            assert_eq!(restored_engine, engine);
            assert_eq!(config.database, "selected_database");
            assert_eq!(
                profile.connection.sql().unwrap().1,
                &match engine {
                    SqlEngine::Mysql => SqlConfig::mysql(),
                    SqlEngine::Postgres => SqlConfig::postgres(),
                    SqlEngine::Sqlite => unreachable!(),
                }
            );

            let encoded = serde_json::to_vec(&position).unwrap();
            let decoded: PanePosition = serde_json::from_slice(&encoded).unwrap();
            assert_eq!(decoded, position);
        }
    }

    #[test]
    fn saved_local_directory_and_redis_database_override_only_the_restored_instance() {
        let local = Profile::local("Local", LocalConfig::default());
        let position = PanePosition {
            profile: local.id,
            directory: Some("/tmp/project with spaces".into()),
            ..Default::default()
        };
        let restored = profile_at_position(&position, std::slice::from_ref(&local)).unwrap();
        assert!(
            matches!(restored.connection, Connection::Local(LocalConfig { cwd: Some(ref path), .. }) if path == &PathBuf::from("/tmp/project with spaces"))
        );
        assert!(matches!(
            local.connection,
            Connection::Local(LocalConfig { cwd: None, .. })
        ));
        let mut redis = Profile::redis("Redis", rustterm::config::RedisConfig::default());
        let position = PanePosition {
            profile: redis.id,
            database: Some(8),
            key_pattern: Some("session:*".into()),
            selected_key: Some(vec![0xff, 0, b'a']),
            ..Default::default()
        };
        let restored = profile_at_position(&position, std::slice::from_ref(&redis)).unwrap();
        assert!(matches!(
            restored.connection,
            Connection::Redis(rustterm::config::RedisConfig { database: 8, .. })
        ));
        let encoded = serde_json::to_vec(&position).unwrap();
        let decoded: PanePosition = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded, position);
        if let Connection::Redis(config) = &mut redis.connection {
            config.topology = rustterm::config::RedisTopology::Cluster;
        }
        let restored = profile_at_position(&position, &[redis]).unwrap();
        assert!(matches!(
            restored.connection,
            Connection::Redis(rustterm::config::RedisConfig { database: 0, .. })
        ));
    }
}
