use crate::i18n::messages as i18n_key;

use super::{
    ActorCommand, COMMAND_CAPACITY, Command, FailureKind, SessionState, budget, is_user_input,
    state::Shared,
    transport::{Output, SHUTDOWN_TIMEOUT, Transport, WriteCommand},
    zmodem,
};
use crate::terminal::{MouseEvent, Terminal};
use std::{
    sync::{Arc, atomic::Ordering},
    time::{Duration, SystemTime},
};
use tokio::sync::mpsc;

mod outbox;
use outbox::Outbox;
#[cfg(test)]
mod backpressure_tests;

const POLL_INTERVAL: Duration = Duration::from_millis(10);
const EXIT_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

pub(super) async fn run(
    mut terminal: Terminal,
    mut commands: mpsc::Receiver<ActorCommand>,
    shared: Arc<Shared>,
    transport: Option<Transport>,
) {
    terminal.set_history_budget(
        shared
            .allocated_budget
            .load(Ordering::Acquire)
            .saturating_sub(shared.retained_base.load(Ordering::Acquire)),
    );
    if let Some(mut transport) = transport {
        let mut transfer = zmodem::Transfer::new(shared.clone(), transport.input.clone());
        let mut deferred_input = std::collections::VecDeque::new();
        let mut deferred_resize = None;
        let mut outbox = Outbox::default();
        let mut read_eof = false;
        let mut exit_received = false;
        let mut exit_code = None;
        let mut drain_deadline = None;
        let mut next_search_step = tokio::time::Instant::now();
        let mut scheduled_query = 0;
        loop {
            if shared.io_stop.is_cancelled() || (read_eof && exit_received && !transfer.active()) {
                break;
            }
            if transfer.active() {
                outbox.quarantine(&mut deferred_resize);
            }
            if transfer.take_discard_deferred() {
                discard_pending_writes(&mut transfer, &mut outbox, &mut deferred_input, &shared);
            }
            let write_pending =
                !transfer.active() && (deferred_resize.is_some() || !outbox.is_empty());
            let search = terminal.search_status();
            if search.query_id != scheduled_query {
                scheduled_query = search.query_id;
                next_search_step = tokio::time::Instant::now();
            }
            let drain =
                drain_deadline.unwrap_or_else(|| tokio::time::Instant::now() + EXIT_DRAIN_TIMEOUT);
            let selection_deadline = transfer.deadline();
            let prefix_deadline = transfer.prefix_deadline();
            let transfer_active = transfer.active();
            tokio::select! {
                _ = shared.io_stop.cancelled() => break,
                permit = transport.input.reserve(), if write_pending => {
                    let Ok(permit) = permit else {
                        shared.failed(FailureKind::Write, crate::i18n::tr(i18n_key::CONNECTIONS_INPUT_COULD_NOT_BE_DELIVERED_AND_THE_CONNECTION_HAS_STOPPED));
                        break;
                    };
                    let write = deferred_resize.take().or_else(|| outbox.pop()).expect("pending terminal write");
                    permit.send(write);
                },
                _ = tokio::time::sleep_until(prefix_deadline.unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_secs(3600))), if prefix_deadline.is_some() => {
                    let bytes = transfer.flush_prefix();
                    if let Err(kind) = feed_output(&bytes, &mut terminal, &shared, &mut outbox) {
                        shared.failed(kind, crate::i18n::tr(i18n_key::CONNECTIONS_CANNOT_DELIVER_THE_TERMINAL_PROTOCOL_RESPONSE));
                        break;
                    }
                },
                _ = tokio::time::sleep_until(selection_deadline.unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_secs(3600))), if selection_deadline.is_some() => {
                    if let Err(kind) = transfer.expire().await {
                        shared.failed(kind, crate::i18n::tr(i18n_key::CONNECTIONS_FAILED_TO_CANCEL_FILE_TRANSFER));
                        break;
                    }
                },
                Some(event) = transfer.events.recv() => {
                    match transfer.event(event).await {
                        Ok(bytes) => {
                            if transfer.take_discard_deferred() {
                                discard_pending_writes(&mut transfer, &mut outbox, &mut deferred_input, &shared);
                            }
                            if let Err(kind) = feed_output(&bytes, &mut terminal, &shared, &mut outbox) {
                                shared.failed(kind, crate::i18n::tr(i18n_key::CONNECTIONS_CANNOT_DELIVER_THE_TERMINAL_PROTOCOL_RESPONSE));
                                break;
                            }
                        },
                        Err(kind) => { shared.failed(kind, crate::i18n::tr(i18n_key::CONNECTIONS_CANNOT_DELIVER_FILE_TRANSFER_DATA)); break; }
                    }
                },
                _ = tokio::time::sleep_until(drain), if drain_deadline.is_some() => {
                    if !(read_eof && exit_received) {
                        shared.failed(FailureKind::Timeout, crate::i18n::tr(i18n_key::CONNECTIONS_PROCESS_EXIT_AND_END_OF_OUTPUT_WERE_NOT_BOTH_CONFIRMED));
                    }
                    break;
                },
                _ = shared.budget_notify.notified() => apply_budget(&mut terminal, &shared),
                _ = shared.snapshot_notify.notified() => publish_snapshot(&mut terminal, &shared),
                _ = tokio::time::sleep_until(next_search_step), if !search.query.is_empty() && !search.complete && !search.paused => {
                    let progress = terminal.search_step(128);
                    next_search_step = tokio::time::Instant::now() + if progress.complete { Duration::from_millis(250) } else { POLL_INTERVAL };
                    shared.changed();
                },
                result = &mut transport.exit, if !exit_received => {
                    exit_received = true;
                    drain_deadline.get_or_insert(tokio::time::Instant::now() + if transfer.active() { SHUTDOWN_TIMEOUT } else { EXIT_DRAIN_TIMEOUT });
                    shared.closing();
                    match result { Ok(Ok(code)) => exit_code = code, Ok(Err(message)) => { shared.failed(FailureKind::Process, message); break; }, Err(_) => { shared.failed(FailureKind::Process, crate::i18n::tr(i18n_key::CONNECTIONS_THE_TRANSPORT_TASK_ENDED_UNEXPECTEDLY)); break; } }
                }
                output = transport.output.recv(), if !read_eof => match output {
                    Some(Output::Data(bytes)) => {
                        match transfer.receive(bytes).await {
                            Ok(bytes) => {
                                if transfer.take_discard_deferred() {
                                    discard_pending_writes(&mut transfer, &mut outbox, &mut deferred_input, &shared);
                                }
                                if let Err(kind) = feed_output(&bytes, &mut terminal, &shared, &mut outbox) {
                                    if !shared.io_stop.is_cancelled() { shared.failed(kind, crate::i18n::tr(i18n_key::CONNECTIONS_CANNOT_DELIVER_THE_PROTOCOL_RESPONSE_THE_CONNECTION_HAS_STOPPED)); }
                                    break;
                                }
                            },
                            Err(kind) => { shared.failed(kind, crate::i18n::tr(i18n_key::CONNECTIONS_CANNOT_DELIVER_THE_FILE_TRANSFER_PROTOCOL_RESPONSE)); break; }
                        }
                    }
                    Some(Output::Eof) | None => {
                        read_eof = true;
                        drain_deadline.get_or_insert(tokio::time::Instant::now() + if transfer.active() { SHUTDOWN_TIMEOUT } else { EXIT_DRAIN_TIMEOUT });
                        shared.closing();
                    },
                    Some(Output::Error(kind,message)) => { shared.failed(kind,message); break; },
                },
                command = async {
                    if !transfer_active && let Some(command) = deferred_input.pop_front() {
                        return Some(command);
                    }
                    commands.recv().await
                }, if transfer_active || outbox.can_accept_command() => {
                    let Some(command) = command else { break; };
                    match command {
                        ActorCommand::ZmodemStart { id, selection } => {
                            if let Err(kind) = transfer.start(id, selection).await {
                                shared.failed(kind, crate::i18n::tr(i18n_key::CONNECTIONS_FILE_TRANSFER_COULD_NOT_START_AND_THE_REMOTE_TRANSFER_COULD));
                                break;
                            }
                            continue;
                        }
                        ActorCommand::ZmodemCancel { id } => {
                            if let Err(kind) = transfer.cancel(id).await {
                                shared.failed(kind, crate::i18n::tr(i18n_key::CONNECTIONS_CANNOT_DELIVER_THE_FILE_TRANSFER_CANCELLATION_MESSAGE));
                                break;
                            }
                            continue;
                        }
                        ActorCommand::User(ref user) if transfer.active() && is_user_input(user) => {
                            // These bytes were admitted before the handshake was seen.
                            // Hold them once, without sending them into the binary protocol.
                            if deferred_input.len() >= COMMAND_CAPACITY {
                                shared.failed(FailureKind::Backpressure, crate::i18n::tr(i18n_key::CONNECTIONS_PENDING_INPUT_EXCEEDED_THE_LIMIT_DURING_FILE_TRANSFER_THE_CONNECTION));
                                break;
                            }
                            deferred_input.push_back(command);
                            continue;
                        }
                        ActorCommand::User(Command::Mouse { .. } | Command::Focus(_)) if transfer.active() => continue,
                        _ => {}
                    }
                    let ephemeral = matches!(&command, ActorCommand::User(Command::Mouse { .. } | Command::Focus(_)));
                    if let Some(write) = command_effect(command, &mut terminal, &shared, !exit_received && !read_eof)
                    {
                        if transfer.active() && matches!(write, WriteCommand::Resize(..)) {
                            deferred_resize = Some(write);
                        } else if let Err(kind) = outbox.push(write, ephemeral) {
                            if !shared.io_stop.is_cancelled() { shared.failed(kind, crate::i18n::tr(i18n_key::CONNECTIONS_INPUT_COULD_NOT_BE_DELIVERED_AND_THE_CONNECTION_HAS_STOPPED)); }
                            break;
                        }
                    }
                }
            }
        }
        let discard_protocol = transfer.active();
        let tail = transfer.shutdown().await;
        if !tail.is_empty() {
            terminal.feed(&tail);
            publish_shell_state(&terminal, &shared);
        }
        drop(transfer);
        match transport
            .shutdown_filtered(&shared, exit_received, &mut terminal, discard_protocol)
            .await
        {
            Ok(Some(code)) => exit_code = Some(code),
            Ok(None) => {}
            Err((kind, message)) => shared.failed(kind, message),
        }
        if !matches!(shared.current_state(), SessionState::Failed { .. }) {
            if let Some((kind, message)) = shared
                .remote_failure
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
            {
                shared.failed(kind, message);
            } else {
                shared.state(SessionState::Disconnected { exit_code });
            }
        }
    }
    publish_shell_state(&terminal, &shared);
    shared.io_stop.cancel();
    if shared.unacknowledged_inputs.load(Ordering::Acquire) > 0 {
        shared.report_input_loss();
    }
    terminal.freeze();
    *shared.ended_at.lock().unwrap_or_else(|e| e.into_inner()) = Some(SystemTime::now());
    *shared
        .final_alternate
        .write()
        .unwrap_or_else(|e| e.into_inner()) = terminal.final_alt_snapshot();
    shared
        .retained_base
        .store(terminal.retained_base_bytes(), Ordering::Release);
    shared.read_only.store(true, Ordering::Release);
    budget::rebalance();
    apply_budget(&mut terminal, &shared);
    shared.changed();
    // Frozen instances keep one authoritative grid. Reconnect creates a new
    // Session; archives cannot receive bytes or emit any transport messages.
    let mut next_search_step = tokio::time::Instant::now();
    let mut scheduled_query = 0;
    loop {
        if shared.actor_stop.is_cancelled() {
            break;
        }
        let search = terminal.search_status();
        if search.query_id != scheduled_query {
            scheduled_query = search.query_id;
            next_search_step = tokio::time::Instant::now();
        }
        tokio::select! {
            _ = shared.actor_stop.cancelled() => break,
            _ = shared.budget_notify.notified() => apply_budget(&mut terminal, &shared),
            _ = shared.snapshot_notify.notified() => publish_snapshot(&mut terminal, &shared),
            _ = tokio::time::sleep_until(next_search_step), if !search.query.is_empty() && !search.complete && !search.paused => {
                    let progress = terminal.search_step(128);
                    next_search_step = tokio::time::Instant::now() + if progress.complete { Duration::from_millis(250) } else { POLL_INTERVAL };
                    shared.changed();
                },
            command = commands.recv() => {
                let Some(command) = command else { break; };
                command_effect(command, &mut terminal, &shared, false);
            }
        }
    }
    shared.snapshot_pending.store(false, Ordering::Release);
}
fn discard_pending_writes(
    transfer: &mut zmodem::Transfer,
    outbox: &mut Outbox,
    deferred_input: &mut std::collections::VecDeque<ActorCommand>,
    shared: &Shared,
) {
    let count = deferred_input.len() + outbox.discard();
    deferred_input.clear();
    for _ in 0..count {
        shared.input_completed();
    }
    transfer.note_discarded_input(count);
}

fn feed_output(
    bytes: &[u8],
    terminal: &mut Terminal,
    shared: &Shared,
    outbox: &mut Outbox,
) -> std::result::Result<(), FailureKind> {
    if bytes.is_empty() {
        return Ok(());
    }
    let replies = terminal.feed(bytes);
    publish_shell_state(terminal, shared);
    shared.changed();
    for reply in replies {
        outbox.reply(reply)?;
    }
    Ok(())
}
fn publish_shell_state(terminal: &Terminal, shared: &Shared) {
    {
        let mut shell = shared
            .shell_state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if shared.actor_stop.is_cancelled() || shared.evicted.load(Ordering::Acquire) {
            return;
        }
        if shell.current_directory.as_deref() != terminal.current_directory() {
            shell.current_directory = terminal.current_directory().map(str::to_owned);
        }
        shell.can_insert_command = terminal.can_insert_command();
    }
    if shared.actor_stop.is_cancelled()
        || shared.io_stop.is_cancelled()
        || shared.read_only.load(Ordering::Acquire)
    {
        return;
    }
    let Some(command) = terminal.last_completed_command() else {
        return;
    };
    let previous_id = shared
        .command_completion
        .borrow()
        .as_ref()
        .map(|command| command.id);
    if previous_id != Some(command.id) {
        let mut history = shared
            .command_history
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if shared.evicted.load(Ordering::Acquire) || shared.actor_stop.is_cancelled() {
            return;
        }
        *history = terminal.command_history();
        shared
            .command_completion
            .send_replace(Some(command.clone()));
    }
}
fn apply_budget(terminal: &mut Terminal, shared: &Shared) {
    terminal.set_history_budget(
        shared
            .allocated_budget
            .load(Ordering::Acquire)
            .saturating_sub(shared.retained_base.load(Ordering::Acquire)),
    );
    shared.changed();
}
fn publish_snapshot(terminal: &mut Terminal, shared: &Shared) {
    let snapshot = Arc::new(terminal.snapshot());
    shared.snapshot.send_if_modified(|cached| {
        // Clear admission before waking the view. Keep the eviction check
        // under the cache lock so a late frame cannot restore it.
        shared.snapshot_pending.store(false, Ordering::Release);
        if shared.evicted.load(Ordering::Acquire) || shared.actor_stop.is_cancelled() {
            return false;
        }
        *cached = snapshot;
        shared.snapshot_count.fetch_add(1, Ordering::Release);
        true
    });
}
fn command_effect(
    command: ActorCommand,
    terminal: &mut Terminal,
    shared: &Shared,
    connected: bool,
) -> Option<WriteCommand> {
    let follows_input = matches!(&command, ActorCommand::User(command) if is_user_input(command));
    let mut write = match command {
        ActorCommand::User(Command::Input(bytes)) if connected => {
            Some(WriteCommand::UserInput(bytes))
        }
        ActorCommand::User(Command::Key {
            key,
            ctrl,
            alt,
            shift,
        }) if connected => Some(WriteCommand::UserInput(
            terminal.encode_key(&key, ctrl, alt, shift),
        )),
        ActorCommand::User(Command::Paste(text)) if connected => {
            Some(WriteCommand::UserInput(terminal.encode_paste(&text)))
        }
        ActorCommand::User(Command::InsertCommandDraft { id }) if connected => {
            Some(WriteCommand::UserInput(terminal.command_draft(id)))
        }
        ActorCommand::User(Command::Mouse {
            button,
            column,
            row,
            pressed,
            motion,
            shift,
            alt,
            ctrl,
        }) if connected => Some(WriteCommand::Data(terminal.encode_mouse(MouseEvent {
            button,
            column,
            row,
            pressed,
            motion,
            shift,
            alt,
            ctrl,
        }))),
        ActorCommand::User(Command::Focus(focused))
            if connected && terminal.modes().focus_reporting =>
        {
            Some(WriteCommand::Data(
                if focused { b"\x1b[I" } else { b"\x1b[O" }.to_vec(),
            ))
        }
        ActorCommand::Resize {
            columns,
            rows,
            receipt,
        } => {
            terminal.resize(columns, rows);
            if shared.read_only.load(Ordering::Acquire) {
                shared
                    .retained_base
                    .store(terminal.retained_base_bytes(), Ordering::Release);
                budget::rebalance();
                apply_budget(terminal, shared);
            }
            receipt.0.store(true, Ordering::Release);
            shared.changed();
            return connected.then_some(WriteCommand::Resize(columns, rows));
        }
        ActorCommand::User(Command::Scroll(lines)) => {
            terminal.scroll(lines);
            None
        }
        ActorCommand::User(Command::AlternateScroll(lines)) if connected => Some(
            WriteCommand::UserInput(terminal.encode_alternate_scroll(lines)),
        ),
        ActorCommand::User(Command::ScrollTo(offset)) => {
            terminal.scroll_to(offset);
            None
        }
        ActorCommand::User(Command::ScrollToBottom) => {
            terminal.scroll_to_bottom();
            None
        }
        ActorCommand::User(Command::NavigateCommand {
            previous,
            failed_only,
        }) => {
            terminal.navigate_command(previous, failed_only);
            None
        }
        ActorCommand::User(Command::SelectCommandOutput { id }) => {
            terminal.select_command_output(id);
            None
        }
        ActorCommand::User(Command::SetCommandOutputCache(enabled)) => {
            terminal.set_command_output_cache(enabled);
            None
        }
        ActorCommand::User(Command::RevealCommand { id }) => {
            terminal.reveal_command(id);
            None
        }
        ActorCommand::User(Command::BeginSelection { column, row, kind }) => {
            terminal.begin_selection(column, row, kind);
            None
        }
        ActorCommand::User(Command::UpdateSelection { column, row }) => {
            terminal.update_selection(column, row);
            None
        }
        ActorCommand::User(Command::ClearSelection) => {
            terminal.clear_selection();
            None
        }
        ActorCommand::User(Command::SelectAll) => {
            terminal.select_all();
            None
        }
        ActorCommand::CopySelection(reply) => {
            if !reply.is_closed()
                && !shared.evicted.load(Ordering::Acquire)
                && !shared.actor_stop.is_cancelled()
            {
                let _ = reply.send(terminal.selected_text());
            }
            return None;
        }
        ActorCommand::User(Command::Search {
            query,
            case_sensitive,
        }) => {
            terminal.start_search(query, case_sensitive);
            None
        }
        ActorCommand::User(Command::CancelSearch) => {
            terminal.cancel_search();
            None
        }
        ActorCommand::User(Command::NextMatch { backwards }) => {
            terminal.search_next(backwards);
            None
        }
        ActorCommand::User(Command::SetTheme(theme)) => {
            terminal.set_theme(crate::terminal::Theme {
                background: theme.background,
                foreground: theme.foreground,
                selection: theme.selection,
                cursor: theme.cursor,
                line_number: theme.line_number,
                trace: theme.trace,
                debug: theme.debug,
                info: theme.info,
                warn: theme.warn,
                error: theme.error,
                fatal: theme.fatal,
                timestamp: theme.timestamp,
                address: theme.address,
                url: theme.url,
                path: theme.path,
            });
            None
        }
        ActorCommand::User(Command::SetHighlight(enabled)) => {
            terminal.set_highlight(enabled);
            None
        }
        ActorCommand::User(Command::SetHistoryBudget { lines, bytes }) => {
            shared.requested_budget.store(bytes, Ordering::Release);
            budget::rebalance();
            terminal.set_history_limits(
                lines,
                shared
                    .allocated_budget
                    .load(Ordering::Acquire)
                    .saturating_sub(shared.retained_base.load(Ordering::Acquire)),
            );
            None
        }
        ActorCommand::User(_)
        | ActorCommand::ZmodemStart { .. }
        | ActorCommand::ZmodemCancel { .. } => None,
    };
    if matches!(&write, Some(WriteCommand::UserInput(bytes)) if bytes.is_empty()) {
        if follows_input {
            shared.input_completed();
        }
        write = None;
    } else if follows_input {
        if connected {
            terminal.scroll_to_bottom();
            if shared.local_echo.load(Ordering::Acquire)
                && let Some(WriteCommand::UserInput(bytes)) = &write
            {
                terminal.feed(&super::telnet::local_echo(bytes));
            }
        } else {
            shared.input_completed();
            shared.report_input_loss();
        }
    }
    shared.changed();
    write
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{SelectionKind, SendError, Session};
    use tokio::sync::oneshot;

    #[tokio::test]
    async fn zmodem_actor_isolates_input_resize_and_terminal_replies() {
        let (session, terminal, commands) = Session::create(80, 24, 0).unwrap();
        session.shared.state(SessionState::Connected);
        let (output, receive) = mpsc::channel(8);
        let (input, mut writes) = mpsc::channel(8);
        let (exit, exited) = oneshot::channel();
        let shared = session.shared.clone();
        let task = tokio::spawn(run(
            terminal,
            commands,
            shared.clone(),
            Some(Transport {
                output: receive,
                input,
                exit: exited,
                tasks: vec![],
                reaper: None,
            }),
        ));
        output
            .send(Output::Data(
                b"\x1b[6n**\x18B00000000000000\r\n\x11".to_vec(),
            ))
            .await
            .unwrap();
        let status = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Some(status) = session.zmodem_status() {
                    break status;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            session.send(Command::Paste("not protocol data".into())),
            Err(SendError::TransferBusy)
        );
        let resize = session.resize(90, 26).unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !resize.is_complete() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            writes.try_recv().is_err(),
            "DSR replies and resize must wait for the transfer"
        );
        session.cancel_zmodem(status.id).unwrap();
        let cancellation = tokio::time::timeout(Duration::from_secs(1), writes.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(cancellation, WriteCommand::Data(bytes) if bytes.starts_with(b"\x18\x18\x18\x18"))
        );
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), writes.recv())
                .await
                .unwrap(),
            Some(WriteCommand::Resize(90, 26))
        ));
        assert!(
            writes.try_recv().is_err(),
            "stale DSR reply must not corrupt the cancelled protocol"
        );
        session.send(Command::Input(b"after".to_vec())).unwrap();
        assert!(
            matches!(tokio::time::timeout(Duration::from_secs(1), writes.recv()).await.unwrap(), Some(WriteCommand::UserInput(bytes)) if bytes == b"after")
        );
        shared.input_completed();
        output.send(Output::Eof).await.unwrap();
        exit.send(Ok(Some(0))).unwrap();
        session.close();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
    }

    #[test]
    fn resize_receipts_confirm_only_their_processed_command() {
        let mut terminal = Terminal::new(80, 24, 0);
        let shared = Shared::new(&terminal);
        let (commands, mut receiver) = mpsc::channel(4);
        let session = Session { commands, shared };
        let first = session.resize(81, 25).unwrap();
        // Identical dimensions must still have independent completion receipts.
        let second = session.resize(81, 25).unwrap();
        assert!(!first.is_complete());
        assert!(!second.is_complete());

        let write = command_effect(
            receiver.try_recv().unwrap(),
            &mut terminal,
            &session.shared,
            true,
        );
        assert!(matches!(write, Some(WriteCommand::Resize(81, 25))));
        assert_eq!(terminal.dimensions(), (81, 25));
        assert!(first.is_complete());
        assert!(!second.is_complete());
        // No snapshot was requested or published to acknowledge the resize.
        assert_eq!(session.snapshot().columns, 80);

        command_effect(
            receiver.try_recv().unwrap(),
            &mut terminal,
            &session.shared,
            false,
        );
        assert!(first.is_complete());
        assert!(second.is_complete());
    }

    #[test]
    fn output_cache_setting_reaches_actor_and_preserves_completion_events() {
        let (session, mut terminal, _commands) = Session::create(80, 24, 10).unwrap();
        let mut completions = session.subscribe_command_completions();
        let mut outbox = Outbox::default();
        for (enabled, id) in [(false, 1), (true, 2)] {
            assert!(
                command_effect(
                    ActorCommand::User(Command::SetCommandOutputCache(enabled)),
                    &mut terminal,
                    &session.shared,
                    true,
                )
                .is_none()
            );
            feed_output(
                b"\x1b]133;B\x07printf test\r\n\x1b]133;C\x07test\r\n\x1b]133;D;0\x07",
                &mut terminal,
                &session.shared,
                &mut outbox,
            )
            .unwrap();
            assert!(completions.has_changed().unwrap());
            let completion = completions.borrow_and_update().clone().unwrap();
            assert_eq!(completion.id, id);
            assert_eq!(completion.exit_status, Some(0));
            assert_eq!(completion.output.is_some(), enabled);
            assert_eq!(completion.output_cache_disabled, !enabled);
            assert_eq!(session.command_history().len(), id as usize);
        }
    }

    #[test]
    fn hidden_command_completions_publish_once_without_snapshots_and_stop_on_disconnect() {
        let (session, mut terminal, _commands) = Session::create(80, 24, 10).unwrap();
        session.set_visible(false);
        let mut completions = session.subscribe_command_completions();
        let frames = session.subscribe_snapshots();
        assert!(completions.borrow_and_update().is_none());
        let mut outbox = Outbox::default();
        feed_output(
            b"\x1b]133;B\x07printf first\r\n\x1b]133;C\x07first\r\n\x1b]133;D;0\x07",
            &mut terminal,
            &session.shared,
            &mut outbox,
        )
        .unwrap();
        assert!(completions.has_changed().unwrap());
        let first = completions.borrow_and_update().as_ref().unwrap().id;
        assert_eq!(session.command_history().len(), 1);
        assert_eq!(session.command_history()[0].id, first);
        assert_eq!(session.snapshot_count(), 1);
        assert!(!frames.has_changed().unwrap());
        feed_output(
            b"unrelated output",
            &mut terminal,
            &session.shared,
            &mut outbox,
        )
        .unwrap();
        assert!(!completions.has_changed().unwrap());
        feed_output(
            b"\r\n\x1b]133;B\x07false\r\n\x1b]133;C\x07\x1b]133;D;1\x07",
            &mut terminal,
            &session.shared,
            &mut outbox,
        )
        .unwrap();
        let last = completions.borrow_and_update().clone().unwrap();
        assert_ne!(last.id, first);
        assert_eq!(last.exit_status, Some(1));
        let history = session.command_history();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].id, first);
        assert_eq!(history[1].id, last.id);
        session.disconnect();
        feed_output(
            b"\r\n\x1b]133;B\x07late\r\n\x1b]133;C\x07\x1b]133;D;0\x07",
            &mut terminal,
            &session.shared,
            &mut outbox,
        )
        .unwrap();
        assert!(!completions.has_changed().unwrap());
        assert_eq!(completions.borrow().as_ref().unwrap().id, last.id);
        assert!(Arc::ptr_eq(&history, &session.command_history()));
        assert_eq!(session.snapshot_count(), 1);
    }

    #[test]
    fn hidden_shell_state_tracks_prompt_and_directory_without_grid_snapshots() {
        let (session, mut terminal, _commands) = Session::create(80, 24, 10).unwrap();
        session.shared.state(SessionState::Connected);
        session.set_visible(false);
        let mut outbox = Outbox::default();
        feed_output(
            b"\x1b]7;file://host/old\x07\x1b]133;B\x07\x1b[?2004h",
            &mut terminal,
            &session.shared,
            &mut outbox,
        )
        .unwrap();
        assert_eq!(session.current_directory().as_deref(), Some("/old"));
        assert!(session.can_insert_command());
        feed_output(
            b"cd /new\r\n\x1b]133;C\x07",
            &mut terminal,
            &session.shared,
            &mut outbox,
        )
        .unwrap();
        assert!(!session.can_insert_command());
        feed_output(
            b"\x1b]133;D;0\x07\x1b]7;file://host/new\x07\x1b]133;B\x07",
            &mut terminal,
            &session.shared,
            &mut outbox,
        )
        .unwrap();
        assert_eq!(session.command_history()[0].cwd.as_deref(), Some("/old"));
        assert_eq!(session.current_directory().as_deref(), Some("/new"));
        assert!(session.can_insert_command());
        assert_eq!(session.snapshot_count(), 1);
        assert!(session.snapshot().shell.cwd.is_none());

        session.disconnect();
        assert!(!session.can_insert_command());
        // The shutdown drain may still contain the final OSC directory update.
        terminal.feed(b"\x1b]7;file://host/final\x07");
        publish_shell_state(&terminal, &session.shared);
        assert_eq!(session.current_directory().as_deref(), Some("/final"));

        session.shared.evicted.store(true, Ordering::Release);
        *session.shared.shell_state.lock().unwrap() = super::super::state::ShellState::default();
        publish_shell_state(&terminal, &session.shared);
        assert!(session.current_directory().is_none());
    }

    #[test]
    fn snapshot_publication_wakes_subscribers_without_changing_data_revision() {
        let (session, mut terminal, mut commands) = Session::create(80, 24, 10).unwrap();
        let mut frames = session.subscribe_snapshots();
        let initial = frames.borrow_and_update().clone();
        terminal.feed(b"ready");
        let revision = session.revision();
        assert!(session.request_snapshot());
        assert!(!session.request_snapshot());
        assert!(commands.try_recv().is_err());
        publish_snapshot(&mut terminal, &session.shared);
        assert!(frames.has_changed().unwrap());
        assert!(!Arc::ptr_eq(&initial, &frames.borrow_and_update()));
        assert_eq!(session.revision(), revision);
        assert!(session.request_snapshot());
    }

    #[test]
    fn evicted_actor_cannot_republish_snapshot_or_clipboard() {
        let mut terminal = Terminal::new(80, 24, 10);
        let shared = Shared::new(&terminal);
        terminal.feed(b"retained old output");
        terminal.begin_selection(0, 0, SelectionKind::Line);
        shared.evicted.store(true, Ordering::Release);
        publish_snapshot(&mut terminal, &shared);
        let (reply, mut receipt) = tokio::sync::oneshot::channel();
        command_effect(
            ActorCommand::CopySelection(reply),
            &mut terminal,
            &shared,
            false,
        );
        assert_eq!(shared.snapshot_count.load(Ordering::Acquire), 1);
        assert_eq!(shared.snapshot.borrow().version, 0);
        assert_eq!(
            receipt.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Closed)
        );
    }
    #[test]
    fn selection_copy_receipts_preserve_command_order_without_retaining_cancelled_results() {
        let (session, mut terminal, mut commands) =
            super::super::Session::create(10, 2, 0).unwrap();
        terminal.feed(b"first last");
        session
            .send(Command::BeginSelection {
                column: 0,
                row: 0,
                kind: SelectionKind::Word,
            })
            .unwrap();
        let mut first = session.copy_selection().unwrap();
        session
            .send(Command::BeginSelection {
                column: 6,
                row: 0,
                kind: SelectionKind::Word,
            })
            .unwrap();
        let cancelled = session.copy_selection().unwrap();
        let mut last = session.copy_selection().unwrap();
        drop(cancelled);
        while let Ok(command) = commands.try_recv() {
            command_effect(command, &mut terminal, &session.shared, false);
        }
        assert_eq!(first.try_recv().unwrap().as_deref(), Some("first"));
        assert_eq!(last.try_recv().unwrap().as_deref(), Some("last"));
        assert!(matches!(
            commands.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }
    #[test]
    fn stale_alternate_scroll_is_acknowledged_without_writing_or_following_output() {
        let mut terminal = Terminal::new(20, 3, 20);
        terminal.feed(b"one\r\ntwo\r\nthree\r\nfour\r\nfive\r\n");
        terminal.feed(b"\x1b[?1049h");
        let command = ActorCommand::User(Command::AlternateScroll(-2));
        terminal.feed(b"\x1b[?1049l");
        terminal.scroll(2);
        let shared = Shared::new(&terminal);
        shared.state(SessionState::Connected);
        shared.unacknowledged_inputs.store(1, Ordering::Release);

        assert!(command_effect(command, &mut terminal, &shared, true).is_none());
        assert_eq!(terminal.snapshot().display_offset, 2);
        assert_eq!(shared.unacknowledged_inputs.load(Ordering::Acquire), 0);
        assert_eq!(shared.current_state(), SessionState::Connected);
    }

    #[test]
    fn queued_input_after_disconnect_is_reported_without_replay_or_secret_text() {
        let mut terminal = Terminal::new(80, 24, 10);
        let shared = Shared::new(&terminal);
        shared.unacknowledged_inputs.store(1, Ordering::Release);
        let write = command_effect(
            ActorCommand::User(Command::Input(b"private-command-marker".to_vec())),
            &mut terminal,
            &shared,
            false,
        );
        assert!(write.is_none());
        assert_eq!(shared.unacknowledged_inputs.load(Ordering::Acquire), 0);
        // A delayed EOF/exit notification must not clear the input-loss failure.
        shared.closing();
        let SessionState::Failed { kind, message } = shared.current_state() else {
            panic!("input loss was hidden");
        };
        assert_eq!(kind, FailureKind::Write);
        assert!(message.contains(shared.input_loss_notice()));
        assert!(!message.contains("private-command-marker"));
        // A later reaper error still reports that the accepted input was not confirmed.
        shared.failed(FailureKind::Process, "process cleanup failed");
        shared.closing();
        let SessionState::Failed { kind, message } = shared.current_state() else {
            panic!("cleanup hid input loss");
        };
        assert_eq!(kind, FailureKind::Process);
        assert_eq!(message.matches(shared.input_loss_notice()).count(), 1);
    }
    #[test]
    fn input_loss_reuses_its_first_notice_without_global_language_changes() {
        let terminal = Terminal::new(80, 24, 10);
        let shared = Shared::new(&terminal);
        // Model a notice captured in an earlier locale without mutating global language.
        let first_notice = "input loss in the first locale";
        shared.input_loss_message.set(first_notice).unwrap();
        shared.failed(FailureKind::Write, first_notice);
        shared.report_input_loss();
        let SessionState::Failed { message, .. } = shared.current_state() else {
            panic!("input loss was hidden");
        };
        assert_eq!(message, first_notice);
        shared.failed(FailureKind::Process, message);
        let SessionState::Failed { message, .. } = shared.current_state() else {
            panic!("input loss was hidden");
        };
        assert_eq!(message, first_notice);
    }
}
