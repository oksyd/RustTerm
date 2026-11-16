//! Windows image clipboard access; data is copied before releasing the OS lock.
use super::{MAX_CONTENT_BYTES, ReadError, SelectionData, content};
use std::{
    mem::DropGuard,
    ptr,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::HWND,
    System::{
        DataExchange::{
            CloseClipboard, GetClipboardData, GetClipboardSequenceNumber,
            IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW,
        },
        Memory::{GlobalLock, GlobalSize, GlobalUnlock},
        Ole::{CF_DIB, CF_DIBV5},
    },
};

enum NativeImage {
    Png(Vec<u8>),
    Dib(Vec<u8>),
}

pub(super) fn png_format() -> u32 {
    static PNG: OnceLock<u32> = OnceLock::new();
    // SAFETY: the format name is a static, terminated UTF-16 string.
    *PNG.get_or_init(|| unsafe { RegisterClipboardFormatW(windows_sys::w!("PNG")) })
}

fn available(format: u32) -> bool {
    // SAFETY: querying a format does not require opening the clipboard.
    format != 0 && unsafe { IsClipboardFormatAvailable(format) != 0 }
}

fn image_formats() -> [u32; 3] {
    [png_format(), u32::from(CF_DIBV5), u32::from(CF_DIB)]
}

pub(super) fn image_available() -> bool {
    image_formats().into_iter().any(available)
}

pub(super) fn read(cancel: Arc<AtomicBool>) -> Result<Option<SelectionData>, ReadError> {
    check_cancel(&cancel)?;
    if !image_available() {
        return Ok(None);
    }
    let (image, sequence) = snapshot(&cancel)?;
    // Decode after closing the system clipboard: large images must not block
    // other applications' copy/paste. Validate before publishing to RDP.
    let content = match image {
        NativeImage::Png(bytes) => content::from_png(bytes)?,
        NativeImage::Dib(bytes) => content::from_dib(bytes)?,
    };
    check_cancel(&cancel)?;
    if clipboard_sequence() != sequence {
        return Err(ReadError::Changed);
    }
    Ok(Some(SelectionData::Content(content)))
}

pub(super) fn clipboard_sequence() -> u32 {
    // SAFETY: this process is attached to the interactive window station.
    unsafe { GetClipboardSequenceNumber() }
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), ReadError> {
    if cancel.load(Ordering::Relaxed) {
        Err(ReadError::Cancelled)
    } else {
        Ok(())
    }
}

fn snapshot(cancel: &AtomicBool) -> Result<(NativeImage, u32), ReadError> {
    let sequence = clipboard_sequence();
    let _close = open_clipboard(ptr::null_mut(), || cancel.load(Ordering::Relaxed))?;
    if sequence != clipboard_sequence() {
        return Err(ReadError::Changed);
    }
    for format in image_formats() {
        if available(format) {
            match copy_data(format) {
                // Delayed rendering can update the sequence inside GetClipboardData.
                Ok(bytes) => {
                    let image = if format == png_format() {
                        NativeImage::Png(bytes)
                    } else {
                        NativeImage::Dib(bytes)
                    };
                    return Ok((image, clipboard_sequence()));
                }
                Err(ReadError::Failed) => continue,
                Err(error) => return Err(error),
            }
        }
    }
    Err(ReadError::Failed)
}

pub(super) struct ClipboardLock(std::marker::PhantomData<*mut ()>);

impl Drop for ClipboardLock {
    fn drop(&mut self) {
        // SAFETY: a lock is constructed only after opening on this thread.
        unsafe {
            CloseClipboard();
        }
    }
}

pub(super) fn open_clipboard(
    window: HWND,
    cancelled: impl Fn() -> bool,
) -> Result<ClipboardLock, ReadError> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if cancelled() {
            return Err(ReadError::Cancelled);
        }
        // SAFETY: callers use their live window or NULL for read-only access.
        if unsafe { OpenClipboard(window) } != 0 {
            return Ok(ClipboardLock(std::marker::PhantomData));
        }
        if Instant::now() >= deadline {
            return Err(ReadError::TimedOut);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The caller keeps the clipboard open. All supported formats use HGLOBAL.
fn copy_data(format: u32) -> Result<Vec<u8>, ReadError> {
    // SAFETY: the clipboard is open on this thread. Its data is borrowed from
    // Windows and copied while locked; it is never freed or used after closing.
    unsafe {
        let handle = GetClipboardData(format);
        if handle.is_null() {
            return Err(ReadError::Failed);
        }
        let size = GlobalSize(handle);
        if size > MAX_CONTENT_BYTES {
            return Err(ReadError::TooLarge);
        }
        if size == 0 {
            return Err(ReadError::Invalid);
        }
        let data = GlobalLock(handle);
        if data.is_null() {
            return Err(ReadError::Failed);
        }
        let _unlock = DropGuard::new(handle, |handle| {
            GlobalUnlock(handle);
        });
        Ok(std::slice::from_raw_parts(data.cast::<u8>(), size).to_vec())
    }
}

#[cfg(test)]
#[path = "windows/tests.rs"]
mod tests;
