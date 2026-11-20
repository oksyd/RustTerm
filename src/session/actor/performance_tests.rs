//! Opt-in synthetic measurements; no remote hosts, credentials or output files.
use super::*;
use crate::session::Session;
use std::time::Instant;
use tokio::sync::oneshot;

#[tokio::test(flavor = "current_thread")]
#[ignore = "synthetic input-queue and snapshot latency measurement"]
async fn terminal_load_latency() {
    let line =
        "\x1b[32mINFO\x1b[0m request=42 中文😀 e\u{301} https://example.invalid/ status=ok\r\n";
    let data = line.repeat(800).into_bytes();
    for count in [1, 4] {
        let mut sessions = Vec::new();
        let mut workers = Vec::new();
        for _ in 0..count {
            let (session, mut terminal, commands) = Session::create(120, 40, 2000).unwrap();
            terminal.feed(&data);
            terminal.start_search("request".into(), true);
            while !terminal.search_step(256).complete {}
            let _ = terminal.snapshot();
            session.shared.transition(LifecycleEvent::Connected);
            let (output, receive) = mpsc::channel(2);
            let (input, writes) = mpsc::channel(8);
            let (exit, exited) = oneshot::channel();
            workers.push(tokio::spawn(run(
                terminal,
                commands,
                session.shared.clone(),
                Some(Transport {
                    output: receive,
                    input,
                    exit: exited,
                    tasks: vec![],
                    reaper: None,
                }),
            )));
            let data = data.clone();
            workers.push(tokio::spawn(async move {
                let _exit = exit;
                while output.send(Output::Data(data.clone())).await.is_ok() {}
            }));
            sessions.push((session, writes));
        }
        let mut input_ms = Vec::new();
        let mut snapshot_ms = Vec::new();
        let started = Instant::now();
        for _ in 0..100 {
            for (session, writes) in &mut sessions {
                let start = Instant::now();
                session.send(Command::Input(vec![b'x'])).unwrap();
                let write = tokio::time::timeout(Duration::from_secs(5), writes.recv())
                    .await
                    .unwrap()
                    .unwrap();
                assert!(matches!(write, WriteCommand::UserInput(bytes) if bytes == b"x"));
                input_ms.push(start.elapsed().as_secs_f64() * 1000.);
                let mut snapshots = session.subscribe_snapshots();
                snapshots.borrow_and_update();
                let start = Instant::now();
                assert!(session.request_snapshot());
                tokio::time::timeout(Duration::from_secs(5), snapshots.changed())
                    .await
                    .unwrap()
                    .unwrap();
                snapshot_ms.push(start.elapsed().as_secs_f64() * 1000.);
            }
        }
        for (name, values) in [("input", &mut input_ms), ("snapshot", &mut snapshot_ms)] {
            values.sort_by(f64::total_cmp);
            println!(
                "sessions={count} {name}_ms p50={:.2} p95={:.2} p99={:.2}",
                values[values.len() / 2],
                values[(values.len() - 1) * 95 / 100],
                values[(values.len() - 1) * 99 / 100]
            );
        }
        println!(
            "sessions={count} elapsed_s={:.2}",
            started.elapsed().as_secs_f64()
        );
        for (session, _) in sessions {
            session.close();
        }
        for worker in workers {
            worker.abort();
            let _ = worker.await;
        }
    }
}
