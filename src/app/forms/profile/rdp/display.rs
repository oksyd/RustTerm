use super::*;
use rustterm::i18n::messages as i18n_key;

pub(super) const RESOLUTIONS: [(u16, u16); 3] = [(1280, 720), (1920, 1080), (2560, 1440)];
const COMMON_SCALES: [u32; 4] = [100, 125, 150, 200];

impl ProfileForm {
    pub(super) fn rdp_display(&self, cx: &mut Context<Self>) -> AnyElement {
        let mode = self.rdp.display_mode();
        let dimensions = self
            .fields
            .get(i18n_key::RDP_DESKTOP_WIDTH_PIXELS, cx)
            .trim()
            .parse::<u16>()
            .ok()
            .zip(
                self.fields
                    .get(i18n_key::RDP_DESKTOP_HEIGHT_PIXELS, cx)
                    .trim()
                    .parse::<u16>()
                    .ok(),
            );
        let preset = dimensions
            .filter(|size| !self.rdp_view.custom_dimensions && RESOLUTIONS.contains(size));
        let saved_scale = self
            .rdp
            .settings
            .desktop_scale
            .filter(|scale| !COMMON_SCALES.contains(scale));
        column().gap(px(12.))
            .when(self.rdp.settings.remote_app.is_none(), |body| body.child(self.rdp_choices(
                "rdp-display-mode", i18n_key::RDP_DESKTOP_SIZE, mode,
                [(DisplayMode::FollowWindow, i18n_key::RDP_FOLLOW_WINDOW), (DisplayMode::Fixed, i18n_key::RDP_FIXED_RESOLUTION), (DisplayMode::VirtualMonitors, i18n_key::RDP_VIRTUAL_MONITORS)],
                |this, mode, window, cx| {
                    if mode == DisplayMode::FollowWindow && this.fields.selected_inputs(DIMENSIONS).iter()
                        .any(|input| input.read(cx).focus_handle().is_focused(window))
                    {
                        this.rdp_view.tab_focus[navigation::RdpPage::Display as usize].focus(window);
                    }
                    this.rdp.settings.dynamic_resolution = mode == DisplayMode::FollowWindow;
                    this.rdp.settings.monitor_count = if mode == DisplayMode::VirtualMonitors {
                        this.rdp.settings.monitor_count.max(2)
                    } else { 1 };
                }, cx,
            )))
            .when(mode == DisplayMode::VirtualMonitors, |body| {
                body.child(self.rdp_choices(
                    "rdp-monitor-count", i18n_key::RDP_VIRTUAL_MONITOR_COUNT, self.rdp.settings.monitor_count,
                    [(2, "2"), (3, "3"), (4, "4")],
                    |this, count, _, _| this.rdp.settings.monitor_count = count, cx,
                ))
                .child(hint(i18n_key::RDP_EQUAL_SIZED_VIRTUAL_MONITORS_ARRANGED_HORIZONTALLY_COMBINED_WIDTH_CANNOT))
            })
            .when(mode != DisplayMode::FollowWindow, |body| {
                body.child(self.rdp_choices(
                    "rdp-resolution", if mode == DisplayMode::VirtualMonitors { i18n_key::RDP_RESOLUTION_PER_MONITOR } else { i18n_key::RDP_RESOLUTION }, preset,
                    [(Some(RESOLUTIONS[0]), "1280 × 720"), (Some(RESOLUTIONS[1]), "1920 × 1080"), (Some(RESOLUTIONS[2]), "2560 × 1440"), (None, i18n_key::RDP_CUSTOM)],
                    |this, dimensions, window, cx| {
                        this.rdp_view.custom_dimensions = dimensions.is_none();
                        if let Some((width, height)) = dimensions {
                            this.rdp_view.tab_focus[navigation::RdpPage::Display as usize].focus(window);
                            this.fields.field(i18n_key::RDP_DESKTOP_WIDTH_PIXELS).update(cx, |input, cx| input.set_value(width.to_string(), cx));
                            this.fields.field(i18n_key::RDP_DESKTOP_HEIGHT_PIXELS).update(cx, |input, cx| input.set_value(height.to_string(), cx));
                        } else {
                            this.fields.field(i18n_key::RDP_DESKTOP_WIDTH_PIXELS).read(cx).focus(window);
                        }
                    }, cx,
                )).when(self.rdp_view.custom_dimensions, |body| body.child(self.fields.selected_rows(DIMENSIONS)))
            })
            .child(self.rdp_choices(
                "rdp-desktop-scale", i18n_key::RDP_REMOTE_DESKTOP_SCALING, self.rdp.settings.desktop_scale,
                [(None, i18n_key::RDP_AUTOMATIC), (Some(100), i18n_key::RDP_SCALE_100_PERCENT), (Some(125), "125%"), (Some(150), "150%"), (Some(200), "200%")],
                |this, scale, _, _| this.rdp.settings.desktop_scale = scale, cx,
            ))
            .when(self.rdp.settings.desktop_scale.is_none(), |body| body.child(hint(i18n_key::RDP_MATCHES_LOCAL_DISPLAY_SCALING_WHEN_CONNECTING)))
            .when_some(saved_scale, |body, scale| body.child(row(
                i18n_key::RDP_SAVED_SCALING,
                div().text_color(rgb(ui::colors().accent)).child(format!("{scale}%")),
            )))
            .child(self.rdp_choices(
                "rdp-quality", i18n_key::RDP_DISPLAY_QUALITY, if self.rdp_view.custom_quality { RdpQuality::Custom } else { self.rdp.settings.quality() },
                [(RdpQuality::Clear, i18n_key::RDP_CLARITY), (RdpQuality::Smooth, i18n_key::RDP_SMOOTHNESS), (RdpQuality::Bandwidth, i18n_key::RDP_LOWER_BANDWIDTH), (RdpQuality::Custom, i18n_key::RDP_CUSTOM)],
                |this, quality, _, _| {
                    this.rdp.settings.apply_quality(quality);
                    this.rdp_view.custom_quality = quality == RdpQuality::Custom;
                }, cx,
            ))
            .when(self.rdp_view.custom_quality, |body| body.child(self.rdp_performance(cx)))
            .when(self.rdp.settings.quality() == RdpQuality::Smooth, |body| body.child(hint(i18n_key::RDP_SMOOTHNESS_DESCRIPTION)))
            .into_any_element()
    }

    pub(super) fn rdp_advanced(&self, cx: &mut Context<Self>) -> AnyElement {
        let certificate = self.original.as_ref().and_then(|profile| {
            let Connection::Rdp(settings) = &profile.connection else {
                return None;
            };
            (!self.rdp.clear_certificate
                && settings.host == self.fields.get(i18n_key::WORKSPACE_HOST, cx)
                && self
                    .fields
                    .get(i18n_key::WORKSPACE_PORT, cx)
                    .trim()
                    .parse::<u16>()
                    == Ok(settings.port))
            .then_some(settings.certificate_sha256.as_ref())
            .flatten()
        });
        column()
            .child(self.fields.selected_rows(ADVANCED))
            .child(hint(i18n_key::RDP_NETWORK_KEEPALIVE_HELP))
            .child(self.rdp_toggle(
                "rdp-prevent-idle",
                i18n_key::RDP_PREVENT_IDLE,
                self.rdp.settings.prevent_idle,
                |settings| settings.prevent_idle = !settings.prevent_idle,
                cx,
            ))
            .child(hint(i18n_key::RDP_PREVENT_IDLE_HELP))
            .child(hint(i18n_key::RDP_SERVER_CERTIFICATES_ARE_VERIFIED_AN_UNTRUSTED_CERTIFICATE_REQUIRES_YOUR))
            .when_some(certificate, |body, certificate| {
                body.child(row(i18n_key::RDP_TRUSTED_CERTIFICATE_SHA_256, div().text_size(px(10.)).child(certificate.clone())))
                    .child(ui::text_button("rdp-clear-certificate", i18n_key::RDP_CLEAR_CERTIFICATE_TRUST, ButtonStyle::Neutral)
                        .tab_index(0)
                        .border_1().border_color(rgb(ui::colors().border))
                        .focus(|style| style.border_color(rgb(ui::colors().accent)))
                        .on_click(cx.listener(|this, _, _, cx| {
                            if this.native_pending || this.prompts.is_blocked() { return; }
                            this.rdp.clear_certificate = true;
                            cx.notify();
                        })))
            })
            .when(self.rdp.clear_certificate, |body| body.child(hint(i18n_key::RDP_SAVING_REMOVES_THE_TRUSTED_CERTIFICATE_THE_NEXT_CONNECTION_WILL_VERIFY)))
            .into_any_element()
    }
    fn rdp_performance(&self, cx: &mut Context<Self>) -> AnyElement {
        column()
            .child(self.rdp_choices(
                "rdp-color-depth",
                i18n_key::RDP_COLOR_DEPTH,
                self.rdp.settings.color_depth,
                [(32, i18n_key::RDP_32_BIT), (16, i18n_key::RDP_16_BIT)],
                |this, depth, _, _| this.rdp.settings.color_depth = depth,
                cx,
            ))
            .child(self.rdp_choices(
                "rdp-local-refresh",
                i18n_key::RDP_LOCAL_REFRESH_LIMIT,
                self.rdp.settings.max_fps,
                [(60, "60 FPS"), (30, "30 FPS")],
                |this, fps, _, _| this.rdp.settings.max_fps = fps,
                cx,
            ))
            .child(hint(
                i18n_key::RDP_LIMITS_LOCAL_PRESENTATION_THE_REMOTE_SERVER_CONTROLS_ITS_OWN_UPDATE,
            ))
            .child(self.rdp_toggle(
                "rdp-lossy-bitmaps",
                i18n_key::RDP_ALLOW_LOSSY_BITMAP_COMPRESSION,
                self.rdp.settings.allow_lossy_bitmaps,
                |settings| settings.allow_lossy_bitmaps = !settings.allow_lossy_bitmaps,
                cx,
            ))
            .child(self.rdp_toggle(
                "rdp-compression",
                i18n_key::RDP_COMPRESS_RDP_TRAFFIC,
                self.rdp.settings.compression,
                |settings| settings.compression = !settings.compression,
                cx,
            ))
            .into_any_element()
    }
}
