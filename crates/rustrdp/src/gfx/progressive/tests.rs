use super::*;

#[test]
fn asymmetric_progressive_tiles_match_freerdp() {
    // Synthetic RLGR1 streams: signed coefficients in every band, varying LL3
    // deltas, and unequal horizontal/vertical quantization. BGRA references were
    // independently decoded by FreeRDP 3.32.1; unused alpha is normalized to 255.
    for (stream, expected) in [
        (
            include_bytes!("../fixtures/progressive-standard.bin").as_slice(),
            include_bytes!("../fixtures/progressive-standard.bgra").as_slice(),
        ),
        (
            include_bytes!("../fixtures/progressive-reduced.bin").as_slice(),
            include_bytes!("../fixtures/progressive-reduced.bgra").as_slice(),
        ),
    ] {
        let mut decoder = Decoder::new(64 * 64 * 4);
        let tiles = decoder.decode_bitmap(1, 10, 64, 64, stream).unwrap();
        assert_eq!(tiles.len(), 1);
        assert_eq!(tiles[0].pixels.len(), expected.len());
        for (index, (actual, expected)) in tiles[0].pixels.iter().zip(expected).enumerate() {
            assert!(
                actual.abs_diff(*expected) <= 1,
                "component {index}: {actual} != {expected}"
            );
        }
    }
}

#[test]
fn signed_upgrades_and_difference_tiles_match_freerdp() {
    // Independent FreeRDP 3.32.1 output for an asymmetric first pass, two
    // RAW refinements and a signed difference tile. Streams are u32-length
    // prefixed; each expected image contains 64x64 opaque BGRA pixels.
    let mut streams = Reader::new(include_bytes!("../fixtures/progressive-upgrades.bin"));
    let expected = include_bytes!("../fixtures/progressive-upgrades.bgra");
    let mut decoder = Decoder::new(64 * 64 * 4);
    for (frame, pixels) in expected.as_chunks::<{ 64 * 64 * 4 }>().0.iter().enumerate() {
        let length = streams.u32_le().unwrap() as usize;
        let tiles = decoder
            .decode_bitmap(1, 10, 64, 64, streams.take(length).unwrap())
            .unwrap();
        assert_eq!(tiles.len(), 1);
        for (index, (actual, expected)) in tiles[0].pixels.iter().zip(pixels).enumerate() {
            assert!(
                actual.abs_diff(*expected) <= 1,
                "frame {frame}, component {index}: {actual} != {expected}"
            );
        }
    }
    streams.finish().unwrap();
}

#[test]
fn new_bitmap_context_refines_shared_tiles_and_survives_context_deletion() {
    // A codec context identifies a progressively rendered bitmap, while the
    // coefficient/sign state belongs to tiles shared by those bitmaps. A new
    // bitmap can refine a tile initialized by an earlier bitmap.
    for deleted in [10, 20] {
        let mut streams = Reader::new(include_bytes!("../fixtures/progressive-upgrades.bin"));
        let expected = include_bytes!("../fixtures/progressive-upgrades.bgra");
        let mut decoder = Decoder::new(64 * 64 * 4);
        for (frame, context) in [10, 20, if deleted == 10 { 20 } else { 10 }]
            .into_iter()
            .enumerate()
        {
            decoder.begin_frame();
            let length = streams.u32_le().unwrap() as usize;
            let tiles = decoder
                .decode_bitmap(1, context, 64, 64, streams.take(length).unwrap())
                .unwrap();
            assert_eq!(tiles.len(), 1);
            let pixels = &expected[frame * 64 * 64 * 4..(frame + 1) * 64 * 64 * 4];
            for (actual, expected) in tiles[0].pixels.iter().zip(pixels) {
                assert!(actual.abs_diff(*expected) <= 1);
            }
            decoder.end_frame();
            if frame == 1 {
                decoder.delete_context(1, deleted);
            }
        }
        decoder.delete_context(1, if deleted == 10 { 20 } else { 10 });
        assert!(decoder.surfaces[&1].tiles[&(0, 0)].pass.is_none());
        assert_eq!(decoder.bytes, TILE_BYTES);
        decoder.delete_surface(1);
        assert_eq!(decoder.bytes, 0);
    }
}

#[test]
fn shared_tile_regions_keep_refinement_alive_after_the_original_context_is_deleted() {
    for tile_first in [false, true] {
        let mut decoder = Decoder::new(64 * 64 * 4);
        decoder.begin_frame();
        let tile = initial(0xccc6, false, 0, &[0; 4]);
        let (first_tiles, second_tiles) = if tile_first {
            (vec![tile], vec![])
        } else {
            (vec![], vec![tile])
        };
        let left = packet(&first_tiles, [0, 0, 32, 64], 6, Some(1));
        let right = packet(&second_tiles, [32, 0, 32, 64], 6, Some(1));
        decoder.decode_bitmap(1, 10, 64, 64, &left).unwrap();
        decoder.decode_bitmap(1, 20, 64, 64, &right).unwrap();
        decoder.end_frame();
        let (deleted, remaining) = if tile_first { (10, 20) } else { (20, 10) };
        decoder.delete_context(1, deleted);
        let upgrade = packet(
            &[refinement([&[255; 8], &[0; 8], &[0; 8]])],
            [0, 0, 64, 64],
            6,
            None,
        );
        let tiles = decoder
            .decode_bitmap(1, remaining, 64, 64, &upgrade)
            .unwrap();
        assert!(
            tiles[0]
                .pixels
                .as_chunks::<4>()
                .0
                .iter()
                .all(|p| *p == [129, 129, 129, 255])
        );
        decoder.delete_context(1, remaining);
        decoder.delete_surface(1);
        assert_eq!(decoder.bytes, 0);
    }
}

#[test]
fn pending_tile_associations_survive_frame_boundaries() {
    let mut streams = Reader::new(include_bytes!("../fixtures/progressive-upgrades.bin"));
    let first_length = streams.u32_le().unwrap() as usize;
    let first = streams.take(first_length).unwrap();
    let upgrade_length = streams.u32_le().unwrap() as usize;
    let upgrade = streams.take(upgrade_length).unwrap();
    let mut decoder = Decoder::new(64 * 64 * 4);
    decoder.begin_frame();
    decoder
        .decode_bitmap(1, 10, 64, 64, &packet(&[], [32, 0, 32, 64], 6, Some(1)))
        .unwrap();
    decoder.end_frame();
    decoder.begin_frame();
    decoder.decode_bitmap(1, 20, 64, 64, first).unwrap();
    decoder.end_frame();
    decoder.delete_context(1, 20);
    decoder.begin_frame();
    let tiles = decoder.decode_bitmap(1, 10, 64, 64, upgrade).unwrap();
    assert_eq!(tiles.len(), 1);
    assert_eq!(tiles[0].pixels.len(), 64 * 64 * 4);
    let expected =
        &include_bytes!("../fixtures/progressive-upgrades.bgra")[64 * 64 * 4..2 * 64 * 64 * 4];
    for (actual, expected) in tiles[0].pixels.iter().zip(expected) {
        assert!(actual.abs_diff(*expected) <= 1);
    }
    decoder.end_frame();
    decoder.delete_context(1, 10);
    decoder.delete_surface(1);
    assert_eq!(decoder.bytes, 0);
}

#[test]
fn pending_context_upgrade_survives_graphics_messages_and_frame_boundaries() {
    use super::super::{CAPABILITIES, Gfx, pdu};
    use crate::{Limits, graphics::Graphics};
    let mut streams = Reader::new(include_bytes!("../fixtures/progressive-upgrades.bin"));
    let first_length = streams.u32_le().unwrap() as usize;
    let first = streams.take(first_length).unwrap().to_vec();
    let upgrade_length = streams.u32_le().unwrap() as usize;
    let upgrade = streams.take(upgrade_length).unwrap().to_vec();
    let expected = include_bytes!("../fixtures/progressive-upgrades.bgra");
    let limits = Limits::default();
    let mut gfx = Gfx::new(limits, true);
    let mut graphics = Graphics::new(64, 64, limits).unwrap();
    let receive = |gfx: &mut Gfx, graphics: &mut Graphics, pdus: Vec<Vec<u8>>| {
        let mut bytes = vec![0xe0, 4];
        bytes.extend(pdus.into_iter().flatten());
        gfx.receive(&bytes, graphics, |_, _| Ok(())).unwrap();
    };
    let wire = |context: u32, stream: Vec<u8>| {
        let mut body = vec![1, 0, 9, 0];
        body.extend(context.to_le_bytes());
        body.push(0x20);
        body.extend((stream.len() as u32).to_le_bytes());
        body.extend(stream);
        pdu(2, &body)
    };
    let caps: Vec<_> = [CAPABILITIES[0].0, 4, CAPABILITIES[0].1]
        .into_iter()
        .flat_map(u32::to_le_bytes)
        .collect();
    let mut reset: Vec<_> = [64u32, 64, 0]
        .into_iter()
        .flat_map(u32::to_le_bytes)
        .collect();
    reset.resize(332, 0);
    receive(
        &mut gfx,
        &mut graphics,
        vec![
            pdu(19, &caps),
            pdu(14, &reset),
            pdu(9, &[1, 0, 64, 0, 64, 0, 0x20]),
            pdu(15, &[1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
            pdu(11, &[0; 8]),
            wire(20, packet(&[], [32, 0, 32, 64], 6, Some(1))),
            pdu(12, &0u32.to_le_bytes()),
            pdu(11, &[0, 0, 0, 0, 1, 0, 0, 0]),
            wire(10, first),
            pdu(12, &1u32.to_le_bytes()),
        ],
    );
    for (actual, expected) in graphics.pixels().iter().zip(&expected[..64 * 64 * 4]) {
        assert!(actual.abs_diff(*expected) <= 1);
    }
    receive(
        &mut gfx,
        &mut graphics,
        vec![
            pdu(3, &[1, 0, 10, 0, 0, 0]),
            pdu(11, &[0, 0, 0, 0, 2, 0, 0, 0]),
            wire(20, upgrade),
            pdu(12, &2u32.to_le_bytes()),
        ],
    );
    for (actual, expected) in graphics
        .pixels()
        .iter()
        .zip(&expected[64 * 64 * 4..2 * 64 * 64 * 4])
    {
        assert!(actual.abs_diff(*expected) <= 1);
    }
}

#[test]
fn deleted_contexts_are_not_resurrected_and_tile_links_are_bounded() {
    let mut decoder = Decoder::new(64 * 64 * 4);
    for _ in 0..8 {
        decoder.begin_frame();
        decoder
            .decode_bitmap(1, 10, 64, 64, &packet(&[], [0, 0, 32, 64], 6, Some(1)))
            .unwrap();
        decoder.delete_context(1, 10);
        decoder
            .decode_bitmap(
                1,
                20,
                64,
                64,
                &packet(
                    &[initial(0xccc6, false, 0, &[0; 4])],
                    [32, 0, 32, 64],
                    6,
                    Some(1),
                ),
            )
            .unwrap();
        assert_eq!(
            decoder.surfaces[&1].tiles[&(0, 0)]
                .pass
                .as_ref()
                .unwrap()
                .contexts,
            BTreeSet::from([20])
        );
        decoder.end_frame();
        assert_eq!(
            decoder.surfaces[&1].tile_contexts[&(0, 0)],
            BTreeSet::from([20])
        );
        decoder.delete_context(1, 20);
        assert!(decoder.surfaces[&1].tile_contexts.is_empty());
        assert_eq!(decoder.bytes, TILE_BYTES);
    }
    decoder.begin_frame();
    decoder.max_bytes = decoder.bytes + 64;
    assert!(matches!(
        decoder.decode_bitmap(1, 30, 64, 64, &packet(&[], [0, 0, 64, 64], 6, None)),
        Err(Error::Limit("progressive tile state"))
    ));
    assert_eq!(decoder.bytes, TILE_BYTES);
    decoder.delete_surface(1);
    assert_eq!(decoder.bytes, 0);
    assert_eq!(decoder.masks, 0);
}

fn block(kind: u16, body: &[u8]) -> Vec<u8> {
    let mut out = kind.to_le_bytes().to_vec();
    out.extend(((body.len() + 6) as u32).to_le_bytes());
    out.extend(body);
    out
}

fn packet(tiles: &[Vec<u8>], rect: [u16; 4], base: u8, progressive: Option<u8>) -> Vec<u8> {
    let mut region = vec![64, 1, 0, 1, u8::from(progressive.is_some()), 0];
    region.extend((tiles.len() as u16).to_le_bytes());
    region.extend((tiles.iter().map(Vec::len).sum::<usize>() as u32).to_le_bytes());
    for value in rect {
        region.extend(value.to_le_bytes());
    }
    region.extend([base * 17; 5]);
    if let Some(quant) = progressive {
        region.push(50);
        region.extend([quant * 17; 15]);
    }
    for tile in tiles {
        region.extend(tile);
    }
    let mut out = block(0xccc3, &[0, 64, 0, 1]);
    out.extend(block(0xccc1, &[0, 0, 0, 0, 1, 0]));
    out.extend(block(0xccc4, &region));
    out.extend(block(0xccc2, &[]));
    out
}

fn initial(kind: u16, difference: bool, quality: u8, component: &[u8]) -> Vec<u8> {
    let mut body = vec![0, 0, 0, 0, 0, 0, 0, u8::from(difference)];
    if kind == 0xccc6 {
        body.push(quality);
    }
    for _ in 0..3 {
        body.extend((component.len() as u16).to_le_bytes());
    }
    body.extend([0, 0]);
    for _ in 0..3 {
        body.extend(component);
    }
    block(kind, &body)
}

fn refinement(raw: [&[u8]; 3]) -> Vec<u8> {
    refinement_at(raw, 255)
}

fn refinement_at(raw: [&[u8]; 3], quality: u8) -> Vec<u8> {
    let mut body = vec![0, 0, 0, 0, 0, 0, 0, quality];
    for bytes in raw {
        body.extend(4u16.to_le_bytes());
        body.extend((bytes.len() as u16).to_le_bytes());
    }
    for bytes in raw {
        body.extend([0; 4]);
        body.extend(bytes);
    }
    block(0xccc7, &body)
}

#[test]
fn microsoft_upgrade_sample_decodes_signed_runs_across_band_boundaries() {
    // MS-RDPEGFX 4.1.2.1.2, SRL bytes emitted for the 25%-to-50% upgrade.
    let mut srl = Srl::new(&[0xa0, 0x01, 0x80, 0x00, 0xc9, 0x49, 0xe0]);
    let result: Vec<_> = [4, 4, 4, 2, 2, 2, 2, 2]
        .into_iter()
        .map(|count| srl.next(count).unwrap())
        .collect();
    assert_eq!(result, [-13, 15, -3, 0, 0, -3, 2, -1]);
    assert!(srl.bits.remaining() < 8);
}

#[test]
fn first_upgrade_and_difference_preserve_reference_after_context_deletion() {
    let mut decoder = Decoder::new(64 * 64 * 4);
    decoder.begin_frame();
    let first = packet(
        &[initial(0xccc6, false, 0, &[0; 4])],
        [0, 0, 64, 64],
        6,
        Some(1),
    );
    let tiles = decoder.decode_bitmap(1, 10, 64, 64, &first).unwrap();
    assert_eq!(tiles.len(), 1);
    assert!(
        tiles[0]
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .all(|pixel| *pixel == [128, 128, 128, 255])
    );
    decoder.end_frame();
    decoder.begin_frame();
    let upgrade = packet(
        &[refinement([&[255; 8], &[0; 8], &[0; 8]])],
        [0, 0, 64, 64],
        6,
        None,
    );
    let tiles = decoder.decode_bitmap(1, 10, 64, 64, &upgrade).unwrap();
    assert!(
        tiles[0]
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .all(|pixel| *pixel == [129, 129, 129, 255])
    );
    decoder.end_frame();
    decoder.delete_context(1, 10);
    assert!(decoder.surfaces[&1].tiles[&(0, 0)].pass.is_none());
    let difference = packet(
        &[initial(0xccc5, true, 255, &[0; 4])],
        [0, 0, 64, 64],
        6,
        None,
    );
    let tiles = decoder.decode_bitmap(1, 20, 64, 64, &difference).unwrap();
    assert!(
        tiles[0]
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .all(|pixel| *pixel == [129, 129, 129, 255])
    );
    decoder.delete_surface(1);
    assert_eq!(decoder.bytes, 0);
    assert_eq!(decoder.masks, 0);
    assert!(decoder.decode_bitmap(1, 30, 64, 64, &difference).is_err());
}

#[test]
fn zero_base_quantization_retains_signed_odd_coefficients_across_passes() {
    for (encoded, sign) in [(0x80, 1), (0xa0, -1)] {
        // RLGR1: initial empty run, +/-1, then a final zero run.
        let data = [encoded, 0, 0, 0, 0];
        let decoded = wavelet::rlgr(&data, false).unwrap();
        assert_eq!(decoded[0], sign);
        assert!(decoded[1..].iter().all(|value| *value == 0));
        let mut decoder = Decoder::new(64 * 64 * 4);
        let first = packet(
            &[initial(0xccc6, false, 0, &data)],
            [0, 0, 64, 64],
            0,
            Some(2),
        );
        decoder.decode_bitmap(1, 10, 64, 64, &first).unwrap();
        assert_eq!(decoder.surfaces[&1].tiles[&(0, 0)].current[0][0], 4 * sign);
        // Zero raw refinement retains a nonzero sign: the next pass still
        // reads that coefficient from RAW instead of switching to SRL.
        let intermediate = packet(
            &[refinement_at([&[0; 9]; 3], 0)],
            [0, 0, 64, 64],
            0,
            Some(1),
        );
        decoder.decode_bitmap(1, 10, 64, 64, &intermediate).unwrap();
        assert_eq!(decoder.surfaces[&1].tiles[&(0, 0)].current[0][0], 4 * sign);
        let mut raw = [0; 9];
        raw[0] = 128; // First non-LL coefficient has a known sign; LL is zero.
        let upgrade = packet(&[refinement([&raw, &raw, &raw])], [0, 0, 64, 64], 0, None);
        decoder.decode_bitmap(1, 10, 64, 64, &upgrade).unwrap();
        assert_eq!(decoder.surfaces[&1].tiles[&(0, 0)].current[0][0], 5 * sign);
        decoder.delete_context(1, 10);
        let difference = packet(
            &[initial(0xccc5, true, 255, &data)],
            [0, 0, 64, 64],
            0,
            None,
        );
        decoder.decode_bitmap(1, 20, 64, 64, &difference).unwrap();
        assert_eq!(decoder.surfaces[&1].tiles[&(0, 0)].current[0][0], 6 * sign);
    }
}

#[test]
fn regions_reuse_frame_tiles_without_replaying_earlier_masks() {
    for tile_first in [false, true] {
        let mut decoder = Decoder::new(64 * 64 * 4);
        decoder.begin_frame();
        let tile = initial(0xccc5, false, 255, &[0; 4]);
        let left_tiles = if tile_first {
            vec![tile.clone()]
        } else {
            Vec::new()
        };
        let right_tiles = if tile_first { Vec::new() } else { vec![tile] };
        let first = packet(&left_tiles, [0, 0, 32, 64], 6, None);
        let second = packet(&right_tiles, [32, 0, 32, 64], 6, None);
        let initial = decoder.decode_bitmap(1, 10, 64, 64, &first).unwrap();
        assert_eq!(initial.len(), usize::from(tile_first));
        let later = decoder.decode_bitmap(1, 10, 64, 64, &second).unwrap();
        assert_eq!(later.len(), 1);
        assert_eq!(
            later[0].update_rectangles,
            [Rect {
                left: 32,
                top: 0,
                right: 64,
                bottom: 64
            }]
        );
        decoder.end_frame();
        decoder.begin_frame();
        let no_tile = packet(&[], [0, 0, 64, 64], 6, None);
        assert!(
            decoder
                .decode_bitmap(1, 10, 64, 64, &no_tile)
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn later_progressive_bitmap_does_not_overwrite_an_intervening_repaint() {
    let mut decoder = Decoder::new(64 * 64 * 4);
    decoder.begin_frame();
    let tile = initial(0xccc5, false, 255, &[0; 4]);
    let left = packet(std::slice::from_ref(&tile), [0, 0, 32, 64], 6, None);
    let right = packet(&[tile], [32, 0, 32, 64], 6, None);
    let mut surface = super::super::Surface {
        width: 64,
        height: 64,
        pixels: vec![0; 64 * 64 * 4],
        origin: None,
        damage: None,
        codecs: Default::default(),
    };
    let apply = |surface: &mut super::super::Surface, tiles: Vec<Tile>| {
        for tile in tiles {
            for rect in tile.update_rectangles {
                super::super::codecs::put_bgra_tile(surface, rect, 0, 0, &tile.pixels).unwrap();
            }
        }
    };
    apply(
        &mut surface,
        decoder.decode_bitmap(1, 10, 64, 64, &left).unwrap(),
    );
    // A different codec repaints the old window location in the same frame.
    let repaint = [19, 31, 47, 255].repeat(32 * 64);
    surface.put(
        Rect {
            left: 0,
            top: 0,
            right: 32,
            bottom: 64,
        },
        &repaint,
        32,
    );
    apply(
        &mut surface,
        decoder.decode_bitmap(1, 10, 64, 64, &right).unwrap(),
    );
    assert_eq!(&surface.pixels[..4], &[19, 31, 47, 255]);
    assert_eq!(&surface.pixels[32 * 4..33 * 4], &[128, 128, 128, 255]);
}

#[test]
fn invalid_upgrade_state_quantization_and_region_bounds_are_rejected() {
    let first = packet(
        &[initial(0xccc6, false, 0, &[0; 4])],
        [0, 0, 64, 64],
        6,
        Some(1),
    );
    let upgrade = packet(&[refinement([&[0; 8]; 3])], [0, 0, 64, 64], 6, None);
    let mut decoder = Decoder::new(64 * 64 * 4);
    for _ in 0..2 {
        assert!(matches!(
            decoder.decode_bitmap(1, 20, 64, 64, &upgrade),
            Err(Error::Invalid("progressive upgrade without tile"))
        ));
        assert!(!decoder.surfaces[&1].contexts.contains(&20));
        assert_eq!(decoder.bytes, 0);
    }
    decoder.decode_bitmap(1, 10, 64, 64, &first).unwrap();
    let larger = packet(&[refinement([&[]; 3])], [0, 0, 64, 64], 8, None);
    let original_bytes = decoder.bytes;
    for _ in 0..2 {
        assert!(matches!(
            decoder.decode_bitmap(1, 20, 64, 64, &larger),
            Err(Error::Invalid("progressive bit position"))
        ));
        assert!(!decoder.surfaces[&1].contexts.contains(&20));
        assert!(
            !decoder.surfaces[&1].tiles[&(0, 0)]
                .pass
                .as_ref()
                .unwrap()
                .contexts
                .contains(&20)
        );
        assert_eq!(decoder.bytes, original_bytes);
    }
    assert!(decoder.decode_bitmap(1, 10, 64, 64, &larger).is_err());
    let bad_rect = packet(&[], [1, 0, 64, 64], 6, None);
    assert!(decoder.decode_bitmap(1, 10, 64, 64, &bad_rect).is_err());
    let bad_quant = packet(&[], [0, 0, 64, 64], 6, Some(9));
    assert!(decoder.decode_bitmap(1, 10, 64, 64, &bad_quant).is_err());
    decoder.delete_context(1, 10);
    for context in [10, 20] {
        assert!(matches!(
            decoder.decode_bitmap(1, context, 64, 64, &upgrade),
            Err(Error::Invalid("progressive upgrade without tile state"))
        ));
        assert!(decoder.surfaces[&1].contexts.is_empty());
        assert_eq!(decoder.bytes, TILE_BYTES);
    }
    decoder.delete_surface(1);
    assert_eq!(decoder.bytes, 0);
}

#[test]
fn ignored_blocks_and_outside_regions_do_not_mutate_frame_pixels() {
    let mut decoder = Decoder::new(64 * 64 * 4);
    let mut data = block(0xccc0, &[0; 6]); // Sync values are explicitly advisory.
    data.extend(block(0xbeef, &[1, 2, 3]));
    data.extend(block(0xccc4, &[255])); // Regions outside codec frames must be ignored.
    data.extend(block(0xccc2, &[]));
    assert!(
        decoder
            .decode_bitmap(1, 10, 64, 64, &data)
            .unwrap()
            .is_empty()
    );
    assert!(decoder.surfaces[&1].tiles.is_empty());
    let mut overlap = block(0xccc1, &[0; 6]);
    overlap.extend(block(0xccc1, &[0; 6]));
    assert!(decoder.decode_bitmap(1, 10, 64, 64, &overlap).is_err());
}

#[test]
fn configured_budget_scales_without_allocating_and_is_shared_by_surfaces() {
    let large = Decoder::new(7680 * 4320 * 4);
    assert!(large.max_bytes >= 7680 * 4320 * 19);
    assert_eq!(large.bytes, 0);
    assert!(large.surfaces.is_empty());

    let mut small = Decoder::new(4);
    let first = packet(
        &[initial(0xccc6, false, 0, &[0; 4])],
        [0, 0, 1, 1],
        6,
        Some(1),
    );
    assert_eq!(small.decode_bitmap(1, 10, 1, 1, &first).unwrap().len(), 1);
    assert!(matches!(
        small.decode_bitmap(2, 10, 1, 1, &first),
        Err(Error::Limit("progressive tile state"))
    ));
    small.delete_surface(1);
    assert_eq!(small.decode_bitmap(2, 10, 1, 1, &first).unwrap().len(), 1);
    small.delete_surface(2);
    assert_eq!(small.bytes, 0);
}
