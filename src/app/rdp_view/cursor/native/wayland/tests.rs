use super::*;
use std::{
    io::Read,
    os::{fd::AsRawFd, unix::net::UnixStream},
    time::{Duration, Instant},
};

struct Original;
wayland_client::delegate_noop!(Original: ignore wl_registry::WlRegistry);
wayland_client::delegate_noop!(Original: ignore wl_compositor::WlCompositor);
wayland_client::delegate_noop!(Original: ignore wl_surface::WlSurface);

fn words(values: &[u32]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_ne_bytes())
        .collect()
}
fn word(bytes: &[u8], index: usize) -> u32 {
    u32::from_ne_bytes(bytes[index * 4..index * 4 + 4].try_into().unwrap())
}
fn ready(fd: i32) -> bool {
    let mut poll = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid stack pollfd; no wait or process-wide state changes.
    unsafe { libc::poll(&mut poll, 1, 0) > 0 }
}

/// Socket-only protocol peer, including the foreign-display queue used in GPUI.
/// It records cursor requests without a compositor, global input or display.
struct Peer {
    stream: UnixStream,
    objects: BTreeMap<u32, &'static str>,
    target: u32,
    pointer: u32,
    cursor_surface: u32,
    sets: Vec<(u32, u32, i32, i32)>,
    buffers: usize,
    destroyed: Vec<(&'static str, u32)>,
}
impl Peer {
    fn event(&mut self, object: u32, opcode: u16, payload: &[u8]) {
        let mut bytes = words(&[
            object,
            ((payload.len() as u32 + 8) << 16) | u32::from(opcode),
        ]);
        bytes.extend_from_slice(payload);
        self.stream.write_all(&bytes).unwrap();
    }
    fn enter(&mut self, surface: u32, serial: u32) {
        self.event(
            self.pointer,
            0,
            &words(&[serial, surface, 20 * 256, 30 * 256]),
        );
    }
    fn motion(&mut self, x: i32, y: i32) {
        self.event(
            self.pointer,
            2,
            &words(&[1, (x * 256) as u32, (y * 256) as u32]),
        );
    }
    fn drain(&mut self) {
        while ready(self.stream.as_raw_fd()) {
            let mut header = [0; 8];
            self.stream.read_exact(&mut header).unwrap();
            let id = word(&header, 0);
            let opcode = word(&header, 1) as u16;
            let size = (word(&header, 1) >> 16) as usize;
            assert!((8..=4096).contains(&size));
            let mut payload = vec![0; size - 8];
            self.stream.read_exact(&mut payload).unwrap();
            match (self.objects[&id], opcode) {
                ("display", 1) => {
                    let registry = word(&payload, 0);
                    self.objects.insert(registry, "registry");
                    for (name, interface, version) in
                        [(1, "wl_compositor", 4), (2, "wl_seat", 5), (3, "wl_shm", 1)]
                    {
                        let mut data = words(&[name, interface.len() as u32 + 1]);
                        data.extend(interface.as_bytes());
                        data.push(0);
                        data.resize(data.len().next_multiple_of(4), 0);
                        data.extend(words(&[version]));
                        self.event(registry, 0, &data);
                    }
                }
                ("registry", 0) => {
                    let new_id = word(&payload, payload.len() / 4 - 1);
                    let kind = match word(&payload, 0) {
                        1 => "compositor",
                        2 => "seat",
                        3 => "shm",
                        _ => panic!("unknown global"),
                    };
                    self.objects.insert(new_id, kind);
                    if kind == "seat" {
                        self.event(new_id, 0, &words(&[1]));
                    }
                }
                ("compositor", 0) => {
                    let surface = word(&payload, 0);
                    self.objects.insert(surface, "surface");
                    if self.target == 0 {
                        self.target = surface;
                    } else {
                        self.cursor_surface = surface;
                    }
                }
                ("seat", 0) => {
                    self.pointer = word(&payload, 0);
                    self.objects.insert(self.pointer, "pointer");
                    self.enter(self.target, 42);
                }
                ("shm", 0) => {
                    self.objects.insert(word(&payload, 0), "pool");
                }
                ("pool", 0) => {
                    self.objects.insert(word(&payload, 0), "buffer");
                    self.buffers += 1;
                }
                ("surface", 1 | 6 | 8 | 9) => {}
                ("pointer", 0) => {
                    self.sets.push((
                        word(&payload, 0),
                        word(&payload, 1),
                        word(&payload, 2) as i32,
                        word(&payload, 3) as i32,
                    ));
                }
                ("pointer", 1) | ("seat", 3) | ("surface" | "buffer", 0) | ("pool", 1) => {
                    self.destroyed.push((self.objects[&id], id));
                    self.event(1, 1, &words(&[id]));
                }
                request => panic!("unexpected cursor request {request:?}"),
            }
        }
    }
}

struct Fixture {
    peer: Peer,
    original: Connection,
    queue: EventQueue<Original>,
    target: wl_surface::WlSurface,
    cursor: Option<Cursor>,
}
impl Fixture {
    fn new() -> Self {
        let (client, server) = UnixStream::pair().unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let original = Connection::from_socket(client).unwrap();
        let queue = original.new_event_queue();
        let qh = queue.handle();
        let registry = original.display().get_registry(&qh, ());
        let compositor = registry.bind::<wl_compositor::WlCompositor, _, _>(1, 4, &qh, ());
        let target = compositor.create_surface(&qh, ());
        // SAFETY: original and target outlive the cursor, as in GPUI.
        let cursor = unsafe {
            Cursor::from_handles(
                NonNull::new(original.backend().display_ptr().cast()).unwrap(),
                NonNull::new(target.id().as_ptr().cast()).unwrap(),
            )
        }
        .unwrap();
        Self {
            peer: Peer {
                stream: server,
                objects: BTreeMap::from([(1, "display")]),
                target: 0,
                pointer: 0,
                cursor_surface: 0,
                sets: Vec::new(),
                buffers: 0,
                destroyed: Vec::new(),
            },
            original,
            queue,
            target,
            cursor: Some(cursor),
        }
    }
    fn pump(&mut self) {
        // Simulate GPUI reading the display; cursor.poll itself never reads it.
        for _ in 0..8 {
            self.original.flush().unwrap();
            self.peer.drain();
            self.queue.dispatch_pending(&mut Original).unwrap();
            if let Some(reader) = self.original.prepare_read()
                && ready(self.original.backend().poll_fd().as_raw_fd())
            {
                reader.read().unwrap();
            }
            if let Some(cursor) = &mut self.cursor {
                cursor.poll().unwrap();
            }
        }
    }
    fn setup(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while self
            .cursor
            .as_ref()
            .unwrap()
            .state
            .seats
            .values()
            .all(|seat| seat.serial.is_none())
        {
            self.pump();
            assert!(Instant::now() < deadline);
        }
        let cursor = self.cursor.as_mut().unwrap();
        cursor.set_bounds(
            Some(Bounds::new(
                point(px(10.), px(10.)),
                gpui::size(px(200.), px(150.)),
            )),
            1.,
        );
        assert!(
            cursor
                .prepare(&Image {
                    width: 4,
                    height: 4,
                    hotspot: (1, 1),
                    scale: 2,
                    pixels: vec![255; 64]
                })
                .unwrap()
        );
    }
}

#[test]
fn pointer_motion_needs_no_frames_uploads_or_repeated_cursor_requests() {
    let mut f = Fixture::new();
    f.setup();
    assert!(f.cursor.as_mut().unwrap().show(true).unwrap());
    f.pump();
    assert_eq!(f.peer.sets, [(42, f.peer.cursor_surface, 1, 1)]);
    for x in 20..120 {
        f.peer.motion(x, 30);
        f.pump();
        assert!(f.cursor.as_mut().unwrap().show(true).unwrap());
    }
    f.pump();
    assert_eq!(f.peer.buffers, 1);
    assert_eq!(f.peer.sets.len(), 1);
    assert!(f.target.is_alive());
    drop(f.cursor.take());
    f.pump();
    assert!(f.target.is_alive());
    assert!(
        f.peer
            .destroyed
            .contains(&("surface", f.peer.cursor_surface))
    );
    assert!(!f.peer.destroyed.contains(&("surface", f.peer.target)));
}

#[test]
fn leaving_reentering_and_software_fallback_restore_cursor_ownership() {
    let mut f = Fixture::new();
    f.setup();
    assert!(f.cursor.as_mut().unwrap().show(true).unwrap());
    f.pump();
    f.peer.motion(5, 5);
    f.pump(); // GPUI now restores the local control's cursor.
    f.cursor.as_mut().unwrap().hide(false).unwrap();
    f.pump();
    assert_eq!(f.peer.sets.len(), 1, "must not hide GPUI's local cursor");
    f.peer.motion(20, 30);
    f.pump();
    assert!(f.cursor.as_mut().unwrap().show(true).unwrap());
    f.pump();
    assert_eq!(f.peer.sets.len(), 2);
    f.cursor.as_mut().unwrap().hide(true).unwrap();
    f.pump();
    assert_eq!(f.peer.sets.last(), Some(&(42, 0, 0, 0)));
    f.peer
        .event(f.peer.pointer, 1, &words(&[43, f.peer.target]));
    f.pump();
    assert!(!f.cursor.as_mut().unwrap().show(true).unwrap());
    f.peer.enter(f.peer.target, 99);
    f.pump();
    assert!(f.cursor.as_mut().unwrap().show(true).unwrap());
    f.pump();
    assert_eq!(f.peer.sets.last(), Some(&(99, f.peer.cursor_surface, 1, 1)));
    drop(f.cursor.take());
    f.pump();
}
