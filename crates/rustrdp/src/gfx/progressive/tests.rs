use super::*;

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
fn invalid_upgrade_context_quantization_and_region_bounds_are_rejected() {
    let first = packet(
        &[initial(0xccc6, false, 0, &[0; 4])],
        [0, 0, 64, 64],
        6,
        Some(1),
    );
    let upgrade = packet(&[refinement([&[0; 8]; 3])], [0, 0, 64, 64], 6, None);
    let mut decoder = Decoder::new(64 * 64 * 4);
    assert!(decoder.decode_bitmap(1, 10, 64, 64, &upgrade).is_err());
    decoder.decode_bitmap(1, 10, 64, 64, &first).unwrap();
    assert!(decoder.decode_bitmap(1, 20, 64, 64, &upgrade).is_err());
    let larger = packet(&[refinement([&[]; 3])], [0, 0, 64, 64], 8, None);
    assert!(decoder.decode_bitmap(1, 10, 64, 64, &larger).is_err());
    let bad_rect = packet(&[], [1, 0, 64, 64], 6, None);
    assert!(decoder.decode_bitmap(1, 10, 64, 64, &bad_rect).is_err());
    let bad_quant = packet(&[], [0, 0, 64, 64], 6, Some(9));
    assert!(decoder.decode_bitmap(1, 10, 64, 64, &bad_quant).is_err());
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
