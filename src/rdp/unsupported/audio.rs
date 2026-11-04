//! Explicit capability rejection until a native audio backend is available.
#![allow(dead_code)]
use super::{RdpError, Result};
use std::sync::{Arc, Mutex, atomic::AtomicBool};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
pub(super) enum Completion {
    Consumed {
        token: rustrdp::AudioToken,
        elapsed_ms: u32,
    },
    Unavailable,
}
pub(super) struct Playback {
    pub completions: mpsc::Receiver<Completion>,
}
impl Playback {
    pub fn new(_: Arc<AtomicBool>) -> Result<Self> {
        Err(RdpError::AudioUnavailable)
    }
    pub fn refresh_mute(&self) {}
    pub fn push(&mut self, _: rustrdp::AudioEvent) -> Result<()> {
        Err(RdpError::AudioUnavailable)
    }
}
pub(super) enum Captured {
    Opened {
        generation: u64,
        available: bool,
    },
    Samples {
        generation: u64,
        data: Vec<u8>,
        transmission: CancellationToken,
        discontinuity: bool,
    },
    Failed,
}
pub(super) struct Capture {
    pub events: mpsc::Receiver<Captured>,
}
impl Capture {
    pub fn new(
        _: rustrdp::CaptureRequest,
        _: Arc<AtomicBool>,
        _: Arc<AtomicBool>,
        _: Arc<Mutex<CancellationToken>>,
    ) -> Result<Self> {
        Err(RdpError::MicrophoneUnavailable)
    }
    pub fn refresh(&self) {}
}
