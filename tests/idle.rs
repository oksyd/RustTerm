#![cfg(unix)]

use rustterm::session::{LocalOptions, Session, SessionState};
use std::{
    thread,
    time::{Duration, Instant},
};

fn until(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !ready() {
        assert!(Instant::now() < deadline, "idle session did not settle");
        thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn no_search_means_idle_active_and_archived_sessions_do_not_publish_revisions() {
    let session = Session::spawn_local(
        LocalOptions {
            shell: "/bin/sh".into(),
            args: vec!["-c".into(), "IFS= read -r line".into()],
            ..Default::default()
        },
        80,
        24,
        100,
    )
    .unwrap();
    until(|| session.state() == SessionState::Connected);
    for archived in [false, true] {
        if archived {
            session.disconnect();
            until(|| session.is_read_only());
        }
        // Sample the idle grid after observing startup/freeze completion.
        let published = session.snapshot_count();
        until(|| session.request_snapshot());
        until(|| session.snapshot_count() > published);
        let revision = session.revision();
        let snapshots = session.snapshot_count();
        thread::sleep(Duration::from_millis(100));
        assert_eq!(session.revision(), revision, "empty search scheduled work");
        assert_eq!(
            session.snapshot_count(),
            snapshots,
            "idle session generated a view"
        );
    }
    session.close();
}
