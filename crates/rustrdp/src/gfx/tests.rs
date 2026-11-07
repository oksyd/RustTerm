use super::*;

fn message(pdus: impl IntoIterator<Item = Vec<u8>>) -> Vec<u8> {
    let mut bytes = vec![0xe0, 4];
    for pdu in pdus {
        bytes.extend(pdu);
    }
    bytes
}

fn reset(width: u32, height: u32) -> Vec<u8> {
    let mut body = Vec::new();
    for value in [width, height, 0] {
        body.extend(value.to_le_bytes());
    }
    body.resize(332, 0);
    pdu(14, &body)
}

fn setup() -> (Gfx, Graphics) {
    let limits = Limits::default();
    let mut gfx = Gfx::new(limits, true);
    let mut output = Graphics::new(2, 2, limits).unwrap();
    let mut caps = Vec::new();
    for value in [CAPABILITIES[0].0, 4, CAPABILITIES[0].1] {
        caps.extend(value.to_le_bytes());
    }
    let create = pdu(9, &[1, 0, 2, 0, 2, 0, 0x20]);
    let map = pdu(15, &[1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    gfx.receive(
        &message([pdu(19, &caps), reset(2, 2), create, map]),
        &mut output,
        |_, _| Ok(()),
    )
    .unwrap();
    (gfx, output)
}

#[test]
fn clarity_keeps_the_pipeline_but_disables_avc_in_every_version() {
    let limits = Limits::default();
    let gfx = Gfx::new(limits, false);
    let bytes = gfx.capabilities();
    let mut reader = Reader::new(&bytes[8..]);
    let count = reader.u16_le().unwrap();
    assert_eq!(usize::from(count), CAPABILITIES.len());
    for _ in 0..count {
        let version = reader.u32_le().unwrap();
        assert_eq!(reader.u32_le().unwrap(), 4);
        let flags = reader.u32_le().unwrap();
        if version >= 0x000a_0002 {
            assert_ne!(flags & 0x20, 0);
            assert_eq!(flags & 0x40, 0);
        } else {
            assert_eq!(flags & 0x10, 0);
        }
        let mut confirm = Vec::new();
        for value in [version, 4, flags] {
            confirm.extend(value.to_le_bytes());
        }
        let mut graphics = Graphics::new(2, 2, limits).unwrap();
        let mut session = Gfx::new(limits, false);
        session
            .receive(&message([pdu(19, &confirm)]), &mut graphics, |_, _| Ok(()))
            .unwrap();
        if version >= 0x000a_0002 {
            confirm[8..12].copy_from_slice(&(flags & !0x20).to_le_bytes());
            let mut session = Gfx::new(limits, false);
            assert!(
                session
                    .receive(&message([pdu(19, &confirm)]), &mut graphics, |_, _| Ok(()))
                    .is_err()
            );
        }
    }
    reader.finish().unwrap();
    let config = crate::ClientConfig {
        graphics_pipeline: true,
        allow_lossy_bitmaps: false,
        frame_acknowledgements: std::num::NonZeroU32::new(3),
        ..crate::ClientConfig::default()
    };
    assert!(config.validate().is_ok());
    assert!(
        crate::ClientConfig {
            frame_acknowledgements: None,
            ..config
        }
        .validate()
        .is_err()
    );
}

#[test]
fn every_offered_version_decodes_the_baseline_desktop() {
    for allow_avc in [false, true] {
        let offered: Vec<_> = Gfx::new(Limits::default(), allow_avc)
            .offered_capabilities()
            .collect();
        for (version, flags) in offered {
            let mut gfx = Gfx::new(Limits::default(), allow_avc);
            let mut output = Graphics::new(2, 2, Limits::default()).unwrap();
            let confirm: Vec<_> = [version, 4, flags]
                .into_iter()
                .flat_map(u32::to_le_bytes)
                .collect();
            let commands = message([
                pdu(19, &confirm),
                reset(2, 2),
                pdu(9, &[1, 0, 2, 0, 2, 0, 0x20]),
                pdu(15, &[1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
                start(1),
                fill([11, 22, 33, 0]),
                pdu(12, &1u32.to_le_bytes()),
            ]);
            let mut completed = 0;
            gfx.receive(&commands, &mut output, |event, graphics| {
                if matches!(event, FrameEvent::End { id: 1, total: 1 }) {
                    completed += 1;
                    assert_eq!(graphics.pixels(), [11, 22, 33, 255].repeat(4));
                }
                Ok(())
            })
            .unwrap();
            assert_eq!(completed, 1);
        }
    }
}

#[test]
fn codec_selection_respects_the_confirmed_version_and_avc_flags() {
    // The server can select 8.0 or 8.1 without AVC even when the client offered
    // H.264 for newer versions. The decoder must follow that selected set.
    for (version, flags, forbidden) in [
        (0x0008_0004u32, 2u32, &[11u16, 14, 15][..]),
        (0x0008_0105, 2, &[11, 14, 15][..]),
        (0x0008_0105, 0x12, &[14, 15][..]),
        (0x000a_0002, 2, &[15][..]),
    ] {
        for &codec in forbidden {
            let mut gfx = Gfx::new(Limits::default(), true);
            let mut output = Graphics::new(2, 2, Limits::default()).unwrap();
            let confirm: Vec<_> = [version, 4, flags]
                .into_iter()
                .flat_map(u32::to_le_bytes)
                .collect();
            gfx.receive(
                &message([pdu(19, &confirm), reset(2, 2)]),
                &mut output,
                |_, _| Ok(()),
            )
            .unwrap();
            let mut bitmap = vec![1, 0];
            bitmap.extend(codec.to_le_bytes());
            bitmap.extend([0x20, 0, 0, 0, 0, 2, 0, 2, 0, 0, 0, 0, 0]);
            assert_eq!(
                gfx.receive(&message([pdu(1, &bitmap)]), &mut output, |_, _| Ok(())),
                Err(Error::Invalid("unnegotiated graphics AVC codec"))
            );
        }
    }
}

#[test]
fn unsupported_scaled_mapping_cannot_be_enabled_by_confirmation() {
    let mut gfx = Gfx::new(Limits::default(), true);
    let mut output = Graphics::new(2, 2, Limits::default()).unwrap();
    let confirm: Vec<_> = [0x000a_0701u32, 4, 0x42]
        .into_iter()
        .flat_map(u32::to_le_bytes)
        .collect();
    assert_eq!(
        gfx.receive(&message([pdu(19, &confirm)]), &mut output, |_, _| Ok(())),
        Err(Error::Invalid("unoffered graphics capabilities"))
    );
}

#[test]
fn unknown_capability_sets_are_ignored_without_advancing_negotiation() {
    let mut gfx = Gfx::new(Limits::default(), true);
    let mut output = Graphics::new(2, 2, Limits::default()).unwrap();
    let unknown: Vec<_> = [0x1234_5678u32, 8, 0, 0]
        .into_iter()
        .flat_map(u32::to_le_bytes)
        .collect();
    gfx.receive(&message([pdu(19, &unknown)]), &mut output, |_, _| Ok(()))
        .unwrap();
    assert_eq!(gfx.version, None);
    let confirm: Vec<_> = [0x0008_0004u32, 4, 2]
        .into_iter()
        .flat_map(u32::to_le_bytes)
        .collect();
    gfx.receive(
        &message([pdu(19, &confirm), reset(2, 2)]),
        &mut output,
        |_, _| Ok(()),
    )
    .unwrap();
    assert_eq!(gfx.version, Some(0x0008_0004));
    gfx.receive(&message([pdu(19, &unknown)]), &mut output, |_, _| Ok(()))
        .unwrap();
    assert_eq!(gfx.version, Some(0x0008_0004));

    // Unknown formats still have to obey their enclosing PDU lengths.
    let mut fresh = Gfx::new(Limits::default(), true);
    assert_eq!(
        fresh.receive(&message([pdu(19, &unknown[..12])]), &mut output, |_, _| Ok(
            ()
        )),
        Err(Error::Truncated)
    );
}

fn start(id: u32) -> Vec<u8> {
    let mut bytes = vec![0; 4];
    bytes.extend(id.to_le_bytes());
    pdu(11, &bytes)
}

fn fill(color: [u8; 4]) -> Vec<u8> {
    let mut bytes = vec![1, 0];
    bytes.extend(color);
    bytes.extend([1, 0, 0, 0, 0, 0, 2, 0, 2, 0]);
    pdu(4, &bytes)
}

#[test]
fn frame_callbacks_observe_each_complete_frame_in_a_coalesced_message() {
    let (mut gfx, mut output) = setup();
    let mut frames = Vec::new();
    let packet = message([
        start(7),
        fill([1, 2, 3, 0]),
        pdu(12, &7u32.to_le_bytes()),
        start(8),
        fill([4, 5, 6, 0]),
        pdu(12, &8u32.to_le_bytes()),
        start(9),
        fill([7, 8, 9, 0]),
    ]);
    gfx.receive(&packet, &mut output, |event, graphics| {
        if let FrameEvent::End { id, total } = event {
            frames.push((id, total, graphics.pixels().to_vec()));
        }
        Ok(())
    })
    .unwrap();
    assert_eq!(frames.len(), 2);
    assert_eq!(
        (&frames[0].0, &frames[0].1, &frames[0].2[..4]),
        (&7, &1, &[1, 2, 3, 255][..])
    );
    assert_eq!(
        (&frames[1].0, &frames[1].1, &frames[1].2[..4]),
        (&8, &2, &[4, 5, 6, 255][..])
    );
    assert!(output.frame_in_progress());
    assert_eq!(&output.pixels()[..4], &[4, 5, 6, 255]);
}

#[test]
fn reset_preserves_surface_and_cache_lifetimes() {
    let (mut gfx, mut output) = setup();
    let mut cache = vec![1, 0];
    cache.extend([0; 8]);
    cache.extend([1, 0, 0, 0, 0, 0, 2, 0, 2, 0]);
    let restore = pdu(7, &[1, 0, 1, 0, 1, 0, 0, 0, 0, 0]);
    gfx.receive(
        &message([
            fill([3, 4, 5, 255]),
            pdu(6, &cache),
            reset(3, 3),
            fill([0; 4]),
            restore,
        ]),
        &mut output,
        |_, _| Ok(()),
    )
    .unwrap();
    assert_eq!((output.width(), output.height()), (3, 3));
    assert_eq!(&output.pixels()[..4], &[3, 4, 5, 255]);
    assert_eq!(gfx.cache.len(), 1);
    assert_eq!(gfx.surfaces.len(), 1);
}

#[test]
fn uncompressed_bitmap_is_top_down_and_records_actual_codec() {
    let (mut gfx, mut output) = setup();
    let mut bitmap = vec![1, 0, 0, 0, 0x20, 0, 0, 0, 0, 2, 0, 2, 0];
    bitmap.extend(16u32.to_le_bytes());
    bitmap.extend([1, 2, 3, 0, 4, 5, 6, 0, 7, 8, 9, 0, 10, 11, 12, 0]);
    gfx.receive(&message([pdu(1, &bitmap)]), &mut output, |_, _| Ok(()))
        .unwrap();
    assert_eq!(
        output.pixels(),
        &[1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255, 10, 11, 12, 255]
    );
    assert_eq!(gfx.codec(), Some(crate::GraphicsCodec::Raw));
}

#[test]
fn moving_pixels_with_surface_copy_and_repaint_marks_both_areas() {
    let (mut gfx, mut output) = setup();
    let mut bitmap = vec![1, 0, 0, 0, 0x20, 0, 0, 0, 0, 2, 0, 2, 0];
    bitmap.extend(16u32.to_le_bytes());
    bitmap.extend([1, 0, 0, 255, 2, 0, 0, 255, 3, 0, 0, 255, 4, 0, 0, 255]);
    gfx.receive(&message([pdu(1, &bitmap)]), &mut output, |_, _| Ok(()))
        .unwrap();
    output.take_damage();

    // Move the left column one pixel right, then repaint its old position.
    let copy = pdu(5, &[1, 0, 1, 0, 0, 0, 0, 0, 1, 0, 2, 0, 1, 0, 1, 0, 0, 0]);
    let repaint = pdu(4, &[1, 0, 9, 0, 0, 255, 1, 0, 0, 0, 0, 0, 1, 0, 2, 0]);
    gfx.receive(&message([copy, repaint]), &mut output, |_, _| Ok(()))
        .unwrap();
    assert_eq!(
        output.pixels(),
        &[9, 0, 0, 255, 1, 0, 0, 255, 9, 0, 0, 255, 3, 0, 0, 255]
    );
    assert_eq!(
        output.take_damage(),
        Some(crate::graphics::Damage {
            x: 0,
            y: 0,
            width: 2,
            height: 2
        })
    );
}

#[test]
fn invalid_surface_bounds_and_expansion_are_rejected() {
    let (mut gfx, mut output) = setup();
    let original = output.pixels().to_vec();
    let bad = pdu(4, &[1, 0, 1, 2, 3, 255, 1, 0, 0, 0, 0, 0, 3, 0, 2, 0]);
    assert!(
        gfx.receive(&message([bad]), &mut output, |_, _| Ok(()))
            .is_err()
    );
    assert_eq!(output.pixels(), original);
    let mut oversized = vec![0xe1, 1, 0];
    oversized.extend(u32::MAX.to_le_bytes());
    assert!(matches!(
        gfx.receive(&oversized, &mut output, |_, _| Ok(())),
        Err(Error::Limit(_))
    ));
}

#[test]
fn graphics_acknowledgement_has_received_id_and_decoded_count() {
    let bytes = Gfx::acknowledge(17, 23);
    assert_eq!(
        bytes,
        &[
            13, 0, 0, 0, 20, 0, 0, 0, 0, 0, 0, 0, 17, 0, 0, 0, 23, 0, 0, 0
        ]
    );
}

#[test]
fn clearcodec_glyph_cache_and_sequence_cross_surfaces_and_output_resizes() {
    let (mut gfx, mut output) = setup();
    let bitmap = |id: u16, data: &[u8]| {
        let mut bytes = id.to_le_bytes().to_vec();
        bytes.extend([8, 0, 0x20, 0, 0, 0, 0, 2, 0, 2, 0]);
        bytes.extend((data.len() as u32).to_le_bytes());
        bytes.extend(data);
        pdu(1, &bytes)
    };
    let first = [1, 0, 17, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3, 4, 5, 4];
    gfx.receive(
        &message([
            bitmap(1, &first),
            pdu(9, &[2, 0, 2, 0, 2, 0, 0x20]),
            reset(3, 3),
            bitmap(2, &[3, 1, 17, 0]),
            pdu(15, &[2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
        ]),
        &mut output,
        |_, _| Ok(()),
    )
    .unwrap();
    assert_eq!(&output.pixels()[..4], &[3, 4, 5, 255]);
    assert_eq!(gfx.surfaces[&1].pixels, gfx.surfaces[&2].pixels);
    assert_eq!(gfx.codec(), Some(crate::GraphicsCodec::Clear));
}
