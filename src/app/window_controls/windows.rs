use gpui::Window;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows_sys::Win32::{
    Foundation::{HWND, POINT},
    UI::{
        Input::KeyboardAndMouse::ReleaseCapture,
        WindowsAndMessaging::{
            GetCursorPos, HTCAPTION, IsZoomed, PostMessageW, SC_MOVE, SW_MAXIMIZE, SW_RESTORE,
            ShowWindowAsync, WM_SYSCOMMAND,
        },
    },
};

fn handle(window: &Window) -> Option<HWND> {
    let RawWindowHandle::Win32(handle) = HasWindowHandle::window_handle(window).ok()?.as_raw()
    else {
        return None;
    };
    Some(handle.hwnd.get() as HWND)
}

pub(super) fn toggle_maximize(window: &Window) -> bool {
    let Some(hwnd) = handle(window) else {
        return false;
    };
    // SAFETY: GPUI supplies this live HWND on its UI thread. Asynchronous show
    // avoids re-entering GPUI while its window/entity is already borrowed.
    unsafe { toggle_maximize_handle(hwnd) }
}

unsafe fn toggle_maximize_handle(hwnd: HWND) -> bool {
    // GPUI's Windows zoom() always uses SW_MAXIMIZE, even when already maximized.
    // Let Windows restore its WINDOWPLACEMENT, including snap/monitor geometry.
    unsafe {
        ShowWindowAsync(
            hwnd,
            if IsZoomed(hwnd) != 0 {
                SW_RESTORE
            } else {
                SW_MAXIMIZE
            },
        ) != 0
    }
}

pub(super) fn start_move(window: &Window) -> bool {
    let Some(hwnd) = handle(window) else {
        return false;
    };
    // SAFETY: all calls are made on the owning UI thread with GPUI's live HWND.
    unsafe {
        let mut point = POINT::default();
        if GetCursorPos(&mut point) == 0 {
            return false;
        }
        start_move_handle(hwnd, point)
    }
}

unsafe fn start_move_handle(hwnd: HWND, point: POINT) -> bool {
    // Client mouse-down may have captured the pointer. Transfer the gesture
    // to Windows so it owns dragging, drag-to-restore and edge snapping.
    // SAFETY: caller supplies a live HWND belonging to the current UI thread.
    unsafe {
        ReleaseCapture();
        // Post the system move directly. Reposting WM_NCLBUTTONDOWN would enter
        // GPUI's caption listener again; synchronous SendMessage would re-enter
        // the borrowed GPUI window during the native move loop.
        PostMessageW(
            hwnd,
            WM_SYSCOMMAND,
            (SC_MOVE | HTCAPTION) as usize,
            screen_position(point),
        ) != 0
    }
}

fn screen_position(point: POINT) -> isize {
    // Win32 mouse messages pack signed screen coordinates into two words.
    // Preserve negative coordinates on monitors left of/above the primary one.
    (u32::from(point.x as u16) | (u32::from(point.y as u16) << 16)) as isize
}

#[cfg(test)]
mod tests;
