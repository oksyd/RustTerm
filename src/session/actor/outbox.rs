use crate::session::{COMMAND_CAPACITY, FailureKind, MAX_INPUT_BYTES, transport::WriteCommand};
use std::collections::VecDeque;

const MAX_PENDING_WRITES: usize = COMMAND_CAPACITY;
const MAX_PENDING_BYTES: usize = COMMAND_CAPACITY * MAX_INPUT_BYTES;

struct PendingWrite {
    command: WriteCommand,
    ephemeral: bool,
}

/// Ordinary terminal writes waiting for transport capacity. ZMODEM owns the
/// transport directly while active, so these writes stay parked in the actor.
#[derive(Default)]
pub(super) struct Outbox {
    pending: VecDeque<PendingWrite>,
    bytes: usize,
}

impl Outbox {
    pub(super) fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    pub(super) fn can_accept_command(&self) -> bool {
        // A command can expand by the two bracketed-paste markers. Leave one
        // slot and one response batch for the peer while user input is paused.
        self.pending.len() + 1 < MAX_PENDING_WRITES
            && self.bytes <= MAX_PENDING_BYTES - (MAX_INPUT_BYTES + 12) - MAX_INPUT_BYTES
    }

    pub(super) fn push(
        &mut self,
        command: WriteCommand,
        ephemeral: bool,
    ) -> Result<(), FailureKind> {
        // A pending resize has not reached the peer yet. Only its latest size
        // matters until an intervening write makes that size observable.
        if let WriteCommand::Resize(..) = command
            && let Some(previous) = self.pending.back_mut()
            && matches!(previous.command, WriteCommand::Resize(..))
        {
            previous.command = command;
            return Ok(());
        }
        let bytes = write_bytes(&command);
        if bytes == 0 && matches!(command, WriteCommand::Data(_)) {
            return Ok(());
        }
        if self.pending.len() >= MAX_PENDING_WRITES
            || self.bytes.saturating_add(bytes) > MAX_PENDING_BYTES
        {
            return Err(FailureKind::Backpressure);
        }
        self.bytes += bytes;
        self.pending.push_back(PendingWrite { command, ephemeral });
        Ok(())
    }

    pub(super) fn reply(&mut self, bytes: Vec<u8>) -> Result<(), FailureKind> {
        if bytes.is_empty() {
            return Ok(());
        }
        if self.bytes.saturating_add(bytes.len()) > MAX_PENDING_BYTES {
            return Err(FailureKind::Backpressure);
        }
        // A batch may contain many short terminal queries. Coalescing adjacent
        // replies preserves byte order without spending one queue slot per query.
        if let Some(PendingWrite {
            command: WriteCommand::Data(previous),
            ephemeral: false,
        }) = self.pending.back_mut()
            && previous.len().saturating_add(bytes.len()) <= MAX_INPUT_BYTES
        {
            self.bytes += bytes.len();
            previous.extend(bytes);
            return Ok(());
        }
        self.push(WriteCommand::Data(bytes), false)
    }

    pub(super) fn pop(&mut self) -> Option<WriteCommand> {
        let pending = self.pending.pop_front()?;
        self.bytes -= write_bytes(&pending.command);
        Some(pending.command)
    }

    pub(super) fn quarantine(&mut self, resize: &mut Option<WriteCommand>) {
        self.pending.retain(|pending| {
            if let WriteCommand::Resize(columns, rows) = &pending.command {
                *resize = Some(WriteCommand::Resize(*columns, *rows));
                false
            } else {
                !pending.ephemeral
            }
        });
        self.bytes = self.pending.iter().map(|p| write_bytes(&p.command)).sum();
    }

    pub(super) fn discard(&mut self) -> usize {
        let inputs = self
            .pending
            .iter()
            .filter(|p| matches!(p.command, WriteCommand::UserInput(_)))
            .count();
        self.pending.clear();
        self.bytes = 0;
        inputs
    }
}

fn write_bytes(command: &WriteCommand) -> usize {
    match command {
        WriteCommand::Data(bytes) | WriteCommand::UserInput(bytes) => bytes.len(),
        WriteCommand::Resize(..) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_quarantine_keeps_order_and_discards_only_undelivered_inputs() {
        let mut outbox = Outbox::default();
        outbox
            .push(WriteCommand::UserInput(b"first".to_vec()), false)
            .unwrap();
        outbox
            .push(WriteCommand::Data(b"mouse".to_vec()), true)
            .unwrap();
        outbox.push(WriteCommand::Resize(80, 24), false).unwrap();
        outbox.reply(b"reply".to_vec()).unwrap();
        outbox.push(WriteCommand::Resize(90, 30), false).unwrap();
        outbox
            .push(WriteCommand::UserInput(b"last".to_vec()), false)
            .unwrap();
        let mut resize = None;
        outbox.quarantine(&mut resize);
        assert!(matches!(resize, Some(WriteCommand::Resize(90, 30))));
        assert!(matches!(outbox.pop(), Some(WriteCommand::UserInput(bytes)) if bytes == b"first"));
        assert!(matches!(outbox.pop(), Some(WriteCommand::Data(bytes)) if bytes == b"reply"));
        assert_eq!(outbox.discard(), 1);
        assert!(outbox.is_empty());
        assert_eq!(outbox.bytes, 0);
    }

    #[test]
    fn pending_writes_are_bounded_by_count_and_bytes() {
        let mut outbox = Outbox::default();
        for _ in 0..MAX_PENDING_WRITES {
            outbox.push(WriteCommand::Data(vec![0]), false).unwrap();
        }
        assert_eq!(
            outbox.push(WriteCommand::Data(vec![1]), false),
            Err(FailureKind::Backpressure)
        );
        outbox.discard();
        outbox.reply(vec![1; MAX_PENDING_BYTES]).unwrap();
        assert_eq!(outbox.reply(vec![2]), Err(FailureKind::Backpressure));
        assert!(
            matches!(outbox.pop(), Some(WriteCommand::Data(bytes)) if bytes.len() == MAX_PENDING_BYTES)
        );
        outbox.reply(vec![3]).unwrap();
        outbox.reply(vec![4]).unwrap();
        assert!(matches!(outbox.pop(), Some(WriteCommand::Data(bytes)) if bytes == [3, 4]));
    }

    #[test]
    fn admission_reserves_paste_expansion_and_terminal_reply_capacity() {
        let mut outbox = Outbox::default();
        while outbox.can_accept_command() {
            outbox
                .push(
                    WriteCommand::UserInput(vec![b'x'; MAX_INPUT_BYTES + 12]),
                    false,
                )
                .unwrap();
        }
        assert!(!outbox.can_accept_command());
        outbox.reply(vec![b'r'; MAX_INPUT_BYTES]).unwrap();
        assert!(!outbox.can_accept_command());
        outbox.pop();
        outbox.pop();
        assert!(outbox.can_accept_command());

        outbox.discard();
        while outbox.can_accept_command() {
            outbox
                .push(WriteCommand::UserInput(vec![0]), false)
                .unwrap();
        }
        outbox.reply(b"\x1b[1;1R".to_vec()).unwrap();
        assert!(!outbox.can_accept_command());
        outbox.pop();
        outbox.pop();
        assert!(outbox.can_accept_command());
    }

    #[test]
    fn resize_bursts_keep_the_latest_size_without_crossing_input_or_replies() {
        let mut outbox = Outbox::default();
        for columns in 80..180 {
            outbox
                .push(WriteCommand::Resize(columns, 24), false)
                .unwrap();
        }
        outbox
            .push(WriteCommand::UserInput(b"key".to_vec()), false)
            .unwrap();
        outbox.push(WriteCommand::Resize(180, 25), false).unwrap();
        outbox.push(WriteCommand::Resize(181, 26), false).unwrap();
        outbox.reply(b"\x1b[26;181R".to_vec()).unwrap();
        outbox.push(WriteCommand::Resize(182, 27), false).unwrap();
        assert!(matches!(outbox.pop(), Some(WriteCommand::Resize(179, 24))));
        assert!(matches!(outbox.pop(), Some(WriteCommand::UserInput(bytes)) if bytes == b"key"));
        assert!(matches!(outbox.pop(), Some(WriteCommand::Resize(181, 26))));
        assert!(
            matches!(outbox.pop(), Some(WriteCommand::Data(bytes)) if bytes == b"\x1b[26;181R")
        );
        assert!(matches!(outbox.pop(), Some(WriteCommand::Resize(182, 27))));
        assert!(outbox.is_empty());
        assert_eq!(outbox.bytes, 0);
    }
}
