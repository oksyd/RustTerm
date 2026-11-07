//! Native windows belonging to one RDP session. Closing a window asks the remote
//! application to close; only a server deletion actually destroys the local view.
mod surface;
mod window;

use super::*;
use gpui::{
    AnyElement, TitlebarOptions, WindowBounds, WindowHandle, WindowOptions, div, point, px, rgb,
    size,
};
use rustrdp::remote_app::{RemoteAppCommand, RemoteAppEvent, RemoteWindow, SystemCommand};
use std::collections::BTreeMap;
use window::ApplicationWindow;

const MAX_PIXELS: u64 = 33_554_432;
const MAX_NATIVE_WINDOWS: usize = 64;
type SizeLimits = ((u16, u16), (u16, u16));

#[derive(Default)]
pub(super) struct Applications {
    windows: BTreeMap<u32, RemoteWindow>,
    native: BTreeMap<u32, WindowHandle<ApplicationWindow>>,
    limits: BTreeMap<u32, SizeLimits>,
    pub(super) active: Option<u32>,
    parent: Option<gpui::AnyWindowHandle>,
    launched: bool,
    suspended: bool,
}

impl RdpView {
    pub(super) fn remote_app_event(
        &mut self,
        event: RemoteAppEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.applications.parent = Some(window.window_handle());
        match event {
            RemoteAppEvent::ExecuteResult { result, status } => {
                self.applications.launched = result == 0;
                if result != 0 {
                    let error = Message::with_args(
                        i18n_key::RDP_REMOTE_APP_LAUNCH_FAILED,
                        vec![
                            ("result", result.into()),
                            ("status", format!("0x{status:08x}").into()),
                        ],
                    );
                    self.disconnect();
                    self.state = State::Failed;
                    self.error = Some(error);
                    self.close_application_windows(cx);
                }
            }
            RemoteAppEvent::Window(remote) => {
                self.applications.suspended = false;
                self.applications.windows.insert(remote.id, remote);
                self.sync_application_windows(window, cx);
                self.refresh_application_frames(cx);
            }
            RemoteAppEvent::Deleted(id) => {
                self.applications.windows.remove(&id);
                self.applications.limits.remove(&id);
                if let Some(handle) = self.applications.native.remove(&id) {
                    let _ = handle.update(cx, |view, window, _| view.remove(window));
                }
            }
            RemoteAppEvent::Desktop { active_window, .. } => {
                self.applications.suspended = false;
                // Do not steal focus from another local application. The remote
                // server can activate a dialog only while this session has focus.
                let active = window.is_window_active()
                    || self.applications.native.values().any(|handle| {
                        handle
                            .update(cx, |_, window, _| window.is_window_active())
                            .unwrap_or(false)
                    });
                if active
                    && !self.window_state.is_locked()
                    && let Some(handle) =
                        active_window.and_then(|id| self.applications.native.get(&id))
                {
                    let _ = handle.update(cx, |_, window, _| window.activate_window());
                }
            }
            RemoteAppEvent::Suspended => {
                self.applications.suspended = true;
                self.notice = Some(i18n_key::RDP_REMOTE_APP_SUSPENDED);
                self.close_application_windows(cx);
            }
            RemoteAppEvent::LocalMoveSize {
                window_id,
                start,
                kind,
                ..
            } => {
                if let Some(handle) = self.applications.native.get(&window_id) {
                    let _ = handle.update(cx, |view, window, cx| {
                        view.local_move_size(start, kind, window, cx)
                    });
                }
            }
            RemoteAppEvent::MinMax {
                window_id,
                min,
                max,
            } => {
                if self.applications.limits.len() < 256
                    || self.applications.limits.contains_key(&window_id)
                {
                    self.applications.limits.insert(window_id, (min, max));
                }
                if let Some(handle) = self.applications.native.get(&window_id) {
                    let _ = handle.update(cx, |view, _, _| view.limits = (min, max));
                }
            }
        }
        cx.notify();
    }

    fn sync_application_windows(&mut self, parent: &Window, cx: &mut Context<Self>) {
        if self.window_state.is_locked() || self.applications.suspended {
            return;
        }
        let pixels: u64 = self
            .applications
            .windows
            .values()
            .filter(|w| w.is_visible())
            .map(|w| {
                let rect = surface::content(w);
                (rect.right - rect.left).max(0) as u64 * (rect.bottom - rect.top).max(0) as u64
            })
            .sum();
        if pixels > MAX_PIXELS
            || self
                .applications
                .windows
                .values()
                .filter(|w| w.is_visible())
                .count()
                > MAX_NATIVE_WINDOWS
        {
            self.disconnect();
            self.error = Some(Message::from_error(rustterm::rdp::RdpError::Limit(
                "RemoteApp windows",
            )));
            self.state = State::Failed;
            self.close_application_windows(cx);
            return;
        }
        for remote in self.applications.windows.values() {
            if !remote.is_visible() {
                if let Some(handle) = self.applications.native.remove(&remote.id) {
                    let _ = handle.update(cx, |view, window, _| view.remove(window));
                }
                continue;
            }
            if let Some(handle) = self.applications.native.get(&remote.id) {
                let _ = handle.update(cx, |view, window, cx| {
                    view.update_remote(remote.clone(), window, cx)
                });
                continue;
            }
            let Some(sender) = self.sender.clone() else {
                return;
            };
            let content = surface::content(remote);
            let scale = parent.scale_factor().max(1.);
            let dimensions = size(
                px((content.right - content.left) as f32 / scale),
                px((content.bottom - content.top) as f32 / scale),
            );
            let decorated = surface::decorated(remote);
            let mut bounds = Bounds::centered(
                parent.display(cx).map(|display| display.id()),
                dimensions,
                cx,
            );
            if let Some(display) = parent.display(cx) {
                bounds.origin = display.bounds().origin
                    + point(
                        px(content.left.max(0) as f32 / scale),
                        px(content.top.max(0) as f32 / scale),
                    );
            }
            if remote.style & 0x00c0_0000 == 0 {
                // Menus/dialogs are positioned relative to their owning local
                // window, even when that window has been moved on the host.
                let owner = self
                    .applications
                    .native
                    .get(&remote.owner)
                    .or_else(|| self.applications.native.values().next());
                if let Some(owner) = owner
                    && let Ok((origin, origin_remote)) = owner.update(cx, |view, window, _| {
                        (window.bounds().origin, surface::content(&view.remote))
                    })
                {
                    bounds.origin = origin
                        + point(
                            px((content.left - origin_remote.left) as f32 / scale),
                            px((content.top - origin_remote.top) as f32 / scale),
                        );
                }
            }
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                app_id: Some("io.rustterm.RustTerm".into()),
                titlebar: decorated.then(|| TitlebarOptions {
                    title: Some(remote.title.clone().into()),
                    ..Default::default()
                }),
                window_decorations: Some(if decorated {
                    gpui::WindowDecorations::Server
                } else {
                    gpui::WindowDecorations::Client
                }),
                is_resizable: remote.style & 0x0004_0000 != 0,
                is_minimizable: remote.style & 0x0002_0000 != 0,
                focus: remote.extended_style & 0x0800_0000 == 0
                    && ((self.visible && parent.is_window_active())
                        || self.applications.active.is_some()),
                kind: if remote.style & 0x00c0_0000 != 0 {
                    gpui::WindowKind::Normal
                } else {
                    gpui::WindowKind::PopUp
                },
                ..Default::default()
            };
            let state = self.window_state.clone();
            let owner = cx.weak_entity();
            let remote = remote.clone();
            let id = remote.id;
            let clipboard = self.config.clipboard;
            let limits = self
                .applications
                .limits
                .get(&id)
                .copied()
                .unwrap_or(((1, 1), (32767, 32767)));
            match cx.open_window(options, |window, cx| {
                cx.new(|cx| {
                    ApplicationWindow::new(remote, owner, sender, state, clipboard, window, cx)
                })
            }) {
                Ok(handle) => {
                    let _ = handle.update(cx, |view, _, _| {
                        view.limits = limits;
                        view.desktop_size = self.desktop_size;
                    });
                    self.applications.native.insert(id, handle);
                }
                Err(_) => {
                    self.error = Some(Message::new(i18n_key::RDP_REMOTE_APP_WINDOW_FAILED));
                }
            }
        }
    }

    pub(super) fn refresh_application_frames(&mut self, cx: &mut Context<Self>) {
        let frame = self
            .frames
            .as_ref()
            .and_then(|frames| frames.borrow().clone());
        if let Some(frame) = frame {
            for handle in self.applications.native.values() {
                let _ = handle.update(cx, |view, window, cx| {
                    view.desktop_size = (frame.width, frame.height);
                    view.hide_local_pointer = frame.hide_local_pointer;
                    if !view.window_state.is_locked()
                        && !self.applications.suspended
                        && let Some(remote) =
                            frame.remote_windows.iter().find(|w| w.id == view.remote.id)
                    {
                        view.surface.update(remote, &frame);
                        view.refresh_images(window);
                    }
                    cx.notify();
                });
            }
        }
    }

    pub(super) fn close_application_windows(&mut self, cx: &mut App) {
        self.applications.active = None;
        for (_, handle) in std::mem::take(&mut self.applications.native) {
            let _ = handle.update(cx, |view, window, _| view.remove(window));
        }
    }

    pub(super) fn notify_application_lock(&mut self, locked: bool, cx: &mut Context<Self>) {
        for handle in self.applications.native.values() {
            let _ = handle.update(cx, |view, window, cx| {
                if locked {
                    view.release();
                    view.clear_images(window);
                }
                cx.notify();
            });
        }
        if !locked {
            let entity = cx.weak_entity();
            if let Some(parent) = self.applications.parent {
                cx.defer(move |cx| {
                    let _ = parent.update(cx, |_, window, cx| {
                        let _ = entity.update(cx, |this, cx| {
                            this.sync_application_windows(window, cx);
                            this.refresh_application_frames(cx);
                        });
                    });
                });
            }
        }
    }

    pub(super) fn application_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .p_4()
            .gap_2()
            .child(tr(i18n_key::RDP_REMOTE_APP_WINDOWS))
            .when(self.applications.windows.is_empty(), |panel| {
                panel.child(tr(if self.applications.launched {
                    i18n_key::RDP_REMOTE_APP_NO_WINDOWS
                } else {
                    i18n_key::RDP_REMOTE_APP_WAITING
                }))
            })
            .children(
                self.applications
                    .windows
                    .values()
                    .filter(|remote| remote.style & 0x00c0_0000 != 0)
                    .map(|remote| {
                        let id = remote.id;
                        div()
                            .id(("remote-app-window", id as usize))
                            .px_3()
                            .py_2()
                            .rounded_md()
                            .bg(rgb(ui::colors().surface))
                            .cursor_pointer()
                            .child(remote.title.clone())
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.send(
                                    RdpCommand::RemoteApp(RemoteAppCommand::System {
                                        window_id: id,
                                        command: SystemCommand::Restore,
                                    }),
                                    cx,
                                );
                                if let Some(handle) = this.applications.native.get(&id) {
                                    let _ =
                                        handle.update(cx, |_, window, _| window.activate_window());
                                }
                                this.sync_application_windows(window, cx);
                            }))
                    }),
            )
            .when_some(self.error.as_ref(), |panel, error| {
                panel.child(
                    div()
                        .text_color(rgb(ui::colors().danger))
                        .child(error.to_string()),
                )
            })
            .when_some(self.notice, |panel, notice| panel.child(tr(notice)))
            .into_any_element()
    }
}
