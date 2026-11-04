use super::{RdpError, Result};
use crate::config::RdpSharedFolder;
use std::sync::{Arc, atomic::AtomicBool};
use tokio::sync::mpsc;
pub(super) struct SharedDrives {
    pub completions: mpsc::Receiver<rustrdp::DriveCompletion>,
}
impl SharedDrives {
    pub fn new(_: &[RdpSharedFolder], _: Arc<AtomicBool>) -> Result<Self> {
        Err(RdpError::Unsupported("folder redirection on this platform"))
    }
    pub fn request(&self, _: rustrdp::DriveRequest) -> Result<()> {
        Err(RdpError::Unsupported("folder redirection on this platform"))
    }
}
