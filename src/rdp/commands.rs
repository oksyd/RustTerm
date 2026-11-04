use super::{QueuedCommand, RdpClipboardAction, RdpCommand};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
use tokio::sync::{Notify, mpsc::error::TryRecvError};
use tokio_util::sync::CancellationToken;

struct State {
    queue: VecDeque<QueuedCommand>,
    payload_bytes: usize,
    senders: usize,
    receiver_alive: bool,
    // An overflow barrier is always last. No new input is accepted until it drains.
    recovering: bool,
}

struct Shared {
    state: Mutex<State>,
    ready: Notify,
    capacity: usize,
    byte_limit: usize,
}

pub(super) struct Sender {
    shared: Arc<Shared>,
}

pub(super) struct Receiver {
    shared: Arc<Shared>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum SendError {
    Full,
    Closed,
}

pub(super) fn channel(capacity: usize, byte_limit: usize) -> (Sender, Receiver) {
    assert!(capacity > 0);
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            queue: VecDeque::with_capacity(capacity + 1),
            payload_bytes: 0,
            senders: 1,
            receiver_alive: true,
            recovering: false,
        }),
        ready: Notify::new(),
        capacity,
        byte_limit,
    });
    (
        Sender {
            shared: shared.clone(),
        },
        Receiver { shared },
    )
}

impl Sender {
    pub(super) fn try_send(
        &self,
        command: QueuedCommand,
        pending_paste: Option<&CancellationToken>,
    ) -> Result<(), SendError> {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if !state.receiver_alive {
            if let Some(paste) = pending_paste {
                paste.cancel();
            }
            return Err(SendError::Closed);
        }
        let release = matches!(command.command, RdpCommand::ReleaseAll);
        if state.recovering {
            if let Some(paste) = pending_paste {
                paste.cancel();
            }
            return if release {
                Ok(())
            } else {
                Err(SendError::Full)
            };
        }
        if coalesce_tail(&mut state.queue, &command.command) {
            return Ok(());
        }
        let bytes = payload_bytes(&command.command);
        if state.queue.len() == self.shared.capacity
            || bytes > self.shared.byte_limit.saturating_sub(state.payload_bytes)
        {
            if let Some(paste) = pending_paste {
                paste.cancel();
            }
            // One slot is reserved for releasing every accepted key/button, even
            // when the rejected command itself was a key-up or focus-loss event.
            if !matches!(
                state.queue.back().map(|entry| &entry.command),
                Some(RdpCommand::ReleaseAll)
            ) {
                state.queue.push_back(QueuedCommand {
                    command: RdpCommand::ReleaseAll,
                    paste: None,
                });
            }
            state.recovering = true;
            drop(state);
            self.shared.ready.notify_one();
            return if release {
                Ok(())
            } else {
                Err(SendError::Full)
            };
        }
        state.payload_bytes += bytes;
        state.queue.push_back(command);
        drop(state);
        self.shared.ready.notify_one();
        Ok(())
    }
}

impl Clone for Sender {
    fn clone(&self) -> Self {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .senders += 1;
        Self {
            shared: self.shared.clone(),
        }
    }
}

impl Drop for Sender {
    fn drop(&mut self) {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.senders -= 1;
        if state.senders == 0 {
            drop(state);
            self.shared.ready.notify_one();
        }
    }
}

impl Receiver {
    pub(super) async fn recv(&mut self) -> Option<QueuedCommand> {
        loop {
            let ready = self.shared.ready.notified();
            tokio::pin!(ready);
            ready.as_mut().enable();
            match self.dequeue() {
                Ok(command) => return Some(command),
                Err(TryRecvError::Disconnected) => return None,
                Err(TryRecvError::Empty) => ready.await,
            }
        }
    }

    fn dequeue(&self) -> Result<QueuedCommand, TryRecvError> {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(command) = state.queue.pop_front() {
            state.payload_bytes -= payload_bytes(&command.command);
            if state.queue.is_empty() {
                state.recovering = false;
            }
            return Ok(command);
        }
        Err(if state.senders == 0 {
            TryRecvError::Disconnected
        } else {
            TryRecvError::Empty
        })
    }

    #[cfg(test)]
    pub(super) fn try_recv(&mut self) -> Result<QueuedCommand, TryRecvError> {
        self.dequeue()
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .queue
            .len()
    }

    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        let mut state = self
            .shared
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.receiver_alive = false;
        for command in state.queue.drain(..) {
            if let Some(paste) = command.paste {
                paste.cancel();
            }
        }
        state.payload_bytes = 0;
    }
}

fn payload_bytes(command: &RdpCommand) -> usize {
    match command {
        RdpCommand::Unicode(text)
        | RdpCommand::Clipboard {
            action: RdpClipboardAction::SendText(text) | RdpClipboardAction::PasteText(text),
            ..
        } => text.capacity(),
        RdpCommand::Clipboard {
            action:
                RdpClipboardAction::SendContent(content) | RdpClipboardAction::PasteContent(content),
            ..
        } => content
            .text
            .as_ref()
            .map_or(0, String::capacity)
            .saturating_add(content.html.as_ref().map_or(0, String::capacity))
            .saturating_add(content.rtf.as_ref().map_or(0, Vec::capacity))
            .saturating_add(content.png.as_ref().map_or(0, Vec::capacity))
            .saturating_add(content.dib.as_ref().map_or(0, Vec::capacity)),
        RdpCommand::Clipboard {
            action: RdpClipboardAction::SendFiles(paths) | RdpClipboardAction::PasteFiles(paths),
            ..
        } => paths.iter().fold(
            paths.capacity() * size_of::<std::path::PathBuf>(),
            |bytes, path| bytes.saturating_add(path.capacity()),
        ),
        _ => 0,
    }
}

fn coalesce_tail(queue: &mut VecDeque<QueuedCommand>, next: &RdpCommand) -> bool {
    if let Some(previous) = queue.back_mut()
        && coalesce(&mut previous.command, next)
    {
        return true;
    }
    // A diagonal scroll emits Y then X for each event. Combine each axis across
    // its perpendicular partner only, without crossing position/input barriers.
    if let (
        Some(QueuedCommand {
            command: RdpCommand::Wheel {
                horizontal, x, y, ..
            },
            ..
        }),
        RdpCommand::Wheel {
            horizontal: next_horizontal,
            x: next_x,
            y: next_y,
            ..
        },
    ) = (queue.back(), next)
        && horizontal != next_horizontal
        && x == next_x
        && y == next_y
        && let Some(index) = queue.len().checked_sub(2)
    {
        return coalesce(&mut queue[index].command, next);
    }
    false
}

fn coalesce(previous: &mut RdpCommand, next: &RdpCommand) -> bool {
    match (previous, next) {
        (
            RdpCommand::Pointer { x, y },
            RdpCommand::Pointer {
                x: next_x,
                y: next_y,
            },
        ) => {
            *x = *next_x;
            *y = *next_y;
            true
        }
        (
            RdpCommand::Resize { width, height },
            RdpCommand::Resize {
                width: next_width,
                height: next_height,
            },
        ) => {
            *width = *next_width;
            *height = *next_height;
            true
        }
        (
            RdpCommand::Wheel {
                horizontal,
                delta,
                x,
                y,
            },
            RdpCommand::Wheel {
                horizontal: next_horizontal,
                delta: next_delta,
                x: next_x,
                y: next_y,
            },
        ) if *horizontal == *next_horizontal
            && *x == *next_x
            && *y == *next_y
            && delta.signum() == next_delta.signum() =>
        {
            if let Some(total) = delta.checked_add(*next_delta) {
                *delta = total;
                true
            } else {
                false
            }
        }
        (RdpCommand::ReleaseAll, RdpCommand::ReleaseAll) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rdp::RdpMouseButton;
    use std::{
        future::Future,
        sync::atomic::{AtomicUsize, Ordering},
        task::{Context, Poll, Wake, Waker},
    };
    use tokio_util::sync::CancellationToken;

    fn queued(command: RdpCommand) -> QueuedCommand {
        QueuedCommand {
            command,
            paste: None,
        }
    }

    fn key(scancode: u16) -> QueuedCommand {
        queued(RdpCommand::Key {
            scancode,
            pressed: true,
        })
    }

    fn wheel(horizontal: bool, delta: i16, x: u16) -> QueuedCommand {
        queued(RdpCommand::Wheel {
            horizontal,
            delta,
            x,
            y: 1,
        })
    }

    #[test]
    fn pointer_bursts_preserve_latest_position_on_each_side_of_buttons() {
        let (sender, mut receiver) = channel(4, 128);
        for x in 0..5_000 {
            sender
                .try_send(queued(RdpCommand::Pointer { x, y: x }), None)
                .unwrap();
        }
        sender
            .try_send(
                queued(RdpCommand::MouseButton {
                    button: RdpMouseButton::Left,
                    pressed: true,
                    x: 4_999,
                    y: 4_999,
                }),
                None,
            )
            .unwrap();
        for x in 5_000..10_000 {
            sender
                .try_send(queued(RdpCommand::Pointer { x, y: x }), None)
                .unwrap();
        }
        assert_eq!(receiver.len(), 3);
        assert!(matches!(
            receiver.try_recv().unwrap().command,
            RdpCommand::Pointer { x: 4_999, y: 4_999 }
        ));
        assert!(matches!(
            receiver.try_recv().unwrap().command,
            RdpCommand::MouseButton {
                pressed: true,
                x: 4_999,
                y: 4_999,
                ..
            }
        ));
        assert!(matches!(
            receiver.try_recv().unwrap().command,
            RdpCommand::Pointer { x: 9_999, y: 9_999 }
        ));
    }

    #[test]
    fn critical_input_and_clipboard_remain_fifo_barriers() {
        let (sender, mut receiver) = channel(8, 128);
        sender
            .try_send(queued(RdpCommand::Pointer { x: 1, y: 1 }), None)
            .unwrap();
        sender.try_send(key(0x1d), None).unwrap();
        sender
            .try_send(queued(RdpCommand::Unicode("x".into())), None)
            .unwrap();
        sender
            .try_send(
                queued(RdpCommand::Clipboard {
                    revision: 1,
                    action: RdpClipboardAction::PasteText("text".into()),
                }),
                None,
            )
            .unwrap();
        sender
            .try_send(queued(RdpCommand::Pointer { x: 2, y: 2 }), None)
            .unwrap();
        assert!(matches!(
            receiver.try_recv().unwrap().command,
            RdpCommand::Pointer { x: 1, y: 1 }
        ));
        assert!(matches!(
            receiver.try_recv().unwrap().command,
            RdpCommand::Key { scancode: 0x1d, .. }
        ));
        assert!(matches!(
            receiver.try_recv().unwrap().command,
            RdpCommand::Unicode(text) if text == "x"
        ));
        assert!(matches!(
            receiver.try_recv().unwrap().command,
            RdpCommand::Clipboard { revision: 1, .. }
        ));
        assert!(matches!(
            receiver.try_recv().unwrap().command,
            RdpCommand::Pointer { x: 2, y: 2 }
        ));
    }

    #[test]
    fn wheel_bursts_preserve_all_units_without_overflow() {
        let (sender, mut receiver) = channel(64, 128);
        for _ in 0..2_000 {
            sender.try_send(wheel(false, 120, 1), None).unwrap();
        }
        assert!(receiver.len() < 10);
        let mut total = 0;
        while let Ok(command) = receiver.try_recv() {
            let RdpCommand::Wheel {
                horizontal: false,
                delta,
                x: 1,
                y: 1,
            } = command.command
            else {
                panic!("unexpected command");
            };
            assert!(delta > 0);
            total += i32::from(delta);
        }
        assert_eq!(total, 240_000);
    }

    #[test]
    fn diagonal_wheel_bursts_preserve_both_axes_without_filling_the_queue() {
        let (sender, mut receiver) = channel(64, 128);
        for _ in 0..2_000 {
            sender.try_send(wheel(false, 120, 1), None).unwrap();
            sender.try_send(wheel(true, -120, 1), None).unwrap();
        }
        assert!(receiver.len() < 20);
        let mut vertical = 0;
        let mut horizontal = 0;
        let mut last_axis = true;
        while let Ok(command) = receiver.try_recv() {
            let RdpCommand::Wheel {
                horizontal: axis,
                delta,
                x: 1,
                y: 1,
            } = command.command
            else {
                panic!("unexpected command");
            };
            assert_ne!(last_axis, axis);
            last_axis = axis;
            if axis {
                assert!(delta < 0);
                horizontal += i32::from(delta);
            } else {
                assert!(delta > 0);
                vertical += i32::from(delta);
            }
        }
        assert_eq!(vertical, 240_000);
        assert_eq!(horizontal, -240_000);
    }

    #[test]
    fn diagonal_wheel_coalescing_never_crosses_input_barriers() {
        for barrier in [
            RdpCommand::Pointer { x: 1, y: 1 },
            RdpCommand::Key {
                scancode: 0x1d,
                pressed: true,
            },
            RdpCommand::MouseButton {
                button: RdpMouseButton::Left,
                pressed: true,
                x: 1,
                y: 1,
            },
            RdpCommand::Clipboard {
                revision: 1,
                action: RdpClipboardAction::RequestText,
            },
        ] {
            let (sender, mut receiver) = channel(8, 128);
            for _ in 0..2 {
                sender.try_send(wheel(false, 10, 1), None).unwrap();
                sender.try_send(wheel(true, 20, 1), None).unwrap();
            }
            sender.try_send(queued(barrier), None).unwrap();
            for _ in 0..2 {
                sender.try_send(wheel(false, 30, 1), None).unwrap();
                sender.try_send(wheel(true, 40, 1), None).unwrap();
            }
            assert_eq!(receiver.len(), 5);
            for (index, expected_delta) in [20, 40, 0, 60, 80].into_iter().enumerate() {
                let command = receiver.try_recv().unwrap().command;
                if index == 2 {
                    assert!(!matches!(command, RdpCommand::Wheel { .. }));
                } else {
                    assert!(matches!(
                        command,
                        RdpCommand::Wheel { delta, .. } if delta == expected_delta
                    ));
                }
            }
        }
    }

    #[test]
    fn diagonal_wheel_coalescing_preserves_direction_changes_and_positions() {
        let (sender, mut receiver) = channel(8, 128);
        for command in [
            wheel(false, 10, 1),
            wheel(true, 20, 1),
            wheel(false, -30, 1),
            wheel(true, 40, 1),
            wheel(false, -50, 2),
            wheel(true, 60, 2),
        ] {
            sender.try_send(command, None).unwrap();
        }
        assert_eq!(receiver.len(), 5);
        for (horizontal, delta, x) in [
            (false, 10, 1),
            (true, 60, 1),
            (false, -30, 1),
            (false, -50, 2),
            (true, 60, 2),
        ] {
            assert!(matches!(
                receiver.try_recv().unwrap().command,
                RdpCommand::Wheel { horizontal: h, delta: d, x: px, .. }
                    if h == horizontal && d == delta && px == x
            ));
        }
    }

    #[test]
    fn wheel_axis_direction_position_and_keys_prevent_coalescing() {
        let (sender, mut receiver) = channel(8, 128);
        for command in [
            wheel(false, 120, 1),
            wheel(true, 120, 1),
            wheel(true, -120, 1),
            wheel(true, -120, 2),
            key(0x1d),
            wheel(true, -120, 2),
        ] {
            sender.try_send(command, None).unwrap();
        }
        for (horizontal, delta, x) in [
            (false, 120, 1),
            (true, 120, 1),
            (true, -120, 1),
            (true, -120, 2),
        ] {
            assert!(matches!(
                receiver.try_recv().unwrap().command,
                RdpCommand::Wheel { horizontal: h, delta: d, x: px, .. }
                    if h == horizontal && d == delta && px == x
            ));
        }
        assert!(matches!(
            receiver.try_recv().unwrap().command,
            RdpCommand::Key { .. }
        ));
        assert!(matches!(
            receiver.try_recv().unwrap().command,
            RdpCommand::Wheel { delta: -120, .. }
        ));
    }

    #[test]
    fn resize_only_replaces_an_adjacent_resize() {
        let (sender, mut receiver) = channel(4, 128);
        for width in 800..1_000 {
            sender
                .try_send(queued(RdpCommand::Resize { width, height: 600 }), None)
                .unwrap();
        }
        sender.try_send(key(0x1d), None).unwrap();
        sender
            .try_send(
                queued(RdpCommand::Resize {
                    width: 1_200,
                    height: 800,
                }),
                None,
            )
            .unwrap();
        assert_eq!(receiver.len(), 3);
        assert!(matches!(
            receiver.try_recv().unwrap().command,
            RdpCommand::Resize { width: 999, .. }
        ));
        assert!(matches!(
            receiver.try_recv().unwrap().command,
            RdpCommand::Key { .. }
        ));
        assert!(matches!(
            receiver.try_recv().unwrap().command,
            RdpCommand::Resize { width: 1_200, .. }
        ));
    }

    #[test]
    fn overflow_reserves_one_release_after_accepted_input_and_recovers() {
        let (sender, mut receiver) = channel(2, 128);
        sender.try_send(key(1), None).unwrap();
        sender.try_send(key(2), None).unwrap();
        assert_eq!(sender.try_send(key(3), None), Err(SendError::Full));
        for _ in 0..100 {
            sender
                .try_send(queued(RdpCommand::ReleaseAll), None)
                .unwrap();
            assert_eq!(sender.try_send(key(4), None), Err(SendError::Full));
        }
        assert_eq!(receiver.len(), 3);
        for expected in [1, 2] {
            assert!(matches!(
                receiver.try_recv().unwrap().command,
                RdpCommand::Key { scancode, .. } if scancode == expected
            ));
            assert_eq!(sender.try_send(key(4), None), Err(SendError::Full));
        }
        assert!(matches!(
            receiver.try_recv().unwrap().command,
            RdpCommand::ReleaseAll
        ));
        sender.try_send(key(5), None).unwrap();
        assert!(matches!(
            receiver.try_recv().unwrap().command,
            RdpCommand::Key { scancode: 5, .. }
        ));
    }

    #[test]
    fn explicit_release_always_fits_and_adjacent_releases_are_idempotent() {
        let (sender, mut receiver) = channel(1, 128);
        sender.try_send(key(1), None).unwrap();
        for _ in 0..100 {
            sender
                .try_send(queued(RdpCommand::ReleaseAll), None)
                .unwrap();
        }
        assert_eq!(receiver.len(), 2);
        receiver.try_recv().unwrap();
        assert!(matches!(
            receiver.try_recv().unwrap().command,
            RdpCommand::ReleaseAll
        ));
        for _ in 0..100 {
            sender
                .try_send(queued(RdpCommand::ReleaseAll), None)
                .unwrap();
        }
        assert_eq!(receiver.len(), 1);
    }

    #[test]
    fn payload_budget_is_aggregate_and_restored_on_receive() {
        let (sender, mut receiver) = channel(8, 8);
        sender
            .try_send(queued(RdpCommand::Unicode("1234".into())), None)
            .unwrap();
        sender
            .try_send(
                queued(RdpCommand::Clipboard {
                    revision: 1,
                    action: RdpClipboardAction::SendText("5678".into()),
                }),
                None,
            )
            .unwrap();
        assert_eq!(
            sender.try_send(queued(RdpCommand::Unicode("9".into())), None),
            Err(SendError::Full)
        );
        assert_eq!(receiver.len(), 3);
        while receiver.try_recv().is_ok() {}
        sender
            .try_send(queued(RdpCommand::Unicode("12345678".into())), None)
            .unwrap();
    }

    #[test]
    fn payload_budget_counts_allocated_text_and_file_paths() {
        let (sender, mut receiver) = channel(8, 32);
        let mut text = String::with_capacity(64);
        text.push('a');
        assert_eq!(
            sender.try_send(queued(RdpCommand::Unicode(text)), None),
            Err(SendError::Full)
        );
        receiver.try_recv().unwrap();
        assert_eq!(
            sender.try_send(
                queued(RdpCommand::Clipboard {
                    revision: 1,
                    action: RdpClipboardAction::SendFiles(vec![std::path::PathBuf::with_capacity(
                        64
                    )]),
                }),
                None
            ),
            Err(SendError::Full)
        );
        assert_eq!(receiver.len(), 1);
    }

    #[derive(Default)]
    struct WakeCounter(AtomicUsize);

    impl Wake for WakeCounter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn receiver_wakes_for_data_and_last_sender_drop() {
        let (sender, mut receiver) = channel(2, 128);
        let cloned = sender.clone();
        let counter = Arc::new(WakeCounter::default());
        let waker = Waker::from(counter.clone());
        let mut cx = Context::from_waker(&waker);
        let mut receive = Box::pin(receiver.recv());
        assert!(receive.as_mut().poll(&mut cx).is_pending());
        sender.try_send(key(1), None).unwrap();
        assert!(counter.0.load(Ordering::Relaxed) > 0);
        assert!(matches!(
            receive.as_mut().poll(&mut cx),
            Poll::Ready(Some(_))
        ));
        drop(receive);
        let mut receive = Box::pin(receiver.recv());
        assert!(receive.as_mut().poll(&mut cx).is_pending());
        drop(sender);
        assert!(receive.as_mut().poll(&mut cx).is_pending());
        let before_close = counter.0.load(Ordering::Relaxed);
        drop(cloned);
        assert!(counter.0.load(Ordering::Relaxed) > before_close);
        assert!(matches!(receive.as_mut().poll(&mut cx), Poll::Ready(None)));
    }

    #[test]
    fn cancelling_a_wait_does_not_lose_a_queued_command() {
        let (sender, mut receiver) = channel(2, 128);
        let mut cx = Context::from_waker(Waker::noop());
        let mut receive = Box::pin(receiver.recv());
        assert!(receive.as_mut().poll(&mut cx).is_pending());
        sender.try_send(key(1), None).unwrap();
        drop(receive);
        let mut receive = Box::pin(receiver.recv());
        assert!(matches!(
            receive.as_mut().poll(&mut cx),
            Poll::Ready(Some(_))
        ));
    }

    #[test]
    fn dropped_receiver_closes_senders_and_cancels_queued_paste() {
        let (sender, receiver) = channel(2, 128);
        let paste = CancellationToken::new();
        sender
            .try_send(
                QueuedCommand {
                    command: RdpCommand::PasteRemote,
                    paste: Some(paste.clone()),
                },
                None,
            )
            .unwrap();
        drop(receiver);
        assert!(paste.is_cancelled());
        assert_eq!(sender.try_send(key(1), None), Err(SendError::Closed));
    }
}
