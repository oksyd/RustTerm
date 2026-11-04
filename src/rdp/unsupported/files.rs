use super::{RdpError, Result};
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;
pub(super) struct OpenFiles;
impl OpenFiles {
    pub fn open(_: Vec<PathBuf>, _: CancellationToken) -> Result<Self> {
        Err(RdpError::Unsupported("clipboard files on this platform"))
    }
    pub fn descriptions(&self) -> Vec<rustrdp::ClipboardFile> {
        Vec::new()
    }
    pub fn file_count(&self) -> usize {
        0
    }
    pub fn read(
        &self,
        _: &rustrdp::ClipboardFileRequest,
        _: &CancellationToken,
    ) -> Result<Vec<u8>> {
        Err(RdpError::FileUnavailable)
    }
}
