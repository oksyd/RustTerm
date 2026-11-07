use super::*;
use gpui::Timer;

const VISIBILITY_INTERVAL: Duration = Duration::from_millis(500);
const DISK_INTERVAL: Duration = Duration::from_secs(30);
const RETRY_INTERVAL: Duration = Duration::from_secs(30);

enum Sample {
    Host(ResourceSnapshot),
    Container(Stats),
}

impl ResourceMonitor {
    pub(super) fn start_sampling(&mut self, cx: &mut Context<Self>) {
        let source = self.source.clone();
        let interests = self.interests.clone();
        let window = self.window_state.clone();
        self.task = Some(cx.spawn(async move |this, cx| {
            let visible = || {
                interests.demand() > 0
                    && window.visibility.is_visible()
                    && !window.is_locked()
            };
            let mut collecting = false;
            let mut last_attempt = None::<Instant>;
            let mut last_disk = None::<Instant>;
            let mut failed = false;
            loop {
                if !source.connected() {
                    let _ = this.update(cx, |this, cx| {
                        this.set_state(ResourceState::Disconnected, cx);
                    });
                    break;
                }
                if !visible() {
                    if collecting {
                        collecting = false;
                        if this.update(cx, |this, cx| {
                            this.reset_rates();
                            this.set_state(ResourceState::Paused, cx);
                        }).is_err() {
                            break;
                        }
                        tokio::select! {
                            _ = source.release_idle() => {},
                            _ = interests.wake.notified() => {},
                            _ = Timer::after(VISIBILITY_INTERVAL) => {},
                        }
                    }
                    // No timer for an unused monitor. Hidden windows are checked
                    // at low frequency so they can resume even on unsupported backends.
                    if !visible() {
                        tokio::select! {
                            _ = interests.wake.notified() => {},
                            _ = Timer::after(VISIBILITY_INTERVAL), if interests.demand() > 0 => {},
                        }
                    }
                    continue;
                }
                if !collecting {
                    collecting = true;
                    last_attempt = None;
                    failed = false;
                    if this.update(cx, |this, cx| {
                        this.reset_rates();
                        this.set_state(ResourceState::Loading, cx);
                    }).is_err() {
                        break;
                    }
                }
                let include_disks = matches!(source, Source::Host(_))
                    && interests.demand() > 1
                    && last_disk.is_none_or(|at| at.elapsed() >= DISK_INTERVAL);
                let interval = if failed { RETRY_INTERVAL } else { SAMPLE_INTERVAL };
                if last_attempt.is_none_or(|at| at.elapsed() >= interval)
                    || (include_disks && !failed && last_disk.is_none())
                {
                    last_attempt = Some(Instant::now());
                    let sample = async {
                        match &source {
                            Source::Host(client) => client.sample(include_disks).await
                                .map(Sample::Host).map_err(|error| error.to_string()),
                            Source::Container(client, id) => client.stats(id).await
                                .map(Sample::Container).map_err(|error| error.to_string()),
                        }
                    };
                    futures_util::pin_mut!(sample);
                    let result = loop {
                        tokio::select! {
                            result = &mut sample => break Some(result),
                            _ = interests.wake.notified() => {
                                if !visible() { break None; }
                            },
                            _ = Timer::after(VISIBILITY_INTERVAL) => {
                                if !visible() { break None; }
                                let _ = this.update(cx, |this, cx| {
                                    this.set_state(freshness(this.state, this.sampled_at, Instant::now()), cx);
                                });
                            },
                        }
                    };
                    // Dropping an unfinished sample closes its SSH channel only.
                    let Some(result) = result else { continue; };
                    if !visible() { continue; }
                    failed = result.is_err();
                    if result.is_ok() && include_disks {
                        last_disk = Some(Instant::now());
                    }
                    if this.update(cx, |this, cx| {
                        match result {
                            Ok(Sample::Host(snapshot)) => this.accept_host(snapshot, cx),
                            Ok(Sample::Container(stats)) => this.accept_container(stats, Instant::now(), cx),
                            Err(error) => {
                                this.reset_rates();
                                this.error = Some(error);
                                this.set_state(if source.connected() { ResourceState::Failed } else { ResourceState::Disconnected }, cx);
                                cx.notify();
                            },
                        }
                    }).is_err() {
                        break;
                    }
                }
                tokio::select! {
                    _ = interests.wake.notified() => {},
                    _ = Timer::after(VISIBILITY_INTERVAL) => {},
                }
            }
        }));
    }
}
