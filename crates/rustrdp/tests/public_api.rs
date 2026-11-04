//! Consumer-side checks: these tests can only use the published API.

use rustrdp::{ClientConfig, Connector, Error, Event, Input, SecurityProtocol, Session, wire};

const SERVER: &[u8] = include_bytes!("../examples/fixtures/server.rdp");
const CONFIRM: &[u8] = &[3, 0, 0, 19, 14, 0xd0, 0, 0, 0, 0, 0, 2, 0, 8, 0, 2, 0, 0, 0];

fn config() -> ClientConfig {
    ClientConfig {
        username: "offline-consumer".into(),
        width: 320,
        height: 200,
        ..ClientConfig::default()
    }
}

fn connector() -> Connector {
    let security = rustrdp::connection::parse_negotiation_response(CONFIRM).unwrap();
    assert_eq!(security, SecurityProtocol::Hybrid);
    Connector::new(config(), security).unwrap()
}

fn frames() -> impl Iterator<Item = &'static [u8]> {
    let mut remaining = SERVER;
    std::iter::from_fn(move || {
        let (_, length) = wire::frame_size(remaining, config().limits.max_pdu).unwrap()?;
        let (frame, rest) = remaining.split_at(length);
        remaining = rest;
        Some(frame)
    })
}

fn establish(mut connector: Connector) -> Session {
    assert!(!connector.start().unwrap().is_empty());
    for frame in frames() {
        let output = connector.advance(frame).unwrap();
        if let Some(session) = output.session {
            return session;
        }
    }
    panic!("transcript never activated a desktop");
}

#[test]
fn transport_chunk_boundaries_preserve_early_graphics_and_session_lifecycle() {
    for chunk_size in [1, 3, 47, SERVER.len()] {
        let mut connector = connector();
        let mut outbound = vec![connector.start().unwrap()];
        let mut session: Option<Session> = None;
        let mut buffer = Vec::new();
        let mut frame_count = 0;
        let mut early_graphics = 0;
        let mut disconnects = 0;
        for chunk in SERVER.chunks(chunk_size) {
            buffer.extend_from_slice(chunk);
            while let Some((_, length)) =
                wire::frame_size(&buffer, config().limits.max_pdu).unwrap()
            {
                if buffer.len() < length {
                    break;
                }
                let frame = &buffer[..length];
                let events = if let Some(session) = &mut session {
                    session.process(frame).unwrap()
                } else {
                    let output = connector.advance(frame).unwrap();
                    outbound.extend(output.packets);
                    if let Some(established) = output.session {
                        assert!(established.is_active());
                        assert_eq!(
                            (established.info().width(), established.info().height()),
                            (320, 200)
                        );
                        let graphics = established.graphics();
                        assert_eq!(graphics.pixels().len(), 320 * 200 * 4);
                        let pixel_offset = (4 * 320 + 3) * 4;
                        assert_eq!(
                            &graphics.pixels()[pixel_offset..pixel_offset + 8],
                            &[0x11, 0x22, 0x33, 255, 0x44, 0x55, 0x66, 255],
                        );
                        assert!(graphics.hide_local_pointer());
                        session = Some(established);
                    } else {
                        early_graphics += output
                            .events
                            .iter()
                            .filter(|event| matches!(event, Event::GraphicsChanged))
                            .count();
                    }
                    output.events
                };
                for event in events {
                    match event {
                        Event::Send(packet) => outbound.push(packet),
                        Event::Disconnected { reason } => {
                            assert_eq!(reason, 3);
                            disconnects += 1;
                        }
                        _ => {}
                    }
                }
                buffer.drain(..length);
                frame_count += 1;
            }
        }
        assert!(buffer.is_empty());
        assert_eq!(frame_count, 16);
        assert_eq!(early_graphics, 2);
        assert_eq!(disconnects, 1);
        for packet in outbound {
            assert_eq!(
                wire::frame_size(&packet, config().limits.max_pdu)
                    .unwrap()
                    .unwrap()
                    .1,
                packet.len()
            );
        }
        let mut session = session.unwrap();
        assert!(!session.is_active());
        assert!(!session.graphics().hide_local_pointer());
        assert!(session.input(Input::ReleaseAll).is_err());
        assert!(session.process(&[0, 5, 6, 0, 0]).is_err());
    }
}

#[test]
fn input_errors_leave_session_usable_but_protocol_errors_close_it() {
    let mut session = establish(connector());
    assert_eq!(
        session.input(Input::Key {
            scancode: 0x1234,
            pressed: true
        }),
        Err(Error::Invalid("keyboard scan code")),
    );
    assert!(session.is_active());
    let pressed = session
        .input(Input::Key {
            scancode: 0x1e,
            pressed: true,
        })
        .unwrap();
    assert!(matches!(pressed.as_slice(), [Event::Send(_)]));
    assert!(matches!(
        session.input(Input::ReleaseAll).unwrap().as_slice(),
        [Event::Send(_)]
    ));
    assert!(session.input(Input::ReleaseAll).unwrap().is_empty());

    // Complete fast-path frame, incomplete pointer-position update.
    assert!(session.process(&[0, 6, 8, 1, 0, 0]).is_err());
    assert!(!session.is_active());
    assert!(session.set_clipboard("after failure".into()).is_err());
    assert!(session.resize(640, 480).is_err());
}

#[test]
fn clipboard_publication_completes_only_after_its_own_format_response() {
    let mut session = establish(connector());
    // Fixed MCS SendDataIndication from server 1002 on cliprdr channel 1004,
    // carrying a complete eight-byte CLIPRDR message. No internal API is used.
    let message = |kind: u8, flags: u8| {
        [
            3, 0, 0, 30, 2, 0xf0, 0x80, 0x68, 0, 1, 3, 0xec, 0x70, 16, 8, 0, 0, 0, 3, 0, 0, 0,
            kind, 0, flags, 0, 0, 0, 0, 0,
        ]
    };
    session.process(&message(1, 0)).unwrap(); // Monitor ready; advertise empty list.
    session.set_clipboard("Ubuntu → Windows".into()).unwrap();
    assert!(!session.clipboard_ready());
    let outputs = session.process(&message(3, 1)).unwrap(); // Initial empty-list ACK.
    assert!(!outputs.contains(&Event::ClipboardPublished));
    assert!(!session.clipboard_ready());

    assert_eq!(
        session.process(&message(3, 1)).unwrap(),
        [Event::ClipboardPublished]
    );
    assert!(session.clipboard_ready());
    assert_eq!(
        session.set_clipboard("Ubuntu → Windows".into()).unwrap(),
        [Event::ClipboardPublished]
    );

    session.set_clipboard("later".into()).unwrap();
    let outputs = session.process(&message(2, 0)).unwrap(); // Remote ownership supersedes it.
    assert!(outputs.contains(&Event::ClipboardPublishRejected));
    assert!(!session.clipboard_ready());
    assert!(
        !session
            .process(&message(3, 1))
            .unwrap()
            .contains(&Event::ClipboardPublished)
    );
    assert!(session.is_active());
}

#[test]
fn framing_bounds_declared_bodies_before_allocating_them() {
    assert_eq!(wire::frame_size(&[3, 0, 0], 1024), Ok(None));
    assert_eq!(
        wire::frame_size(&[3, 0, 0, 19], 1024),
        Ok(Some((wire::FrameKind::X224, 19)))
    );
    assert!(matches!(
        wire::frame_size(&[3, 0, 0xff, 0xff], 1024),
        Err(Error::Limit(_))
    ));
    assert_eq!(wire::credssp_size(&[0x30, 0x82, 0x01], 1024), Ok(None));
    assert!(matches!(
        wire::credssp_size(&[0x30, 0x82, 0xff, 0xff], 1024),
        Err(Error::Limit(_))
    ));
}

#[cfg(feature = "licensing")]
#[test]
fn optional_license_client_accepts_servers_that_need_no_cal_exchange() {
    use rustrdp::connection::licensing::{LicenseEntropy, LicenseIdentity};

    // Fixed values are ONLY for this offline fixture. A live caller must supply
    // fresh CSPRNG output and an identity persisted with its server-bound CAL.
    let connector = connector()
        .with_licensing(
            LicenseIdentity {
                platform_id: 0x0401_0000,
                hardware_id: [1; 16],
            },
            LicenseEntropy::new([2; 32], [3; 48]),
            None,
        )
        .unwrap();
    assert!(establish(connector).is_active());
}
