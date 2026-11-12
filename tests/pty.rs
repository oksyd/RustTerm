//! These tests execute real PTYs rather than mocking a byte stream.
#![cfg(unix)]

#[path = "pty/shell_interaction.rs"]
mod shell_interaction;

use rustterm::session::{Command, LocalOptions, SendError, Session, SessionState};
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
        100,
    )
    .expect("create session")
}

#[track_caller]
fn wait_until(mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(8);
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting for PTY");
        thread::sleep(Duration::from_millis(10));
    }
}

fn snapshot_text(session: &Session) -> String {
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

fn fresh_text(session: &Session) -> String {
    let count = session.snapshot_count();
    wait_until(|| session.request_snapshot());
    wait_until(|| session.snapshot_count() > count);
    snapshot_text(session)
}

#[test]
fn real_shell_round_trip_and_exit_preserves_tail() {
    let session = spawn(
        "printf 'READY\\n'; IFS= read -r line; printf 'reply:%s\\ntail-marker\\n' \"$line\"; exit 7",
    );
    wait_until(|| session.state() == SessionState::Connected);
    session.send(Command::Input(b"hello\r".to_vec())).unwrap();
    wait_until(|| {
        matches!(
            session.state(),
            SessionState::Disconnected { exit_code: Some(7) }
        )
    });
    let text = fresh_text(&session);
    assert!(text.contains("reply:hello"), "{text}");
    assert!(text.contains("tail-marker"), "{text}");
}

#[test]
fn resize_reaches_child() {
    let session = spawn("IFS= read -r line; stty size; exit 0");
    wait_until(|| session.state() == SessionState::Connected);
    session
        .send(Command::Resize {
            columns: 111,
            rows: 31,
        })
        .unwrap();
    session.send(Command::Input(b"\r".to_vec())).unwrap();
    wait_until(|| matches!(session.state(), SessionState::Disconnected { .. }));
    let text = fresh_text(&session);
    assert!(text.contains("31 111"), "{text}");
    assert_eq!(session.snapshot().columns, 111);
    assert_eq!(session.snapshot().rows, 31);
}

#[test]
fn protocol_response_round_trip() {
    let session = spawn(
        "stty -echo -icanon min 1 time 0; printf '\\033[6n'; reply=$(dd bs=1 count=6 2>/dev/null | od -An -tx1); printf 'response:%s\\n' \"$reply\"",
    );
    wait_until(|| matches!(session.state(), SessionState::Disconnected { .. }));
    let text = fresh_text(&session);
    assert!(
        text.contains("1b 5b 31 3b 31 52"),
        "DSR response missing: {text}"
    );
}

#[test]
fn close_is_nonblocking_and_reaps_child() {
    let session = spawn("trap '' HUP; printf 'PID=%s\\n' \"$$\"; while :; do sleep 1; done");
    wait_until(|| session.state() == SessionState::Connected);
    let mut pid = None;
    wait_until(|| {
        let text = fresh_text(&session);
        pid = text.lines().find_map(|line| {
            line.trim()
                .strip_prefix("PID=")
                .and_then(|value| value.trim().parse::<i32>().ok())
        });
        pid.is_some()
    });
    let start = Instant::now();
    session.close();
    assert!(start.elapsed() < Duration::from_millis(100));
    wait_until(|| matches!(session.state(), SessionState::Disconnected { .. }));
    // SAFETY: signal 0 only tests existence; it sends no signal. The direct
    // child must be reaped before the actor publishes Disconnected.
    assert_eq!(unsafe { libc::kill(pid.unwrap(), 0) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
    assert_eq!(
        session.send(Command::Input(vec![1])),
        Err(SendError::Closed)
    );
}

#[test]
fn hidden_session_does_not_generate_snapshots() {
    let session =
        spawn("i=0; while [ \"$i\" -lt 300 ]; do printf 'line %s\\n' \"$i\"; i=$((i+1)); done");
    let initial = session.snapshot_count();
    wait_until(|| matches!(session.state(), SessionState::Disconnected { .. }));
    assert!(session.revision() > 2);
    assert_eq!(
        session.snapshot_count(),
        initial,
        "hidden output generated views"
    );
    let text = fresh_text(&session);
    assert!(text.contains("line 299"), "{text}");
    assert!(session.snapshot().history_size <= 100);
}

#[test]
fn invalid_executable_is_reported_without_blocking_caller() {
    let session = Session::spawn_local(
        LocalOptions {
            shell: "/does-not-exist/rustterm-shell".into(),
            ..Default::default()
        },
        80,
        24,
        100,
    )
    .unwrap();
    wait_until(|| matches!(session.state(), SessionState::Failed { .. }));
}

#[test]
fn admission_rejects_oversized_input_and_invalid_dimensions() {
    let session = spawn("IFS= read -r line");
    wait_until(|| session.state() == SessionState::Connected);
    assert_eq!(
        session.send(Command::Input(vec![0; 65537])),
        Err(SendError::TooLarge)
    );
    assert_eq!(
        session.send(Command::Resize {
            columns: 0,
            rows: 24
        }),
        Err(SendError::InvalidSize)
    );
    session.close();
    wait_until(|| matches!(session.state(), SessionState::Disconnected { .. }));
}

#[test]
fn cancellation_interrupts_backpressured_input() {
    let session = spawn("stty -echo -icanon; printf 'READY\\n'; exec sleep 60");
    wait_until(|| session.state() == SessionState::Connected);
    wait_until(|| fresh_text(&session).contains("READY"));
    let mut accepted = 0;
    for _ in 0..100 {
        match session.send(Command::Input(vec![b'x'; 65536])) {
            Ok(()) => accepted += 1,
            Err(SendError::Busy) => break,
            result => panic!("unexpected admission result: {result:?}"),
        }
    }
    assert!(accepted > 0);
    thread::sleep(Duration::from_millis(50));
    let start = Instant::now();
    session.close();
    assert!(start.elapsed() < Duration::from_millis(100));
    wait_until(|| matches!(session.state(), SessionState::Disconnected { .. }));
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "cancellation waited for I/O timeout"
    );
}

#[test]
fn four_sessions_keep_independent_grids_and_exit_statuses() {
    let sessions: Vec<_> = (0..4).map(|index| spawn(&format!(
        "i=0; while [ \"$i\" -lt 1000 ]; do printf 'pane{index}:%s\\n' \"$i\"; i=$((i+1)); done; exit {index}"
    ))).collect();
    for (index, session) in sessions.iter().enumerate() {
        wait_until(|| {
            session.state()
                == SessionState::Disconnected {
                    exit_code: Some(index as u32),
                }
        });
        assert_eq!(session.snapshot_count(), 1);
        let text = fresh_text(session);
        assert!(text.contains(&format!("pane{index}:999")), "{text}");
        for other in 0..4 {
            if other != index {
                assert!(!text.contains(&format!("pane{other}:")));
            }
        }
    }
}

#[test]
fn close_during_startup_still_reaps_created_process() {
    let session = spawn("exec sleep 60");
    session.close();
    wait_until(|| matches!(session.state(), SessionState::Disconnected { .. }));
}
