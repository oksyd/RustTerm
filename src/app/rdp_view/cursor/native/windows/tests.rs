use super::*;
use windows_sys::Win32::{
    Graphics::Gdi::{CreateCompatibleDC, DeleteDC, SelectObject},
    UI::{
        Shell::GetWindowSubclass,
        WindowsAndMessaging::{
            CreateWindowExW, DI_NORMAL, DestroyWindow, DrawIconEx, GetIconInfo, IDC_IBEAM,
            SendMessageW, WM_APP, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_OVERLAPPEDWINDOW,
        },
    },
};

struct TestWindow(HWND);
impl TestWindow {
    fn new() -> Self {
        let class: Vec<_> = "STATIC\0".encode_utf16().collect();
        let window = unsafe {
            CreateWindowExW(
                WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                class.as_ptr(),
                ptr::null(),
                WS_OVERLAPPEDWINDOW,
                10,
                10,
                200,
                100,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null(),
            )
        };
        assert!(!window.is_null());
        Self(window)
    }
}
impl Drop for TestWindow {
    fn drop(&mut self) {
        unsafe { DestroyWindow(self.0) };
    }
}

struct RestoreCursor(HCURSOR);
impl RestoreCursor {
    fn new() -> Self {
        Self(unsafe { GetCursor() })
    }
}
impl Drop for RestoreCursor {
    fn drop(&mut self) {
        unsafe { SetCursor(self.0) };
    }
}

fn image() -> Image {
    Image {
        width: 3,
        height: 2,
        hotspot: (1, 1),
        scale: 1,
        pixels: vec![
            0, 0, 255, 255, 0, 0, 0, 0, 0, 128, 0, 128, 255, 0, 0, 255, 255, 255, 255, 255, 0, 0,
            0, 255,
        ],
    }
}

fn draw(image: &NativeImage) -> Vec<u8> {
    let header = BITMAPV5HEADER {
        bV5Size: size_of::<BITMAPV5HEADER>() as u32,
        bV5Width: 3,
        bV5Height: -2,
        bV5Planes: 1,
        bV5BitCount: 32,
        bV5Compression: BI_BITFIELDS,
        bV5RedMask: 0x00ff0000,
        bV5GreenMask: 0x0000ff00,
        bV5BlueMask: 0x000000ff,
        bV5AlphaMask: 0xff000000,
        ..Default::default()
    };
    let mut bits = ptr::null_mut();
    // SAFETY: private 3x2 DIB and memory DC; never draws onto the user's screen.
    unsafe {
        let bitmap = Bitmap(CreateDIBSection(
            ptr::null_mut(),
            (&header as *const BITMAPV5HEADER).cast(),
            DIB_RGB_COLORS,
            &mut bits,
            ptr::null_mut(),
            0,
        ));
        assert!(!bitmap.0.is_null() && !bits.is_null());
        let pixels = std::slice::from_raw_parts_mut(bits.cast::<u8>(), 24);
        for pixel in pixels.as_chunks_mut::<4>().0 {
            pixel.copy_from_slice(&[40, 60, 80, 255]);
        }
        let dc = CreateCompatibleDC(ptr::null_mut());
        assert!(!dc.is_null());
        let previous = SelectObject(dc, bitmap.0);
        let drawn = DrawIconEx(dc, 0, 0, image.0, 3, 2, 0, ptr::null_mut(), DI_NORMAL);
        SelectObject(dc, previous);
        DeleteDC(dc);
        assert_ne!(drawn, 0);
        pixels.to_vec()
    }
}

#[test]
fn native_bitmap_preserves_hotspot_alpha_transparency_and_row_order() {
    let native = NativeImage::new(&image()).unwrap();
    let mut info = ICONINFO::default();
    assert_ne!(unsafe { GetIconInfo(native.0, &mut info) }, 0);
    let _color = Bitmap(info.hbmColor);
    let _mask = Bitmap(info.hbmMask);
    assert_eq!((info.fIcon, info.xHotspot, info.yHotspot), (0, 1, 1));
    let drawn = draw(&native);
    assert_eq!(&drawn[0..3], &[0, 0, 255]);
    assert_eq!(&drawn[4..7], &[40, 60, 80]);
    for (channel, expected) in drawn[8..11].iter().zip([20u8, 158, 40]) {
        assert!(
            channel.abs_diff(expected) <= 1,
            "premultiplied alpha blending: {:?}",
            &drawn[8..11]
        );
    }
    assert_eq!(&drawn[12..15], &[255, 0, 0]);
    assert_eq!(&drawn[16..19], &[255, 255, 255]);
    assert_eq!(&drawn[20..23], &[0, 0, 0]);

    let transparent = Image {
        pixels: vec![0; 24],
        ..image()
    };
    let drawn = draw(&NativeImage::new(&transparent).unwrap());
    for pixel in drawn.as_chunks::<4>().0 {
        assert_eq!(&pixel[..3], &[40, 60, 80]);
    }
}

#[test]
fn shape_replacement_and_suspend_do_not_leave_a_destroyed_or_foreign_cursor_selected() {
    let _restore = RestoreCursor::new();
    let window = TestWindow::new();
    let mut cursor = unsafe { Cursor::from_handle(window.0) }.unwrap();
    cursor.prepare(&image()).unwrap();
    let first = cursor.state.handle();
    unsafe { SetCursor(first) };
    cursor.prepare(&image()).unwrap();
    let second = cursor.state.handle();
    assert_ne!(first, second);
    assert_eq!(unsafe { GetCursor() }, second);
    cursor.suspend();
    assert_eq!(unsafe { GetCursor() }, unsafe {
        LoadCursorW(ptr::null_mut(), IDC_ARROW)
    });

    let local = unsafe { LoadCursorW(ptr::null_mut(), IDC_IBEAM) };
    unsafe { SetCursor(local) };
    cursor.hide(false).unwrap();
    cursor.suspend();
    drop(cursor);
    assert_eq!(unsafe { GetCursor() }, local);
}

#[test]
fn subclass_registration_is_released_with_either_view_or_window_and_is_unique_per_pane() {
    let window = TestWindow::new();
    let first = unsafe { Cursor::from_handle(window.0) }.unwrap();
    let mut second = unsafe { Cursor::from_handle(window.0) }.unwrap();
    let reference = Rc::as_ptr(&first.state) as usize;
    let weak = Rc::downgrade(&first.state);
    assert_eq!(Rc::strong_count(&first.state), 2);
    let mut data = 0;
    assert_ne!(
        unsafe { GetWindowSubclass(window.0, Some(subclass), reference, &mut data) },
        0
    );
    assert_eq!(data, reference);
    // Forward unrelated application messages through both subclasses.
    unsafe { SendMessageW(window.0, WM_APP + 101, 0, 0) };
    drop(first);
    assert_eq!(
        unsafe { GetWindowSubclass(window.0, Some(subclass), reference, &mut data) },
        0
    );
    assert!(weak.upgrade().is_none());
    assert_eq!(Rc::strong_count(&second.state), 2);
    drop(window);
    assert!(second.poll().is_err());
    assert!(second.state.window.get().is_null());
    assert_eq!(Rc::strong_count(&second.state), 1);
}

#[test]
fn invalid_cursor_buffers_are_rejected_before_creating_native_resources() {
    for invalid in [
        Image {
            width: 0,
            ..image()
        },
        Image {
            height: 1025,
            ..image()
        },
        Image {
            pixels: vec![0; 23],
            ..image()
        },
        Image {
            hotspot: (3, 1),
            ..image()
        },
        Image {
            hotspot: (-1, 0),
            ..image()
        },
        Image {
            scale: 2,
            ..image()
        },
    ] {
        assert!(NativeImage::new(&invalid).is_err());
    }
}

#[test]
fn forwarding_can_release_the_owning_view_without_invalidating_the_callback() {
    const DROP_VIEW: u32 = WM_APP + 102;
    unsafe extern "system" fn drop_view(
        window: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        _id: usize,
        data: usize,
    ) -> LRESULT {
        if message == DROP_VIEW {
            // SAFETY: the test retains this holder until subclass removal.
            let holder = unsafe { &*(data as *const RefCell<Option<Cursor>>) };
            holder.borrow_mut().take();
        }
        unsafe { DefSubclassProc(window, message, wparam, lparam) }
    }
    let window = TestWindow::new();
    let holder = RefCell::new(None);
    let reference = &holder as *const RefCell<Option<Cursor>> as usize;
    assert_ne!(
        unsafe { SetWindowSubclass(window.0, Some(drop_view), reference, reference) },
        0
    );
    let cursor = unsafe { Cursor::from_handle(window.0) }.unwrap();
    let weak = Rc::downgrade(&cursor.state);
    holder.replace(Some(cursor));
    // The cursor handler forwards to drop_view, which removes that handler
    // while it is on the stack. The callback retains State until it returns.
    unsafe { SendMessageW(window.0, DROP_VIEW, 0, 0) };
    assert!(holder.borrow().is_none());
    assert!(weak.upgrade().is_none());
    assert_ne!(
        unsafe { RemoveWindowSubclass(window.0, Some(drop_view), reference) },
        0
    );
}

#[test]
fn window_messages_keep_native_motion_and_preserve_resize_and_local_cursors() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        HTRIGHT, IDC_SIZEWE, WM_NCHITTEST, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE, WindowFromPoint,
    };
    const STYLE_CHANGED: u32 = WM_APP + 103;
    unsafe extern "system" fn framework_cursor(
        window: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        _id: usize,
        data: usize,
    ) -> LRESULT {
        if message == WM_NCHITTEST {
            // SAFETY: the test retains the counter until subclass removal.
            let hits = unsafe { &*(data as *const Cell<u32>) };
            hits.set(hits.get() + 1);
        }
        if message == STYLE_CHANGED {
            unsafe { SetCursor(lparam as HCURSOR) };
            return 0;
        }
        if message == WM_SETCURSOR && lparam as u16 == HTRIGHT as u16 {
            unsafe { SetCursor(LoadCursorW(ptr::null_mut(), IDC_SIZEWE)) };
            return 1;
        }
        unsafe { DefSubclassProc(window, message, wparam, lparam) }
    }
    let _restore = RestoreCursor::new();
    let mut position = POINT::default();
    assert_ne!(unsafe { GetCursorPos(&mut position) }, 0);
    let class: Vec<_> = "BUTTON\0".encode_utf16().collect();
    // Place a small non-activating test window under the existing pointer;
    // never warp the mouse, focus another application, or capture input.
    let window = TestWindow(unsafe {
        CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            class.as_ptr(),
            ptr::null(),
            WS_POPUP | WS_VISIBLE,
            position.x - 20,
            position.y - 20,
            100,
            100,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null(),
        )
    });
    assert!(!window.0.is_null());
    assert_eq!(unsafe { WindowFromPoint(position) }, window.0);
    let hit_tests = Cell::new(0u32);
    assert_ne!(
        unsafe {
            SetWindowSubclass(
                window.0,
                Some(framework_cursor),
                1,
                &hit_tests as *const Cell<u32> as usize,
            )
        },
        0
    );
    let mut cursor = unsafe { Cursor::from_handle(window.0) }.unwrap();
    cursor.prepare(&image()).unwrap();
    cursor.set_bounds(
        Some(Bounds::new(
            point(px(10.), px(10.)),
            gpui::size(px(40.), px(40.)),
        )),
        1.25,
    );
    let hits_before = hit_tests.get();
    assert!(cursor.show(true).unwrap());
    assert_eq!(
        hit_tests.get(),
        hits_before,
        "cursor updates must not reenter GPUI hit testing"
    );
    let native = cursor.state.handle();
    assert_eq!(unsafe { GetCursor() }, native);
    for _ in 0..100 {
        assert_eq!(
            unsafe { SendMessageW(window.0, WM_SETCURSOR, window.0 as usize, HTCLIENT as isize) },
            1
        );
        assert_eq!(unsafe { GetCursor() }, native);
    }
    // A deferred GPUI None must not leave the native cursor hidden.
    unsafe { SendMessageW(window.0, STYLE_CHANGED, 0, 0) };
    assert_eq!(unsafe { GetCursor() }, native);
    unsafe { SendMessageW(window.0, WM_SETCURSOR, window.0 as usize, HTRIGHT as isize) };
    assert_eq!(unsafe { GetCursor() }, unsafe {
        LoadCursorW(ptr::null_mut(), IDC_SIZEWE)
    });
    let local = unsafe { LoadCursorW(ptr::null_mut(), IDC_IBEAM) };
    unsafe { SendMessageW(window.0, STYLE_CHANGED, 0, local as isize) };
    assert_eq!(unsafe { GetCursor() }, local);

    cursor.show(true).unwrap();
    cursor.hide(true).unwrap();
    assert!(unsafe { GetCursor() }.is_null());
    cursor.show(true).unwrap();
    cursor.suspend();
    unsafe { SendMessageW(window.0, STYLE_CHANGED, 0, local as isize) };
    assert_eq!(unsafe { GetCursor() }, local);
    // Logical bounds are converted using monitor DPI, and exclude local UI.
    cursor.set_bounds(
        Some(Bounds::new(
            point(px(50.), px(50.)),
            gpui::size(px(20.), px(20.)),
        )),
        1.25,
    );
    cursor.show(true).unwrap();
    unsafe { SendMessageW(window.0, STYLE_CHANGED, 0, 0) };
    assert!(unsafe { GetCursor() }.is_null());
    drop(cursor);
    assert_ne!(
        unsafe { RemoveWindowSubclass(window.0, Some(framework_cursor), 1) },
        0
    );
}
