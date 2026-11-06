use super::{Tab, Workspace};
use crate::app::database_view::{DatabaseLaunch, DatabaseView};
use crate::app::diagnostics;
use crate::app::elasticsearch_view::{ElasticsearchLaunch, ElasticsearchView};
use crate::app::file_transfer::{FileTransferEvent, FileTransferView};
use crate::app::layout::{Axis, PaneNode};
use crate::app::pane::{Pane, PaneEvent, SessionTool};
use crate::app::rdp_view::{RdpLaunch, RdpView};
use crate::app::redis_view::{RedisLaunch, RedisView};
use crate::app::terminal_view::TerminalView;
use gpui::prelude::*;
use gpui::{App, Context, Entity, PromptLevel, Window};
use rustterm::config::{Connection, LocalConfig, Profile, SqlEngine};
use rustterm::i18n::messages as i18n_key;
use rustterm::i18n::{Message, tr};
use rustterm::session::{LocalOptions, Session};

use super::layout::split_ratio;

impl Workspace {
    pub(super) fn pane_display_name(&self, id: usize) -> Option<String> {
        let profile = self.pane_profiles.get(&id)?;
        Some(self.session_names.name(id, &profile.name))
    }

    fn refresh_session_name(&self, id: usize, cx: &mut Context<Self>) {
        if let Some(Pane::Terminal(view)) = self.panes.get(&id)
            && let Some(profile) = self.pane_profiles.get(&id)
        {
            // The view receives display metadata; the saved profile stays intact.
            let mut display = profile.clone();
            display.name = self.session_names.name(id, &profile.name);
            view.update(cx, |view, cx| view.set_profile(&display, cx));
        }
    }

    pub(super) fn profile_for_pane(&self, id: usize, cx: &App) -> Option<Profile> {
        let original = self.pane_profiles.get(&id)?;
        let mut profile = profile_for_launch(original, &self.config.profiles);
        if let Some(Pane::Database(view)) = self.panes.get(&id)
            && let Some((engine, previous)) = original.connection.sql()
        {
            let current = match &mut profile.connection {
                Connection::Mysql(config) if engine == SqlEngine::Mysql => Some(config),
                Connection::Postgres(config) if engine == SqlEngine::Postgres => Some(config),
                _ => None,
            };
            if let Some(current) = current
                && current.same_auth_identity(previous)
            {
                current.database = view.read(cx).database().to_owned();
            }
        }
        Some(profile)
    }
    pub(super) fn active_files(&self) -> Option<&Entity<FileTransferView>> {
        (self.tools.get(&self.focused) == Some(&SessionTool::Files))
            .then(|| self.files.get(&self.focused))
            .flatten()
    }
    pub(super) fn open_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(files) = self.files.get(&self.focused) {
            self.tools.insert(self.focused, SessionTool::Files);
            files.read(cx).focus(window);
            cx.notify();
            return;
        }
        let Some(Pane::Terminal(pane)) = self.panes.get(&self.focused) else {
            return;
        };
        let Some(client) = pane.read(cx).sftp_client() else {
            self.error = Some(i18n_key::WORKSPACE_SFTP_REQUIRES_A_CONNECTED_SSH_SESSION.into());
            cx.notify();
            return;
        };
        let files = cx
            .new(|cx| FileTransferView::new(client, self.window_state.prompts.clone(), window, cx));
        let pane_id = self.focused;
        let subscription = cx.subscribe_in(&files, window, move |this, _, event, window, cx| {
            if matches!(event, FileTransferEvent::Updated) {
                cx.notify();
                return;
            }
            if matches!(event, FileTransferEvent::Hide) && this.focused == pane_id {
                this.close_utility_panel(window, cx);
                return;
            }
            if matches!(event, FileTransferEvent::FollowDirectory) && this.focused == pane_id {
                this.follow_terminal_directory(window, cx);
                return;
            }
            if matches!(event, FileTransferEvent::Close) {
                this.files.remove(&pane_id);
                this.files_subscriptions.remove(&pane_id);
                if this.tools.get(&pane_id) == Some(&SessionTool::Files) {
                    this.tools.remove(&pane_id);
                }
                if this.focused == pane_id {
                    if !this.utility_is_open() {
                        this.utility_overlay = false;
                    }
                    this.sync_visibility(cx);
                    this.restore_focus(window, cx);
                }
                cx.notify();
            }
        });
        self.files_subscriptions.insert(pane_id, subscription);
        self.files.insert(pane_id, files);
        self.tools.insert(pane_id, SessionTool::Files);
        self.files[&pane_id].read(cx).focus(window);
        self.menu = None;
        cx.notify();
    }
    pub(super) fn open_or_focus_profile(
        &mut self,
        profile: Profile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.closing
            || self.window_state.is_locked()
            || self.modal.is_some()
            || self.input_composing(window, cx)
            || self.window_state.prompts.is_busy()
        {
            return;
        }
        let existing = reusable_profile_pane(
            &profile,
            std::iter::once(self.focused)
                .chain(
                    self.tabs
                        .get(self.active)
                        .into_iter()
                        .flat_map(|tab| tab.layout.leaves()),
                )
                .chain(self.tabs.iter().flat_map(|tab| tab.layout.leaves()))
                .filter_map(|id| self.pane_profiles.get(&id).map(|profile| (id, profile))),
        );
        if let Some(id) = existing {
            self.sidebar_overlay_open = false;
            self.focus_pane(id, window, cx);
        } else {
            self.connect_profile(profile, None, window, cx);
        }
    }

    pub(super) fn connect_profile(
        &mut self,
        profile: Profile,
        axis: Option<Axis>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.closing
            || self.window_state.is_locked()
            || self.modal.is_some()
            || self.input_composing(window, cx)
            || self.window_state.prompts.is_busy()
        {
            return;
        }
        if let Some(axis) = axis {
            if self
                .panes
                .get(&self.focused)
                .is_none_or(|pane| pane.input_composing(cx))
            {
                return;
            }
            let same_kind = matches!(
                (&profile.connection, self.panes.get(&self.focused)),
                (
                    Connection::Local(_) | Connection::Ssh(_) | Connection::Telnet(_),
                    Some(Pane::Terminal(_))
                ) | (Connection::Redis(_), Some(Pane::Redis(_)))
                    | (Connection::Elasticsearch(_), Some(Pane::Elasticsearch(_)))
                    | (Connection::Rdp(_), Some(Pane::Rdp(_)))
                    | (
                        Connection::Mysql(_) | Connection::Postgres(_) | Connection::Sqlite(_),
                        Some(Pane::Database(_))
                    )
            );
            if same_kind && !self.can_split_target(self.focused, axis, cx) {
                self.error =
                    Some(i18n_key::REDIS_THIS_PANE_IS_TOO_SMALL_TO_SPLIT_ENLARGE_THE_WINDOW.into());
                cx.notify();
                return;
            }
            if self
                .tabs
                .get(self.active)
                .is_some_and(|tab| tab.layout.leaves().len() >= 16)
            {
                self.error =
                    Some(i18n_key::WORKSPACE_A_TAB_CAN_CONTAIN_UP_TO_16_PANES_OPEN_A.into());
                cx.notify();
                return;
            }
        }
        self.sidebar_overlay_open = false;
        self.open_profile_at(
            profile,
            None,
            axis.map(|axis| (self.focused, axis)),
            window,
            cx,
        );
    }
    pub(super) fn add_session(
        &mut self,
        profile: Profile,
        session: anyhow::Result<Session>,
        reconnect: Option<usize>,
        split: Option<(usize, Axis)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title = match &profile.connection {
            Connection::Local(_)
            | Connection::Redis(_)
            | Connection::Elasticsearch(_)
            | Connection::Rdp(_)
            | Connection::Mysql(_)
            | Connection::Postgres(_)
            | Connection::Sqlite(_) => profile.name.clone(),
            Connection::Ssh(s) => {
                format!("{} · {}@{}:{}", profile.name, s.username, s.host, s.port)
            }
            Connection::Telnet(telnet) => {
                format!("{} · {}:{}", profile.name, telnet.host, telnet.port)
            }
        };
        let session = if let Some(id) = reconnect {
            if !self.panes.contains_key(&id) {
                return;
            }
            let session = match session {
                Ok(session) => session,
                Err(error) => {
                    self.error = Some(Message::from_error(error));
                    cx.notify();
                    return;
                }
            };
            if let Some(Pane::Terminal(pane)) = self.panes.get(&id) {
                let refocus = self.replacement_has_focus(id, window, cx);
                let directory = self.pane_profiles.get(&id).and_then(|previous| {
                    same_ssh_target(&previous.connection, &profile.connection)
                        .then(|| pane.read(cx).reconnect_directory())
                        .flatten()
                });
                // Reuse an existing terminal so its history and preferences survive.
                self.files.remove(&id);
                self.files_subscriptions.remove(&id);
                if self.tools.get(&id) == Some(&SessionTool::Files) {
                    self.tools.remove(&id);
                }
                pane.update(cx, |pane, cx| {
                    pane.reconnect(session, cx);
                    pane.offer_restore_directory(directory, cx);
                    pane.set_title(title, cx);
                    pane.set_profile(&profile, cx);
                });
                self.finish_reconnect(id, profile, refocus, window, cx);
                return;
            }
            Ok(session)
        } else {
            session
        };
        let id = reconnect.unwrap_or_else(|| self.id());
        let refocus = self.replacement_has_focus(id, window, cx);
        let entity = cx.new(|cx| {
            let mut pane = TerminalView::new(
                id,
                title,
                session,
                &self.config.settings,
                self.window_state.clone(),
                window,
                cx,
            );
            pane.set_profile(&profile, cx);
            pane
        });
        let subscription = cx.subscribe_in(&entity, window, |this, _entity, event, window, cx| {
            this.pane_event(event, window, cx)
        });
        if let Some(previous) = self.panes.insert(id, Pane::Terminal(entity)) {
            self.files.remove(&id);
            self.files_subscriptions.remove(&id);
            self.command_views.remove(&id);
            self.command_subscriptions.remove(&id);
            self.tools.remove(&id);
            previous.close(cx);
        }
        self.subscriptions.insert(id, subscription);
        if reconnect.is_some() {
            self.finish_reconnect(id, profile, refocus, window, cx);
        } else {
            self.place_pane(id, profile, split, window, cx);
        }
    }
    pub(super) fn add_redis(
        &mut self,
        profile: Profile,
        launch: RedisLaunch,
        reconnect: Option<usize>,
        split: Option<(usize, Axis)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if reconnect.is_some_and(|id| !self.panes.contains_key(&id)) {
            return;
        }
        let id = reconnect.unwrap_or_else(|| self.id());
        let refocus = self.replacement_has_focus(id, window, cx);
        let view = cx.new(|cx| {
            RedisView::new(
                id,
                profile.name.clone(),
                launch,
                self.window_state.clone(),
                window,
                cx,
            )
        });
        let subscription = cx.subscribe_in(&view, window, |this, _, event, window, cx| {
            this.pane_event(event, window, cx)
        });
        if let Some(previous) = self.panes.insert(id, Pane::Redis(view)) {
            self.files.remove(&id);
            self.files_subscriptions.remove(&id);
            self.command_views.remove(&id);
            self.command_subscriptions.remove(&id);
            self.tools.remove(&id);
            previous.close(cx);
        }
        self.subscriptions.insert(id, subscription);
        if reconnect.is_some() {
            self.finish_reconnect(id, profile, refocus, window, cx);
        } else {
            self.place_pane(id, profile, split, window, cx);
        }
    }
    pub(super) fn add_elasticsearch(
        &mut self,
        profile: Profile,
        launch: ElasticsearchLaunch,
        reconnect: Option<usize>,
        split: Option<(usize, Axis)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if reconnect.is_some_and(|id| !self.panes.contains_key(&id)) {
            return;
        }
        let id = reconnect.unwrap_or_else(|| self.id());
        let refocus = self.replacement_has_focus(id, window, cx);
        let draft = self.panes.get(&id).and_then(|pane| match pane {
            Pane::Elasticsearch(view) => Some(view.read(cx).draft(cx)),
            _ => None,
        });
        let view = cx.new(|cx| {
            ElasticsearchView::new(
                id,
                profile.name.clone(),
                launch,
                self.window_state.clone(),
                window,
                cx,
            )
        });
        if let Some(draft) = draft {
            view.update(cx, |view, cx| view.restore_draft(draft, cx));
        }
        let subscription = cx.subscribe_in(&view, window, |this, _, event, window, cx| {
            this.pane_event(event, window, cx)
        });
        if let Some(previous) = self.panes.insert(id, Pane::Elasticsearch(view)) {
            self.files.remove(&id);
            self.files_subscriptions.remove(&id);
            self.command_views.remove(&id);
            self.command_subscriptions.remove(&id);
            self.tools.remove(&id);
            previous.close(cx);
        }
        self.subscriptions.insert(id, subscription);
        if reconnect.is_some() {
            self.finish_reconnect(id, profile, refocus, window, cx);
        } else {
            self.place_pane(id, profile, split, window, cx);
        }
    }
    pub(super) fn add_database(
        &mut self,
        profile: Profile,
        launch: DatabaseLaunch,
        reconnect: Option<usize>,
        split: Option<(usize, Axis)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if reconnect.is_some_and(|id| !self.panes.contains_key(&id)) {
            return;
        }
        let id = reconnect.unwrap_or_else(|| self.id());
        let refocus = self.replacement_has_focus(id, window, cx);
        let draft = self.panes.get(&id).and_then(|pane| match pane {
            Pane::Database(view) if view.read(cx).engine() == launch.config.engine() => {
                Some(view.read(cx).draft(cx))
            }
            _ => None,
        });
        let view = cx.new(|cx| {
            DatabaseView::new(
                id,
                profile.name.clone(),
                launch,
                self.window_state.clone(),
                window,
                cx,
            )
        });
        if let Some(draft) = draft {
            view.update(cx, |view, cx| view.restore_draft(draft, cx));
        }
        let subscription = cx.subscribe_in(&view, window, |this, _, event, window, cx| {
            this.pane_event(event, window, cx)
        });
        if let Some(previous) = self.panes.insert(id, Pane::Database(view)) {
            self.files.remove(&id);
            self.files_subscriptions.remove(&id);
            self.command_views.remove(&id);
            self.command_subscriptions.remove(&id);
            self.tools.remove(&id);
            previous.close(cx);
        }
        self.subscriptions.insert(id, subscription);
        // Keep the saved identity as the baseline; the view owns its current database.
        let profile = self
            .config
            .profiles
            .iter()
            .find(|saved| saved.id == profile.id)
            .cloned()
            .unwrap_or(profile);
        if reconnect.is_some() {
            self.finish_reconnect(id, profile, refocus, window, cx);
        } else {
            self.place_pane(id, profile, split, window, cx);
        }
    }
    pub(super) fn add_rdp(
        &mut self,
        profile: Profile,
        launch: RdpLaunch,
        reconnect: Option<usize>,
        split: Option<(usize, Axis)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if reconnect.is_some_and(|id| !self.panes.contains_key(&id)) {
            return;
        }
        let id = reconnect.unwrap_or_else(|| self.id());
        let refocus = self.replacement_has_focus(id, window, cx);
        let view = cx.new(|cx| {
            RdpView::new(
                id,
                profile.name.clone(),
                launch,
                &self.config.settings,
                self.window_state.clone(),
                window,
                cx,
            )
        });
        let subscription = cx.subscribe_in(&view, window, |this, _, event, window, cx| {
            this.pane_event(event, window, cx)
        });
        if let Some(previous) = self.panes.insert(id, Pane::Rdp(view)) {
            self.files.remove(&id);
            self.files_subscriptions.remove(&id);
            self.command_views.remove(&id);
            self.command_subscriptions.remove(&id);
            self.tools.remove(&id);
            previous.close(cx);
        }
        self.subscriptions.insert(id, subscription);
        if reconnect.is_some() {
            self.finish_reconnect(id, profile, refocus, window, cx);
        } else {
            self.place_pane(id, profile, split, window, cx);
        }
    }

    fn replacement_has_focus(&self, id: usize, window: &Window, cx: &App) -> bool {
        self.focused == id
            && (self
                .command_views
                .get(&id)
                .is_some_and(|commands| commands.read(cx).owns_focus(window, cx))
                || self
                    .panes
                    .get(&id)
                    .is_some_and(|pane| pane.focus_handle(cx).contains_focused(window, cx))
                || self
                    .files
                    .get(&id)
                    .is_some_and(|files| files.read(cx).owns_focus(window, cx)))
    }

    fn finish_reconnect(
        &mut self,
        id: usize,
        profile: Profile,
        refocus: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for tab in &mut self.tabs {
            if matches!(tab.layout, PaneNode::Leaf(pane_id) if pane_id == id) {
                tab.title = profile.name.clone();
            }
        }
        self.session_names.assign(id, &profile, None);
        self.pane_profiles.insert(id, profile);
        self.refresh_session_name(id, cx);
        self.regroup_tabs();
        self.scroll_active_tab_into_view();
        if self.utility_overlay && !self.utility_is_open() {
            self.utility_overlay = false;
        }
        self.sync_visibility(cx);
        if !self.lock_screen.locked {
            if let Some(tab) = self.tabs.get(self.active) {
                window.set_window_title(&format!("{} — RustTerm", self.workspace_tab_title(tab)));
            }
            if refocus
                && self.focused == id
                && self.modal.is_none()
                && !self.palette.open
                && !self.authentication.read(cx).is_active()
                && !self.window_state.prompts.is_busy()
                && !self.input_composing(window, cx)
            {
                self.restore_focus(window, cx);
            }
        }
        cx.notify();
    }
    fn place_pane(
        &mut self,
        id: usize,
        profile: Profile,
        split: Option<(usize, Axis)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let split_destination = match split {
            Some((target, axis)) => {
                let Some(tab) = self
                    .tabs
                    .iter()
                    .position(|tab| tab.layout.leaves().contains(&target))
                else {
                    self.error = Some(
                        i18n_key::WORKSPACE_THE_TARGET_PANE_WAS_CLOSED_CHOOSE_A_DESTINATION_AND_CONNECT
                            .into(),
                    );
                    if let Some(pane) = self.panes.remove(&id) {
                        pane.close(cx);
                    }
                    self.subscriptions.remove(&id);
                    cx.notify();
                    return;
                };
                let extent = self
                    .fitted_layout(&self.tabs[tab], cx)
                    .pane_size(target, self.terminal_size)
                    .expect("validated target belongs to tab");
                let existing = self.panes[&target].minimum_size(cx);
                let incoming = self.panes[&id].minimum_size(cx);
                let ratio = if self.terminal_size == (0., 0.) {
                    // Initial local panes are created before the first layout pass.
                    Some(0.5)
                } else {
                    split_ratio(axis, extent, existing, incoming)
                };
                let Some(ratio) = ratio else {
                    self.error = Some(
                        i18n_key::REDIS_THIS_PANE_IS_TOO_SMALL_TO_SPLIT_ENLARGE_THE_WINDOW.into(),
                    );
                    if let Some(pane) = self.panes.remove(&id) {
                        pane.close(cx);
                    }
                    self.subscriptions.remove(&id);
                    cx.notify();
                    return;
                };
                Some((tab, target, axis, ratio))
            }
            None => None,
        };
        let saved_number = self.restoring_session_number(profile.id);
        self.session_names.assign(id, &profile, saved_number);
        self.pane_profiles.insert(id, profile.clone());
        self.refresh_session_name(id, cx);
        let destination = if let Some((tab, target, axis, ratio)) = split_destination {
            let split_id = self.id();
            let inserted = self.tabs[tab]
                .layout
                .split(target, id, split_id, axis, ratio);
            debug_assert!(inserted, "validated split target belongs to tab");
            tab
        } else {
            let tab_id = self.id();
            self.tabs.push(Tab {
                id: tab_id,
                title: profile.name,
                layout: PaneNode::Leaf(id),
                focused: id,
            });
            self.tabs.len() - 1
        };
        self.zoomed = None;
        self.focused = id;
        self.tabs[destination].focused = id;
        self.set_active(destination, window, cx);
        diagnostics::record(&self.path, "session-created", id);
        self.capture_restored_pane(id, window, cx);
    }
    pub(super) fn local_session(&self, config: &LocalConfig) -> anyhow::Result<Session> {
        Session::spawn_local(
            LocalOptions {
                shell: config.shell.clone(),
                args: config.args.clone(),
                cwd: config.cwd.clone(),
                env: config.env.clone(),
            },
            80,
            24,
            self.config.settings.history_lines,
        )
    }
    pub(super) fn pane_event(
        &mut self,
        event: &PaneEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if (self.closing || self.lock_screen.locked) && !matches!(event, PaneEvent::Activity) {
            return;
        }
        match event {
            PaneEvent::CommandFinished {
                index,
                run,
                command,
            } => {
                self.command_notice = Some(super::command_notice::CommandNotice {
                    pane: *index,
                    run: *run,
                    command: command.clone(),
                });
            }
            PaneEvent::DirectoryChanged(id, directory) => {
                if self.files_follow.contains(id)
                    && let Some(files) = self.files.get(id)
                {
                    files.update(cx, |files, cx| files.navigate_to(directory.clone(), cx));
                }
            }
            PaneEvent::Focused(id) => {
                if let Some(tab) = self.tabs.get_mut(self.active)
                    && tab.layout.leaves().contains(id)
                {
                    self.focused = *id;
                    tab.focused = *id;
                }
            }
            PaneEvent::NewTab => {
                let profile = self
                    .config
                    .profiles
                    .iter()
                    .find(|p| matches!(p.connection, Connection::Local(_)))
                    .cloned()
                    .unwrap_or_else(|| {
                        Profile::local(tr(i18n_key::COMMON_LOCAL_TERMINAL), LocalConfig::default())
                    });
                self.open_profile(profile, None, window, cx);
            }
            PaneEvent::NextTab => {
                self.set_active((self.active + 1) % self.tabs.len().max(1), window, cx)
            }
            PaneEvent::NextPane => {
                let ids = self
                    .tabs
                    .get(self.active)
                    .map(|tab| tab.layout.leaves())
                    .unwrap_or_default();
                if !ids.is_empty() {
                    let current = ids.iter().position(|id| *id == self.focused).unwrap_or(0);
                    self.focus_pane(ids[(current + 1) % ids.len()], window, cx);
                }
            }
            PaneEvent::Split(axis) => self.split(*axis, window, cx),
            PaneEvent::Zoom => self.toggle_zoom(window, cx),
            PaneEvent::Files => self.toggle_tool(SessionTool::Files, window, cx),
            PaneEvent::Commands => self.toggle_tool(SessionTool::Commands, window, cx),
            PaneEvent::Resources => self.toggle_tool(SessionTool::Resources, window, cx),
            PaneEvent::Reconnect(id) => {
                self.reset_reconnect(*id);
                if let Some(profile) = self.profile_for_pane(*id, cx) {
                    self.open_profile(profile, Some(*id), window, cx);
                }
            }
            PaneEvent::DatabaseChanged { index, database } => {
                if let Some(mut profile) = self.profile_for_pane(*index, cx) {
                    let config = match &mut profile.connection {
                        Connection::Mysql(config) | Connection::Postgres(config) => config,
                        _ => return,
                    };
                    config.database = database.clone();
                    if let Err(error) = config.validate() {
                        self.error = Some(Message::from_error(error));
                    } else {
                        self.reset_reconnect(*index);
                        self.open_profile(profile, Some(*index), window, cx);
                    }
                }
            }
            PaneEvent::RdpCertificateTrusted { index, fingerprint } => {
                self.remember_rdp_certificate(*index, fingerprint, cx);
            }
            PaneEvent::Close(id) => self.confirm_close(vec![*id], window, cx),
            PaneEvent::Activity => {
                for (id, pane) in &self.panes {
                    let state = pane.diagnostic_state(cx);
                    if self.diagnostic_states.insert(*id, state) != Some(state) {
                        diagnostics::record(&self.path, state, *id);
                    }
                }
            }
        }
        cx.notify();
    }
    pub(super) fn split(&mut self, axis: Axis, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(profile) = self.profile_for_pane(self.focused, cx) {
            self.connect_profile(profile, Some(axis), window, cx);
        }
    }
    pub(super) fn confirm_close(
        &mut self,
        ids: Vec<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.confirming_close {
            return;
        }
        let running = ids
            .iter()
            .filter(|id| self.panes.get(id).is_some_and(|p| p.is_running(cx)))
            .count();
        let unsaved = ids.iter().any(|id| {
            self.panes
                .get(id)
                .is_some_and(|pane| pane.has_unsaved_changes(cx))
        });
        let files_busy = ids.iter().any(|id| {
            self.files.get(id).is_some_and(|files| {
                files.read(cx).is_busy() || files.read(cx).has_pending_edits(cx)
            })
        });
        if running == 0 && !unsaved && !files_busy {
            self.remove_panes(&ids, window, cx);
            return;
        }
        let Some(permit) = self.window_state.prompts.acquire() else {
            return;
        };
        self.confirming_close = true;
        let response = window.prompt(
            PromptLevel::Warning,
            &rustterm::trf!(
                i18n_key::WORKSPACE_CLOSE_PANES_CONFIRMATION,
                count = ids.len()
            ),
            Some(tr(
                i18n_key::REDIS_CONNECTIONS_AND_TRANSFERS_WILL_CLOSE_UNSAVED_DATA_EDITS_AND_PANE,
            )),
            &[
                tr(i18n_key::COMMON_CANCEL),
                tr(i18n_key::WORKSPACE_END_AND_CLOSE),
            ],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let yes = response.await == Ok(1);
            let _ = cx.update(|window, cx| {
                this.update(cx, |this, cx| {
                    let valid = permit.is_valid();
                    drop(permit);
                    this.confirming_close = false;
                    if yes && valid {
                        this.remove_panes(&ids, window, cx);
                    }
                    cx.notify();
                })
            });
        })
        .detach();
    }
    fn remove_panes(&mut self, ids: &[usize], window: &mut Window, cx: &mut Context<Self>) {
        self.files_follow.retain(|id| !ids.contains(id));
        let active_id = self.tabs.get(self.active).map(|tab| tab.id);
        for id in ids {
            self.tools.remove(id);
            self.command_views.remove(id);
            self.command_subscriptions.remove(id);
            self.files.remove(id);
            self.files_subscriptions.remove(id);
            if let Some(pane) = self.panes.remove(id) {
                pane.close(cx);
            }
            self.subscriptions.remove(id);
            self.pane_profiles.remove(id);
            self.session_names.remove(*id);
            self.diagnostic_states.remove(id);
            diagnostics::record(&self.path, "session-closed", *id);
        }
        self.tabs = std::mem::take(&mut self.tabs)
            .into_iter()
            .filter_map(|mut tab| {
                let mut layout = Some(tab.layout.clone());
                for id in ids {
                    layout = layout.and_then(|node| node.remove(*id));
                }
                layout.map(|node| {
                    tab.layout = node;
                    tab
                })
            })
            .collect();
        self.split_bounds.clear();
        self.dragging = None;
        self.active = active_id
            .and_then(|id| self.tabs.iter().position(|tab| tab.id == id))
            .unwrap_or_else(|| self.active.min(self.tabs.len().saturating_sub(1)));
        self.set_active(self.active, window, cx);
        cx.notify();
    }
}
fn reusable_profile_pane<'a>(
    profile: &Profile,
    panes: impl IntoIterator<Item = (usize, &'a Profile)>,
) -> Option<usize> {
    if matches!(profile.connection, Connection::Ssh(_)) {
        return None;
    }
    panes
        .into_iter()
        .find_map(|(id, opened)| (opened.id == profile.id).then_some(id))
}

fn profile_for_launch(original: &Profile, profiles: &[Profile]) -> Profile {
    profiles
        .iter()
        .find(|p| p.id == original.id)
        .unwrap_or(original)
        .clone()
}

fn same_ssh_target(previous: &Connection, next: &Connection) -> bool {
    let (Connection::Ssh(previous), Connection::Ssh(next)) = (previous, next) else {
        return false;
    };
    previous.host == next.host
        && previous.port == next.port
        && previous.username == next.username
        && previous.jump_hosts.len() == next.jump_hosts.len()
        && previous
            .jump_hosts
            .iter()
            .zip(&next.jump_hosts)
            .all(|(previous, next)| {
                previous.host == next.host
                    && previous.port == next.port
                    && previous.username == next.username
            })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opening_an_ssh_profile_always_creates_another_session() {
        let profile = Profile::ssh("SSH", Default::default());
        assert_eq!(reusable_profile_pane(&profile, []), None);
        assert_eq!(
            reusable_profile_pane(&profile, [(3, &profile), (7, &profile)]),
            None
        );
    }

    #[test]
    fn other_connection_types_reuse_only_the_same_profile_in_focus_order() {
        use rustterm::config::SqlConfig;
        for profile in [
            Profile::rdp("RDP", Default::default()),
            Profile::redis("Redis", Default::default()),
            Profile::mysql("MySQL", SqlConfig::mysql()),
            Profile::postgres("PostgreSQL", SqlConfig::postgres()),
            Profile::sqlite("SQLite", Default::default()),
            Profile::telnet("Telnet", Default::default()),
            Profile::local("Local", Default::default()),
        ] {
            let mut other = profile.clone();
            other.id = rustterm::config::Uuid::new_v4();
            assert_eq!(
                reusable_profile_pane(&profile, []),
                None,
                "{}",
                profile.name
            );
            assert_eq!(
                reusable_profile_pane(&profile, [(1, &other)]),
                None,
                "{}",
                profile.name
            );
            assert_eq!(
                reusable_profile_pane(&profile, [(1, &other), (7, &profile), (3, &profile)]),
                Some(7),
                "{}",
                profile.name
            );
            assert_eq!(
                reusable_profile_pane(&profile, [(3, &profile), (7, &profile)]),
                Some(3),
                "{}",
                profile.name
            );
        }
    }

    #[test]
    fn reconnect_directory_stays_with_the_same_ssh_destination_and_route() {
        use rustterm::config::{AuthMethod, CredentialRef, SshConfig, SshJumpHost};
        let original = SshConfig {
            host: "server".into(),
            username: "alice".into(),
            jump_hosts: vec![SshJumpHost {
                host: "gateway".into(),
                port: 22,
                username: "jump".into(),
                identity_file: None,
                agent_socket: None,
            }],
            ..Default::default()
        };
        let connection = Connection::Ssh(original.clone());
        let mut updated = original.clone();
        updated.keepalive_secs = 45;
        updated.auth = AuthMethod::Password {
            credential: Some(CredentialRef::new()),
        };
        assert!(same_ssh_target(&connection, &Connection::Ssh(updated)));
        for field in [
            "host",
            "port",
            "username",
            "jump_host",
            "jump_user",
            "jump_count",
        ] {
            let mut changed = original.clone();
            match field {
                "host" => changed.host = "another-server".into(),
                "port" => changed.port = 2222,
                "username" => changed.username = "bob".into(),
                "jump_host" => changed.jump_hosts[0].host = "other-gateway".into(),
                "jump_user" => changed.jump_hosts[0].username = "other-user".into(),
                "jump_count" => changed.jump_hosts.clear(),
                _ => unreachable!(),
            }
            assert!(
                !same_ssh_target(&connection, &Connection::Ssh(changed)),
                "{field}"
            );
        }
        assert!(!same_ssh_target(
            &connection,
            &Connection::Local(LocalConfig::default())
        ));
    }

    #[test]
    fn reconnect_uses_current_profile_but_existing_instance_keeps_its_snapshot() {
        let original = Profile::local("local", LocalConfig::default());
        let mut edited = original.clone();
        edited.connection = Connection::Local(LocalConfig {
            shell: "/bin/sh".into(),
            ..LocalConfig::default()
        });
        let next = profile_for_launch(&original, &[edited.clone()]);
        assert_eq!(next.connection, edited.connection);
        assert_eq!(
            profile_for_launch(&original, &[]).connection,
            original.connection
        );
    }
}
