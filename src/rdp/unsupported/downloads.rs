#![allow(dead_code)]
use super::{RdpError, Result};
use std::{path::PathBuf, sync::Arc};
pub(super) enum Next {
    Request {
        index: u32,
        offset: u64,
        length: u32,
    },
    Ready {
        directory: Arc<tempfile::TempDir>,
        paths: Vec<PathBuf>,
    },
}
pub(super) struct Completed {
    pub offer_id: u64,
    pub bytes_received: u64,
    pub total_bytes: u64,
    pub result: Result<Next>,
}
pub(super) struct Downloads;
impl Downloads {
    pub fn new(_: Option<PathBuf>) -> Self {
        Self
    }
    pub fn active_offer(&self) -> Option<u64> {
        None
    }
    pub fn is_busy(&self) -> bool {
        false
    }
    pub fn begin(&mut self, _: u64, _: Vec<rustrdp::ClipboardFile>) -> Result<()> {
        Err(RdpError::Unsupported("clipboard files on this platform"))
    }
    pub fn accept(&mut self, _: rustrdp::ClipboardFileRequest, _: Vec<u8>) -> Result<()> {
        Err(RdpError::FileUnavailable)
    }
    pub fn cancel(&mut self) {}
    pub async fn next(&mut self) -> Option<Completed> {
        std::future::pending().await
    }
}
