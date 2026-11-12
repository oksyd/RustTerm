use super::*;
use gpui::{
    AnyElement, ContentMask, CursorStyle, ElementInputHandler, HitboxBehavior, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, canvas, div, point, px, rgb, size,
};
use rustterm::i18n::messages as i18n_key;
use rustterm::rdp::RdpTile;
use std::sync::Weak;
use std::time::Instant;
use ui::{ButtonStyle, Icon};

pub(super) struct TileImage {
    source: Weak<RdpTile>,
    image: Arc<RenderImage>,
    region: Bounds<u16>,
    uploaded: bool,
}

impl Render for RdpView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.config.remote_app.is_none() {
            self.refresh_images(window);
        }
        let index = self.index;
        let fullscreen = window.is_fullscreen();
        let header = ui::panel_header()
            .min_w_0()
            .child(ui::connection_address(
                ("rdp-address", index),
                Icon::Desktop,
                "RDP",
                self.target(),
                self.title.clone(),
            ))
            .when(self.config.remote_app.is_none(), |header| {
                header.child(
                    ui::text_button(("rdp-fit", index), i18n_key::RDP_FIT, ButtonStyle::Neutral)
                        .tab_index(0)
                        .flex_shrink_0()
                        .whitespace_nowrap()
                        .when(self.zoom == Zoom::Fit, |button| {
                            button
                                .bg(rgb(ui::colors().selected))
                                .text_color(rgb(ui::colors().accent))
                        })
                        .tooltip(|_, cx| {
                            cx.new(|_| {
                                ui::Tooltip(
                                    tr(i18n_key::RDP_SCALE_DESKTOP_TO_FIT_THE_PANE).to_owned(),
                                )
                            })
                            .into()
                        })
                        .on_mouse_down(MouseButton::Left, |_, window, cx| {
                            window.prevent_default();
                            cx.stop_propagation();
                        })
                        .on_click(cx.listener(|this, _, window, cx| {
                            if this.display_action_available(window) {
                                this.release_control(cx);
                                this.zoom = Zoom::Fit;
                                this.pan = (0., 0.);
                                this.focus.focus(window);
                                cx.emit(PaneEvent::Activity);
                                cx.notify();
                            }
                        })),
                )
            })
            .when(self.config.remote_app.is_none(), |header| {
                header.child(
                    ui::icon_button(
                        ("rdp-fullscreen", index),
                        if fullscreen {
                            Icon::ExitFullscreen
                        } else {
                            Icon::Maximize
                        },
                        if fullscreen {
                            i18n_key::WORKSPACE_EXIT_FULLSCREEN
                        } else {
                            i18n_key::WORKSPACE_ENTER_FULLSCREEN
                        },
                    )
                    .when(fullscreen, |button| button.bg(rgb(ui::colors().selected)))
                    .tab_index(0)
                    .on_mouse_down(MouseButton::Left, |_, window, cx| {
                        window.prevent_default();
                        cx.stop_propagation();
                    })
                    .on_click(cx.listener(|this, _, window, cx| {
                        if this.display_action_available(window) {
                            this.release_control(cx);
                            window.toggle_fullscreen();
                            this.focus.focus(window);
                            cx.emit(PaneEvent::Activity);
                            cx.notify();
                        }
                    })),
                )
            })
            .child(self.render_menu_button(cx))
            .when(self.pane_close_visible, |header| {
                header.child(
                    ui::icon_button(
                        ("rdp-close", index),
                        Icon::Close,
                        i18n_key::TERMINAL_CLOSE_PANE_CTRL_SHIFT_W,
                    )
                    .tab_index(0)
                    .on_click(cx.listener(|this, _, _, cx| cx.emit(PaneEvent::Close(this.index)))),
                )
            });
        let entity = cx.entity();
        let paint_entity = entity.clone();
        let mut body = div()
            .id(("rdp-desktop", index))
            .relative()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .bg(rgb(0x151619))
            .cursor(CursorStyle::Arrow);
        if self.config.remote_app.is_none() && self.surface_visible() {
            for button in [MouseButton::Left, MouseButton::Right, MouseButton::Middle] {
                body = body
                    .on_mouse_down(
                        button,
                        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                            if !this.contains_desktop(event.position) {
                                this.release_control(cx);
                                return;
                            }
                            this.focus.focus(window);
                            if !this.captures_keyboard() {
                                this.track_pointer_button(button, true);
                                this.request_control(window, cx);
                                window.prevent_default();
                                cx.stop_propagation();
                                return;
                            }
                            this.mouse_button(button, true, event.position, event.modifiers, cx);
                        }),
                    )
                    .on_mouse_up(
                        button,
                        cx.listener(move |this, event: &MouseUpEvent, _, cx| {
                            this.mouse_button(button, false, event.position, event.modifiers, cx);
                        }),
                    )
                    .on_mouse_up_out(
                        button,
                        cx.listener(move |this, event: &MouseUpEvent, _, cx| {
                            this.mouse_button(button, false, event.position, event.modifiers, cx);
                        }),
                    );
            }
            body = body
                .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                    this.mouse_move(event.position, cx)
                }))
                .on_scroll_wheel(cx.listener(Self::scroll))
                .child(
                    canvas(
                        move |bounds, window, cx| {
                            let hidden_bounds = entity.update(cx, |this, _| {
                                this.bounds = Some(bounds);
                                this.pan.0 = this.pan.0.clamp(
                                    0.,
                                    (f32::from(this.desktop_size.0) - f32::from(bounds.size.width))
                                        .max(0.),
                                );
                                this.pan.1 = this.pan.1.clamp(
                                    0.,
                                    (f32::from(this.desktop_size.1)
                                        - f32::from(bounds.size.height))
                                    .max(0.),
                                );
                                (this.state == State::Connected
                                    && this.remote_input_enabled()
                                    && !this.images.is_empty()
                                    && this.hide_local_pointer)
                                    .then(|| {
                                        desktop_bounds(bounds, this.image_size, this.zoom, this.pan)
                                            .intersect(&bounds)
                                    })
                            });
                            // Scope the cursor request to actual desktop pixels.
                            // GPUI updates hit-testing on mouse movement, so the
                            // letterbox stays Arrow without a redraw timer.
                            hidden_bounds
                                .map(|bounds| window.insert_hitbox(bounds, HitboxBehavior::Normal))
                        },
                        move |bounds, pointer_hitbox, window, cx| {
                            if let Some(hitbox) = pointer_hitbox {
                                window.set_cursor_style(CursorStyle::None, &hitbox);
                            }
                            paint_entity.update(cx, |this, cx| {
                                if this.remote_input_enabled() {
                                    window.handle_input(
                                        &this.focus,
                                        ElementInputHandler::new(bounds, paint_entity.clone()),
                                        cx,
                                    );
                                }
                                this.paint_desktop(bounds, window);
                            });
                        },
                    )
                    .size_full(),
                );
        }
        if self.config.remote_app.is_some() && self.state == State::Connected {
            body = body.child(self.application_panel(cx));
        } else if self.images.is_empty() || self.state != State::Connected {
            body = body.child(self.render_connection_state(cx));
        }
        if !self.preedit.is_empty() {
            let mut composition = div()
                .id(("rdp-composition", index))
                .absolute()
                .bottom(px(8.))
                .left(px(8.))
                .right(px(8.))
                .px(px(8.))
                .py(px(5.))
                .rounded(px(3.))
                .bg(rgb(ui::colors().surface))
                .cursor(CursorStyle::Arrow)
                .occlude()
                .text_size(px(13.))
                .text_color(rgb(ui::colors().text))
                .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                .child(self.preedit.clone());
            for button in [MouseButton::Left, MouseButton::Right, MouseButton::Middle] {
                composition = composition
                    .on_mouse_down(button, |_, _, cx| cx.stop_propagation())
                    .on_mouse_up(button, |_, _, cx| cx.stop_propagation());
            }
            body = body.child(composition);
        }
        div()
            .id(("rdp-pane", index))
            .relative()
            .size_full()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .overflow_hidden()
            .bg(rgb(ui::colors().bg))
            .text_color(rgb(ui::colors().text))
            .text_size(px(13.))
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::key_down))
            .on_key_up(cx.listener(Self::key_up))
            .on_modifiers_changed(cx.listener(Self::modifiers_changed))
            .child(header)
            .child(body)
            .children(self.render_menu(cx))
    }
}

impl RdpView {
    fn display_action_available(&self, window: &Window) -> bool {
        self.surface_visible()
            && window.is_window_active()
            && !self.closed
            && self.preedit.is_empty()
            && !self.window_state.prompts.is_busy()
    }

    fn render_menu_button(&self, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        ui::icon_button(
            ("rdp-menu", self.index),
            Icon::More,
            i18n_key::RDP_REMOTE_DESKTOP_ACTIONS,
        )
        .tab_index(0)
        .when(self.menu_open, |button| {
            button.bg(rgb(ui::colors().selected))
        })
        .on_mouse_down(MouseButton::Left, |_, window, cx| {
            window.prevent_default();
            cx.stop_propagation();
        })
        .on_click(cx.listener(|this, _, window, cx| this.toggle_menu(window, cx)))
    }

    pub fn render_status(&self, cx: &mut Context<Self>) -> AnyElement {
        use super::clipboard::Feedback;

        let feedback = self
            .error
            .as_ref()
            .filter(|_| self.state == State::Connected)
            .map(|error| (error.render(), ui::colors().danger, true))
            .or_else(|| match &self.clipboard.feedback {
                Some(feedback @ Feedback::Rejected(_)) => {
                    Some((feedback.text(), ui::colors().danger, true))
                }
                _ => None,
            })
            .or_else(|| {
                self.notice
                    .map(|notice| (tr(notice).to_owned(), ui::colors().muted, true))
            })
            .or_else(|| {
                self.incoming_files
                    .as_ref()
                    .map(|transfer| (transfer.text(), ui::colors().muted, false))
            })
            .or_else(|| {
                self.file_transfer
                    .as_ref()
                    .filter(|transfer| transfer.available && transfer.bytes_sent > 0)
                    .map(|transfer| (transfer.text(), ui::colors().muted, false))
            })
            .or_else(|| {
                self.clipboard.feedback.as_ref().map(|feedback| {
                    let color = match feedback {
                        Feedback::ReadingFiles | Feedback::Waiting | Feedback::Requesting => {
                            ui::colors().muted
                        }
                        Feedback::RemoteEmpty => ui::colors().warning,
                        Feedback::RemoteReady => ui::colors().accent,
                        _ => ui::colors().text,
                    };
                    (feedback.text(), color, true)
                })
            });
        let has_feedback = feedback.is_some();
        let (control_label, control_color) = self.control_status();
        let (text, color, dismissible) = feedback.unwrap_or_else(|| {
            if self.state == State::Connected {
                (tr(control_label).to_owned(), control_color, false)
            } else {
                (self.title.clone(), ui::colors().muted, false)
            }
        });
        let tooltip = if has_feedback {
            format!("{text}\n{}", self.control_details())
        } else {
            self.control_details()
        };
        let resolution = format!("{} × {}", self.desktop_size.0, self.desktop_size.1);
        let status_details = format!(
            "{}\n{} · {}\n{}",
            self.title,
            self.status_text(),
            self.status_details(),
            tr(control_label),
        );
        div()
            .id(("rdp-statusbar", self.index))
            .h_full()
            .flex_1()
            .min_w_0()
            .flex()
            .items_center()
            .gap(px(8.))
            .px_3()
            .text_size(px(11.))
            .tooltip(move |_, cx| cx.new(|_| ui::Tooltip(status_details.clone())).into())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    if this.menu_open {
                        this.close_menu(window, cx);
                    }
                    window.prevent_default();
                    cx.stop_propagation();
                }),
            )
            .child(
                ui::icon(if self.captures_keyboard() {
                    Icon::Key
                } else {
                    Icon::Desktop
                })
                .size(px(12.))
                .text_color(rgb(control_color)),
            )
            .child(
                div()
                    .id(("rdp-feedback", self.index))
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(rgb(color))
                    .tooltip(move |_, cx| cx.new(|_| ui::Tooltip(tooltip.clone())).into())
                    .child(text),
            )
            .when(dismissible, |context| {
                context.child(
                    ui::icon_button(
                        ("rdp-dismiss", self.index),
                        Icon::Close,
                        i18n_key::TERMINAL_DISMISS_NOTICE,
                    )
                    .size(px(22.))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if this.state == State::Connected {
                            this.error = None;
                        }
                        this.notice = None;
                        this.clipboard.feedback = None;
                        cx.emit(PaneEvent::Activity);
                        cx.notify();
                    })),
                )
            })
            .when(!has_feedback && self.state == State::Connected, |context| {
                context.child(
                    div()
                        .flex_shrink_0()
                        .font_family(ui::MONOSPACE_FONT)
                        .text_color(rgb(ui::colors().muted))
                        .child(resolution),
                )
            })
            .child(
                div()
                    .id(("rdp-status", self.index))
                    .flex_shrink_0()
                    .max_w(px(140.))
                    .truncate()
                    .text_color(rgb(self.status_color()))
                    .child(self.status_text()),
            )
            .into_any_element()
    }

    fn paint_desktop(&mut self, viewport: Bounds<Pixels>, window: &mut Window) {
        if self.images.is_empty() || !self.surface_visible() {
            self.invalidate_frame_acknowledgements();
            return;
        }
        let desktop = desktop_bounds(viewport, self.image_size, self.zoom, self.pan);
        let visible = desktop.intersect(&viewport);
        let device_scale = window.scale_factor();
        let started = Instant::now();
        let mut uploads = 0;
        let mut upload_bytes = 0;
        let mut errors = 0;
        let mut painted_tiles = 0;
        window.with_content_mask(Some(ContentMask { bounds: visible }), |window| {
            for tile in &mut self.images {
                let bounds = tile_bounds(tile.region, desktop, self.image_size, device_scale);
                if bounds.is_empty() || !bounds.intersects(&visible) {
                    continue;
                }
                // The image includes a repeated edge pixel for linear sampling;
                // only its core is visible, bounded by shared physical edges.
                let image_bounds = padded_tile_bounds(bounds, tile.region.size);
                window.with_content_mask(Some(ContentMask { bounds }), |window| {
                    match window.paint_image(
                        image_bounds,
                        Default::default(),
                        tile.image.clone(),
                        0,
                        false,
                    ) {
                        Ok(()) => {
                            painted_tiles += 1;
                            if !tile.uploaded {
                                tile.uploaded = true;
                                uploads += 1;
                                upload_bytes += (u64::from(tile.region.size.width) + 2)
                                    * (u64::from(tile.region.size.height) + 2)
                                    * 4;
                            }
                        }
                        Err(_) => errors += 1,
                    }
                });
            }
        });
        if self.state == State::Connected {
            let cursor = self.cursor.paint(desktop, visible, window);
            self.performance.cursor_painted(cursor);
            self.performance
                .painted(started.elapsed(), uploads, upload_bytes, errors);
            if painted_tiles > 0 && errors == 0 {
                self.acknowledge_consumed_frame();
            } else {
                // Do not release server capacity for an incomplete desktop.
                self.invalidate_frame_acknowledgements();
            }
        }
    }

    fn acknowledge_consumed_frame(&mut self) {
        let Some(token) = &self.image_acknowledgement else {
            return;
        };
        if !token.requires_presentation() {
            return;
        }
        if self.consumed_acknowledgement.as_ref() == Some(token) {
            return;
        }
        let Some(sender) = self.sender.clone() else {
            return;
        };
        // Legacy Surface frames require presentation (MS-RDPRFX 2.2.3.1).
        // Graphics pipeline frames were already acknowledged when their output
        // bitmap was copied. GPU image retirement has its own frame callbacks.
        sender.frame_presented(token.clone());
        self.consumed_acknowledgement = Some(token.clone());
    }

    pub(super) fn invalidate_frame_acknowledgements(&mut self) {
        self.consumed_acknowledgement = None;
    }

    fn refresh_images(&mut self, window: &mut Window) {
        if !self.surface_visible() {
            return;
        }
        let frame = self
            .frames
            .as_mut()
            .and_then(|frames| frames.borrow_and_update().clone());
        let Some(frame) = frame else {
            return;
        };
        let started = Instant::now();
        let mut prepared_tiles = 0;
        let mut prepared_bytes = 0;
        let mut retired = Vec::new();
        if frame.tiles.len() < self.images.len() {
            for tile in self.images.drain(frame.tiles.len()..) {
                retired.push(tile.image);
            }
        }
        for (index, tile) in frame.tiles.iter().enumerate() {
            let source = Arc::downgrade(tile);
            if self
                .images
                .get(index)
                .is_some_and(|image| image.source.ptr_eq(&source))
            {
                continue;
            }
            let buffer = tile_buffer(tile);
            prepared_tiles += 1;
            prepared_bytes += buffer.as_raw().len() as u64;
            let image = TileImage {
                source,
                image: Arc::new(RenderImage::new(vec![image::Frame::new(buffer)])),
                region: Bounds::new(point(tile.x, tile.y), size(tile.width, tile.height)),
                uploaded: false,
            };
            if let Some(previous) = self.images.get_mut(index) {
                let previous = std::mem::replace(previous, image);
                retired.push(previous.image);
            } else {
                self.images.push(image);
            }
        }
        self.image_size = (frame.width, frame.height);
        self.image_acknowledgement = frame.acknowledgement.clone();
        self.hide_local_pointer = self
            .cursor
            .hides_platform_cursor()
            .unwrap_or(frame.hide_local_pointer);
        self.cursor.frame(frame.clone());
        retire_images(window, retired);
        self.performance
            .prepared(&frame, prepared_tiles, prepared_bytes, started.elapsed());
    }

    pub(super) fn clear_images(&mut self, window: &mut Window) {
        self.cursor.clear(window);
        let retired = self.images.drain(..).map(|tile| tile.image).collect();
        retire_images(window, retired);
        self.image_size = (0, 0);
        self.image_acknowledgement = None;
        self.invalidate_frame_acknowledgements();
    }

    fn render_connection_state(&self, cx: &mut Context<Self>) -> AnyElement {
        let detail = match self.state {
            State::Connecting => i18n_key::RDP_NEGOTIATING_A_SECURE_REMOTE_DESKTOP_CONNECTION,
            State::WaitingCertificate => i18n_key::RDP_CONFIRM_THE_SERVER_CERTIFICATE_TO_CONTINUE,
            State::Connected => i18n_key::RDP_WAITING_FOR_THE_FIRST_DESKTOP_FRAME,
            State::Disconnected => i18n_key::RDP_THE_REMOTE_DESKTOP_CONNECTION_IS_CLOSED,
            State::Failed => i18n_key::RDP_THE_REMOTE_DESKTOP_CONNECTION_COULD_NOT_BE_ESTABLISHED,
        };
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(12.))
            .p(px(20.))
            .bg(rgb(ui::colors().bg))
            .cursor(CursorStyle::Arrow)
            .child(
                ui::icon(Icon::Desktop)
                    .size(px(30.))
                    .text_color(rgb(self.status_color())),
            )
            .child(
                div()
                    .text_size(px(14.))
                    .text_color(rgb(self.status_color()))
                    .child(self.status_text()),
            )
            .child(
                div()
                    .text_size(px(12.))
                    .text_color(rgb(ui::colors().muted))
                    .child(tr(detail)),
            )
            .when_some(self.error.as_ref(), |d, error| {
                d.child(
                    div()
                        .max_w(px(520.))
                        .text_size(px(12.))
                        .text_color(rgb(ui::colors().danger))
                        .child(error.render()),
                )
            })
            .when(!self.is_running(), |d| {
                d.child(
                    ui::text_button(
                        ("rdp-reconnect", self.index),
                        i18n_key::COMMON_RECONNECT,
                        ButtonStyle::Primary,
                    )
                    .on_click(
                        cx.listener(|this, _, _, cx| cx.emit(PaneEvent::Reconnect(this.index))),
                    ),
                )
            })
            .into_any_element()
    }

    pub(super) fn remote_position(&self, point: Point<Pixels>, clamp: bool) -> Option<(u16, u16)> {
        let viewport = self.bounds?;
        let desktop = desktop_bounds(viewport, self.desktop_size, self.zoom, self.pan);
        map_position(point, viewport, desktop, self.desktop_size, clamp)
    }
}

// An image may have been painted in a synchronous draw that GPUI has not yet
// submitted. In particular, Blade queues atlas uploads until submission and
// reuses texture IDs after drop_image. Removing an image immediately can make
// those queued uploads target a different texture. Keep every retired batch
// alive through an intervening submission, including consecutive redraws.
pub(super) fn retire_images(window: &Window, images: Vec<Arc<RenderImage>>) {
    if images.is_empty() {
        return;
    }
    window.on_next_frame(move |window, _| {
        // Callbacks run before the platform decides whether to draw. Force an
        // intervening submission even if the window otherwise became idle.
        window.refresh();
        window.on_next_frame(move |window, _| {
            for image in images {
                let _ = window.drop_image(image);
            }
        });
    });
}

fn desktop_bounds(
    viewport: Bounds<Pixels>,
    desktop: (u16, u16),
    zoom: Zoom,
    pan: (f32, f32),
) -> Bounds<Pixels> {
    let vw = f32::from(viewport.size.width).max(1.);
    let vh = f32::from(viewport.size.height).max(1.);
    let width = f32::from(desktop.0).max(1.);
    let height = f32::from(desktop.1).max(1.);
    let scale = match zoom {
        Zoom::Fit => (vw / width).min(vh / height),
        Zoom::Actual => 1.,
    };
    let rendered = (width * scale, height * scale);
    let offset = match zoom {
        Zoom::Fit => ((vw - rendered.0) / 2., (vh - rendered.1) / 2.),
        Zoom::Actual => (
            ((vw - width) / 2.).max(0.) - pan.0.clamp(0., (width - vw).max(0.)),
            ((vh - height) / 2.).max(0.) - pan.1.clamp(0., (height - vh).max(0.)),
        ),
    };
    Bounds::new(
        viewport.origin + point(px(offset.0), px(offset.1)),
        size(px(rendered.0), px(rendered.1)),
    )
}

pub(super) fn tile_buffer(tile: &RdpTile) -> image::RgbaImage {
    let stride = usize::from(tile.width) * 4;
    let mut pixels = Vec::with_capacity((stride + 8) * (usize::from(tile.height) + 2));
    let first_row = &tile.pixels[..stride];
    let last_row = &tile.pixels[tile.pixels.len() - stride..];
    // GPUI's atlas uses linear sampling without per-image UV clamping. Repeat
    // one pixel around the image so magnification cannot sample another tile.
    for row in std::iter::once(first_row)
        .chain(tile.pixels.chunks_exact(stride))
        .chain(std::iter::once(last_row))
    {
        pixels.extend_from_slice(&row[..4]);
        pixels.extend_from_slice(row);
        pixels.extend_from_slice(&row[row.len() - 4..]);
    }
    // Published tiles contain validated packed BGRA, which GPUI consumes
    // directly despite image's RgbaImage type name.
    image::RgbaImage::from_raw(
        u32::from(tile.width) + 2,
        u32::from(tile.height) + 2,
        pixels,
    )
    .expect("RDP tile dimensions match its pixel buffer")
}

pub(super) fn tile_bounds(
    region: Bounds<u16>,
    desktop: Bounds<Pixels>,
    dimensions: (u16, u16),
    device_scale: f32,
) -> Bounds<Pixels> {
    // Transform shared edges from desktop coordinates, rather than accumulating
    // rounded tile widths: neighboring tiles must meet at fractional fit scales.
    let map = |point: Point<u16>| {
        let logical = desktop.origin
            + gpui::point(
                desktop.size.width * (f32::from(point.x) / f32::from(dimensions.0)),
                desktop.size.height * (f32::from(point.y) / f32::from(dimensions.1)),
            );
        (logical * device_scale).map(|coordinate| coordinate.round()) / device_scale
    };
    Bounds::from_corners(map(region.origin), map(region.bottom_right()))
}

pub(super) fn padded_tile_bounds(
    core: Bounds<Pixels>,
    dimensions: gpui::Size<u16>,
) -> Bounds<Pixels> {
    let gutter = point(
        core.size.width / f32::from(dimensions.width),
        core.size.height / f32::from(dimensions.height),
    );
    Bounds::from_corners(core.origin - gutter, core.bottom_right() + gutter)
}

fn map_position(
    position: Point<Pixels>,
    viewport: Bounds<Pixels>,
    image: Bounds<Pixels>,
    desktop: (u16, u16),
    clamp: bool,
) -> Option<(u16, u16)> {
    if desktop.0 == 0 || desktop.1 == 0 || image.size.width <= px(0.) || image.size.height <= px(0.)
    {
        return None;
    }
    if !clamp && (!viewport.contains(&position) || !image.contains(&position)) {
        return None;
    }
    // Form the scale first: at 100% it is exactly 1, so integer coordinates
    // cannot round down one pixel through a divide-then-multiply round trip.
    // f64 also keeps fractional fit coordinates accurate before flooring.
    let scale_x = f64::from(desktop.0) / f64::from(f32::from(image.size.width));
    let scale_y = f64::from(desktop.1) / f64::from(f32::from(image.size.height));
    let x = (f64::from(f32::from(position.x - image.left())) * scale_x).floor();
    let y = (f64::from(f32::from(position.y - image.top())) * scale_y).floor();
    Some((
        x.clamp(0., f64::from(desktop.0 - 1)) as u16,
        y.clamp(0., f64::from(desktop.1 - 1)) as u16,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fitted_tiles_share_edges_and_cover_partial_edge_tiles() {
        let dimensions = (1531, 1081);
        let device_scale = 1.25;
        let viewport = Bounds::new(point(px(20.5), px(76.25)), size(px(831.25), px(567.75)));
        let desktop = desktop_bounds(viewport, dimensions, Zoom::Fit, (0., 0.));
        let left = tile_bounds(
            Bounds::new(point(0, 0), size(510, 510)),
            desktop,
            dimensions,
            device_scale,
        );
        let next = tile_bounds(
            Bounds::new(point(510, 0), size(510, 510)),
            desktop,
            dimensions,
            device_scale,
        );
        let bottom = tile_bounds(
            Bounds::new(point(0, 510), size(510, 510)),
            desktop,
            dimensions,
            device_scale,
        );
        let last = tile_bounds(
            Bounds::new(point(1530, 1020), size(1, 61)),
            desktop,
            dimensions,
            device_scale,
        );
        let snap = |point: Point<Pixels>| {
            (point * device_scale).map(|coordinate| coordinate.round()) / device_scale
        };
        assert_eq!(left.origin, snap(desktop.origin));
        assert_eq!(left.right(), next.left());
        assert_eq!(left.bottom(), bottom.top());
        assert_eq!(last.bottom_right(), snap(desktop.bottom_right()));
    }

    #[test]
    fn tile_gutter_repeats_edges_and_corners() {
        let tile = RdpTile {
            x: 0,
            y: 0,
            width: 2,
            height: 2,
            pixels: vec![1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255, 10, 11, 12, 255],
        };
        let buffer = tile_buffer(&tile);
        assert_eq!(buffer.dimensions(), (4, 4));
        for y in 0..4 {
            for x in 0..4 {
                let source_x = usize::from(x >= 2);
                let source_y = usize::from(y >= 2);
                let offset = (source_y * 2 + source_x) * 4;
                assert_eq!(buffer.get_pixel(x, y).0, tile.pixels[offset..offset + 4]);
            }
        }
    }

    #[test]
    fn padded_images_cover_snapped_tiles_after_gpui_rounding() {
        let dimensions = (1531, 1081);
        for device_scale in [1., 1.25, 1.5, 2.] {
            for zoom in [0.8 / 510., 0.3, 0.8, 1., 1.25, 2.6] {
                let desktop = Bounds::new(
                    point(px(0.8), px(0.8)),
                    size(
                        px(f32::from(dimensions.0) * zoom),
                        px(f32::from(dimensions.1) * zoom),
                    ),
                );
                for region in [
                    Bounds::new(point(0, 0), size(510, 510)),
                    Bounds::new(point(510, 510), size(510, 510)),
                    Bounds::new(point(1530, 1020), size(1, 61)),
                ] {
                    let core = tile_bounds(region, desktop, dimensions, device_scale);
                    if core.is_empty() {
                        continue;
                    }
                    let image = padded_tile_bounds(core, region.size)
                        .scale(device_scale)
                        .map_origin(|coordinate| coordinate.floor())
                        .map_size(|length| length.ceil());
                    let clip = core.scale(device_scale);
                    assert!(image.left() <= clip.left());
                    assert!(image.top() <= clip.top());
                    assert!(image.right() >= clip.right());
                    assert!(image.bottom() >= clip.bottom());
                }
            }
        }
    }

    #[test]
    fn subpixel_edge_tiles_can_collapse_without_painting() {
        let region = Bounds::new(point(510, 0), size(1, 510));
        let desktop = Bounds::new(point(px(0.), px(0.)), size(px(51.1), px(51.)));
        let core = tile_bounds(region, desktop, (511, 510), 1.);
        assert!(core.is_empty());
    }

    #[test]
    fn panned_tiles_exclude_offscreen_regions_and_clip_partial_tiles() {
        let dimensions = (1920, 1080);
        let viewport = Bounds::new(point(px(10.), px(20.)), size(px(800.), px(600.)));
        let desktop = desktop_bounds(viewport, dimensions, Zoom::Actual, (1000., 480.));
        let first = tile_bounds(
            Bounds::new(point(0, 0), size(512, 512)),
            desktop,
            dimensions,
            1.,
        );
        let last = tile_bounds(
            Bounds::new(point(1536, 1024), size(384, 56)),
            desktop,
            dimensions,
            1.,
        );
        let visible = desktop.intersect(&viewport);
        assert!(!first.intersects(&visible));
        assert!(last.intersects(&visible));
        assert_eq!(last.intersect(&visible).size, size(px(264.), px(56.)));
    }

    #[test]
    fn fit_coordinates_exclude_letterbox_and_include_viewport_origin() {
        let viewport = Bounds::new(point(px(20.), px(76.)), size(px(800.), px(600.)));
        let image = desktop_bounds(viewport, (1920, 1080), Zoom::Fit, (0., 0.));
        assert_eq!(image.origin, point(px(20.), px(151.)));
        assert_eq!(
            map_position(
                point(px(420.), px(376.)),
                viewport,
                image,
                (1920, 1080),
                false
            ),
            Some((960, 540))
        );
        assert_eq!(
            map_position(
                point(px(100.), px(80.)),
                viewport,
                image,
                (1920, 1080),
                false
            ),
            None
        );
        assert_eq!(
            map_position(
                point(px(1000.), px(1000.)),
                viewport,
                image,
                (1920, 1080),
                true
            ),
            Some((1919, 1079))
        );
    }

    #[test]
    fn actual_pixels_map_pan_without_losing_remote_coordinates() {
        let viewport = Bounds::new(point(px(10.), px(20.)), size(px(800.), px(600.)));
        let image = desktop_bounds(viewport, (1920, 1080), Zoom::Actual, (1000., 480.));
        assert_eq!(
            map_position(
                point(px(10.), px(20.)),
                viewport,
                image,
                (1920, 1080),
                false
            ),
            Some((1000, 480))
        );
        assert_eq!(
            map_position(
                point(px(810.), px(620.)),
                viewport,
                image,
                (1920, 1080),
                true
            ),
            Some((1800, 1079))
        );
    }
}
