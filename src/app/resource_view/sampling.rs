use super::{ResourceState, ResourceView};
use gpui::{Context, Timer};
use std::time::{Duration, Instant};

const SAMPLE_INTERVAL: Duration = Duration::from_secs(3);
const DISK_INTERVAL: Duration = Duration::from_secs(30);
const VISIBILITY_INTERVAL: Duration = Duration::from_secs(1);

impl ResourceView {
    pub(super) fn start_sampling(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        self.set_state(ResourceState::Loading, cx);
        let client = self.client.clone();
        let window_state = self.window_state.clone();
        self.task = Some(cx.spawn(async move |this, cx| {
            let visible = || window_state.visibility.is_visible() && !window_state.is_locked();
            let mut last_disk_attempt = None::<Instant>;
            let mut paused = false;
            loop {
                if !client.is_connected() {
                    let _ = this.update(cx, |this, cx| {
                        this.set_state(ResourceState::Disconnected, cx)
                    });
                    break;
                }
                if !visible() {
                    if !paused {
                        paused = true;
                        client.reset_rates();
                        if this
                            .update(cx, |this, cx| {
                                this.break_trend();
                                this.set_state(ResourceState::Paused, cx);
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                    Timer::after(VISIBILITY_INTERVAL).await;
                    continue;
                }
                if paused {
                    paused = false;
                    if this
                        .update(cx, |this, cx| this.set_state(ResourceState::Loading, cx))
                        .is_err()
                    {
                        break;
                    }
                }
                let include_disks =
                    last_disk_attempt.is_none_or(|at| at.elapsed() >= DISK_INTERVAL);
                let sample = client.sample(include_disks);
                futures_util::pin_mut!(sample);
                let result = loop {
                    tokio::select! {
                        result = &mut sample => break Some(result),
                        _ = Timer::after(VISIBILITY_INTERVAL) => {
                            if !visible() { break None; }
                        }
                    }
                };
                let Some(result) = result else {
                    continue;
                };
                if !visible() {
                    continue;
                }
                let succeeded = result.is_ok();
                if succeeded && include_disks {
                    last_disk_attempt = Some(Instant::now());
                }
                if this
                    .update(cx, |this, cx| match result {
                        Ok(snapshot) => this.accept_sample(snapshot, cx),
                        Err(error) => {
                            this.client.reset_rates();
                            this.error = Some(error.to_string());
                            this.state = if this.client.is_connected() {
                                ResourceState::Failed
                            } else {
                                ResourceState::Disconnected
                            };
                            cx.notify();
                        }
                    })
                    .is_err()
                    || !succeeded
                {
                    break;
                }
                let next = Instant::now();
                while next.elapsed() < SAMPLE_INTERVAL {
                    Timer::after(
                        VISIBILITY_INTERVAL.min(SAMPLE_INTERVAL.saturating_sub(next.elapsed())),
                    )
                    .await;
                    if !visible() {
                        break;
                    }
                }
            }
        }));
    }
}
