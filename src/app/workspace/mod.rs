mod chrome;
mod command_notice;
mod connections;
mod dialogs;
mod editing;
mod layout;
mod locking;
mod menus;
mod palette;
mod panes;
mod persistence;
mod probing;
mod recovery;
mod render;
mod resources;
mod session_names;
mod sidebar;
mod tab_groups;
mod utilities;

use crate::app::command_view::CommandHistoryView;
use crate::app::file_transfer::FileTransferView;
use crate::app::forms::{Fields, ProfileForm, SettingsForm};
use crate::app::interaction::WindowState;
use crate::app::layout::{Axis, PaneNode};
use crate::app::pane::{Pane, SessionTool};
use crate::app::text_input::{InputEvent, TextInput};
use crate::app::{Startup, diagnostics};
use gpui::prelude::*;
use gpui::{Bounds, Context, Entity, FocusHandle, Pixels, PromptLevel, Subscription, Window};
use menus::Menu;
use persistence::profile_reference;
use rustterm::config::{AppConfig, ConfigLock, CredentialRef, KnownHosts, Profile, Uuid};
use rustterm::i18n::messages as i18n_key;
use rustterm::i18n::{Message, set_language, tr};
use rustterm::session::Session;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

struct Tab {
    id: usize,
    title: String,
    layout: PaneNode,
    focused: usize,
}
enum Modal {
    Tunnels(Entity<crate::app::tunnel_panel::TunnelPanel>),
    Profile(Entity<ProfileForm>),
    Settings(Entity<SettingsForm>),
    Group {
        editing: Option<Uuid>,
        parent: Option<Uuid>,
        fields: Fields,
    },
    Auth {
        profile: Box<Profile>,
        reconnect: Option<usize>,
        split: Option<(usize, Axis)>,
        secret: Entity<TextInput>,
        sentinel_secret: Option<Entity<TextInput>>,
        auto_connect: bool,
        remember: bool,
        tunnel: Option<Box<TunnelAuth>>,
    },
    Trust(KnownHosts),
    Loading,
    Help,
}

struct TunnelAuth {
    profile: Profile,
    secret: Entity<TextInput>,
    remember: bool,
}

#[derive(Clone, Copy)]
enum DragTarget {
    Split(usize),
    Sidebar,
    Files,
}
#[derive(Clone, Copy)]
enum LeaveAction {
    CloseModal,
    Exit,
}

pub struct Workspace {
    config: AppConfig,
    path: PathBuf,
    _lock: ConfigLock,
    tabs: Vec<Tab>,
    tab_scroll: gpui::ScrollHandle,
    tab_scroll_edges: (bool, bool),
    collapsed_tab_groups: HashSet<Uuid>,
    tab_group_colors: tab_groups::TabGroupColors,
    save_confirmation: Option<gpui::Task<()>>,
    panes: HashMap<usize, Pane>,
    pane_profiles: HashMap<usize, Profile>,
    session_names: session_names::SessionNames,
    subscriptions: HashMap<usize, Subscription>,
    active: usize,
    focused: usize,
    next_id: usize,
    modal: Option<Modal>,
    profile_test: Option<probing::ProfileTest>,
    error: Option<Message>,
    command_notice: Option<command_notice::CommandNotice>,
    confirming_close: bool,
    split_bounds: HashMap<usize, (Axis, Bounds<Pixels>)>,
    dragging: Option<DragTarget>,
    collapsed: HashSet<Uuid>,
    saving: bool,
    save_failed: bool,
    last_save_error: Option<Message>,
    diagnostic_states: HashMap<usize, &'static str>,
    save_again: bool,
    _bounds: Subscription,
    window_state: WindowState,
    modal_focus: FocusHandle,
    modal_subscriptions: Vec<Subscription>,
    modal_generation: usize,
    bound_modal: Vec<gpui::EntityId>,
    modal_bound: bool,
    terminal_size: (f32, f32),
    closing: bool,
    pending_credentials: usize,
    pending_credential_autofill: HashSet<gpui::EntityId>,
    pending_credential_writes: HashMap<Uuid, CredentialRef>,
    credential_cleanup: HashSet<CredentialRef>,
    persisted_credentials: HashSet<CredentialRef>,
    session_filter: Entity<TextInput>,
    _filter_subscription: Subscription,
    menu: Option<Menu>,
    menu_index: usize,
    menu_focus: FocusHandle,
    menu_scroll: gpui::ScrollHandle,
    submenu_scroll: gpui::ScrollHandle,
    application_menu_bounds: [Option<Bounds<Pixels>>; 5],
    workspace_focus: FocusHandle,
    pane_switcher_focus: FocusHandle,
    utility_focus: FocusHandle,
    popup_return_focus: Option<gpui::WeakFocusHandle>,
    modal_cancel_focus: FocusHandle,
    modal_scroll: gpui::ScrollHandle,
    modal_save_focus: FocusHandle,
    files: HashMap<usize, Entity<FileTransferView>>,
    files_subscriptions: HashMap<usize, Subscription>,
    files_follow: HashSet<usize>,
    tools: HashMap<usize, SessionTool>,
    command_views: HashMap<usize, Entity<CommandHistoryView>>,
    command_subscriptions: HashMap<usize, Subscription>,
    resources: resources::Resources,
    selected_profile: Option<Uuid>,
    sidebar_focus: FocusHandle,
    sidebar_scroll: gpui::ScrollHandle,
    sidebar_visible: bool,
    sidebar_width: f32,
    sidebar_overlay_open: bool,
    utility_overlay: bool,
    zoomed: Option<usize>,
    compact_panes: bool,
    error_banner_height: f32,
    menu_anchor: gpui::Point<Pixels>,
    modal_observer: Option<Subscription>,
    modal_saving: bool,
    lock_screen: locking::LockState,
    authentication: Entity<crate::app::auth_prompt::AuthPrompt>,
    _authentication_subscription: Subscription,
    recovery: recovery::Recovery,
    palette: palette::Palette,
    updates: Entity<crate::app::updates::Updates>,
    _updates_subscription: Subscription,
    update_restart_subscription: Option<Subscription>,
    restart_requested: bool,
    restart: std::rc::Rc<std::cell::RefCell<Option<PathBuf>>>,
}
impl Workspace {
    pub fn new(
        config: AppConfig,
        path: PathBuf,
        lock: ConfigLock,
        startup: Startup,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        crate::app::window_frame::WindowFrame::new(window).configure(window);
        set_language(config.settings.language);
        Session::set_global_history_budget(config.settings.global_history_bytes);
        let bounds = cx.observe_window_bounds(window, |this, window, cx| {
            this.scroll_active_tab_into_view();
            let tiled = matches!(
                window.window_decorations(),
                gpui::Decorations::Client { tiling } if tiling.is_tiled()
            );
            if !window.is_fullscreen() && !window.is_maximized() && !tiled {
                // Persist the full surface size, matching WindowOptions at startup.
                let size = window.viewport_size();
                this.config.settings.window_width = f32::from(size.width).clamp(480., 16384.);
                this.config.settings.window_height = f32::from(size.height).clamp(300., 16384.);
            }
            cx.notify();
        });
        let persisted_credentials = config
            .profiles
            .iter()
            .filter_map(profile_reference)
            .collect();
        let session_filter = cx.new(|cx| {
            let mut input =
                TextInput::new(i18n_key::WORKSPACE_FILTER_BY_NAME_HOST_OR_GROUP, false, cx);
            input.set_compact(true, cx);
            input
        });
        let filter_subscription =
            cx.subscribe_in(&session_filter, window, |this, _, event, window, cx| {
                if matches!(event, InputEvent::Submit)
                    && !this.lock_screen.locked
                    && this.modal.is_none()
                    && !this.session_filter.read(cx).is_composing()
                {
                    let profile = this.selected_filtered_profile(cx);
                    if let Some(profile) = profile {
                        this.open_or_focus_profile(profile, window, cx);
                    }
                }
                cx.notify();
            });
        let window_state = WindowState::new(window, &path);
        let authentication =
            cx.new(|cx| crate::app::auth_prompt::AuthPrompt::new(window_state.clone(), window, cx));
        let authentication_subscription =
            cx.subscribe_in(&authentication, window, |this, prompt, _, window, cx| {
                if prompt.read(cx).is_active() {
                    this.menu = None;
                } else {
                    this.restore_focus(window, cx);
                }
                cx.notify();
            });
        let lock_screen = locking::LockState::new(&config.app_lock, window, cx);
        let updates = cx.new(|cx| crate::app::updates::Updates::new(&config.settings, cx));
        let updates_subscription = cx.observe(&updates, |_, _, cx| cx.notify());
        let mut this = Self {
            updates,
            _updates_subscription: updates_subscription,
            update_restart_subscription: None,
            restart_requested: false,
            restart: startup.restart.clone(),
            config,
            path,
            _lock: lock,
            tabs: Vec::new(),
            tab_scroll: gpui::ScrollHandle::new(),
            tab_scroll_edges: (false, false),
            collapsed_tab_groups: HashSet::new(),
            tab_group_colors: tab_groups::TabGroupColors::default(),
            save_confirmation: None,
            panes: HashMap::new(),
            pane_profiles: HashMap::new(),
            session_names: session_names::SessionNames::default(),
            subscriptions: HashMap::new(),
            active: 0,
            focused: 0,
            next_id: 1,
            modal: None,
            profile_test: None,
            error: None,
            command_notice: None,
            confirming_close: false,
            split_bounds: HashMap::new(),
            dragging: None,
            collapsed: HashSet::new(),
            saving: false,
            save_failed: false,
            last_save_error: None,
            diagnostic_states: HashMap::new(),
            save_again: false,
            _bounds: bounds,
            window_state,
            modal_focus: cx.focus_handle(),
            modal_subscriptions: Vec::new(),
            modal_generation: 0,
            bound_modal: Vec::new(),
            modal_bound: false,
            terminal_size: (0., 0.),
            closing: false,
            pending_credentials: 0,
            pending_credential_autofill: HashSet::new(),
            pending_credential_writes: HashMap::new(),
            credential_cleanup: HashSet::new(),
            persisted_credentials,
            session_filter,
            _filter_subscription: filter_subscription,
            menu: None,
            menu_index: 0,
            menu_focus: cx.focus_handle(),
            menu_scroll: gpui::ScrollHandle::new(),
            submenu_scroll: gpui::ScrollHandle::new(),
            application_menu_bounds: [None; 5],
            workspace_focus: cx.focus_handle(),
            pane_switcher_focus: cx.focus_handle(),
            utility_focus: cx.focus_handle(),
            popup_return_focus: None,
            modal_cancel_focus: cx.focus_handle(),
            modal_scroll: gpui::ScrollHandle::new(),
            modal_save_focus: cx.focus_handle(),
            files: HashMap::new(),
            files_subscriptions: HashMap::new(),
            files_follow: HashSet::new(),
            tools: HashMap::new(),
            command_views: HashMap::new(),
            command_subscriptions: HashMap::new(),
            resources: resources::Resources::default(),
            selected_profile: None,
            sidebar_focus: cx.focus_handle(),
            sidebar_scroll: gpui::ScrollHandle::new(),
            sidebar_visible: false,
            sidebar_width: 224.,
            sidebar_overlay_open: false,
            utility_overlay: false,
            zoomed: None,
            compact_panes: false,
            error_banner_height: 0.,
            menu_anchor: gpui::Point::default(),
            modal_observer: None,
            modal_saving: false,
            lock_screen,
            authentication,
            _authentication_subscription: authentication_subscription,
            recovery: recovery::Recovery::default(),
            palette: palette::Palette::new(window, cx),
        };
        this.initialize_lock(startup, window, cx);
        let weak = cx.weak_entity();
        window.on_window_should_close(cx, move |window, cx| {
            weak.update(cx, |this, cx| this.request_exit(window, cx))
                .unwrap_or(true)
        });
        this
    }
    fn id(&mut self) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        id
    }
    fn request_exit(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.updates.read(cx).prepare_exit() {
            if self.lock_screen.locked {
                self.lock_screen.error = Some(i18n_key::UPDATES_BUSY_EXIT.into());
            } else {
                self.error = Some(i18n_key::UPDATES_BUSY_EXIT.into());
            }
            cx.notify();
            return false;
        }
        if self.lock_screen.locked {
            return self.close_locked(window, cx);
        }
        if self.modal_saving || self.modal_composing(cx) {
            return false;
        }
        if self.modal_dirty(cx) {
            self.request_leave_modal(LeaveAction::Exit, window, cx);
            return false;
        }
        self.menu = None;
        if self.confirming_close {
            return false;
        }
        let running = self.panes.values().filter(|p| p.is_running(cx)).count();
        let Some(permit) = self.window_state.prompts.acquire() else {
            return false;
        };
        self.confirming_close = true;
        let files_busy = self
            .files
            .values()
            .any(|files| files.read(cx).is_busy() || files.read(cx).has_pending_edits(cx));
        let unsaved = self.panes.values().any(|pane| pane.has_unsaved_changes(cx));
        let response = (running > 0 || files_busy || unsaved).then(|| {
            window.prompt(
                PromptLevel::Warning,
                &rustterm::trf!(i18n_key::WORKSPACE_END_RUNNING_SESSIONS_AND_QUIT, running = running),
                Some(tr(i18n_key::REDIS_ALL_CONNECTIONS_AND_TRANSFERS_WILL_CLOSE_UNSAVED_DATA_EDITS_WILL)),
                &[tr(i18n_key::COMMON_CANCEL), tr(i18n_key::WORKSPACE_QUIT)],
                cx,
            )
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Some(response) = response
                && response.await != Ok(1)
            {
                let _ = this.update(cx, |this, _| {
                    this.confirming_close = false;
                    this.restart_requested = false;
                });
                return;
            }
            let started = this
                .update(cx, |this, cx| {
                    if !permit.is_valid() {
                        this.confirming_close = false;
                        this.restart_requested = false;
                        cx.notify();
                        return false;
                    }
                    this.persist_workspace(cx);
                    this.closing = true;
                    this.files.clear();
                    this.files_subscriptions.clear();
                    this.dismiss_modal(cx);
                    let mut runs = 0;
                    let mut snapshots = 0;
                    for pane in this.panes.values() {
                        if let Pane::Terminal(view) = pane {
                            let (count, generated) = view.read(cx).metrics();
                            runs += count;
                            snapshots += generated;
                        }
                        pane.close(cx);
                    }
                    diagnostics::metrics(&this.path, this.panes.len(), runs, snapshots);
                    this.save(cx);
                    cx.notify();
                    true
                })
                .unwrap_or(false);
            if !started {
                return;
            }
            for _ in 0..60 {
                gpui::Timer::after(Duration::from_millis(100)).await;
                if this
                    .update(cx, |this, cx| {
                        !this.saving
                            && this.pending_credentials == 0
                            && this.panes.values().all(|p| !p.is_running(cx))
                    })
                    .unwrap_or(true)
                {
                    break;
                }
            }
            let incomplete = this
                .update(cx, |this, cx| {
                    let forced = this.panes.values().any(|p| p.is_running(cx));
                    if forced {
                        diagnostics::record(&this.path, "forced-application-exit", 0);
                    }
                    this.saving || this.save_failed || this.pending_credentials > 0
                })
                .unwrap_or(false);
            if incomplete {
                let response = cx.update(|window, cx| {
                    if !permit.is_valid() {
                        return None;
                    }
                    Some(window.prompt(
                        PromptLevel::Warning,
                        tr(i18n_key::WORKSPACE_SOME_SAVE_OPERATIONS_ARE_STILL_UNCONFIRMED),
                        Some(tr(
                            i18n_key::WORKSPACE_GO_BACK_TO_REVIEW_ERRORS_AND_RETRY_QUITTING_NOW_MAY,
                        )),
                        &[
                            tr(i18n_key::WORKSPACE_GO_BACK),
                            tr(i18n_key::WORKSPACE_QUIT_ANYWAY),
                        ],
                        cx,
                    ))
                });
                let confirmed = match response {
                    Ok(Some(response)) => response.await == Ok(1),
                    _ => false,
                };
                if !confirmed || !permit.is_valid() {
                    let _ = this.update(cx, |this, cx| {
                        this.confirming_close = false;
                        this.restart_requested = false;
                        this.closing = false;
                        cx.notify();
                    });
                    return;
                }
            }
            let _ = cx.update(|window, cx| {
                this.update(cx, |this, cx| {
                    if permit.is_valid() {
                        if this.restart_requested {
                            *this.restart.borrow_mut() = this
                                .updates
                                .read(cx)
                                .installation
                                .as_ref()
                                .map(|installation| installation.executable());
                        }
                        window.remove_window();
                    } else {
                        this.confirming_close = false;
                        this.restart_requested = false;
                        this.closing = false;
                        cx.notify();
                    }
                })
            });
        })
        .detach();
        false
    }
}
