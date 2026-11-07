use super::*;
use gpui::{
    ContentMask, ElementInputHandler, EntityInputHandler, KeyDownEvent, KeyUpEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, ScrollWheelEvent, UTF16Selection, WeakEntity,
    canvas,
};
use rustterm::rdp::RdpTile;
use std::sync::Weak;

pub(super) struct ApplicationWindow {
    pub remote: RemoteWindow,
    pub window_state: WindowState,
    pub surface: surface::Surface,
    pub limits: SizeLimits,
    owner: WeakEntity<RdpView>,
    sender: RdpSender,
    focus: FocusHandle,
    images: Vec<(Weak<RdpTile>, Arc<RenderImage>)>,
    bounds: Option<Bounds<Pixels>>,
    modifiers: Modifiers,
    keys: BTreeSet<u16>,
    wheel: (f32, f32),
    preedit: String,
    selection: Range<usize>,
    clipboard: bool,
    removed: bool,
    local_move: bool,
    expected_size: gpui::Size<Pixels>,
    position_anchor: (Point<Pixels>, (i32, i32)),
    pub desktop_size: (u16, u16),
    pub hide_local_pointer: bool,
    resize_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl ApplicationWindow {
    pub fn new(
        remote: RemoteWindow,
        owner: WeakEntity<RdpView>,
        sender: RdpSender,
        window_state: WindowState,
        clipboard: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        focus.focus(window);
        let entity = cx.weak_entity();
        window.on_window_should_close(cx, move |_, cx| {
            entity
                .update(cx, |this, _| {
                    if this.removed {
                        return true;
                    }
                    this.send(RdpCommand::RemoteApp(RemoteAppCommand::System {
                        window_id: this.remote.id,
                        command: SystemCommand::Close,
                    }));
                    false // Keep the window for any server-side save/confirmation dialog.
                })
                .unwrap_or(true)
        });
        let subscriptions = vec![
            cx.observe_window_activation(window, |this: &mut Self, window, cx| {
                if !window.is_window_active() {
                    this.release();
                }
                let _ = this.owner.update(cx, |owner, _| {
                    if window.is_window_active() {
                        owner.applications.active = Some(this.remote.id);
                    } else if owner.applications.active == Some(this.remote.id) {
                        owner.applications.active = None;
                        owner.cancel_file_clipboard_read();
                    }
                });
                this.send(RdpCommand::RemoteApp(RemoteAppCommand::Activate {
                    window_id: this.remote.id,
                    enabled: window.is_window_active(),
                }));
                if window.is_window_active() {
                    this.focus.focus(window);
                }
                cx.notify();
            }),
            cx.on_blur(&focus, window, |this: &mut Self, _, _| this.release()),
            cx.observe_window_bounds(window, |this: &mut Self, window, cx| {
                this.resized(window, cx)
            }),
            cx.on_release_in(window, |this: &mut Self, window, _| {
                if !this.removed && (window.is_window_active() || !this.keys.is_empty()) {
                    this.release();
                }
                this.clear_images(window);
            }),
        ];
        let position_anchor = (window.bounds().origin, remote.offset);
        let this = Self {
            remote,
            owner,
            sender,
            window_state,
            clipboard,
            limits: ((1, 1), (32767, 32767)),
            focus,
            surface: Default::default(),
            images: Vec::new(),
            bounds: None,
            modifiers: window.modifiers(),
            keys: BTreeSet::new(),
            wheel: (0., 0.),
            preedit: String::new(),
            selection: 0..0,
            removed: false,
            local_move: false,
            expected_size: window.viewport_size(),
            position_anchor,
            desktop_size: (32767, 32767),
            hide_local_pointer: false,
            resize_task: None,
            _subscriptions: subscriptions,
        };
        if window.is_window_active() && this.remote.extended_style & 0x0800_0000 == 0 {
            this.send(RdpCommand::RemoteApp(RemoteAppCommand::Activate {
                window_id: this.remote.id,
                enabled: true,
            }));
        }
        this
    }

    fn enabled(&self) -> bool {
        !self.removed && !self.window_state.is_locked() && !self.window_state.prompts.is_busy()
    }

    fn send(&self, command: RdpCommand) -> bool {
        if !self.enabled() && !matches!(command, RdpCommand::ReleaseAll) {
            return false;
        }
        match self.sender.try_send(command) {
            Ok(()) => true,
            Err(rustterm::rdp::RdpError::InputBusy) => {
                self.window_state
                    .record_values("rdp-remote-app-input-busy", vec![u64::from(self.remote.id)]);
                false
            }
            Err(_) => {
                self.sender.close();
                false
            }
        }
    }

    pub fn release(&mut self) {
        let _ = self.sender.try_send(RdpCommand::ReleaseAll);
        self.keys.clear();
        self.modifiers = Modifiers::default();
        self.preedit.clear();
        self.selection = 0..0;
    }

    pub fn remove(&mut self, window: &mut Window) {
        self.removed = true;
        self.resize_task = None;
        if window.is_window_active() || !self.keys.is_empty() {
            self.release();
        }
        self.clear_images(window);
        window.remove_window();
    }

    pub fn update_remote(
        &mut self,
        remote: RemoteWindow,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.set_window_title(&remote.title);
        let content = surface::content(&remote);
        let scale = window.scale_factor().max(1.);
        let size = size(
            px((content.right - content.left) as f32 / scale),
            px((content.bottom - content.top) as f32 / scale),
        );
        if self.remote.size != remote.size || self.remote.client_size != remote.client_size {
            self.resize_task = None;
            self.expected_size = size;
            window.resize(size);
        }
        if self.remote.offset != remote.offset {
            self.position_anchor = (window.bounds().origin, remote.offset);
        }
        self.remote = remote;
        cx.notify();
    }

    pub fn local_move_size(
        &mut self,
        start: bool,
        kind: u16,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !cfg!(target_os = "linux") || !self.enabled() {
            return;
        }
        self.local_move = start;
        if !start {
            self.resize_task = None;
            self.release();
            return;
        }
        use gpui::ResizeEdge;
        match kind {
            1 => window.start_window_resize(ResizeEdge::Left),
            2 => window.start_window_resize(ResizeEdge::Right),
            3 => window.start_window_resize(ResizeEdge::Top),
            4 => window.start_window_resize(ResizeEdge::TopLeft),
            5 => window.start_window_resize(ResizeEdge::TopRight),
            6 => window.start_window_resize(ResizeEdge::Bottom),
            7 => window.start_window_resize(ResizeEdge::BottomLeft),
            8 => window.start_window_resize(ResizeEdge::BottomRight),
            9 => window.start_window_move(),
            _ => {
                self.local_move = false;
            } // keyboard operations stay on the server
        }
        // Also finish a drag cancelled before the first configure event.
        self.resized(window, cx);
    }

    fn resized(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let size = window.viewport_size();
        let moved = window.bounds().origin != self.position_anchor.0;
        if (size == self.expected_size && !self.local_move && !moved) || !self.enabled() {
            return;
        }
        self.expected_size = size;
        let scale = window.scale_factor();
        let content = surface::content(&self.remote);
        let border = (
            self.remote.size.0 as i32 - (content.right - content.left),
            self.remote.size.1 as i32 - (content.bottom - content.top),
        );
        let width = (f32::from(size.width) * scale).round() as i32 + border.0;
        let height = (f32::from(size.height) * scale).round() as i32 + border.1;
        let min = self.limits.0;
        let max = self.limits.1;
        let width = width.clamp(
            i32::from(min.0).max(1),
            i32::from(max.0).max(i32::from(min.0)).max(1),
        );
        let height = height.clamp(
            i32::from(min.1).max(1),
            i32::from(max.1).max(i32::from(min.1)).max(1),
        );
        let delta = window.bounds().origin - self.position_anchor.0;
        let mut bounds = self.remote.bounds();
        bounds.left = (self.position_anchor.1.0 + (f32::from(delta.x) * scale).round() as i32)
            .clamp(0, (i32::from(self.desktop_size.0) - width).max(0));
        bounds.top = (self.position_anchor.1.1 + (f32::from(delta.y) * scale).round() as i32)
            .clamp(0, (i32::from(self.desktop_size.1) - height).max(0));
        bounds.right = bounds.left.saturating_add(width).min(32767);
        bounds.bottom = bounds.top.saturating_add(height).min(32767);
        let margins = self.remote.resize_margins;
        bounds.left = bounds.left.saturating_sub(margins[0] as i32).max(-32768);
        bounds.top = bounds.top.saturating_sub(margins[1] as i32).max(-32768);
        bounds.right = bounds.right.saturating_add(margins[2] as i32).min(32767);
        bounds.bottom = bounds.bottom.saturating_add(margins[3] as i32).min(32767);
        self.resize_task = Some(cx.spawn(async move |this, cx| {
            gpui::Timer::after(std::time::Duration::from_millis(120)).await;
            let _ = this.update(cx, |this, _| {
                this.send(RdpCommand::RemoteApp(RemoteAppCommand::Move {
                    window_id: this.remote.id,
                    bounds,
                }));
                this.local_move = false;
                this.release();
            });
        }));
    }

    pub fn clear_images(&mut self, window: &mut Window) {
        super::super::render::retire_images(
            window,
            self.images.drain(..).map(|(_, image)| image).collect(),
        );
    }

    pub fn refresh_images(&mut self, window: &mut Window) {
        let mut retired = Vec::new();
        if self.images.len() > self.surface.tiles.len() {
            retired.extend(
                self.images
                    .drain(self.surface.tiles.len()..)
                    .map(|(_, image)| image),
            );
        }
        for (index, tile) in self.surface.tiles.iter().enumerate() {
            let source = Arc::downgrade(tile);
            if self
                .images
                .get(index)
                .is_some_and(|(previous, _)| previous.ptr_eq(&source))
            {
                continue;
            }
            let image = Arc::new(RenderImage::new(vec![image::Frame::new(
                super::super::render::tile_buffer(tile),
            )]));
            if let Some(previous) = self.images.get_mut(index) {
                retired.push(std::mem::replace(previous, (source, image)).1);
            } else {
                self.images.push((source, image));
            }
        }
        super::super::render::retire_images(window, retired);
    }

    fn paint(&mut self, bounds: Bounds<Pixels>, window: &mut Window) {
        self.bounds = Some(bounds);
        if self.window_state.is_locked() {
            return;
        }
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            for (tile, (_, image)) in self.surface.tiles.iter().zip(&self.images) {
                let region = Bounds::new(point(tile.x, tile.y), size(tile.width, tile.height));
                let core = super::super::render::tile_bounds(
                    region,
                    bounds,
                    self.surface.size,
                    window.scale_factor(),
                );
                let padded = super::super::render::padded_tile_bounds(core, region.size);
                window.with_content_mask(Some(ContentMask { bounds: core }), |window| {
                    let _ = window.paint_image(padded, Default::default(), image.clone(), 0, false);
                });
            }
        });
    }

    fn position(&self, position: Point<Pixels>) -> Option<(u16, u16)> {
        let bounds = self.bounds?;
        if bounds.size.width <= px(0.) || bounds.size.height <= px(0.) {
            return None;
        }
        let rect = surface::content(&self.remote);
        let x = rect.left
            + ((f32::from(position.x - bounds.origin.x) / f32::from(bounds.size.width))
                .clamp(0., 1.)
                * (rect.right - rect.left - 1).max(0) as f32)
                .round() as i32;
        let y = rect.top
            + ((f32::from(position.y - bounds.origin.y) / f32::from(bounds.size.height))
                .clamp(0., 1.)
                * (rect.bottom - rect.top - 1).max(0) as f32)
                .round() as i32;
        Some((x.clamp(0, 32767) as u16, y.clamp(0, 32767) as u16))
    }

    fn modifiers(&mut self, current: Modifiers) {
        for (scancode, pressed) in
            super::super::input::modifier_changes(self.modifiers, current, &self.keys, true)
                .into_iter()
                .flatten()
        {
            if self.send(RdpCommand::Key { scancode, pressed }) {
                if pressed {
                    self.keys.insert(scancode);
                } else {
                    self.keys.remove(&scancode);
                }
            }
        }
        self.modifiers = current;
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !self.enabled() || self.local_move || !self.preedit.is_empty() {
            return;
        }
        let key = &event.keystroke;
        if self.clipboard
            && key.modifiers.control
            && !key.modifiers.alt
            && !key.modifiers.shift
            && matches!(key.key.as_str(), "c" | "x")
        {
            let _ = self.owner.update(cx, |owner, cx| {
                owner.applications.active = Some(self.remote.id);
                if let Some(revision) = owner.begin_clipboard(cx) {
                    let local = owner.read_clipboard(cx);
                    owner.send(
                        RdpCommand::Clipboard {
                            revision,
                            action: RdpClipboardAction::Cancel,
                        },
                        cx,
                    );
                    owner.clipboard.expect_remote_copy(local);
                    owner.file_export_snapshot = Some(cx.background_executor().spawn(async {
                        super::super::file_clipboard::export::Snapshot::capture()
                    }));
                }
            });
        }
        if let Some(text) = crate::app::text_input::alt_gr_text(key) {
            self.release();
            self.send(RdpCommand::Unicode(text.to_owned()));
            window.prevent_default();
            cx.stop_propagation();
            return;
        }

        if let Some(force_local) =
            super::super::input::paste_shortcut(&key.key, key.modifiers, self.clipboard)
        {
            self.release();
            let _ = self.owner.update(cx, |owner, cx| {
                owner.applications.active = Some(self.remote.id);
                owner.start_clipboard_action(true, force_local, window, cx);
            });
            window.prevent_default();
            cx.stop_propagation();
            return;
        }
        if let Some((scancode, modifiers)) =
            super::super::input::key_scancode(key, window.modifiers())
        {
            self.modifiers(modifiers);
            if self.send(RdpCommand::Key {
                scancode,
                pressed: true,
            }) {
                self.keys.insert(scancode);
            }
            cx.stop_propagation();
            // Suppress platform text dispatch for scancode input, so Windows'
            // own IME receives letters even when the host layout is English.
            window.prevent_default();
        }
    }

    fn key_up(&mut self, event: &KeyUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        if let Some((scancode, _)) =
            super::super::input::key_scancode(&event.keystroke, self.modifiers)
            && self.keys.remove(&scancode)
        {
            self.send(RdpCommand::Key {
                scancode,
                pressed: false,
            });
        }
        self.modifiers(event.keystroke.modifiers);
        cx.stop_propagation();
    }

    fn mouse(
        &mut self,
        button: MouseButton,
        pressed: bool,
        position: Point<Pixels>,
        modifiers: Modifiers,
    ) {
        if self.local_move {
            return;
        }
        self.modifiers(modifiers);
        let button = match button {
            MouseButton::Left => RdpMouseButton::Left,
            MouseButton::Right => RdpMouseButton::Right,
            MouseButton::Middle => RdpMouseButton::Middle,
            MouseButton::Navigate(gpui::NavigationDirection::Back) => RdpMouseButton::Back,
            MouseButton::Navigate(gpui::NavigationDirection::Forward) => RdpMouseButton::Forward,
        };
        if let Some((x, y)) = self.position(position) {
            self.send(RdpCommand::MouseButton {
                button,
                pressed,
                x,
                y,
            });
        }
    }
}

impl Render for ApplicationWindow {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let mut root = div()
            .id("remote-app")
            .size_full()
            .overflow_hidden()
            .relative()
            .bg(rgb(0x202020))
            .cursor(
                if self.hide_local_pointer
                    && !self.images.is_empty()
                    && !self.window_state.is_locked()
                {
                    gpui::CursorStyle::None
                } else {
                    gpui::CursorStyle::Arrow
                },
            )
            .text_color(rgb(0xffffff))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key_down))
            .on_key_up(cx.listener(Self::key_up))
            .on_modifiers_changed(cx.listener(
                |this, event: &gpui::ModifiersChangedEvent, _, _| this.modifiers(event.modifiers),
            ));
        if self.window_state.is_locked() {
            return root.child(tr(i18n_key::RDP_REMOTE_APP_SUSPENDED));
        }
        for button in MouseButton::all() {
            root = root
                .on_mouse_down(
                    button,
                    cx.listener(move |this, event: &MouseDownEvent, window, _| {
                        this.focus.focus(window);
                        this.mouse(button, true, event.position, event.modifiers);
                    }),
                )
                .on_mouse_up(
                    button,
                    cx.listener(move |this, event: &MouseUpEvent, _, _| {
                        this.mouse(button, false, event.position, event.modifiers)
                    }),
                )
                .on_mouse_up_out(
                    button,
                    cx.listener(move |this, event: &MouseUpEvent, _, _| {
                        this.mouse(button, false, event.position, event.modifiers)
                    }),
                );
        }
        root.on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, _| {
            if !this.local_move
                && let Some((x, y)) = this.position(event.position)
            {
                this.send(RdpCommand::Pointer { x, y });
            }
        }))
        .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
            if let Some((x, y)) = this.position(event.position) {
                for (horizontal, delta) in
                    super::super::input::wheel_deltas(event.delta, &mut this.wheel)
                {
                    if delta != 0 {
                        this.send(RdpCommand::Wheel {
                            horizontal,
                            delta,
                            x,
                            y,
                        });
                    }
                }
            }
            cx.stop_propagation();
        }))
        .child(
            canvas(
                |_, _, _| (),
                move |bounds, _, window, cx| {
                    entity.update(cx, |this, cx| {
                        if this.enabled() {
                            window.handle_input(
                                &this.focus,
                                ElementInputHandler::new(bounds, entity.clone()),
                                cx,
                            );
                        }
                        this.paint(bounds, window);
                    });
                },
            )
            .size_full(),
        )
        .when(!self.preedit.is_empty(), |root| {
            root.child(
                div()
                    .absolute()
                    .bottom_0()
                    .left_0()
                    .bg(rgb(0x202020))
                    .child(self.preedit.clone()),
            )
        })
    }
}

impl EntityInputHandler for ApplicationWindow {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let bytes = super::super::input::utf16_range(&self.preedit, range);
        *adjusted = Some(
            self.preedit[..bytes.start].encode_utf16().count()
                ..self.preedit[..bytes.end].encode_utf16().count(),
        );
        Some(self.preedit[bytes].to_owned())
    }
    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.selection.clone(),
            reversed: false,
        })
    }
    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        (!self.preedit.is_empty()).then(|| 0..self.preedit.encode_utf16().count())
    }
    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.preedit.clear();
        self.selection = 0..0;
        cx.notify();
    }
    fn replace_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.enabled() && text.len() <= 8192 && !text.chars().any(char::is_control) {
            self.release();
            if !text.is_empty() {
                self.send(RdpCommand::Unicode(text.to_owned()));
            }
            cx.notify();
        }
    }
    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selected: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.enabled() || text.chars().any(char::is_control) {
            return;
        }
        let bytes = range
            .map(|r| super::super::input::utf16_range(&self.preedit, r))
            .unwrap_or(0..self.preedit.len());
        if self.preedit.len() - bytes.len() + text.len() > 8192 {
            return;
        }
        let start = self.preedit[..bytes.start].encode_utf16().count();
        self.preedit.replace_range(bytes, text);
        let length = text.encode_utf16().count();
        self.selection = selected
            .map(|r| {
                start + r.start.min(length)..start + r.end.min(length).max(r.start.min(length))
            })
            .unwrap_or(start + length..start + length);
        cx.notify();
    }
    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        Some(Bounds::new(
            point(bounds.origin.x, bounds.bottom() - px(24.)),
            size(px(1.), px(24.)),
        ))
    }
    fn character_index_for_point(
        &mut self,
        _: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        Some(0)
    }
}
