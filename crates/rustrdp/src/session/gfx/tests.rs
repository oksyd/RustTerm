use crate::{Channel, Event, GraphicsCodec, Input, MouseButton, Session, wire};

fn channel_packet(payload: &[u8]) -> Vec<u8> {
    let mut body = (payload.len() as u32).to_le_bytes().to_vec();
    body.extend(3u32.to_le_bytes());
    body.extend(payload);
    let mut packet = wire::mcs_send_data(1002, 1004, &body).unwrap();
    packet[7] = 0x68;
    packet
}

fn pdu(kind: u16, body: &[u8]) -> Vec<u8> {
    let mut bytes = kind.to_le_bytes().to_vec();
    bytes.extend([0, 0]);
    bytes.extend(((body.len() + 8) as u32).to_le_bytes());
    bytes.extend(body);
    bytes
}

fn graphics_packet(commands: impl IntoIterator<Item = Vec<u8>>) -> Vec<u8> {
    let mut bytes = vec![0x30, 7, 0xe0, 4];
    for command in commands {
        bytes.extend(command);
    }
    channel_packet(&bytes)
}

fn reset(width: u32, height: u32) -> Vec<u8> {
    let mut body = Vec::new();
    for value in [width, height, 0] {
        body.extend(value.to_le_bytes());
    }
    body.resize(332, 0);
    pdu(14, &body)
}

fn mouse_events(packet: &[u8], fastpath: bool) -> Vec<(u16, u16, u16)> {
    let mut slow = packet.to_vec();
    let (count, mut reader) = if fastpath {
        let mut reader = wire::Reader::new(packet);
        let count = (reader.u8().unwrap() >> 2) & 15;
        assert_ne!(count, 0);
        assert_eq!(wire::per_length(&mut reader).unwrap(), packet.len());
        (usize::from(count), reader)
    } else {
        slow[7] = 0x68;
        let mut reader = wire::Reader::new(wire::mcs_indication(&slow).unwrap().2);
        reader.skip(14).unwrap();
        assert_eq!(reader.u8().unwrap(), 28); // PDUTYPE2_INPUT
        reader.skip(3).unwrap();
        let count = usize::from(reader.u16_le().unwrap());
        reader.skip(2).unwrap();
        (count, reader)
    };
    let events = (0..count)
        .map(|_| {
            if fastpath {
                assert_eq!(reader.u8().unwrap(), 0x20); // FASTPATH_INPUT_EVENT_MOUSE
            } else {
                reader.skip(4).unwrap();
                assert_eq!(reader.u16_le().unwrap(), 0x8001); // INPUT_EVENT_MOUSE
            }
            (
                reader.u16_le().unwrap(),
                reader.u16_le().unwrap(),
                reader.u16_le().unwrap(),
            )
        })
        .collect();
    reader.finish().unwrap();
    events
}

fn frame(id: u32, pixel: [u8; 4]) -> Vec<Vec<u8>> {
    let mut begin = vec![0; 4];
    begin.extend(id.to_le_bytes());
    let mut bitmap = vec![1, 0, 0, 0, 0x20, 0, 0, 0, 0, 2, 0, 2, 0];
    bitmap.extend(16u32.to_le_bytes());
    for _ in 0..4 {
        bitmap.extend(pixel);
    }
    vec![pdu(11, &begin), pdu(1, &bitmap), pdu(12, &id.to_le_bytes())]
}

fn outgoing_dynamic(events: Vec<Event>) -> Vec<Vec<u8>> {
    events
        .into_iter()
        .map(|event| {
            let Event::Send(mut packet) = event else {
                panic!("expected graphics channel packet")
            };
            packet[7] = 0x68;
            let (initiator, channel, payload) = wire::mcs_indication(&packet).unwrap();
            assert_eq!((initiator, channel), (1001, 1004));
            let mut reader = wire::Reader::new(payload);
            let size = reader.u32_le().unwrap() as usize;
            assert_eq!(reader.u32_le().unwrap() & 3, 3);
            let bytes = reader.take(size).unwrap().to_vec();
            reader.finish().unwrap();
            bytes
        })
        .collect()
}

fn session() -> Session {
    let baseline = crate::session::tests::session();
    let mut config = baseline.config;
    config.graphics_pipeline = true;
    config.allow_lossy_bitmaps = true;
    config.frame_acknowledgements = std::num::NonZeroU32::new(3);
    let mut info = baseline.info;
    info.channels.push(Channel {
        id: 1004,
        name: "drdynvc".into(),
    });
    assert!(!info.frame_acknowledgements);
    let mut session = Session::new(config, info).unwrap();
    session.process(&channel_packet(&[0x50, 0, 1, 0])).unwrap();
    let mut create = vec![0x10, 7];
    create.extend(b"Microsoft::Windows::RDS::Graphics\0");
    let responses = outgoing_dynamic(session.process(&channel_packet(&create)).unwrap());
    assert_eq!(responses.len(), 2);
    assert_eq!(responses[0], [0x10, 7, 0, 0, 0, 0]);
    assert_eq!(&responses[1][..4], &[0x30, 7, 0x12, 0]);

    let mut caps = Vec::new();
    for value in [0x000a_0701u32, 4, 0xc2] {
        caps.extend(value.to_le_bytes());
    }
    session
        .process(&graphics_packet([
            pdu(19, &caps),
            reset(2, 2),
            pdu(9, &[1, 0, 2, 0, 2, 0, 0x20]),
            pdu(15, &[1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
        ]))
        .unwrap();
    session
}

#[test]
fn graphics_resize_updates_concurrent_mouse_bounds_without_reactivation() {
    for fastpath in [false, true] {
        let mut session = session();
        session.info.fastpath_input = fastpath;
        session
            .process(&graphics_packet([reset(1280, 720)]))
            .unwrap();
        session.resume_input();
        let input = session.input_controller();
        let held = input
            .encode(Input::MouseButton {
                button: MouseButton::Left,
                pressed: true,
                x: 1279,
                y: 719,
            })
            .unwrap()
            .unwrap();
        let mut previous = (1279, 719);
        for (width, height) in [(1920, 1080), (800, 600), (1600, 900)] {
            let events = session
                .process(&graphics_packet([reset(width, height)]))
                .unwrap();
            let (width, height) = (width as u16, height as u16);
            assert!(events.contains(&Event::Resized { width, height }));
            assert_eq!((session.info.width, session.info.height), (width, height));
            // RESETGRAPHICS keeps the session active; no resume_input call is
            // needed, and previously accepted button presses must survive.
            assert!(session.is_active());
            assert!(held.generation.is_valid());
            assert!(session.keep_awake().unwrap().is_empty());
            let release = input.encode(Input::ReleaseAll).unwrap().unwrap();
            assert_eq!(release.packets.len(), 1);
            assert_eq!(
                mouse_events(&release.packets[0], fastpath),
                [(
                    0x1000,
                    previous.0.min(width - 1),
                    previous.1.min(height - 1)
                )]
            );
            let motion = input
                .encode(Input::Pointer {
                    x: u16::MAX,
                    y: u16::MAX,
                })
                .unwrap()
                .unwrap();
            assert_eq!(motion.packets.len(), 1);
            assert_eq!(
                mouse_events(&motion.packets[0], fastpath),
                [(0x0800, width - 1, height - 1)]
            );
            session.sync_input_pointer().unwrap();
            assert_eq!(session.graphics.pointer_position(), (width - 1, height - 1));
            input
                .encode(Input::MouseButton {
                    button: MouseButton::Left,
                    pressed: true,
                    x: width - 1,
                    y: height - 1,
                })
                .unwrap()
                .unwrap();
            previous = (width - 1, height - 1);
        }
    }
}

#[test]
fn graphics_channel_frames_acknowledge_after_consumption_without_legacy_capability() {
    let mut session = session();
    let packet = graphics_packet(
        frame(70, [1, 2, 3, 0])
            .into_iter()
            .chain(frame(2, [4, 5, 6, 0])),
    );
    let mut frames = Vec::new();
    let events = session
        .process_with_frames(&packet, |frame| {
            frames.push((
                frame.id,
                frame.graphics.pixels().to_vec(),
                frame.acknowledgement.unwrap(),
            ));
        })
        .unwrap();
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0].0, 70);
    assert_eq!(&frames[0].1[..4], &[1, 2, 3, 255]);
    assert_eq!(frames[1].0, 2);
    assert_eq!(&frames[1].1[..4], &[4, 5, 6, 255]);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::Send(_) | Event::SendTunnel { .. }))
    );
    let receipts: Vec<_> = events
        .into_iter()
        .filter_map(|event| match event {
            Event::GraphicsFrameEnd {
                acknowledgement, ..
            } => acknowledgement,
            _ => None,
        })
        .collect();
    assert_eq!(receipts, [frames[0].2.clone(), frames[1].2.clone()]);
    assert_eq!(session.graphics_codec(), Some(GraphicsCodec::Raw));
    let packets = outgoing_dynamic(session.acknowledge_frame(&frames[1].2).unwrap());
    assert_eq!(packets.len(), 2);
    for (packet, (id, total)) in packets.iter().zip([(70u32, 1u32), (2, 2)]) {
        let mut expected = vec![0x30, 7];
        let mut ack = 0u32.to_le_bytes().to_vec();
        ack.extend(id.to_le_bytes());
        ack.extend(total.to_le_bytes());
        expected.extend(pdu(13, &ack));
        assert_eq!(packet, &expected);
    }
    assert!(session.acknowledge_frame(&frames[0].2).unwrap().is_empty());
    assert!(session.acknowledge_frame(&frames[1].2).unwrap().is_empty());
}

#[test]
fn copied_graphics_output_acknowledges_only_completed_frames_before_presentation() {
    let mut session = session();
    let mut next = frame(20, [8, 9, 10, 0]);
    let next_end = next.pop().unwrap();
    let packet = graphics_packet(frame(19, [1, 2, 3, 0]).into_iter().chain(next));
    let mut output = None;
    session
        .process_with_frames(&packet, |frame| {
            output = Some((
                frame.graphics.pixels().to_vec(),
                frame.acknowledgement.unwrap(),
            ));
        })
        .unwrap();
    assert!(session.graphics_frame_in_progress());
    let (pixels, receipt) = output.unwrap();
    assert_eq!(&pixels[..4], &[1, 2, 3, 255]);
    let packets = outgoing_dynamic(session.acknowledge_graphics_output(&receipt).unwrap());
    let mut expected = vec![0x30, 7];
    let mut ack = 0u32.to_le_bytes().to_vec();
    ack.extend(19u32.to_le_bytes());
    ack.extend(1u32.to_le_bytes());
    expected.extend(pdu(13, &ack));
    assert_eq!(packets, [expected]);
    // Neither a duplicate output copy nor a later GUI paint sends a second ACK.
    assert!(
        session
            .acknowledge_graphics_output(&receipt)
            .unwrap()
            .is_empty()
    );
    assert!(session.acknowledge_frame(&receipt).unwrap().is_empty());
    let mut next_receipt = None;
    session
        .process_with_frames(&graphics_packet([next_end]), |frame| {
            assert_eq!(&frame.graphics.pixels()[..4], &[8, 9, 10, 255]);
            next_receipt = frame.acknowledgement;
        })
        .unwrap();
    assert_eq!(
        outgoing_dynamic(
            session
                .acknowledge_graphics_output(&next_receipt.unwrap())
                .unwrap()
        )
        .len(),
        1
    );
}

#[test]
fn closing_graphics_channel_invalidates_pending_presentation_receipts() {
    let mut session = session();
    let events = session
        .process(&graphics_packet(frame(1, [3, 4, 5, 0])))
        .unwrap();
    let receipt = events
        .into_iter()
        .find_map(|event| match event {
            Event::GraphicsFrameEnd {
                acknowledgement, ..
            } => acknowledgement,
            _ => None,
        })
        .unwrap();
    session.process(&channel_packet(&[0x40, 7])).unwrap();
    assert!(session.acknowledge_frame(&receipt).unwrap().is_empty());
}
