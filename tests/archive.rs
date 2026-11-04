#![cfg(unix)]
use rustterm::session::{Command, LocalOptions, SelectionKind, Session, SessionState};
use std::{
    thread,
    time::{Duration, Instant},
};
fn spawn(script: &str) -> Session {
    Session::spawn_local(
        LocalOptions {
            shell: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            ..Default::default()
        },
        80,
        24,
        500,
    )
    .unwrap()
}
fn until(mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !f() {
        assert!(Instant::now() < deadline, "archive timeout");
        thread::sleep(Duration::from_millis(10));
    }
}
fn refresh(session: &Session) {
    let count = session.snapshot_count();
    until(|| session.request_snapshot());
    until(|| session.snapshot_count() > count);
}
fn text(session: &Session) -> String {
    session
        .snapshot()
        .lines
        .iter()
        .map(|line| {
            line.cells
                .iter()
                .map(|cell| cell.text.as_str())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}
#[test]
fn disconnected_history_supports_search_copy_and_metadata() {
    let session = spawn("printf 'hello 中文\\nERROR final\\n'; IFS= read -r line");
    until(|| session.state() == SessionState::Connected);
    until(|| {
        refresh(&session);
        text(&session).contains("ERROR final")
    });
    session.disconnect();
    until(|| session.is_read_only());
    assert!(session.ended_at().is_some());
    assert!(session.ended_at().unwrap() >= session.created_at());
    assert!(!session.id().is_nil());
    session
        .send(Command::BeginSelection {
            column: 0,
            row: 0,
            kind: SelectionKind::Line,
        })
        .unwrap();
    let mut receipt = session.copy_selection().unwrap();
    let mut copied = None;
    until(|| {
        match receipt.try_recv() {
            Ok(text) => copied = Some(text),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {}
            Err(error) => panic!("selection copy failed: {error}"),
        }
        copied.is_some()
    });
    assert!(copied.flatten().unwrap().contains("hello 中文"));
    session
        .send(Command::Search {
            query: "ERROR".into(),
            case_sensitive: true,
        })
        .unwrap();
    until(|| {
        refresh(&session);
        let s = &session.snapshot().search;
        s.complete && s.matches == 1
    });
    session
        .send(Command::NextMatch { backwards: false })
        .unwrap();
    until(|| {
        refresh(&session);
        session
            .snapshot()
            .lines
            .iter()
            .flat_map(|line| &line.cells)
            .any(|cell| cell.search_highlight.is_some())
    });
    assert!(session.send(Command::Paste("never sent".into())).is_err());
}
#[test]
fn disconnected_alternate_screen_keeps_frozen_final_view_and_main_history() {
    let session =
        spawn("printf 'main-history\\r\\n\\033[?1049hfinal-fullscreen'; IFS= read -r line");
    until(|| session.state() == SessionState::Connected);
    until(|| {
        refresh(&session);
        session.snapshot().alternate_screen
    });
    session.disconnect();
    until(|| session.is_read_only());
    refresh(&session);
    assert!(text(&session).contains("main-history"));
    let final_screen = session.final_alt_snapshot().unwrap();
    assert!(
        final_screen
            .lines
            .iter()
            .flat_map(|line| &line.cells)
            .map(|cell| cell.text.as_str())
            .collect::<String>()
            .contains("final-fullscreen")
    );
    assert!(!final_screen.cursor.visible);
}
#[test]
#[ignore = "manual resource probe; run explicitly on a test machine"]
fn concurrent_sessions_1_4_16_50_drain_and_release_without_hidden_views() {
    for count in [1, 4, 16, 50] {
        let sessions:Vec<_>=(0..count).map(|index|{
            let session=spawn(&format!("i=0; while [ $i -lt 100 ]; do printf 'session{index}:%s\\n' $i; i=$((i+1)); done"));
            session.set_visible(index==0);session
        }).collect();
        for (index, session) in sessions.iter().enumerate() {
            until(|| session.is_read_only());
            assert!(
                !session.is_evicted(),
                "unexpected archive eviction at {count} sessions"
            );
            assert_eq!(
                session.snapshot_count(),
                1,
                "background generated snapshots"
            );
            refresh(session);
            assert!(
                text(session).contains(&format!("session{index}:99")),
                "{}",
                text(session)
            );
            session.close();
        }
    }
}
#[test]
fn archive_base_is_charged_and_zero_budget_releases_whole_archive() {
    let session = spawn("printf 'retained output'");
    until(|| session.is_read_only());
    assert!(!session.is_evicted());
    session
        .send(Command::SetHistoryBudget { lines: 0, bytes: 0 })
        .unwrap();
    until(|| session.is_evicted());
    assert!(session.send(Command::Scroll(1)).is_err());
    assert_eq!(session.snapshot().columns, 2);
    assert!(session.final_alt_snapshot().is_none());
}

#[test]
fn resizing_frozen_grid_recomputes_its_budget_and_evicts_when_it_no_longer_fits() {
    let session = spawn("printf 'archive'");
    until(|| session.is_read_only());
    let mut reference = rustterm::terminal::Terminal::new(80, 24, 0);
    reference.freeze();
    let budget = reference.retained_base_bytes() + 512 * 1024;
    session
        .send(Command::SetHistoryBudget {
            lines: 100,
            bytes: budget,
        })
        .unwrap();
    refresh(&session);
    assert!(!session.is_evicted());
    session
        .send(Command::Resize {
            columns: 120,
            rows: 40,
        })
        .unwrap();
    until(|| session.is_evicted());
    assert_eq!(session.allocated_history_budget(), 0);
}
