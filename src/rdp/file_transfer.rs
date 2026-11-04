//! Bounded background file work; the protocol driver remains responsive during disk I/O.

use super::{RdpError, RdpEvent, Result, files::OpenFiles};
use rustrdp::ClipboardFileRequest;
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};
use tokio::task::{JoinError, JoinSet};
use tokio_util::sync::CancellationToken;

const MAX_JOBS: usize = 16;

pub(super) enum Completed {
    Prepared {
        revision: u64,
        result: Result<Arc<OpenFiles>>,
    },
    Read {
        request: ClipboardFileRequest,
        result: Result<Vec<u8>>,
    },
}

struct Offer {
    files: Option<Arc<OpenFiles>>,
    file_count: usize,
    cancel: CancellationToken,
    bytes_sent: u64,
    deliveries: usize,
}

pub(super) struct Transfers {
    jobs: JoinSet<Completed>,
    preparing: Option<CancellationToken>,
    offers: BTreeMap<u64, Offer>,
    stop: CancellationToken,
}

impl Transfers {
    pub fn new() -> Self {
        Self {
            jobs: JoinSet::new(),
            preparing: None,
            offers: BTreeMap::new(),
            stop: CancellationToken::new(),
        }
    }

    pub fn is_busy(&self) -> bool {
        !self.jobs.is_empty()
    }

    pub async fn next(&mut self) -> Option<std::result::Result<Completed, JoinError>> {
        self.jobs.join_next().await
    }

    pub fn cancel_preparation(&mut self) {
        if let Some(cancel) = self.preparing.take() {
            cancel.cancel();
        }
    }

    pub fn prepare(&mut self, revision: u64, paths: Vec<PathBuf>) -> Result<()> {
        self.cancel_preparation();
        if self.jobs.len() >= MAX_JOBS {
            return Err(RdpError::FileUnavailable);
        }
        let cancel = self.stop.child_token();
        self.preparing = Some(cancel.clone());
        self.jobs.spawn_blocking(move || Completed::Prepared {
            revision,
            result: OpenFiles::open(paths, cancel).map(Arc::new),
        });
        Ok(())
    }

    pub fn retain(&mut self, revision: u64, files: Arc<OpenFiles>) -> RdpEvent {
        let count = files.file_count();
        self.offers.insert(
            revision,
            Offer {
                files: Some(files),
                file_count: count,
                cancel: self.stop.child_token(),
                bytes_sent: 0,
                deliveries: 0,
            },
        );
        RdpEvent::FileTransfer {
            offer_id: revision,
            files: count,
            bytes_sent: 0,
            available: true,
        }
    }

    pub fn request(&mut self, request: ClipboardFileRequest) -> bool {
        let Some(offer) = self.offers.get(&request.offer_id) else {
            return false;
        };
        if self.jobs.len() >= MAX_JOBS || offer.cancel.is_cancelled() {
            return false;
        }
        let Some(files) = offer.files.clone() else {
            return false;
        };
        let cancel = offer.cancel.clone();
        self.jobs.spawn_blocking(move || Completed::Read {
            request,
            result: files.read(&request, &cancel),
        });
        true
    }

    pub fn contains(&self, offer_id: u64) -> bool {
        self.offers
            .get(&offer_id)
            .is_some_and(|offer| offer.files.is_some() && !offer.cancel.is_cancelled())
    }

    pub fn queued(&mut self, offer_id: u64) -> bool {
        let Some(offer) = self.offers.get_mut(&offer_id) else {
            return false;
        };
        if offer.files.is_none() {
            return false;
        }
        offer.deliveries += 1;
        true
    }

    pub fn sent(&mut self, offer_id: u64, bytes: u64) -> Option<RdpEvent> {
        let offer = self.offers.get_mut(&offer_id)?;
        if offer.deliveries == 0 {
            return None;
        }
        offer.deliveries -= 1;
        offer.bytes_sent = offer.bytes_sent.saturating_add(bytes);
        let finished = offer.files.is_none() && offer.deliveries == 0;
        let event = RdpEvent::FileTransfer {
            offer_id,
            files: offer.file_count,
            bytes_sent: offer.bytes_sent,
            available: !finished,
        };
        if finished {
            self.offers.remove(&offer_id);
        }
        Some(event)
    }

    pub fn release(&mut self, offer_id: u64) -> Option<RdpEvent> {
        let offer = self.offers.get_mut(&offer_id)?;
        offer.cancel.cancel();
        // Already queued responses own their byte buffers. Only progress needs
        // to survive until receipt; retaining file descriptors would leak a
        // growing number of old offers during a slow network upload.
        offer.files = None;
        if offer.deliveries != 0 {
            return None;
        }
        let event = RdpEvent::FileTransfer {
            offer_id,
            files: offer.file_count,
            bytes_sent: offer.bytes_sent,
            available: false,
        };
        self.offers.remove(&offer_id);
        Some(event)
    }

    pub fn cancel(&mut self) {
        self.cancel_preparation();
        for offer in self.offers.values() {
            offer.cancel.cancel();
        }
    }
}

impl Drop for Transfers {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opened(directory: &tempfile::TempDir) -> Arc<OpenFiles> {
        let path = directory.path().join("file");
        std::fs::write(&path, b"123456789").unwrap();
        Arc::new(OpenFiles::open(vec![path], CancellationToken::new()).unwrap())
    }

    #[test]
    fn released_sources_close_while_progress_waits_for_the_last_delivery() {
        let directory = tempfile::tempdir().unwrap();
        let files = opened(&directory);
        let weak = Arc::downgrade(&files);
        let mut transfers = Transfers::new();
        transfers.retain(7, files);
        assert!(transfers.queued(7));
        assert!(transfers.queued(7));
        assert!(transfers.release(7).is_none());
        assert!(weak.upgrade().is_none());
        assert!(!transfers.contains(7));
        assert!(!transfers.request(ClipboardFileRequest {
            id: 1,
            offer_id: 7,
            index: 0,
            offset: 0,
            length: 1
        }));
        assert!(matches!(
            transfers.sent(7, 4),
            Some(RdpEvent::FileTransfer {
                offer_id: 7,
                files: 1,
                bytes_sent: 4,
                available: true
            })
        ));
        assert!(matches!(
            transfers.sent(7, 5),
            Some(RdpEvent::FileTransfer {
                offer_id: 7,
                files: 1,
                bytes_sent: 9,
                available: false
            })
        ));
        assert!(transfers.sent(7, 1).is_none());
        assert!(!transfers.offers.contains_key(&7));
    }

    #[test]
    fn delivered_offer_remains_readable_until_protocol_releases_it() {
        let directory = tempfile::tempdir().unwrap();
        let mut transfers = Transfers::new();
        transfers.retain(8, opened(&directory));
        assert!(transfers.queued(8));
        assert!(matches!(
            transfers.sent(8, 9),
            Some(RdpEvent::FileTransfer {
                bytes_sent: 9,
                available: true,
                ..
            })
        ));
        assert!(transfers.contains(8));
        assert!(matches!(
            transfers.release(8),
            Some(RdpEvent::FileTransfer {
                offer_id: 8,
                bytes_sent: 9,
                available: false,
                ..
            })
        ));
        assert!(!transfers.queued(8));
        assert!(!transfers.contains(8));
    }
}
