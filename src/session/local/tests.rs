use super::*;
use crate::session::MAX_INPUT_BYTES;
use rustix::process::{WaitOptions, waitpid};
use std::collections::BTreeMap;
use std::time::Duration;

async fn open_script(script: &str) -> LocalPty {
    let options = LocalOptions {
        shell: "/bin/sh".into(),
        args: vec!["-c".into(), script.into()],
        cwd: None,
        env: BTreeMap::new(),
    };
    tokio::task::spawn_blocking(move || open_local(options, 80, 24))
        .await
        .unwrap()
        .unwrap()
}

async fn wait_ready(output: &mut mpsc::Receiver<Output>) {
    tokio::time::timeout(Duration::from_secs(2), async {
        let mut bytes = Vec::new();
        loop {
            match output.recv().await {
                Some(Output::Data(data)) => {
                    bytes.extend(data);
                    if bytes.windows(5).any(|part| part == b"ready") {
                        break;
                    }
                }
                other => panic!("shell exited before readiness: {other:?}"),
            }
        }
    })
    .await
    .unwrap();
}

fn assert_reaped(pid: Pid) {
    assert_eq!(
        waitpid(Some(pid), WaitOptions::NOHANG).unwrap_err(),
        rustix::io::Errno::CHILD
    );
}

#[tokio::test]
async fn natural_exit_retains_tail_output_and_reaps_child() {
    let mut terminal = Terminal::new(80, 24, 10);
    let shared = Shared::new(&terminal);
    let _stop = shared.io_stop.clone().drop_guard();
    let pty = open_script("printf 'tail-before-exit'; exit 7").await;
    let pid =
        Pid::from_raw(pty.process.child.as_ref().unwrap().process_id().unwrap() as i32).unwrap();
    let mut transport = start_transport(pty, shared.clone());
    let status = tokio::time::timeout(Duration::from_secs(2), &mut transport.exit)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(status, Some(7));
    let bytes = tokio::time::timeout(Duration::from_secs(2), async {
        let mut bytes = Vec::new();
        loop {
            match transport.output.recv().await {
                Some(Output::Data(data)) => bytes.extend(data),
                Some(Output::Eof) => return bytes,
                other => panic!("unexpected PTY output: {other:?}"),
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(bytes, b"tail-before-exit");
    transport
        .shutdown_filtered(&shared, true, &mut terminal, false)
        .await
        .unwrap();
    assert_reaped(pid);
}

#[tokio::test]
async fn cancellation_terminates_and_reaps_a_running_child() {
    let mut terminal = Terminal::new(80, 24, 10);
    let shared = Shared::new(&terminal);
    let _stop = shared.io_stop.clone().drop_guard();
    let pty = open_script("printf ready; exec sleep 60").await;
    let pid =
        Pid::from_raw(pty.process.child.as_ref().unwrap().process_id().unwrap() as i32).unwrap();
    let mut transport = start_transport(pty, shared.clone());
    wait_ready(&mut transport.output).await;
    assert!(waitpid(Some(pid), WaitOptions::NOHANG).unwrap().is_none());
    transport
        .shutdown_filtered(&shared, false, &mut terminal, false)
        .await
        .unwrap();
    assert_reaped(pid);
}

#[tokio::test]
async fn blocked_writer_observes_cancellation() {
    let terminal = Terminal::new(80, 24, 10);
    let shared = Shared::new(&terminal);
    let _stop = shared.io_stop.clone().drop_guard();
    let LocalPty {
        reader,
        writer,
        pidfd,
        process,
    } = open_script("stty raw -echo; printf ready; exec sleep 60").await;
    let (output_tx, mut output) = mpsc::channel(1);
    let reader = tokio::spawn(read_output(reader, output_tx, shared.clone()));
    let (exit_tx, exit) = oneshot::channel();
    let reaper = tokio::spawn(wait_process(process, pidfd, exit_tx, shared.clone()));
    wait_ready(&mut output).await;
    let bytes = [b'x'; MAX_INPUT_BYTES];
    let write = write_cancellable(&writer, &bytes, &shared);
    tokio::pin!(write);
    tokio::select! {
        result = &mut write => panic!("writer should be blocked by unread PTY input: {result:?}"),
        _ = tokio::time::sleep(Duration::from_millis(20)) => {},
    }
    shared.io_stop.cancel();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), &mut write)
            .await
            .unwrap()
            .unwrap_err()
            .kind(),
        io::ErrorKind::ConnectionAborted
    );
    drop(output);
    reader.await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), reaper)
        .await
        .unwrap()
        .unwrap();
    assert!(exit.await.unwrap().is_ok());
}
