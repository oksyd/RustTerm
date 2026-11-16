//! Native image publication with one application-wide clipboard message window.
use super::super::{
    MAX_CONTENT_BYTES, ReadError, content,
    windows::{clipboard_sequence, open_clipboard, png_format},
};
use super::ExportError;
use std::sync::atomic::{AtomicBool, Ordering};
mod worker;
use tokio_util::sync::CancellationToken;
use windows_sys::Win32::System::Ole::{CF_DIBV5, CF_UNICODETEXT};

pub(crate) struct Content(Vec<(u32, Vec<u8>)>);

impl Content {
    pub(super) fn prepare(content: &rustrdp::ClipboardContent) -> Result<Self, ExportError> {
        if content.len() > MAX_CONTENT_BYTES {
            return Err(ExportError::TooLarge);
        }
        let image = if let Some(png) = &content.png {
            content::from_png(png.clone())
        } else if let Some(dib) = &content.dib {
            content::from_dib(dib.clone())
        } else {
            return Err(ExportError::Unavailable);
        }
        .map_err(|error| match error {
            super::super::ReadError::TooLarge => ExportError::TooLarge,
            _ => ExportError::Invalid,
        })?;
        let png = png_format();
        if png == 0 {
            return Err(ExportError::Failed);
        }
        let mut formats = vec![
            (png, image.png.expect("normalized image has PNG")),
            (
                u32::from(CF_DIBV5),
                image.dib.expect("normalized image has DIBV5"),
            ),
        ];
        if let Some(text) = &content.text {
            if text.contains('\0') {
                return Err(ExportError::Invalid);
            }
            formats.push((
                u32::from(CF_UNICODETEXT),
                text.encode_utf16()
                    .chain(std::iter::once(0))
                    .flat_map(u16::to_le_bytes)
                    .collect(),
            ));
        }
        if formats.iter().map(|(_, data)| data.len()).sum::<usize>() > MAX_CONTENT_BYTES {
            return Err(ExportError::TooLarge);
        }
        Ok(Self(formats))
    }
}

pub(super) struct Owner {
    sequence: u32,
    publisher: &'static worker::Publisher,
    cancelled: CancellationToken,
    started: AtomicBool,
    published: AtomicBool,
}

impl Owner {
    pub(super) fn begin(sequence: u32) -> Result<Self, ExportError> {
        if sequence != clipboard_sequence() {
            return Err(ExportError::Changed);
        }
        Ok(Self {
            sequence,
            publisher: worker::Publisher::get()?,
            cancelled: CancellationToken::new(),
            started: AtomicBool::new(false),
            published: AtomicBool::new(false),
        })
    }

    pub(super) fn is_current(&self) -> bool {
        let current = if self.published.load(Ordering::Acquire) {
            self.publisher.owns_clipboard()
        } else {
            self.sequence == clipboard_sequence()
        };
        if !current {
            self.cancel();
        }
        current && !self.cancelled.is_cancelled()
    }

    pub(super) fn cancellation(&self) -> CancellationToken {
        self.cancelled.clone()
    }

    pub(super) fn cancel(&self) {
        self.cancelled.cancel();
    }

    pub(super) fn publish(&self, content: Content) -> Result<(), ExportError> {
        if !self.is_current() {
            return Err(ExportError::Changed);
        }
        if self.started.swap(true, Ordering::AcqRel) {
            return Err(ExportError::Failed);
        }
        match self
            .publisher
            .publish(self.sequence, content, self.cancelled.clone())
        {
            Ok(()) => {
                self.published.store(true, Ordering::Release);
                Ok(())
            }
            Err(error) => {
                self.cancel();
                Err(error)
            }
        }
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        self.cancel(); // Fully rendered data stays available independently of this pane.
    }
}
