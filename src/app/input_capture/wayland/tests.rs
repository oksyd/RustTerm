use super::*;
use std::{
    io::{Read, Write},
    os::{fd::AsRawFd, unix::net::UnixStream},
    sync::mpsc,
    thread,
    time::Duration,
};
use wayland_client::protocol::wl_compositor;

#[test]
fn surface_lease_rejects_overlap_and_releases_on_drop() {
    let first = SurfaceLease::acquire(1, 2).unwrap();
    assert!(SurfaceLease::acquire(1, 2).is_err());
    // Failed acquisition must not release the original reservation.
    assert!(SurfaceLease::acquire(1, 2).is_err());
    drop(first);
    assert!(SurfaceLease::acquire(1, 2).is_ok());
}

#[test]
fn surface_lease_is_scoped_to_both_display_and_surface() {
    let _first = SurfaceLease::acquire(10, 20).unwrap();
    let _other_surface = SurfaceLease::acquire(10, 21).unwrap();
    let _other_display = SurfaceLease::acquire(11, 20).unwrap();
}

// A deliberately small wire peer, not a compositor. It exercises the actual
// libwayland foreign-display queue and protocol serializers with no display,
// GPU, global input grab, server framework, or environment changes.
fn word(bytes: &[u8], index: usize) -> u32 {
    u32::from_ne_bytes(bytes[index * 4..index * 4 + 4].try_into().unwrap())
}

fn words(values: &[u32]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_ne_bytes())
        .collect()
}

fn string(value: &str) -> Vec<u8> {
    let mut bytes = words(&[value.len() as u32 + 1]);
    bytes.extend_from_slice(value.as_bytes());
    bytes.push(0);
    bytes.resize(bytes.len().next_multiple_of(4), 0);
    bytes
}

fn event(stream: &mut UnixStream, object: u32, opcode: u16, payload: &[u8]) {
    let mut bytes = words(&[
        object,
        ((payload.len() as u32 + 8) << 16) | u32::from(opcode),
    ]);
    bytes.extend_from_slice(payload);
    stream.write_all(&bytes).unwrap();
}

enum ServerCommand {
    Keys(Vec<(u32, bool)>, mpsc::Sender<()>),
    Finish { revoke: bool },
}

fn mock_server(
    mut stream: UnixStream,
    commands: mpsc::Receiver<ServerCommand>,
    initially_held: Vec<u32>,
) -> Vec<&'static str> {
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut objects = BTreeMap::from([(1, "wl_display")]);
    let mut surface = 0;
    let mut keyboard = 0;
    let mut destroyed = Vec::new();
    loop {
        let mut header = [0; 8];
        stream.read_exact(&mut header).unwrap();
        let object = word(&header, 0);
        let opcode = word(&header, 1) as u16;
        let size = (word(&header, 1) >> 16) as usize;
        assert!((8..=4096).contains(&size));
        let mut payload = vec![0; size - 8];
        stream.read_exact(&mut payload).unwrap();
        match (objects[&object], opcode) {
            ("wl_display", 1) => {
                let registry = word(&payload, 0);
                objects.insert(registry, "wl_registry");
                for (name, interface, version) in [
                    (1, "wl_compositor", 4),
                    (2, "wl_seat", 7),
                    (3, "zwp_keyboard_shortcuts_inhibit_manager_v1", 1),
                ] {
                    let mut global = words(&[name]);
                    global.extend(string(interface));
                    global.extend(words(&[version]));
                    event(&mut stream, registry, 0, &global);
                }
            }
            ("wl_display", 0) => {
                let callback = word(&payload, 0);
                event(&mut stream, callback, 0, &words(&[0]));
                event(&mut stream, 1, 1, &words(&[callback]));
            }
            ("wl_registry", 0) => {
                let name = word(&payload, 0);
                let interface_len = word(&payload, 1) as usize;
                let new_id_offset = 3 + interface_len.div_ceil(4);
                let new_id = word(&payload, new_id_offset);
                let interface = match name {
                    1 => "wl_compositor",
                    2 => "wl_seat",
                    3 => "zwp_keyboard_shortcuts_inhibit_manager_v1",
                    _ => panic!("unexpected global"),
                };
                objects.insert(new_id, interface);
                if name == 2 {
                    event(&mut stream, new_id, 0, &words(&[2])); // Keyboard capability.
                }
            }
            ("wl_compositor", 0) => {
                surface = word(&payload, 0);
                objects.insert(surface, "wl_surface");
            }
            ("wl_seat", 1) => {
                keyboard = word(&payload, 0);
                objects.insert(keyboard, "wl_keyboard");
                assert_ne!(surface, 0);
                let mut enter = words(&[1, surface, initially_held.len() as u32 * 4]);
                enter.extend(words(&initially_held));
                event(&mut stream, keyboard, 1, &enter);
            }
            ("zwp_keyboard_shortcuts_inhibit_manager_v1", 1) => {
                let inhibitor = word(&payload, 0);
                assert_eq!(word(&payload, 1), surface);
                assert_eq!(objects[&word(&payload, 2)], "wl_seat");
                objects.insert(inhibitor, "zwp_keyboard_shortcuts_inhibitor_v1");
                event(&mut stream, inhibitor, 0, &[]); // System confirmation.
                loop {
                    match commands.recv_timeout(Duration::from_secs(3)).unwrap() {
                        ServerCommand::Keys(keys, done) => {
                            for (key, pressed) in keys {
                                event(
                                    &mut stream,
                                    keyboard,
                                    3,
                                    &words(&[1, 0, key, u32::from(pressed)]),
                                );
                            }
                            done.send(()).unwrap();
                        }
                        ServerCommand::Finish { revoke } => {
                            if revoke {
                                event(&mut stream, inhibitor, 1, &[]); // System revocation.
                            }
                            break;
                        }
                    }
                }
            }
            ("zwp_keyboard_shortcuts_inhibitor_v1", 0)
            | ("zwp_keyboard_shortcuts_inhibit_manager_v1", 0)
            | ("wl_keyboard", 0)
            | ("wl_seat", 3) => {
                destroyed.push(objects[&object]);
                if destroyed.len() == 4 {
                    return destroyed;
                }
            }
            request => panic!("unexpected request {request:?}"),
        }
    }
}

struct Original;

wayland_client::delegate_noop!(Original: ignore wl_registry::WlRegistry);
wayland_client::delegate_noop!(Original: ignore wl_compositor::WlCompositor);
wayland_client::delegate_noop!(Original: ignore wl_surface::WlSurface);

fn drive_until(
    original: &Connection,
    original_queue: &mut EventQueue<Original>,
    capture: &mut Capture,
    expected: CaptureStatus,
) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        read_ready(original, original_queue, 5);
        if capture.poll_status() == expected {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "native capture did not reach {expected:?}"
        );
    }
}

fn read_ready(original: &Connection, queue: &mut EventQueue<Original>, wait_ms: i32) -> bool {
    queue.dispatch_pending(&mut Original).unwrap();
    original.flush().unwrap();
    // Simulate GPUI's normal display reader. The capture itself only drains
    // its own pending queue; this verifies that foreign queue separation.
    if let Some(reader) = original.prepare_read() {
        let mut fd = libc::pollfd {
            fd: original.backend().poll_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one live stack pollfd, with a bounded wait and no GUI.
        if unsafe { libc::poll(&mut fd, 1, wait_ms) } > 0 {
            reader.read().unwrap();
            return true;
        }
    }
    false
}

fn send_keys(
    original: &Connection,
    queue: &mut EventQueue<Original>,
    capture: &mut Capture,
    control: &mpsc::Sender<ServerCommand>,
    keys: &[(u32, bool)],
) {
    let (done, ready) = mpsc::channel();
    control
        .send(ServerCommand::Keys(keys.to_vec(), done))
        .unwrap();
    // All bytes are on the socket before draining, so negative assertions need
    // no arbitrary sleep or assumption about the server thread's scheduling.
    ready.recv_timeout(Duration::from_secs(3)).unwrap();
    while read_ready(original, queue, 0) {
        assert_eq!(capture.poll_status(), CaptureStatus::Active);
    }
    assert_eq!(capture.poll_status(), CaptureStatus::Active);
}

fn protocol_lifecycle(
    revoke: bool,
    initially_held: Vec<u32>,
    exercise: impl FnOnce(
        &Connection,
        &mut EventQueue<Original>,
        &mut Capture,
        &mpsc::Sender<ServerCommand>,
    ),
) {
    let (client, server) = UnixStream::pair().unwrap();
    let (control, request) = mpsc::channel();
    let server = thread::spawn(move || mock_server(server, request, initially_held));
    let original = Connection::from_socket(client).unwrap();
    let mut queue = original.new_event_queue::<Original>();
    let handle = queue.handle();
    let registry = original.display().get_registry(&handle, ());
    let compositor = registry.bind::<wl_compositor::WlCompositor, _, _>(1, 4, &handle, ());
    let surface = compositor.create_surface(&handle, ());
    // SAFETY: both native objects belong to original, which outlives capture.
    let mut capture = unsafe {
        Capture::from_handles(
            NonNull::new(original.backend().display_ptr().cast()).unwrap(),
            NonNull::new(surface.id().as_ptr().cast()).unwrap(),
        )
    }
    .unwrap();
    assert_eq!(capture.state.status, CaptureStatus::Pending);
    drive_until(&original, &mut queue, &mut capture, CaptureStatus::Active);
    assert!(
        surface.is_alive(),
        "borrowing must not replace the original surface"
    );
    exercise(&original, &mut queue, &mut capture, &control);
    control.send(ServerCommand::Finish { revoke }).unwrap();
    if revoke {
        drive_until(&original, &mut queue, &mut capture, CaptureStatus::Denied);
        capture.release(); // Repeated release must send no duplicate destructor.
    }
    drop(capture);
    assert!(
        surface.is_alive(),
        "capture destruction must not destroy GPUI's surface"
    );
    let destroyed = server.join().unwrap();
    assert_eq!(destroyed[0], "zwp_keyboard_shortcuts_inhibitor_v1");
    assert_eq!(destroyed.len(), 4);
}

#[test]
fn foreign_queue_observes_system_activation_and_revocation() {
    protocol_lifecycle(true, Vec::new(), |_, _, _, _| {});
}

#[test]
fn dropping_active_capture_releases_protocol_objects() {
    protocol_lifecycle(false, Vec::new(), |_, _, _, _| {});
}

const RELEASE_CHORD: &[(u32, bool)] = &[(29, true), (56, true), (29, false), (56, false)];

#[test]
fn raw_keyboard_release_rejects_altgr_chords_and_pointer_interruption() {
    protocol_lifecycle(false, Vec::new(), |original, queue, capture, control| {
        send_keys(original, queue, capture, control, RELEASE_CHORD);
        assert!(capture.take_release_request());
        assert!(!capture.take_release_request());

        // Right Alt / AltGr, and Ctrl+Alt+Delete, must stay remote shortcuts.
        for keys in [
            &[(100, true), (100, false)][..],
            &[
                (29, true),
                (56, true),
                (111, true),
                (111, false),
                (29, false),
                (56, false),
            ][..],
        ] {
            send_keys(original, queue, capture, control, keys);
            assert!(!capture.take_release_request());
        }
        send_keys(original, queue, capture, control, &RELEASE_CHORD[..2]);
        capture.interrupt_release_gesture();
        send_keys(original, queue, capture, control, &RELEASE_CHORD[2..]);
        assert!(!capture.take_release_request());

        // A held mouse button/composition blocks every attempt until released.
        capture.set_release_blocked(true);
        for _ in 0..2 {
            send_keys(original, queue, capture, control, RELEASE_CHORD);
            assert!(!capture.take_release_request());
        }
        capture.set_release_blocked(false);
        send_keys(original, queue, capture, control, RELEASE_CHORD);
        assert!(capture.take_release_request());
    });
}

#[test]
fn keyboard_enter_held_modifiers_do_not_form_a_new_release_gesture() {
    protocol_lifecycle(false, vec![29, 56], |original, queue, capture, control| {
        assert!(capture.has_held_keys());
        send_keys(original, queue, capture, control, &RELEASE_CHORD[2..]);
        assert!(!capture.has_held_keys());
        assert!(!capture.take_release_request());
        send_keys(original, queue, capture, control, RELEASE_CHORD);
        assert!(capture.take_release_request());
    });
}
