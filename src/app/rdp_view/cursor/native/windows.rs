//! A window-scoped native cursor. Windows moves it without repainting GPUI.
use super::Image;
use anyhow::{Result, bail, ensure};
use gpui::{Bounds, Pixels, Window, point, px};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::{
    cell::{Cell, RefCell},
    ptr,
    rc::Rc,
};
use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM},
    Graphics::Gdi::{
        BI_BITFIELDS, BITMAPINFO, BITMAPV5HEADER, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS,
        DeleteObject, HBITMAP, ScreenToClient,
    },
    UI::{
        Input::KeyboardAndMouse::IsWindowEnabled,
        Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
        WindowsAndMessaging::{
            CWP_SKIPDISABLED, CWP_SKIPINVISIBLE, CWP_SKIPTRANSPARENT, ChildWindowFromPointEx,
            CreateIconIndirect, DestroyCursor, GetCursor, GetCursorPos, GetDesktopWindow, HCURSOR,
            HTCLIENT, ICONINFO, IDC_ARROW, LoadCursorW, SetCursor, WM_NCDESTROY, WM_SETCURSOR,
            WM_USER,
        },
    },
};

pub(super) struct Cursor {
    // Rc confines native handles and subclass registration to the UI thread.
    state: Rc<State>,
}

struct State {
    window: Cell<HWND>,
    bounds: Cell<Option<Bounds<Pixels>>>,
    device_scale: Cell<f32>,
    enabled: Cell<bool>,
    image: RefCell<Option<NativeImage>>,
}

impl Cursor {
    pub fn new(window: &Window) -> Result<Self> {
        let RawWindowHandle::Win32(handle) = HasWindowHandle::window_handle(window)?.as_raw()
        else {
            bail!("cursor window is not Win32");
        };
        // SAFETY: GPUI provides its live HWND on the owning UI thread.
        unsafe { Self::from_handle(handle.hwnd.get() as HWND) }
    }

    unsafe fn from_handle(window: HWND) -> Result<Self> {
        let state = Rc::new(State {
            window: Cell::new(window),
            bounds: Cell::new(None),
            device_scale: Cell::new(1.),
            enabled: Cell::new(false),
            image: RefCell::new(None),
        });
        // The subclass owns one reference until removal or WM_NCDESTROY.
        // Each callback holds another, since forwarding can drop the view.
        let reference = Rc::into_raw(state.clone()) as usize;
        // SAFETY: stable allocation, unique subclass id, same-thread HWND.
        if unsafe { SetWindowSubclass(window, Some(subclass), reference, reference) } == 0 {
            unsafe { drop(Rc::from_raw(reference as *const State)) };
            bail!("could not install native cursor handler");
        }
        Ok(Self { state })
    }

    pub fn poll(&mut self) -> Result<()> {
        ensure!(
            !self.state.window.get().is_null(),
            "cursor window was destroyed"
        );
        Ok(())
    }

    pub fn prepare(&mut self, image: &Image) -> Result<bool> {
        let next = NativeImage::new(image)?;
        let handle = next.0;
        let previous = self.state.image.replace(Some(next));
        // Switch before freeing the old image; never destroy an in-use cursor.
        if previous
            .as_ref()
            .is_some_and(|previous| unsafe { GetCursor() } == previous.0)
        {
            unsafe { SetCursor(handle) };
        }
        Ok(true)
    }

    pub fn set_bounds(&mut self, bounds: Option<Bounds<Pixels>>, device_scale: f32) {
        self.state.bounds.set(bounds);
        self.state.device_scale.set(device_scale);
    }

    pub fn show(&mut self, hovered: bool) -> Result<bool> {
        self.state.enabled.set(hovered);
        if hovered && self.state.over_desktop() {
            self.state.install();
        } else {
            self.state.restore_local();
        }
        // Keep the native rendering path while another window covers ours;
        // WM_SETCURSOR will select the image when this window is exposed again.
        Ok(hovered && !self.state.handle().is_null())
    }

    pub fn hide(&mut self, hovered: bool) -> Result<()> {
        self.state.enabled.set(false);
        if hovered && self.state.over_desktop() {
            // Only retire our own image. GPUI may already have restored a local
            // control's cursor after a tab switch or a pointer Default update.
            if unsafe { GetCursor() } == self.state.handle() {
                unsafe { SetCursor(ptr::null_mut()) };
            }
        } else {
            self.state.restore_local();
        }
        Ok(())
    }

    pub fn suspend(&mut self) {
        self.state.enabled.set(false);
        self.state.bounds.set(None);
        self.state.restore_local();
    }
}

impl Drop for Cursor {
    fn drop(&mut self) {
        self.suspend();
        self.state.detach(false);
    }
}

impl State {
    fn handle(&self) -> HCURSOR {
        self.image
            .borrow()
            .as_ref()
            .map_or(ptr::null_mut(), |image| image.0)
    }

    fn over_desktop(&self) -> bool {
        let window = self.window.get();
        let Some(bounds) = self.bounds.get() else {
            return false;
        };
        let scale = self.device_scale.get();
        if window.is_null() || !scale.is_finite() || scale <= 0. {
            return false;
        }
        let mut position = POINT::default();
        // SAFETY: live UI-thread HWND, writable stack point. Recheck the actual
        // window under the pointer so queued updates cannot affect other apps.
        unsafe {
            let desktop = GetDesktopWindow();
            if GetCursorPos(&mut position) == 0 {
                return false;
            }
            let mut desktop_position = position;
            // WindowFromPoint sends WM_NCHITTEST and would reenter GPUI while
            // its window is borrowed. This shallow desktop-child lookup uses
            // bounds/styles only, without invoking a window procedure.
            IsWindowEnabled(window) != 0
                && ScreenToClient(desktop, &mut desktop_position) != 0
                && ChildWindowFromPointEx(
                    desktop,
                    desktop_position,
                    CWP_SKIPINVISIBLE | CWP_SKIPDISABLED | CWP_SKIPTRANSPARENT,
                ) == window
                && ScreenToClient(window, &mut position) != 0
                && bounds.contains(&point(
                    px(position.x as f32 / scale),
                    px(position.y as f32 / scale),
                ))
        }
    }

    fn install(&self) {
        let image = self.handle();
        if !image.is_null() && unsafe { GetCursor() } != image {
            unsafe { SetCursor(image) };
        }
    }

    fn restore_local(&self) {
        let image = self.handle();
        if !image.is_null() && unsafe { GetCursor() } == image {
            // Never overwrite a cursor already chosen by GPUI or another pane.
            unsafe { SetCursor(LoadCursorW(ptr::null_mut(), IDC_ARROW)) };
        }
    }

    fn detach(&self, destroying: bool) {
        let window = self.window.get();
        if window.is_null() {
            return;
        }
        let reference = self as *const Self as usize;
        // SAFETY: called on the registering thread, matching callback and id.
        if unsafe { RemoveWindowSubclass(window, Some(subclass), reference) } != 0 || destroying {
            self.window.set(ptr::null_mut());
            // SAFETY: release exactly the registration's Rc::into_raw reference.
            // Cursor or the current callback still retains this State.
            unsafe { drop(Rc::from_raw(reference as *const Self)) };
        }
    }
}

unsafe extern "system" fn subclass(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    reference: usize,
) -> LRESULT {
    let state = reference as *const State;
    // SAFETY: registration retains a reference. Retain across DefSubclassProc,
    // which may reenter GPUI and release the owning RDP view or its window.
    unsafe { Rc::increment_strong_count(state) };
    let state = unsafe { Rc::from_raw(state) };
    if message == WM_NCDESTROY {
        state.enabled.set(false);
        state.restore_local();
        state.detach(true);
    } else if message == WM_SETCURSOR
        && wparam == window as usize
        && lparam as u16 == HTCLIENT as u16
        && state.enabled.get()
        && state.over_desktop()
        && !state.handle().is_null()
    {
        state.install();
        return 1;
    }
    // SAFETY: preserve GPUI's message chain, including caption/resize controls.
    let result = unsafe { DefSubclassProc(window, message, wparam, lparam) };
    // GPUI queues cursor-style changes. Reapply after a queued None, without
    // depending on private GPUI message numbers or replacing local I-beams.
    if message >= WM_USER
        && state.enabled.get()
        && unsafe { GetCursor() }.is_null()
        && state.over_desktop()
    {
        state.install();
    }
    result
}

struct NativeImage(HCURSOR);
impl NativeImage {
    fn new(image: &Image) -> Result<Self> {
        ensure!(
            (1..=1024).contains(&image.width)
                && (1..=1024).contains(&image.height)
                && image.scale == 1
                && image.pixels.len() == image.width as usize * image.height as usize * 4
                && (0..image.width as i32).contains(&image.hotspot.0)
                && (0..image.height as i32).contains(&image.hotspot.1),
            "invalid native cursor image"
        );
        let header = BITMAPV5HEADER {
            bV5Size: size_of::<BITMAPV5HEADER>() as u32,
            bV5Width: image.width as i32,
            bV5Height: -(image.height as i32), // top-down BGRA, no vertical mirroring
            bV5Planes: 1,
            bV5BitCount: 32,
            bV5Compression: BI_BITFIELDS,
            bV5RedMask: 0x00ff0000,
            bV5GreenMask: 0x0000ff00,
            bV5BlueMask: 0x000000ff,
            bV5AlphaMask: 0xff000000,
            ..Default::default()
        };
        let mut pixels = ptr::null_mut();
        // SAFETY: a complete V5 header starts with the bitmap-info header; the
        // returned allocation contains exactly width * height * 4 writable bytes.
        let color = Bitmap(unsafe {
            CreateDIBSection(
                ptr::null_mut(),
                (&header as *const BITMAPV5HEADER).cast::<BITMAPINFO>(),
                DIB_RGB_COLORS,
                &mut pixels,
                ptr::null_mut(),
                0,
            )
        });
        ensure!(
            !color.0.is_null() && !pixels.is_null(),
            "could not create cursor bitmap"
        );
        // CreateIconIndirect expects straight-alpha BGRA and applies alpha
        // itself. Undo premultiplication after filtering, otherwise the native
        // cursor's translucent edges and shadows are multiplied twice.
        let destination =
            unsafe { std::slice::from_raw_parts_mut(pixels.cast::<u8>(), image.pixels.len()) };
        for (source, target) in image
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .zip(destination.as_chunks_mut::<4>().0)
        {
            let alpha = u16::from(source[3]);
            target[3] = source[3];
            for channel in 0..3 {
                target[channel] = (u16::from(source[channel]) * 255 + alpha / 2)
                    .checked_div(alpha)
                    .unwrap_or(0)
                    .min(255) as u8;
            }
        }
        // CreateBitmap uses WORD-aligned mask rows. A transparent AND mask also
        // preserves an entirely transparent image when all alpha bytes are zero.
        let stride = (image.width as usize).div_ceil(16) * 2;
        let mut mask = vec![0u8; stride * image.height as usize];
        for (index, pixel) in image.pixels.as_chunks::<4>().0.iter().enumerate() {
            if pixel[3] == 0 {
                let (x, y) = (index % image.width as usize, index / image.width as usize);
                mask[y * stride + x / 8] |= 0x80 >> (x % 8);
            }
        }
        let mask = Bitmap(unsafe {
            CreateBitmap(
                image.width as i32,
                image.height as i32,
                1,
                1,
                mask.as_ptr().cast(),
            )
        });
        ensure!(!mask.0.is_null(), "could not create cursor mask");
        let info = ICONINFO {
            fIcon: 0,
            xHotspot: image.hotspot.0 as u32,
            yHotspot: image.hotspot.1 as u32,
            hbmMask: mask.0,
            hbmColor: color.0,
        };
        // SAFETY: CreateIconIndirect copies both owned bitmaps before returning.
        let handle = unsafe { CreateIconIndirect(&info) };
        ensure!(!handle.is_null(), "could not create native cursor");
        Ok(Self(handle))
    }
}

impl Drop for NativeImage {
    fn drop(&mut self) {
        // SAFETY: sole owner of this dynamically-created cursor. Clear any
        // remaining selection before destruction, including error paths.
        unsafe {
            if GetCursor() == self.0 {
                SetCursor(LoadCursorW(ptr::null_mut(), IDC_ARROW));
            }
            DestroyCursor(self.0);
        }
    }
}

struct Bitmap(HBITMAP);
impl Drop for Bitmap {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: owned bitmap, never selected into a device context.
            unsafe { DeleteObject(self.0) };
        }
    }
}

#[cfg(test)]
mod tests;
