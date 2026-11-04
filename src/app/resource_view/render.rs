use super::{ResourceEvent, ResourceState, ResourceView, memory_percent};
use crate::app::ui;
use gpui::{
    AnyElement, Context, Div, FontWeight, IntoElement, MouseButton, PathBuilder, Render, Window,
    canvas, div, point, prelude::*, px, relative, rgb,
};
use rustterm::i18n::messages as i18n_key;
use rustterm::{i18n::tr, session::DiskUsage};

const MEMORY: u32 = 0xa9bfdc;

impl Render for ResourceView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mut body = div()
            .id("resource-metrics")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .map(ui::preserve_scroll_axes)
            .px(px(16.));
        if self.state == ResourceState::Failed {
            body = body.child(
                div()
                    .py_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .text_color(rgb(ui::WARNING))
                    .child(self.error.clone().unwrap_or_else(|| {
                        tr(i18n_key::RESOURCES_RESOURCE_INFORMATION_IS_UNAVAILABLE).into()
                    }))
                    .child(
                        ui::text_button(
                            "resource-retry",
                            i18n_key::RESOURCES_RETRY_RESOURCE_SAMPLING,
                            ui::ButtonStyle::Neutral,
                        )
                        .tab_index(0)
                        .on_click(cx.listener(|this, _, _, cx| this.retry(cx))),
                    ),
            );
        } else if self.state == ResourceState::Disconnected {
            body = body.child(
                div()
                    .py_4()
                    .text_color(rgb(ui::MUTED))
                    .child(tr(i18n_key::RESOURCES_CONNECTION_DISCONNECTED)),
            );
        } else {
            body = body
                .child(self.cpu())
                .child(self.memory())
                .child(self.network(cx))
                .child(self.disks());
        }
        div()
            .id("server-resources")
            .size_full()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(rgb(ui::RAISED))
            .text_color(rgb(ui::TEXT))
            .text_size(px(12.))
            .track_focus(&self.focus)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    // Focusable children already claimed this click. Preserve
                    // their focus so Enter/Space can activate the same action.
                    if !window.default_prevented() {
                        this.focus.focus(window);
                    }
                    cx.stop_propagation();
                }),
            )
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape"
                    && !this.window_state.is_locked()
                    && !this.window_state.prompts.is_busy()
                {
                    cx.emit(ResourceEvent::Close);
                    cx.stop_propagation();
                }
            }))
            .child(self.header())
            .child(ui::scrollable(body, None))
    }
}

impl ResourceView {
    fn header(&self) -> Div {
        div()
            .flex_shrink_0()
            .px_4()
            .py_2()
            .text_size(px(11.))
            .text_color(rgb(ui::MUTED))
            .child(tr(match self.state {
                ResourceState::Loading => i18n_key::RESOURCES_COLLECTING,
                ResourceState::Live => i18n_key::RESOURCES_UPDATED_JUST_NOW,
                ResourceState::Paused => i18n_key::RESOURCES_SAMPLING_PAUSED,
                ResourceState::Failed => i18n_key::RESOURCES_RESOURCE_INFORMATION_IS_UNAVAILABLE,
                ResourceState::Disconnected => i18n_key::RESOURCES_CONNECTION_DISCONNECTED,
            }))
    }

    fn cpu(&self) -> Div {
        let sample = self.current_snapshot();
        let value = sample.and_then(|sample| sample.cpu_percent);
        let mut section = section().child(
            div()
                .flex()
                .items_center()
                .gap_3()
                .child(label("CPU"))
                .child(div().text_size(px(19.)).child(percent(value)))
                .child(div().flex_1().min_w_0()),
        );
        if let Some(sample) = sample {
            section = section.child(detail(rustterm::trf!(
                i18n_key::RESOURCES_LOAD_ONE_FIVE_FIFTEEN,
                one = format!("{:.2}", sample.load_average[0]),
                five = format!("{:.2}", sample.load_average[1]),
                fifteen = format!("{:.2}", sample.load_average[2]),
            )));
            section = section
                .child(sparkline(
                    vec![(
                        self.trend.iter().map(|point| point.cpu).collect(),
                        ui::ACCENT,
                    )],
                    Some(100.),
                    0.,
                    48.,
                ))
                .child(detail(rustterm::trf!(
                    i18n_key::RESOURCES_CPU_COUNT_RECENT_SAMPLES,
                    count = sample.cpu_count
                )));
        } else {
            section = section.child(detail("—".into()));
        }
        section
    }

    fn memory(&self) -> Div {
        let sample = self.current_snapshot();
        let usage = sample.and_then(memory_percent);
        let value = sample.map_or_else(
            || "—".into(),
            |sample| {
                format!(
                    "{} / {} · {}",
                    bytes(sample.memory.used_bytes as f64),
                    bytes(sample.memory.total_bytes as f64),
                    percent(usage)
                )
            },
        );
        let mut section = section()
            .child(row(label(i18n_key::RESOURCES_MEMORY), div().child(value)))
            .child(meter(usage, MEMORY));
        if let Some(sample) = sample {
            section = section.child(row(
                detail(rustterm::trf!(
                    i18n_key::RESOURCES_AVAILABLE_SIZE,
                    size = bytes(sample.memory.available_bytes as f64)
                )),
                detail(format!(
                    "Swap {} / {}",
                    bytes(sample.memory.swap_used_bytes as f64),
                    bytes(sample.memory.swap_total_bytes as f64)
                )),
            ));
            section = section.child(sparkline(
                vec![(
                    self.trend.iter().map(|point| point.memory).collect(),
                    MEMORY,
                )],
                Some(100.),
                0.,
                36.,
            ));
        } else {
            section = section.child(detail("—".into()));
        }
        section
    }

    fn network(&self, cx: &Context<Self>) -> Div {
        let sample = self.current_snapshot();
        let network = sample.and_then(|sample| {
            sample
                .networks
                .iter()
                .find(|network| Some(&network.name) == self.network.as_ref())
        });
        let mut section = section()
            .child(row(
                label(i18n_key::TERMINAL_NETWORK),
                detail(network.map_or_else(|| "—".into(), |network| single_line(&network.name))),
            ))
            .child(row(
                div().text_color(rgb(MEMORY)).child(format!(
                    "↓ {}",
                    rate(network.and_then(|network| network.rx_bytes_per_sec))
                )),
                div().text_color(rgb(ui::ACCENT)).child(format!(
                    "↑ {}",
                    rate(network.and_then(|network| network.tx_bytes_per_sec))
                )),
            ));
        if sample.is_some() {
            section = section.child(sparkline(
                vec![
                    (
                        self.trend.iter().map(|point| point.received).collect(),
                        MEMORY,
                    ),
                    (
                        self.trend.iter().map(|point| point.sent).collect(),
                        ui::ACCENT,
                    ),
                ],
                None,
                0.,
                44.,
            ));
            for (index, network) in sample
                .into_iter()
                .flat_map(|sample| &sample.networks)
                .enumerate()
            {
                let name = network.name.clone();
                section = section.child(
                    row(
                        detail(single_line(&network.name)),
                        detail(format!(
                            "↓ {}   ↑ {}",
                            rate(network.rx_bytes_per_sec),
                            rate(network.tx_bytes_per_sec)
                        )),
                    )
                    .id(("resource-interface", index))
                    .px_1()
                    .py_1()
                    .rounded_sm()
                    .when(self.network.as_ref() == Some(&name), |row| {
                        row.bg(rgb(ui::SELECTED))
                    })
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(ui::HOVER)))
                    .tab_index(0)
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.select_network(name.clone(), cx)),
                    ),
                );
            }
        }
        section
    }

    fn disks(&self) -> Div {
        let sample = self.current_snapshot();
        let mut disks: Vec<_> = sample
            .into_iter()
            .flat_map(|sample| &sample.disks)
            .collect();
        disks.sort_by(|a, b| b.used_percent.total_cmp(&a.used_percent));
        let low_space = disks.first().is_some_and(|disk| disk.used_percent >= 90.);
        let mut section = section().border_b_0().child(row(
            label(i18n_key::RESOURCES_DISK_SPACE),
            div()
                .text_size(px(10.))
                .text_color(rgb(ui::WARNING))
                .child(if low_space {
                    tr(i18n_key::RESOURCES_LOW_DISK_SPACE)
                } else {
                    ""
                }),
        ));
        if disks.is_empty() {
            return section.child(detail(
                tr(i18n_key::RESOURCES_DISK_INFORMATION_IS_UNAVAILABLE).into(),
            ));
        }
        for (index, disk) in disks.iter().enumerate() {
            let color = if disk.used_percent >= 95. {
                ui::DANGER
            } else if disk.used_percent >= 90. {
                ui::WARNING
            } else {
                MEMORY
            };
            section = section.child(disk_row(disk, color, index));
            section = section.child(meter(Some(disk.used_percent), color));
        }
        if let Some(at) = sample.and_then(|sample| sample.disks_sampled_at) {
            section = section.child(detail(rustterm::trf!(
                i18n_key::RESOURCES_DISK_SAMPLE_SECONDS_S_AGO,
                seconds = at.elapsed().as_secs()
            )));
        }
        section
    }
}

fn section() -> Div {
    div()
        .py_3()
        .flex()
        .flex_col()
        .gap_2()
        .border_b_1()
        .border_color(rgb(ui::BORDER))
}

fn label(text: &'static str) -> Div {
    div().font_weight(FontWeight::SEMIBOLD).child(tr(text))
}

fn detail(text: String) -> Div {
    div()
        .text_size(px(10.))
        .text_color(rgb(ui::MUTED))
        .child(text)
}

fn row(left: Div, right: Div) -> Div {
    div()
        .flex()
        .items_center()
        .justify_between()
        .gap_2()
        .child(left.min_w_0())
        .child(right.flex_shrink_0())
}

fn meter(value: Option<f64>, color: u32) -> Div {
    div()
        .h(px(4.))
        .w_full()
        .rounded_sm()
        .bg(rgb(ui::BORDER))
        .when_some(value, |bar, value| {
            bar.child(
                div()
                    .h_full()
                    .w(relative((value / 100.).clamp(0., 1.) as f32))
                    .rounded_sm()
                    .bg(rgb(color)),
            )
        })
}

fn disk_row(disk: &DiskUsage, color: u32, index: usize) -> Div {
    let mount = single_line(&disk.mount_point);
    let tooltip = mount.clone();
    row(
        div().min_w_0().child(
            div()
                .id(("resource-mount", index))
                .truncate()
                .font_family(ui::MONOSPACE_FONT)
                .text_size(px(11.))
                .child(mount)
                .tooltip(move |_, cx| cx.new(|_| ui::Tooltip(tooltip.clone())).into()),
        ),
        div()
            .text_size(px(11.))
            .text_color(rgb(color))
            .child(rustterm::trf!(
                i18n_key::RESOURCES_PERCENT_SIZE_FREE,
                percent = percent(Some(disk.used_percent)),
                size = bytes(disk.available_bytes as f64)
            )),
    )
}

fn sparkline(
    series: Vec<(Vec<Option<f64>>, u32)>,
    maximum: Option<f64>,
    width: f32,
    height: f32,
) -> AnyElement {
    let maximum = maximum.unwrap_or_else(|| {
        series
            .iter()
            .flat_map(|(values, _)| values)
            .flatten()
            .copied()
            .fold(1., f64::max)
    });
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let width = f32::from(bounds.size.width).max(1.);
            let height = (f32::from(bounds.size.height) - 3.).max(1.);
            for (values, color) in &series {
                let mut builder = PathBuilder::stroke(px(1.5));
                let mut connected = false;
                let mut segments = 0;
                for (index, value) in values.iter().enumerate() {
                    let Some(value) = value.filter(|value| value.is_finite()) else {
                        connected = false;
                        continue;
                    };
                    let x = width * index as f32 / (super::TREND_POINTS - 1) as f32;
                    let y = height * (1. - (value / maximum).clamp(0., 1.) as f32) + 1.5;
                    let position = point(bounds.left() + px(x), bounds.top() + px(y));
                    if connected {
                        builder.line_to(position);
                        segments += 1;
                    } else {
                        builder.move_to(position);
                    }
                    connected = true;
                }
                if segments > 0
                    && let Ok(path) = builder.build()
                {
                    window.paint_path(path, rgb(*color));
                }
            }
        },
    )
    .h(px(height))
    .when(width > 0., |canvas| canvas.w(px(width)))
    .when(width <= 0., |canvas| canvas.w_full())
    .into_any_element()
}

fn percent(value: Option<f64>) -> String {
    value.map_or_else(|| "—".into(), |value| format!("{value:.1}%"))
}

fn bytes(value: f64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = value.max(0.);
    let mut unit = 0;
    while value >= 1024. && unit + 1 < units.len() {
        value /= 1024.;
        unit += 1;
    }
    if unit == 0 {
        format!("{value:.0} {}", units[unit])
    } else {
        format!("{value:.1} {}", units[unit])
    }
}

fn rate(value: Option<f64>) -> String {
    value.map_or_else(|| "—".into(), |value| format!("{}/s", bytes(value)))
}

fn single_line(value: &str) -> String {
    let mut text = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_control() {
            text.extend(ch.escape_default());
        } else {
            text.push(ch);
        }
    }
    text
}
