use super::*;
#[test]
fn shared_interests_pause_only_after_the_last_consumer_stops() {
    let interests = Interests::default();
    let status = interests.subscribe();
    let panel = interests.subscribe();
    assert_eq!(interests.demand(), 0);
    status.set(true, false);
    assert_eq!(interests.demand(), 1);
    panel.set(true, true);
    assert_eq!(interests.demand(), 2);
    status.set(false, false);
    assert_eq!(interests.demand(), 2);
    drop(panel);
    assert_eq!(interests.demand(), 0);
    status.set(true, false);
    assert_eq!(interests.demand(), 1);
    drop(status);
    assert_eq!(interests.demand(), 0);
}
#[test]
fn freshness_never_labels_missing_or_expired_samples_live() {
    let now = Instant::now();
    assert_eq!(
        freshness(ResourceState::Live, None, now),
        ResourceState::Stale
    );
    assert_eq!(
        freshness(ResourceState::Live, Some(now), now + STALE_AFTER),
        ResourceState::Stale
    );
    assert_eq!(
        freshness(
            ResourceState::Live,
            Some(now),
            now + STALE_AFTER - Duration::from_nanos(1)
        ),
        ResourceState::Live
    );
    for state in [
        ResourceState::Paused,
        ResourceState::Failed,
        ResourceState::Disconnected,
        ResourceState::Loading,
    ] {
        assert_eq!(freshness(state, Some(now), now + STALE_AFTER), state);
    }
}
#[test]
fn container_rates_require_comparable_counters_and_interfaces() {
    let stats = |rx, tx| {
        serde_json::from_value::<Stats>(
            serde_json::json!({"networks":{"eth0":{"rx_bytes":rx,"tx_bytes":tx}}}),
        )
        .unwrap()
    };
    let a = stats(100, 200);
    let b = stats(300, 400);
    assert_eq!(
        container_rates(&b, &a, Duration::from_secs(5)),
        (Some(40.), Some(40.))
    );
    assert_eq!(
        container_rates(&a, &b, Duration::from_secs(5)),
        (None, None)
    );
    assert_eq!(container_rates(&b, &a, Duration::ZERO), (None, None));
    let mut changed = b.clone();
    changed.networks.insert("eth1".into(), Default::default());
    assert_eq!(
        container_rates(&changed, &a, Duration::from_secs(5)),
        (None, None)
    );
}
#[test]
fn unknown_memory_limit_does_not_display_a_false_percentage() {
    let mut summary = ResourceSummary {
        memory_used: Some(1),
        ..Default::default()
    };
    assert_eq!(summary.memory_percent(), None);
    summary.memory_total = Some(0);
    assert_eq!(summary.memory_percent(), None);
    summary.memory_total = Some(4);
    assert_eq!(summary.memory_percent(), Some(25.));
}
