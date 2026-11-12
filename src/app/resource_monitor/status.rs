use super::{ResourceMonitor, ResourceState};
use crate::app::{
    resource_view::{bytes, percent, rate},
    ui,
};
use gpui::{
    Context, Entity, EventEmitter, IntoElement, Render, Subscription, Window, div, prelude::*, px,
    rgb,
};
use rustterm::i18n::{messages as key, tr};

pub enum MetricsEvent {
    Open,
}
pub struct MetricsView {
    monitor: Entity<ResourceMonitor>,
    _subscription: Subscription,
}
impl EventEmitter<MetricsEvent> for MetricsView {}
impl MetricsView {
    pub fn new(monitor: Entity<ResourceMonitor>, cx: &mut Context<Self>) -> Self {
        let subscription = cx.observe(&monitor, |_, _, cx| cx.notify());
        Self {
            monitor,
            _subscription: subscription,
        }
    }
}
impl ResourceMonitor {
    pub fn state_label(&self) -> &'static str {
        tr(match self.state() {
            ResourceState::Loading => key::RESOURCES_COLLECTING,
            ResourceState::Live => key::RESOURCES_UPDATED_JUST_NOW,
            ResourceState::Stale => key::RESOURCES_STALE,
            ResourceState::Paused => key::RESOURCES_SAMPLING_PAUSED,
            ResourceState::Failed => key::RESOURCES_STATUS_UNAVAILABLE,
            ResourceState::Disconnected => key::RESOURCES_CONNECTION_DISCONNECTED,
        })
    }
    fn tooltip(&self) -> String {
        let scope = tr(if self.is_container() {
            key::RESOURCES_CONTAINER
        } else {
            key::RESOURCES_HOST
        });
        let mut text = format!("{scope} · {}", self.state_label());
        if let Some(summary) = self.summary() {
            let memory = summary.memory_used.zip(summary.memory_total).map_or_else(
                || "—".into(),
                |(used, total)| format!("{} / {}", bytes(used as f64), bytes(total as f64)),
            );
            text.push_str(&format!(
                "\nCPU {} · {} {} ({})\n{} · ↓ {}  ↑ {}",
                percent(summary.cpu),
                tr(key::RESOURCES_MEMORY),
                memory,
                percent(summary.memory_percent()),
                summary
                    .network
                    .as_deref()
                    .unwrap_or(tr(key::TERMINAL_NETWORK)),
                rate(summary.rx),
                rate(summary.tx)
            ));
        }
        if let Some(age) = self.age() {
            text.push_str(&format!(
                "\n{}",
                rustterm::trf!(key::RESOURCES_SAMPLE_AGE, seconds = age.as_secs())
            ));
        }
        if self.state() == ResourceState::Failed
            && let Some(error) = &self.error
        {
            text.push('\n');
            text.push_str(error);
        }
        text.push_str(&format!("\n{}", tr(key::RESOURCES_OPEN_DETAILS)));
        text
    }
}
impl Render for MetricsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let width = f32::from(window.viewport_size().width);
        let compact = width < 700.;
        let monitor = self.monitor.read(cx);
        let summary = monitor.summary();
        let tooltip = self.monitor.clone();
        let scope = tr(if monitor.is_container() {
            key::RESOURCES_CONTAINER
        } else {
            key::RESOURCES_HOST
        });
        div()
            .id("status-resource-summary")
            .h_full()
            .px_2()
            .flex()
            .items_center()
            .gap_2()
            .flex_shrink_0()
            .border_l_1()
            .border_color(rgb(ui::colors().border))
            .text_size(px(11.))
            .cursor_pointer()
            .tab_index(0)
            .hover(|s| s.bg(rgb(ui::colors().hover)))
            .tooltip(move |_, cx| {
                let text = tooltip.read(cx).tooltip();
                cx.new(|_| ui::Tooltip(text)).into()
            })
            .when(compact, |bar| {
                bar.child(ui::icon(ui::Icon::Pulse).text_color(rgb(
                    if matches!(
                        monitor.state(),
                        ResourceState::Failed | ResourceState::Stale
                    ) {
                        ui::colors().warning
                    } else {
                        ui::colors().muted
                    },
                )))
            })
            .when(width >= 900., |bar| {
                bar.child(div().text_color(rgb(ui::colors().muted)).child(scope))
            })
            .when_some(summary.filter(|_| !compact), |bar, summary| {
                bar.child(format!("CPU {}", percent(summary.cpu)))
                    .child(format!(
                        "{} {}",
                        tr(key::RESOURCES_MEMORY),
                        percent(summary.memory_percent())
                    ))
                    .when(width >= 1100., |bar| {
                        bar.child(format!("↓ {}  ↑ {}", rate(summary.rx), rate(summary.tx)))
                    })
            })
            .when(!compact && summary.is_none(), |bar| {
                bar.child(
                    div()
                        .text_color(rgb(
                            if matches!(
                                monitor.state(),
                                ResourceState::Failed | ResourceState::Stale
                            ) {
                                ui::colors().warning
                            } else {
                                ui::colors().muted
                            },
                        ))
                        .child(monitor.state_label()),
                )
            })
            .on_click(cx.listener(|_, _, _, cx| cx.emit(MetricsEvent::Open)))
    }
}
