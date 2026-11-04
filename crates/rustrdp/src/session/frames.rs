use std::{cmp::Ordering, collections::VecDeque, sync::Arc};

#[cfg(test)]
use crate::{ConnectionInfo, wire};
use crate::{Error, Result};

use super::Event;

// The advertised window is a hint, so permit a bounded backlog beyond it.
const MAX_PENDING_FRAMES: usize = 1024;

/// An opaque receipt for a completed frame in one activation of one session.
///
/// Obtain it from [`crate::GraphicsFrame::acknowledgement`] or
/// [`Event::GraphicsFrameEnd`]. Retain it with the corresponding rendered frame
/// and submit it to [`crate::Session::acknowledge_frame`] only after consuming
/// that frame. Tokens from older activations cannot acknowledge reused frame IDs.
/// Tokens from the same session are ordered by receipt across reactivations;
/// tokens from different sessions are incomparable. Keep the greatest token when
/// asynchronous presentation callbacks complete out of order.
#[derive(Clone, Debug)]
pub struct FrameToken {
    session: Arc<()>,
    sequence: u64,
    frame_id: u32,
}

impl FrameToken {
    /// The server's frame ID, for diagnostics. Identity also includes the session
    /// and activation, so this value alone must not be used to confirm a frame.
    pub fn frame_id(&self) -> u32 {
        self.frame_id
    }
}

impl PartialEq for FrameToken {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.session, &other.session) && self.sequence == other.sequence
    }
}

impl Eq for FrameToken {}

impl PartialOrd for FrameToken {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Arc::ptr_eq(&self.session, &other.session).then(|| self.sequence.cmp(&other.sequence))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Acknowledgement {
    Legacy,
    #[cfg(feature = "graphics")]
    Graphics {
        total: u32,
    },
}

pub(super) struct Acknowledgements {
    session: Option<Arc<()>>,
    enabled: bool,
    next_sequence: u64,
    pending: VecDeque<(u64, u32, Acknowledgement)>,
}

impl Acknowledgements {
    pub fn new(enabled: bool) -> Self {
        Self {
            session: None,
            enabled,
            next_sequence: 0,
            pending: VecDeque::new(),
        }
    }

    pub fn reset(&mut self, enabled: bool) {
        self.enabled = enabled;
        // Keep receipt order across activations. Clearing pending invalidates
        // old receipts without making late callbacks appear newer than a reset.
        self.pending.clear();
    }

    pub fn complete(&mut self, frame_id: u32) -> Result<Option<FrameToken>> {
        if !self.enabled {
            return Ok(None);
        }
        self.push(frame_id, Acknowledgement::Legacy).map(Some)
    }

    #[cfg(feature = "graphics")]
    pub fn complete_graphics(&mut self, frame_id: u32, total: u32) -> Result<FrameToken> {
        self.push(frame_id, Acknowledgement::Graphics { total })
    }

    fn push(&mut self, frame_id: u32, kind: Acknowledgement) -> Result<FrameToken> {
        let session = self.session.get_or_insert_with(|| Arc::new(()));
        if self.pending.len() == MAX_PENDING_FRAMES {
            return Err(Error::Limit("unacknowledged graphics frames"));
        }
        let sequence = self
            .next_sequence
            .checked_add(1)
            .ok_or(Error::Limit("graphics frame sequence"))?;
        self.pending.push_back((sequence, frame_id, kind));
        self.next_sequence = sequence;
        Ok(FrameToken {
            session: Arc::clone(session),
            sequence,
            frame_id,
        })
    }

    #[cfg(test)]
    pub fn acknowledge(&mut self, token: &FrameToken, info: &ConnectionInfo) -> Result<Vec<Event>> {
        self.acknowledge_with(token, |id, _| {
            Ok(vec![Event::Send(wire::share_data(
                info.user_id,
                info.io_channel_id,
                info.share_id,
                0x38,
                &id.to_le_bytes(),
            )?)])
        })
    }

    pub fn acknowledge_with(
        &mut self,
        token: &FrameToken,
        mut encode: impl FnMut(u32, Acknowledgement) -> Result<Vec<Event>>,
    ) -> Result<Vec<Event>> {
        if !self
            .session
            .as_ref()
            .is_some_and(|session| Arc::ptr_eq(session, &token.session))
        {
            return Ok(Vec::new());
        }
        let Some(position) = self
            .pending
            .iter()
            .position(|&(sequence, id, _)| sequence == token.sequence && id == token.frame_id)
        else {
            return Ok(Vec::new());
        };
        // Encode all packets before committing the acknowledgement. A local
        // encoding failure must leave the pending receipt sequence retryable.
        let mut events = Vec::new();
        for &(_, id, kind) in self.pending.iter().take(position + 1) {
            events.extend(encode(id, kind)?);
        }
        self.pending.drain(..=position);
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> ConnectionInfo {
        super::super::tests::session().info
    }

    fn ids(events: Vec<Event>) -> Vec<u32> {
        events
            .into_iter()
            .map(|event| {
                let Event::Send(packet) = event else {
                    panic!("expected frame acknowledgement");
                };
                let mut mcs = wire::Reader::new(wire::x224_payload(&packet).unwrap());
                assert_eq!(mcs.u8().unwrap(), 0x64);
                mcs.skip(5).unwrap();
                let length = wire::per_length(&mut mcs).unwrap();
                let mut share = wire::Reader::new(mcs.take(length).unwrap());
                mcs.finish().unwrap();
                assert_eq!(share.u16_le().unwrap(), 22);
                assert_eq!(share.u16_le().unwrap(), 0x17);
                assert_eq!(share.u16_le().unwrap(), 1001);
                assert_eq!(share.u32_le().unwrap(), 1);
                share.skip(4).unwrap();
                assert_eq!(share.u8().unwrap(), 0x38);
                assert_eq!(share.u8().unwrap(), 0);
                assert_eq!(share.u16_le().unwrap(), 0);
                let id = share.u32_le().unwrap();
                share.finish().unwrap();
                id
            })
            .collect()
    }

    #[cfg(feature = "graphics")]
    #[test]
    fn pipeline_receipts_preserve_counts_without_legacy_ack_negotiation() {
        let mut acknowledgements = Acknowledgements::new(false);
        assert_eq!(acknowledgements.complete(1).unwrap(), None);
        let first = acknowledgements.complete_graphics(91, 1).unwrap();
        let second = acknowledgements.complete_graphics(3, 2).unwrap();
        let mut encoded = Vec::new();
        acknowledgements
            .acknowledge_with(&second, |id, kind| {
                encoded.push((id, kind));
                Ok(Vec::new())
            })
            .unwrap();
        assert_eq!(
            encoded,
            [
                (91, Acknowledgement::Graphics { total: 1 }),
                (3, Acknowledgement::Graphics { total: 2 })
            ]
        );
        assert!(
            acknowledgements
                .acknowledge_with(&first, |_, _| panic!("stale frame"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn tokens_bind_identity_to_the_session_activation_and_receive_order() {
        let mut first = Acknowledgements::new(true);
        let mut second = Acknowledgements::new(true);
        let old = first.complete(7).unwrap().unwrap();
        let other = second.complete(7).unwrap().unwrap();
        assert_ne!(old, other);
        assert_eq!(old, old.clone());
        first.reset(true);
        let fresh = first.complete(7).unwrap().unwrap();
        assert_ne!(old, fresh);
        let repeated = first.complete(7).unwrap().unwrap();
        assert_ne!(fresh, repeated);
        assert_eq!(fresh.frame_id(), repeated.frame_id());
    }

    #[test]
    fn receipt_order_is_stable_across_reactivation_and_frame_id_reuse() {
        let mut acknowledgements = Acknowledgements::new(true);
        let old = acknowledgements.complete(u32::MAX).unwrap().unwrap();
        let before_reset = acknowledgements.complete(7).unwrap().unwrap();
        assert!(before_reset > old);
        acknowledgements.reset(false);
        assert_eq!(acknowledgements.complete(7).unwrap(), None);
        acknowledgements.reset(true);
        let fresh = acknowledgements.complete(7).unwrap().unwrap();
        assert!(fresh > before_reset);
        assert!(fresh > old);
        assert_eq!(fresh.partial_cmp(&fresh.clone()), Some(Ordering::Equal));
        assert!(
            acknowledgements
                .acknowledge(&before_reset, &info())
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            ids(acknowledgements.acknowledge(&fresh, &info()).unwrap()),
            [7]
        );
    }

    #[test]
    fn independent_sessions_have_no_receipt_order() {
        let mut first = Acknowledgements::new(true);
        let mut second = Acknowledgements::new(true);
        let first = first.complete(7).unwrap().unwrap();
        let second = second.complete(7).unwrap().unwrap();
        assert_ne!(first, second);
        assert_eq!(first.partial_cmp(&second), None);
        assert_eq!(second.partial_cmp(&first), None);
    }

    #[test]
    fn pending_frames_are_bounded_beyond_the_advertised_window() {
        let mut acknowledgements = Acknowledgements::new(true);
        for id in 0..MAX_PENDING_FRAMES {
            acknowledgements.complete(id as u32).unwrap();
        }
        assert_eq!(
            acknowledgements.complete(1024),
            Err(Error::Limit("unacknowledged graphics frames"))
        );
        acknowledgements.reset(false);
        for id in 0..MAX_PENDING_FRAMES + 1 {
            assert_eq!(acknowledgements.complete(id as u32).unwrap(), None);
        }
        assert!(acknowledgements.pending.is_empty());
    }

    #[test]
    fn acknowledgement_follows_receive_order_and_confirms_skipped_frames() {
        let mut acknowledgements = Acknowledgements::new(true);
        let first = acknowledgements.complete(u32::MAX - 1).unwrap().unwrap();
        let second = acknowledgements.complete(0).unwrap().unwrap();
        let third = acknowledgements.complete(0).unwrap().unwrap();
        let fourth = acknowledgements.complete(9).unwrap().unwrap();
        let info = info();
        assert_eq!(
            ids(acknowledgements.acknowledge(&second, &info).unwrap()),
            [u32::MAX - 1, 0]
        );
        assert_eq!(acknowledgements.pending.len(), 2);
        assert!(
            acknowledgements
                .acknowledge(&first, &info)
                .unwrap()
                .is_empty()
        );
        assert!(
            acknowledgements
                .acknowledge(&second, &info)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            ids(acknowledgements.acknowledge(&fourth, &info).unwrap()),
            [0, 9]
        );
        assert!(
            acknowledgements
                .acknowledge(&third, &info)
                .unwrap()
                .is_empty()
        );
        assert!(acknowledgements.pending.is_empty());
    }

    #[test]
    fn server_all_in_flight_identifier_is_echoed_without_synthesizing_receipts() {
        let mut acknowledgements = Acknowledgements::new(true);
        let special = acknowledgements.complete(u32::MAX).unwrap().unwrap();
        let next = acknowledgements.complete(0).unwrap().unwrap();
        let info = info();
        assert_eq!(
            ids(acknowledgements.acknowledge(&special, &info).unwrap()),
            [u32::MAX]
        );
        assert_eq!(acknowledgements.pending.len(), 1);
        assert_eq!(
            ids(acknowledgements.acknowledge(&next, &info).unwrap()),
            [0]
        );
    }

    #[test]
    fn foreign_and_previous_activation_tokens_never_confirm_reused_ids() {
        let mut acknowledgements = Acknowledgements::new(true);
        let mut foreign = Acknowledgements::new(true);
        let old = acknowledgements.complete(7).unwrap().unwrap();
        let foreign = foreign.complete(7).unwrap().unwrap();
        acknowledgements.reset(true);
        let fresh = acknowledgements.complete(7).unwrap().unwrap();
        let info = info();
        assert!(
            acknowledgements
                .acknowledge(&old, &info)
                .unwrap()
                .is_empty()
        );
        assert!(
            acknowledgements
                .acknowledge(&foreign, &info)
                .unwrap()
                .is_empty()
        );
        assert_eq!(acknowledgements.pending.len(), 1);
        assert_eq!(
            ids(acknowledgements.acknowledge(&fresh, &info).unwrap()),
            [7]
        );
        acknowledgements.reset(false);
        assert!(
            acknowledgements
                .acknowledge(&fresh, &info)
                .unwrap()
                .is_empty()
        );
    }
}
