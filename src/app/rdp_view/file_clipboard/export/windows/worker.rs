use super::{Content, ExportError, ReadError, clipboard_sequence, open_clipboard};
use std::{
    ptr,
    sync::{OnceLock, mpsc},
    time::Duration,
};
use tokio_util::sync::CancellationToken;
use windows_sys::Win32::{
    Foundation::{GlobalFree, HGLOBAL, HWND},
    System::{
        DataExchange::{
            AddClipboardFormatListener, EmptyClipboard, GetClipboardOwner, SetClipboardData,
        },
        Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock},
    },
    UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, DispatchMessageW, GetMessageW, HWND_MESSAGE, MSG,
        PostMessageW, WM_APP, WM_CLIPBOARDUPDATE,
    },
};

const PUBLISH: u32 = WM_APP + 1;
const TIMEOUT: Duration = Duration::from_secs(4);

struct Publish {
    sequence: u32,
    content: Content,
    cancelled: CancellationToken,
    reply: mpsc::SyncSender<Result<(), ExportError>>,
}

pub(super) struct Publisher {
    window: usize,
    commands: mpsc::SyncSender<Publish>,
}

impl Publisher {
    pub(super) fn get() -> Result<&'static Self, ExportError> {
        static PUBLISHER: OnceLock<Result<Publisher, ExportError>> = OnceLock::new();
        PUBLISHER
            .get_or_init(Self::start)
            .as_ref()
            .map_err(|error| *error)
    }

    fn start() -> Result<Self, ExportError> {
        let (commands, incoming) = mpsc::sync_channel(1);
        let (ready, started) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("rdp-clipboard".into())
            .spawn(move || {
                let window = Window::new();
                match window {
                    Ok(window) => {
                        if ready.send(Ok(window.0 as usize)).is_ok() {
                            run(window, incoming);
                        }
                    }
                    Err(error) => {
                        let _ = ready.send(Err(error));
                    }
                }
            })
            .map_err(|_| ExportError::Failed)?;
        let window = started
            .recv_timeout(TIMEOUT)
            .map_err(|_| ExportError::Failed)??;
        Ok(Self { window, commands })
    }

    pub(super) fn owns_clipboard(&self) -> bool {
        // SAFETY: the window lives on the publication thread for application lifetime.
        unsafe { GetClipboardOwner() as usize == self.window }
    }

    pub(super) fn publish(
        &self,
        sequence: u32,
        content: Content,
        cancelled: CancellationToken,
    ) -> Result<(), ExportError> {
        let (reply, result) = mpsc::sync_channel(1);
        self.commands
            .try_send(Publish {
                sequence,
                content,
                cancelled: cancelled.clone(),
                reply,
            })
            .map_err(|_| ExportError::Failed)?;
        // SAFETY: wake the owning message thread; no pointers cross through the message.
        if unsafe { PostMessageW(self.window as HWND, PUBLISH, 0, 0) } == 0 {
            cancelled.cancel();
            return Err(ExportError::Failed);
        }
        result.recv_timeout(TIMEOUT).unwrap_or_else(|_| {
            cancelled.cancel();
            Err(ExportError::Failed)
        })
    }
}

fn run(window: Window, incoming: mpsc::Receiver<Publish>) {
    let mut current: Option<CancellationToken> = None;
    let mut message = MSG::default();
    // SAFETY: this thread created the window and owns its message queue.
    while unsafe { GetMessageW(&mut message, ptr::null_mut(), 0, 0) } > 0 {
        match message.message {
            PUBLISH => {
                while let Ok(command) = incoming.try_recv() {
                    let result = write(
                        &window,
                        command.sequence,
                        command.content,
                        &command.cancelled,
                        &mut current,
                    );
                    if result.is_ok() {
                        current = Some(command.cancelled.clone());
                    } else {
                        command.cancelled.cancel();
                    }
                    let _ = command.reply.send(result);
                }
            }
            WM_CLIPBOARDUPDATE => {
                // Windows may synthesize additional formats without changing ownership.
                // Only another owner's copy invalidates our fully rendered payload.
                if unsafe { GetClipboardOwner() } != window.0
                    && let Some(previous) = current.take()
                {
                    previous.cancel();
                }
            }
            _ => unsafe {
                DispatchMessageW(&message);
            },
        }
    }
    if let Some(previous) = current {
        previous.cancel();
    }
}

fn write(
    window: &Window,
    sequence: u32,
    content: Content,
    cancelled: &CancellationToken,
    current: &mut Option<CancellationToken>,
) -> Result<(), ExportError> {
    // Prepare all memory before EmptyClipboard so allocation failures cannot
    // erase the user's current copy. Only this message thread writes native data.
    let buffers = content
        .0
        .into_iter()
        .map(|(format, bytes)| Buffer::new(&bytes).map(|buffer| (format, buffer)))
        .collect::<Result<Vec<_>, _>>()?;
    let lock = open_clipboard(window.0, || cancelled.is_cancelled()).map_err(|error| {
        if error == ReadError::Cancelled {
            ExportError::Changed
        } else {
            ExportError::Failed
        }
    })?;
    if sequence != clipboard_sequence() {
        return Err(ExportError::Changed);
    }
    // SAFETY: this thread has the clipboard open with its own live HWND.
    if unsafe { EmptyClipboard() } == 0 {
        return Err(ExportError::Failed);
    }
    if let Some(previous) = current.take() {
        previous.cancel();
    }
    for (format, buffer) in buffers {
        buffer.publish(format)?;
    }
    drop(lock);
    if cancelled.is_cancelled() || unsafe { GetClipboardOwner() } != window.0 {
        return Err(ExportError::Changed);
    }
    Ok(())
}

struct Window(HWND);
impl Window {
    fn new() -> Result<Self, ExportError> {
        // SAFETY: STATIC is a system class. The message-only window never shows UI.
        let window = Self(unsafe {
            CreateWindowExW(
                0,
                windows_sys::w!("STATIC"),
                ptr::null(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null(),
            )
        });
        if window.0.is_null() || unsafe { AddClipboardFormatListener(window.0) } == 0 {
            return Err(ExportError::Failed);
        }
        Ok(window)
    }
}
impl Drop for Window {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: this thread owns the window; destruction removes its listener.
            unsafe {
                DestroyWindow(self.0);
            }
        }
    }
}

struct Buffer(HGLOBAL);
impl Buffer {
    fn new(bytes: &[u8]) -> Result<Self, ExportError> {
        // SAFETY: this guard owns the allocation until transferred to Windows.
        unsafe {
            let memory = Self(GlobalAlloc(GMEM_MOVEABLE, bytes.len()));
            if memory.0.is_null() {
                return Err(ExportError::Failed);
            }
            let data = GlobalLock(memory.0);
            if data.is_null() {
                return Err(ExportError::Failed);
            }
            ptr::copy_nonoverlapping(bytes.as_ptr(), data.cast(), bytes.len());
            GlobalUnlock(memory.0);
            Ok(memory)
        }
    }

    fn publish(mut self, format: u32) -> Result<(), ExportError> {
        // SAFETY: the memory is unlocked and this thread owns the open clipboard.
        // Only success transfers the buffer to Windows.
        if format == 0 || unsafe { SetClipboardData(format, self.0) }.is_null() {
            return Err(ExportError::Failed);
        }
        self.0 = ptr::null_mut();
        Ok(())
    }
}
impl Drop for Buffer {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: only buffers not transferred to Windows remain non-null.
            unsafe {
                GlobalFree(self.0);
            }
        }
    }
}
