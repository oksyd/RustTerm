//! Native keyboard-shortcut inhibition for a single application window.
//!
//! A guard is owned by the RDP view and must be released before its window is
//! destroyed. It neither changes pointer confinement nor creates another GUI
//! connection. Pending requests are never reported as exclusive capture.

use anyhow::{Result, bail};
use gpui::Window;
#[cfg(target_os = "linux")]
use rustterm::i18n::messages as i18n_key;

pub(crate) mod release_gesture;

#[cfg(target_os = "linux")]
mod wayland;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureStatus {
    Pending,
    Active,
    Denied,
}

pub struct InputCapture {
    #[cfg(target_os = "linux")]
    backend: wayland::Capture,
}

impl InputCapture {
    /// Request inhibition on the existing native window. The caller must keep
    /// the window alive until this guard has been dropped, including on shutdown.
    pub fn request(window: &Window) -> Result<Self> {
        #[cfg(target_os = "linux")]
        {
            // GPUI 0.2.2's X11 HasWindowHandle and HasDisplayHandle panic rather
            // than returning Unavailable. Do not call those traits on X11.
            if gpui::guess_compositor() != "Wayland" {
                bail!(rustterm::i18n::Message::new(
                    i18n_key::RDP_EXCLUSIVE_KEYBOARD_CONTROL_REQUIRES_A_NATIVE_WAYLAND_SESSION,
                ));
            }
            Ok(Self {
                backend: wayland::Capture::request(window)?,
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = window;
            bail!("Native keyboard capture is unavailable on this platform")
        }
    }

    /// Dispatch only already queued native events; never wait for the server.
    pub fn poll_status(&mut self) -> CaptureStatus {
        #[cfg(target_os = "linux")]
        {
            self.backend.poll_status()
        }
        #[cfg(not(target_os = "linux"))]
        {
            CaptureStatus::Denied
        }
    }

    /// Consume a physical release gesture after dispatching native events with
    /// poll_status. Both pending requests and active capture can be cancelled.
    pub fn take_release_request(&mut self) -> bool {
        #[cfg(target_os = "linux")]
        {
            self.backend.take_release_request()
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }

    /// Native physical keys held at the last poll, including the initial Enter
    /// snapshot. The view waits for these to clear before forwarding new input.
    pub fn has_held_keys(&self) -> bool {
        #[cfg(target_os = "linux")]
        {
            self.backend.has_held_keys()
        }
        #[cfg(not(target_os = "linux"))]
        {
            false
        }
    }

    /// Cancel the current gesture for an instantaneous local interaction such
    /// as scrolling. Ordinary GPUI KeyDown events must not call this: the native
    /// keyboard observer sees stable physical keys in their original order.
    pub fn interrupt_release_gesture(&mut self) {
        #[cfg(target_os = "linux")]
        self.backend.interrupt_release_gesture();
    }

    /// Keep gestures disabled while any pointer button or IME composition is
    /// held. This persists across native events until the caller clears it.
    pub fn set_release_blocked(&mut self, blocked: bool) {
        #[cfg(target_os = "linux")]
        self.backend.set_release_blocked(blocked);
        #[cfg(not(target_os = "linux"))]
        let _ = blocked;
    }

    pub fn release(&mut self) {
        #[cfg(target_os = "linux")]
        self.backend.release();
    }
}

impl Drop for InputCapture {
    fn drop(&mut self) {
        self.release();
    }
}
