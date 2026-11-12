use std::sync::{
    Arc, Mutex, MutexGuard,
    atomic::{AtomicBool, Ordering},
};

use super::{Input, InputState};
use crate::{ConnectionInfo, Result};

/// Identifies input encoded for one active desktop generation.
///
/// Check this immediately before writing a queued packet. Reactivation, failure,
/// and session destruction invalidate older generations. Once a packet write
/// starts, finish the packet or close the transport; never truncate a write.
#[derive(Clone, Debug)]
pub struct InputGeneration(Arc<AtomicBool>);

impl InputGeneration {
    /// Whether this input still belongs to the active desktop.
    pub fn is_valid(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Encoded input ready for the caller's ordered transport writer.
pub struct EncodedInput {
    /// Complete input packets, in transmission order.
    pub packets: Vec<Vec<u8>>,
    /// Validity guard for every packet in this batch.
    pub generation: InputGeneration,
    /// Whether the caller should schedule [`crate::Session::sync_input_pointer`].
    pub pointer_changed: bool,
}

struct State {
    input: InputState,
    // Releases may still be queued when deactivation invalidates their packets.
    // Retain every possibly held key/button until the next activation. The sets
    // are bounded by the validated scan codes and the five mouse buttons.
    release_candidates: InputState,
    release_pending: bool,
    info: ConnectionInfo,
    generation: Option<InputGeneration>,
    pointer_changed: bool,
    closed: bool,
}

/// A small, independently locked input encoder sharing the session's key and
/// button state. Encoding does not borrow or lock the graphics decoder.
///
/// Obtain this from [`crate::Session::input_controller`]. It is suspended until
/// [`crate::Session::resume_input`] is called after activation packets have been
/// queued. Serialize calls with other local input operations and preserve packet
/// order when handing batches to the transport.
#[derive(Clone)]
pub struct InputController(Arc<Mutex<State>>);

impl InputController {
    fn state(&self) -> MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// Encode without waiting for graphics processing. Inactive or closed
    /// desktops return `None`; they never accumulate input for later replay.
    pub fn encode(&self, input: Input) -> Result<Option<EncodedInput>> {
        let mut state = self.state();
        let Some(generation) = state.generation.clone() else {
            return Ok(None);
        };
        let pointer_changed = moves_pointer(&input);
        let State {
            input: encoder,
            info,
            ..
        } = &mut *state;
        let packets = encoder.encode(input, info)?;
        Self::record_release_candidates(&mut state);
        state.pointer_changed |= pointer_changed;
        Ok(Some(EncodedInput {
            packets,
            generation,
            pointer_changed,
        }))
    }

    pub(crate) fn resume(&self, info: &ConnectionInfo) {
        let mut state = self.state();
        if !state.closed && !state.release_pending && state.generation.is_none() {
            state.info = info.clone();
            state.generation = Some(InputGeneration(Arc::new(AtomicBool::new(true))));
        }
    }

    pub(crate) fn resize(&self, width: u16, height: u16) {
        let mut state = self.state();
        // Graphics resets can resize an active desktop without deactivation.
        // Update the encoder immediately without revoking its input generation
        // or clearing held keys/buttons.
        state.info.width = width;
        state.info.height = height;
        let (x, y) = state.input.position();
        let position = (x.min(width - 1), y.min(height - 1));
        state.pointer_changed |= position != (x, y);
        state.input.position = position;
    }

    fn invalidate(state: &mut State) {
        if let Some(generation) = state.generation.take() {
            generation.0.store(false, Ordering::Release);
        }
    }

    pub(crate) fn suspend(&self) {
        let mut state = self.state();
        Self::invalidate(&mut state);
        state.pointer_changed = false;
        state.release_pending = true;
    }

    pub(crate) fn release_after_activation(&self, info: &ConnectionInfo) -> Result<Vec<Vec<u8>>> {
        let mut state = self.state();
        if !state.release_pending {
            return Ok(Vec::new());
        }
        let (x, y) = state.input.position();
        let position = (x.min(info.width - 1), y.min(info.height - 1));
        state.release_candidates.position = position;
        // Capabilities and dimensions can change across reactivation.
        if !info.mouse_extended {
            state.release_candidates.buttons.retain(|button| {
                !matches!(
                    button,
                    super::MouseButton::Back | super::MouseButton::Forward
                )
            });
        }
        let packets = state.release_candidates.encode(Input::ReleaseAll, info)?;
        state.input = InputState {
            position,
            ..InputState::default()
        };
        state.release_pending = false;
        Ok(packets)
    }

    fn record_release_candidates(state: &mut State) {
        state
            .release_candidates
            .keys
            .extend(state.input.keys.iter().copied());
        state
            .release_candidates
            .buttons
            .extend(state.input.buttons.iter().copied());
    }

    pub(crate) fn close(&self) {
        let mut state = self.state();
        Self::invalidate(&mut state);
        state.closed = true;
        state.pointer_changed = false;
    }

    pub(crate) fn encode_local(&self, input: Input, info: &ConnectionInfo) -> Result<Vec<Vec<u8>>> {
        let mut state = self.state();
        let pointer_changed = moves_pointer(&input);
        let packets = state.input.encode(input, info)?;
        Self::record_release_candidates(&mut state);
        state.pointer_changed |= pointer_changed;
        Ok(packets)
    }

    pub(crate) fn take_pointer(&self) -> Option<(u16, u16)> {
        let mut state = self.state();
        std::mem::take(&mut state.pointer_changed).then(|| state.input.position())
    }

    #[cfg(test)]
    pub(crate) fn position(&self) -> (u16, u16) {
        self.state().input.position()
    }

    pub(crate) fn keep_awake(
        &self,
        position: (u16, u16),
        info: &ConnectionInfo,
    ) -> Result<Vec<Vec<u8>>> {
        self.state().input.keep_awake(position, info)
    }
}

fn moves_pointer(input: &Input) -> bool {
    matches!(
        input,
        Input::Pointer { .. } | Input::MouseButton { .. } | Input::Wheel { .. }
    )
}

pub(crate) struct SessionInput(InputController);

impl SessionInput {
    pub fn new(info: &ConnectionInfo) -> Self {
        Self(InputController(Arc::new(Mutex::new(State {
            input: InputState::default(),
            release_candidates: InputState::default(),
            release_pending: false,
            info: info.clone(),
            generation: None,
            pointer_changed: false,
            closed: false,
        }))))
    }

    pub fn controller(&self) -> InputController {
        self.0.clone()
    }
}

impl std::ops::Deref for SessionInput {
    type Target = InputController;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Drop for SessionInput {
    fn drop(&mut self) {
        self.close();
    }
}
