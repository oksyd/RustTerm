use super::{
    ActorCommand, FailureKind, SessionState, actor,
    state::Shared,
    transport::{
        OUTPUT_CAPACITY, Output, Transport, WRITE_CAPACITY, WRITE_STALL_TIMEOUT, WriteCommand,
        write_all_cancellable,
    },
};
use crate::{
    docker::{DockerClient, Shell},
    terminal::Terminal,
};
use std::{sync::Arc, time::Duration};
use tokio::{
    io::AsyncReadExt,
    sync::{mpsc, oneshot, watch},
};

#[allow(clippy::too_many_arguments)]
pub(super) async fn run(
    client: DockerClient,
    id: String,
    shell: Shell,
    terminal: Terminal,
    commands: mpsc::Receiver<ActorCommand>,
    shared: Arc<Shared>,
    columns: u16,
    rows: u16,
) {
    let result = tokio::select! {
        _=shared.io_stop.cancelled()=>Err(crate::docker::Error::Cancelled),
        result=client.console(id, shell, columns, rows)=>result,
    };
    let transport = match result {
        Ok(console) => {
            let exec_id = console.id.clone();
            let (mut reader, mut writer) = tokio::io::split(console);
            let (out, output) = mpsc::channel(OUTPUT_CAPACITY);
            let (input, mut writes) = mpsc::channel(WRITE_CAPACITY);
            let (exit_send, exit) = oneshot::channel();
            let (size, mut sizes) = watch::channel((columns, rows));
            let read_shared = shared.clone();
            let read_out = out.clone();
            let read_client = client.clone();
            let read_id = exec_id.clone();
            let read = tokio::spawn(async move {
                let mut buffer = [0u8; 8192];
                loop {
                    let result = tokio::select! { _=read_shared.io_stop.cancelled()=>break, result=reader.read(&mut buffer)=>result };
                    match result {
                        Ok(0) => break,
                        Ok(n) => {
                            if read_out
                                .send(Output::Data(buffer[..n].to_vec()))
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                        Err(error) => {
                            let _ = read_out
                                .send(Output::Error(FailureKind::Read, error.to_string()))
                                .await;
                            break;
                        }
                    }
                }
                drop(reader);
                let code = tokio::select! { _=read_shared.io_stop.cancelled()=>Ok(None), result=tokio::time::timeout(Duration::from_secs(3),read_client.exec_exit(&read_id))=>match result { Ok(Ok(code))=>Ok(code), Ok(Err(error))=>Err(error.to_string()), Err(_)=>Ok(None) } };
                let _ = exit_send.send(code);
                let _ = read_out.send(Output::Eof).await;
            });
            let write_shared = shared.clone();
            let write_out = out.clone();
            let write = tokio::spawn(async move {
                loop {
                    let command = tokio::select! { _=write_shared.io_stop.cancelled()=>break, command=writes.recv()=>command };
                    let Some(command) = command else { break };
                    let tracked = matches!(&command, WriteCommand::UserInput(_));
                    match command {
                        WriteCommand::Data(bytes) | WriteCommand::UserInput(bytes) => {
                            if let Err(error) = write_all_cancellable(
                                &mut writer,
                                &bytes,
                                &write_shared.io_stop,
                                WRITE_STALL_TIMEOUT,
                            )
                            .await
                            {
                                let _ = write_out
                                    .send(Output::Error(FailureKind::Write, error.to_string()))
                                    .await;
                                break;
                            }
                            if tracked && !write_shared.io_stop.is_cancelled() {
                                write_shared.input_completed();
                            }
                        }
                        WriteCommand::Resize(columns, rows) => {
                            size.send_replace((columns, rows));
                        }
                    }
                }
            });
            let resize_shared = shared.clone();
            let resize = tokio::spawn(async move {
                // Send the initial size too, for engines predating ConsoleSize on exec creation.
                loop {
                    let (columns, rows) = *sizes.borrow_and_update();
                    let result = tokio::select! { _=resize_shared.io_stop.cancelled()=>break, result=client.resize_exec(&exec_id,columns,rows)=>result };
                    if let Err(error) = result {
                        let _ = out
                            .send(Output::Error(FailureKind::Resize, error.to_string()))
                            .await;
                        break;
                    }
                    tokio::select! { _=resize_shared.io_stop.cancelled()=>break, result=sizes.changed()=>if result.is_err() { break; } }
                }
            });
            shared.live_state(SessionState::Connected);
            Some(Transport {
                output,
                input,
                exit,
                tasks: vec![read, write, resize],
                reaper: None,
            })
        }
        Err(error) => {
            if shared.io_stop.is_cancelled() {
                shared.state(SessionState::Disconnected { exit_code: None });
            } else {
                shared.failed(FailureKind::Protocol, error.to_string());
            }
            None
        }
    };
    actor::run(terminal, commands, shared, transport).await;
}
