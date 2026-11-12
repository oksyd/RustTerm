use super::*;
use std::{
    ptr,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::RECT,
    UI::{
        Input::KeyboardAndMouse::{GetCapture, SetCapture},
        WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, DispatchMessageW, GetWindowRect, MSG, PM_REMOVE,
            PeekMessageW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_OVERLAPPEDWINDOW,
        },
    },
};

struct TestWindow(HWND);
impl TestWindow {
    fn new() -> Self {
        let class: Vec<_> = "STATIC\0".encode_utf16().collect();
        // SAFETY: built-in window class; no custom callback, borrowed pointers
        // only for this call, and the resulting window is owned by this guard.
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                class.as_ptr(),
                ptr::null(),
                WS_OVERLAPPEDWINDOW,
                40,
                50,
                400,
                280,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null(),
            )
        };
        assert!(!hwnd.is_null());
        Self(hwnd)
    }
    fn rect(&self) -> (i32, i32, i32, i32) {
        let mut rect = RECT::default();
        // SAFETY: live HWND and writable stack rectangle.
        assert_ne!(unsafe { GetWindowRect(self.0, &mut rect) }, 0);
        (rect.left, rect.top, rect.right, rect.bottom)
    }
    fn wait_maximized(&self, expected: bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let mut msg = MSG::default();
            // SAFETY: dispatch only this test window's queued messages.
            while unsafe { PeekMessageW(&mut msg, self.0, 0, 0, PM_REMOVE) } != 0 {
                unsafe {
                    DispatchMessageW(&msg);
                }
            }
            if (unsafe { IsZoomed(self.0) } != 0) == expected {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "window did not change maximize state"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}
impl Drop for TestWindow {
    fn drop(&mut self) {
        // SAFETY: this guard is the sole owner, on the creating thread.
        unsafe {
            DestroyWindow(self.0);
        }
    }
}

#[test]
fn maximize_button_restores_native_window_placement_on_the_next_click() {
    let window = TestWindow::new();
    let original = window.rect();
    for _ in 0..2 {
        // SAFETY: the guard owns a valid HWND on this thread.
        assert!(unsafe { toggle_maximize_handle(window.0) });
        window.wait_maximized(true);
        assert!(unsafe { toggle_maximize_handle(window.0) });
        window.wait_maximized(false);
        assert_eq!(window.rect(), original);
    }
}

#[test]
fn caption_drag_releases_capture_and_posts_one_native_move() {
    let window = TestWindow::new();
    // SAFETY: capture and message retrieval are confined to this test's window.
    unsafe {
        SetCapture(window.0);
    }
    assert_eq!(unsafe { GetCapture() }, window.0);
    assert!(unsafe { start_move_handle(window.0, POINT { x: -1234, y: -56 }) });
    assert!(unsafe { GetCapture() }.is_null());
    let mut msg = MSG::default();
    assert_ne!(
        unsafe { PeekMessageW(&mut msg, window.0, WM_SYSCOMMAND, WM_SYSCOMMAND, PM_REMOVE) },
        0
    );
    assert_eq!(msg.wParam, (SC_MOVE | HTCAPTION) as usize);
    assert_eq!((msg.lParam as u16) as i16, -1234);
    assert_eq!(((msg.lParam as u32 >> 16) as u16) as i16, -56);
    // Do not dispatch a modal move loop or synthesize global input in this test.
    assert_eq!(
        unsafe { PeekMessageW(&mut msg, window.0, WM_SYSCOMMAND, WM_SYSCOMMAND, PM_REMOVE) },
        0
    );
}
