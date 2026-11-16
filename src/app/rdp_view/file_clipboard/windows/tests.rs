use super::*;
use image::ImageEncoder;
use std::{ptr, sync::Mutex};
use windows_sys::Win32::{
    Foundation::{GlobalFree, HWND},
    System::{
        DataExchange::*,
        Memory::*,
        Ole::{CF_DIB, CF_DIBV5},
    },
    UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow, HWND_MESSAGE},
};

static CLIPBOARD_TEST: Mutex<()> = Mutex::new(());

fn publish(
    export: &super::super::export::Export,
    content: super::super::export::Content,
) -> Result<(), super::super::export::ExportError> {
    // A real application pumps its window messages while the clipboard worker
    // publishes. In particular, EmptyClipboard notifies this fixture's old HWND.
    std::thread::scope(|scope| {
        let worker = scope.spawn(move || export.publish_content(content));
        while !worker.is_finished() {
            use windows_sys::Win32::UI::WindowsAndMessaging::{
                DispatchMessageW, MSG, PM_REMOVE, PeekMessageW,
            };
            let mut message = MSG::default();
            // SAFETY: dispatch only messages belonging to this fixture's thread.
            unsafe {
                while PeekMessageW(&mut message, ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                    DispatchMessageW(&message);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        worker.join().unwrap()
    })
}

struct Clipboard(HWND);
impl Clipboard {
    fn new() -> Self {
        // SAFETY: a private message-only STATIC window owns these test payloads.
        let window = unsafe {
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
        };
        assert!(!window.is_null());
        Self(window)
    }
    fn put(&self, format: u32, bytes: &[u8]) {
        // SAFETY: movable memory is copied while locked, then ownership passes
        // to the OS only when SetClipboardData succeeds. The test owns its HWND.
        unsafe {
            assert_ne!(OpenClipboard(self.0), 0);
            assert_ne!(EmptyClipboard(), 0);
            let memory = GlobalAlloc(GMEM_MOVEABLE, bytes.len());
            assert!(!memory.is_null());
            let data = GlobalLock(memory);
            assert!(!data.is_null());
            ptr::copy_nonoverlapping(bytes.as_ptr(), data.cast(), bytes.len());
            GlobalUnlock(memory);
            if SetClipboardData(format, memory).is_null() {
                GlobalFree(memory);
                CloseClipboard();
                panic!("could not set native clipboard fixture");
            }
            assert_ne!(CloseClipboard(), 0);
        }
    }
}
impl Drop for Clipboard {
    fn drop(&mut self) {
        // SAFETY: this fixture owns the window and removes its clipboard payload.
        unsafe {
            if OpenClipboard(self.0) != 0 {
                EmptyClipboard();
                CloseClipboard();
            }
            DestroyWindow(self.0);
        }
    }
}

#[test]
fn windows_bitmaps_and_png_become_browser_paste_images() {
    let _serial = CLIPBOARD_TEST.lock().unwrap();
    let clipboard = Clipboard::new();
    let rgba = [255, 0, 0, 255, 0, 0, 255, 255]; // red top row, blue bottom row
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(&rgba, 1, 2, image::ExtendedColorType::Rgba8)
        .unwrap();
    let mut dib = vec![0; 40];
    for (offset, value) in [(0, 40u32), (4, 1), (8, 2), (20, 8)] {
        dib[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    dib[12..14].copy_from_slice(&1u16.to_le_bytes());
    dib[14..16].copy_from_slice(&24u16.to_le_bytes());
    dib.extend_from_slice(&[255, 0, 0, 0, 0, 0, 255, 0]); // BGR, bottom-up, padded rows
    let dib_v5 = super::super::content::from_mime(vec![("image/png", png.clone())])
        .unwrap()
        .dib
        .unwrap();
    // SAFETY: the format name is a static, terminated UTF-16 string.
    let png_id = unsafe { RegisterClipboardFormatW(windows_sys::w!("PNG")) };
    for (format, data) in [
        (u32::from(CF_DIB), dib),
        (u32::from(CF_DIBV5), dib_v5),
        (png_id, png),
    ] {
        clipboard.put(format, &data);
        assert!(image_available());
        let Some(SelectionData::Content(content)) = read(Arc::new(AtomicBool::new(false))).unwrap()
        else {
            panic!("native image was lost");
        };
        let image = image::load_from_memory(content.png.as_ref().expect("PNG representation"))
            .unwrap()
            .into_rgba8();
        assert_eq!(image.as_raw(), &rgba);
        assert_eq!((image.width(), image.height()), (1, 2));
        assert!(content.dib.is_some());
        assert!(content.text.is_none());
    }

    // Cancellation never consumes the clipboard; malformed image data is not
    // forwarded, and a failed read releases the OS lock for the next copy.
    assert!(matches!(
        read(Arc::new(AtomicBool::new(true))),
        Err(ReadError::Cancelled)
    ));
    clipboard.put(png_id, b"not a PNG");
    assert!(matches!(
        read(Arc::new(AtomicBool::new(false))),
        Err(ReadError::Invalid)
    ));
    clipboard.put(
        u32::from(windows_sys::Win32::System::Ole::CF_UNICODETEXT),
        &[b'x', 0, 0, 0],
    );
    assert!(!image_available());
    assert!(matches!(read(Arc::new(AtomicBool::new(false))), Ok(None)));
}

#[test]
fn remote_images_are_pasteable_locally_and_never_replace_a_newer_copy() {
    use super::super::{
        content,
        export::{Export, ExportError, Snapshot},
    };

    let _serial = CLIPBOARD_TEST.lock().unwrap();
    let clipboard = Clipboard::new();
    let rgba = [255, 0, 0, 255, 0, 0, 255, 64];
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(&rgba, 1, 2, image::ExtendedColorType::Rgba8)
        .unwrap();
    let image = content::from_png(png.clone()).unwrap();
    clipboard.put(png_format(), &png);
    let stale = Snapshot::capture().unwrap();
    clipboard.put(png_format(), &png); // Even identical bytes are a newer user copy.
    assert!(matches!(Export::begin(stale), Err(ExportError::Changed)));

    let stale = Export::begin(Snapshot::capture().unwrap()).unwrap();
    clipboard.put(png_format(), &png);
    assert_eq!(
        publish(
            &stale,
            super::super::export::prepare_content(&image).unwrap()
        ),
        Err(ExportError::Changed)
    );

    let cancelled = Export::begin(Snapshot::capture().unwrap()).unwrap();
    cancelled.cancel();
    assert_eq!(
        publish(
            &cancelled,
            super::super::export::prepare_content(&image).unwrap()
        ),
        Err(ExportError::Changed)
    );

    let sequence = clipboard_sequence();
    assert!(matches!(
        super::super::export::prepare_content(&rustrdp::ClipboardContent {
            png: Some(b"invalid image".to_vec()),
            ..Default::default()
        }),
        Err(ExportError::Invalid)
    ));
    assert_eq!(clipboard_sequence(), sequence);

    for content in [
        rustrdp::ClipboardContent {
            png: image.png.clone(),
            ..Default::default()
        },
        rustrdp::ClipboardContent {
            dib: image.dib.clone(),
            ..Default::default()
        },
    ] {
        let export = Export::begin(Snapshot::capture().unwrap()).unwrap();
        publish(
            &export,
            super::super::export::prepare_content(&content).unwrap(),
        )
        .unwrap();
        assert!(export.is_current());
        assert!(available(png_format()));
        assert!(available(u32::from(CF_DIBV5)));
        let Some(SelectionData::Content(local)) = read(Arc::new(AtomicBool::new(false))).unwrap()
        else {
            panic!("remote image did not reach the Windows clipboard");
        };
        assert_eq!(
            image::load_from_memory(local.png.as_ref().unwrap())
                .unwrap()
                .into_rgba8()
                .as_raw(),
            &rgba
        );
        let ownership = export.cancellation();
        clipboard.put(png_format(), &png);
        assert!(!export.is_current());
        assert!(ownership.is_cancelled());
        assert_eq!(
            publish(
                &export,
                super::super::export::prepare_content(&content).unwrap()
            ),
            Err(ExportError::Changed)
        );
    }

    // Fully rendered data belongs to Windows and remains pasteable after the
    // session's export handle goes away.
    let export = Export::begin(Snapshot::capture().unwrap()).unwrap();
    publish(
        &export,
        super::super::export::prepare_content(&image).unwrap(),
    )
    .unwrap();
    drop(export);
    assert!(matches!(
        read(Arc::new(AtomicBool::new(false))),
        Ok(Some(SelectionData::Content(_)))
    ));
}
