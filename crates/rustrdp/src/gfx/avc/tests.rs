use super::*;
use openh264::{
    encoder::{Encoder, EncoderConfig, Profile},
    formats::YUVBuffer,
};

struct PlanarFixture {
    planes: [Vec<u8>; 3],
}

impl PlanarFixture {
    fn new() -> Self {
        let mut planes = [
            vec![0xee; 40 * 32],
            vec![0xee; 24 * 16],
            vec![0xee; 28 * 16],
        ];
        for y in 0..32 {
            for x in 0..32 {
                planes[0][y * 40 + x] = ((17 * y + 3 * x) % 251) as u8;
            }
        }
        for y in 0..16 {
            for x in 0..16 {
                planes[1][y * 24 + x] = ((64 + 11 * y + 2 * x) % 251) as u8;
                planes[2][y * 28 + x] = ((128 + 5 * y + x) % 251) as u8;
            }
        }
        Self { planes }
    }
}

impl YUVSource for PlanarFixture {
    fn dimensions(&self) -> (usize, usize) {
        (32, 32)
    }
    fn strides(&self) -> (usize, usize, usize) {
        (40, 24, 28)
    }
    fn y(&self) -> &[u8] {
        &self.planes[0]
    }
    fn u(&self) -> &[u8] {
        &self.planes[1]
    }
    fn v(&self) -> &[u8] {
        &self.planes[2]
    }
}

fn bounds() -> Rect {
    Rect {
        left: 0,
        top: 0,
        right: 32,
        bottom: 32,
    }
}

#[test]
fn avc444_auxiliary_views_follow_their_distinct_plane_layouts() {
    // MS-RDPEGFX 3.3.8.3.2/.3: v1 interleaves eight-row bands; v2
    // partitions full-frame rows and alternates the two chroma planes.
    let fixture = PlanarFixture::new();
    let cases: [(u8, &[(usize, usize, u8, u8)]); 2] = [
        (
            2,
            &[
                (0, 1, 0, 136),
                (17, 1, 51, 187),
                (3, 2, 77, 134),
                (18, 17, 75, 211),
                (31, 31, 233, 118),
            ],
        ),
        (
            3,
            &[
                (1, 0, 0, 48),
                (31, 31, 70, 118),
                (0, 1, 64, 80),
                (2, 1, 128, 136),
                (16, 17, 160, 176),
                (18, 17, 172, 180),
            ],
        ),
    ];
    for (operation, samples) in cases {
        let mut planes = [vec![77; 32 * 32], vec![101; 32 * 32], vec![151; 32 * 32]];
        combine(&mut planes, 32, &fixture, bounds(), operation).unwrap();
        for &(x, y, u, v) in samples {
            assert_eq!(
                (planes[1][y * 32 + x], planes[2][y * 32 + x]),
                (u, v),
                "operation {operation}, pixel ({x}, {y})"
            );
        }
        assert!(planes[0].iter().all(|value| *value == 77));
        for y in (0..32).step_by(2) {
            for x in (0..32).step_by(2) {
                assert_eq!((planes[1][y * 32 + x], planes[2][y * 32 + x]), (101, 151));
            }
        }
    }
}

#[test]
fn luma_reconstruction_preserves_pixels_outside_regions_and_honors_strides() {
    let fixture = PlanarFixture::new();
    let mut planes = [vec![250; 32 * 32], vec![251; 32 * 32], vec![252; 32 * 32]];
    let region = Rect {
        left: 17,
        top: 17,
        right: 18,
        bottom: 18,
    };
    combine(&mut planes, 32, &fixture, region, 1).unwrap();
    assert_eq!((planes[0][0], planes[1][0], planes[2][0]), (250, 251, 252));
    assert_eq!(
        (
            planes[0][16 * 32 + 16],
            planes[1][16 * 32 + 16],
            planes[2][16 * 32 + 16]
        ),
        (250, 168, 176)
    );
    assert_eq!(
        (
            planes[0][31 * 32 + 31],
            planes[1][31 * 32 + 31],
            planes[2][31 * 32 + 31]
        ),
        (250, 251, 252)
    );
    assert_eq!(planes[0][17 * 32 + 17], fixture.planes[0][17 * 40 + 17]);
    assert_eq!(planes[0][16 * 32 + 15], 250);
    assert_eq!(planes[0][15 * 32 + 16], 250);
}

#[test]
fn neighboring_main_view_does_not_replace_luma_needed_by_later_auxiliary_view() {
    let first = PlanarFixture::new();
    let mut second = PlanarFixture::new();
    second.planes[0].fill(250);
    let mut planes = [vec![0; 32 * 32], vec![128; 32 * 32], vec![128; 32 * 32]];
    let first_region = Rect {
        left: 2,
        top: 2,
        right: 4,
        bottom: 4,
    };
    let second_region = Rect {
        left: 6,
        top: 2,
        right: 8,
        bottom: 4,
    };
    combine(&mut planes, 32, &first, first_region, 1).unwrap();
    let previous_luma = planes[0][2 * 32 + 2];
    combine(&mut planes, 32, &second, second_region, 1).unwrap();
    combine(&mut planes, 32, &second, first_region, 3).unwrap();
    assert_eq!(planes[0][2 * 32 + 2], previous_luma);
    assert_eq!(planes[0][2 * 32 + 6], 250);
}

#[test]
fn desktop_color_conversion_uses_full_range_bt709() {
    assert_eq!(to_bgra(0, 128, 128), [0, 0, 0, 255]);
    assert_eq!(to_bgra(255, 128, 128), [255, 255, 255, 255]);
    assert_eq!(to_bgra(54, 99, 255), [0, 0, 254, 255]);
    assert_eq!(to_bgra(182, 30, 12), [0, 255, 0, 255]);
    assert_eq!(to_bgra(18, 255, 116), [254, 0, 0, 255]);
    assert_eq!(to_bgra(96, 90, 200), [25, 69, 209, 255]);
}

#[test]
fn avc444_auxiliary_update_uses_the_last_main_colors_of_its_own_region() {
    let mut encoder = Encoder::new().unwrap();
    let mut decoders = [14, 15].map(|codec| {
        (
            codec,
            Avc::new(32, 32, Limits::default()).unwrap(),
            vec![0; 32 * 32 * 4],
        )
    });
    // Two distinct regions in the same macroblock, followed by an auxiliary
    // update of the first one. Only the second region receives the new color.
    for (operation, rect, y, chroma) in [
        (1u32, [2u16, 2, 4, 4], 100, 128),
        (1, [6, 2, 8, 4], 200, 160),
        (2, [2, 2, 4, 4], 128, 128),
    ] {
        let mut yuv = vec![y; 32 * 32];
        yuv.extend(vec![chroma; 16 * 16 * 2]);
        let encoded = encoder
            .encode(&YUVBuffer::from_vec(yuv, 32, 32))
            .unwrap()
            .to_vec();
        let mut wire = (operation << 30).to_le_bytes().to_vec();
        wire.extend(1u32.to_le_bytes());
        for edge in rect {
            wire.extend(edge.to_le_bytes());
        }
        wire.extend([0, 100]);
        wire.extend(encoded);
        for (codec, decoder, pixels) in &mut decoders {
            decoder.decode(*codec, &wire, pixels, 32, 32).unwrap();
        }
    }
    for (codec, _, pixels) in &decoders {
        for y in 2..4 {
            for x in 2..4 {
                let offset = (y * 32 + x) * 4;
                let pixel = &pixels[offset..offset + 4];
                assert!(
                    pixel[..3].iter().all(|value| value.abs_diff(100) <= 4),
                    "codec {codec}, pixel ({x}, {y}): {pixel:?}"
                );
                assert_eq!(pixel[3], 255);
            }
        }
    }
}

#[test]
fn avc420_decodes_a_real_idr_masks_damage_and_rejects_oversized_sps() {
    let mut yuv = vec![96; 32 * 32];
    yuv.extend_from_slice(&[90; 16 * 16]);
    yuv.extend_from_slice(&[200; 16 * 16]);
    let mut encoder = Encoder::new().unwrap();
    let encoded = encoder
        .encode(&YUVBuffer::from_vec(yuv, 32, 32))
        .unwrap()
        .to_vec();
    let limits = Limits {
        max_width: 32,
        max_height: 32,
        max_bitmap_bytes: 32 * 32 * 4,
        ..Limits::default()
    };
    validate_parameter_sets(&encoded, limits).unwrap();
    assert!(matches!(
        validate_parameter_sets(
            &encoded,
            Limits {
                max_width: 16,
                ..limits
            }
        ),
        Err(Error::Limit("H.264 picture dimensions"))
    ));
    assert!(matches!(
        validate_parameter_sets(
            &encoded,
            Limits {
                max_height: 16,
                ..limits
            }
        ),
        Err(Error::Limit("H.264 picture dimensions"))
    ));
    assert!(matches!(
        validate_parameter_sets(
            &encoded,
            Limits {
                max_bitmap_bytes: 32 * 32 * 4 - 1,
                ..limits
            }
        ),
        Err(Error::Limit("H.264 picture allocation"))
    ));

    let region = Rect {
        left: 4,
        top: 4,
        right: 28,
        bottom: 28,
    };
    let mut payload = 1u32.to_le_bytes().to_vec();
    for edge in [region.left, region.top, region.right, region.bottom] {
        payload.extend_from_slice(&edge.to_le_bytes());
    }
    payload.extend_from_slice(&[0, 100]);
    payload.extend_from_slice(&encoded);
    let mut pixels = vec![0xa5; 32 * 32 * 4];
    let mut decoder = Avc::new(32, 32, limits).unwrap();
    assert_eq!(
        decoder.decode(11, &payload, &mut pixels, 32, 32).unwrap(),
        [region]
    );
    for y in 0..32 {
        for x in 0..32 {
            let pixel = &pixels[(y * 32 + x) * 4..(y * 32 + x + 1) * 4];
            if (4..28).contains(&x) && (4..28).contains(&y) {
                for (&actual, expected) in pixel.iter().zip([25, 69, 209, 255]) {
                    assert!(
                        actual.abs_diff(expected) <= 3,
                        "pixel ({x}, {y}): {pixel:?}"
                    );
                }
            } else {
                assert_eq!(pixel, [0xa5; 4]);
            }
        }
    }
}

#[test]
fn audit_counts_decodes_without_pictures_and_resets() {
    let mut encoder = Encoder::new().unwrap();
    let encoded = encoder
        .encode(&YUVBuffer::from_vec(vec![128; 32 * 32 * 3 / 2], 32, 32))
        .unwrap()
        .to_vec();
    let headers: Vec<u8> = openh264::nal_units(&encoded)
        .filter(|nal| {
            let start = nal.iter().position(|byte| *byte != 0).unwrap() + 1;
            matches!(nal[start] & 0x1f, 7 | 8)
        })
        .flatten()
        .copied()
        .collect();
    assert!(!headers.is_empty());
    let mut decoder = Avc::new(32, 32, Limits::default()).unwrap();
    let mut pixels = vec![0; 32 * 32 * 4];
    let mut payload = 1u32.to_le_bytes().to_vec();
    for edge in [0u16, 0, 32, 32] {
        payload.extend_from_slice(&edge.to_le_bytes());
    }
    payload.extend_from_slice(&[0, 100]);
    let metadata_len = payload.len();
    payload.extend_from_slice(&headers);
    decoder.decode(11, &payload, &mut pixels, 32, 32).unwrap();
    payload.truncate(metadata_len);
    payload.extend_from_slice(&encoded);
    decoder.decode(11, &payload, &mut pixels, 32, 32).unwrap();
    assert_eq!(decoder.take_audit(), (2, 1));
    assert_eq!(decoder.take_audit(), (0, 0));
}

#[test]
fn avc_incremental_frames_replace_the_old_window_position() {
    check_incremental_window_moves(Profile::Baseline);
}

#[test]
fn avc_main_profile_incremental_frames_replace_the_old_window_position() {
    check_incremental_window_moves(Profile::Main);
}

#[test]
fn avc_high_profile_incremental_frames_replace_the_old_window_position() {
    check_incremental_window_moves(Profile::High);
}

fn check_incremental_window_moves(profile: Profile) {
    let mut encoder = Encoder::with_api_config(
        OpenH264API::from_source(),
        EncoderConfig::new().profile(profile),
    )
    .unwrap();
    let mut decoders = [
        (
            11,
            Avc::new(64, 32, Limits::default()).unwrap(),
            vec![0; 64 * 32 * 4],
        ),
        (
            15,
            Avc::new(64, 32, Limits::default()).unwrap(),
            vec![0; 64 * 32 * 4],
        ),
    ];
    let mut mismatches = Vec::new();
    for (frame, left) in [0usize, 16, 32, 48].into_iter().enumerate() {
        let mut yuv = vec![24; 64 * 32];
        for y in 8..24 {
            yuv[y * 64 + left..y * 64 + left + 16].fill(224);
        }
        yuv.extend_from_slice(&[128; 32 * 16 * 2]);
        let encoded = encoder
            .encode(&YUVBuffer::from_vec(yuv, 64, 32))
            .unwrap()
            .to_vec();
        assert!(!encoded.is_empty());
        let region = if frame == 0 {
            Rect {
                left: 0,
                top: 0,
                right: 64,
                bottom: 32,
            }
        } else {
            Rect {
                left: (left - 16) as u16,
                top: 8,
                right: (left + 16) as u16,
                bottom: 24,
            }
        };
        let mut payload = 1u32.to_le_bytes().to_vec();
        for edge in [region.left, region.top, region.right, region.bottom] {
            payload.extend_from_slice(&edge.to_le_bytes());
        }
        payload.extend_from_slice(&[0, 100]);
        payload.extend_from_slice(&encoded);
        for (codec, decoder, pixels) in &mut decoders {
            let mut wire = Vec::new();
            if *codec == 15 {
                // AVC444v2 LC=1: the luma view is the only bitstream.
                wire.extend_from_slice(&0x4000_0000u32.to_le_bytes());
            }
            wire.extend_from_slice(&payload);
            decoder.decode(*codec, &wire, pixels, 64, 32).unwrap();
            // Inspect the entire movement trail, including regions excluded
            // from this update: a delayed picture leaves multiple old windows.
            for x in [8usize, 24, 40, 56] {
                let expected = if x == left + 8 { 224 } else { 24 };
                let actual = pixels[(16 * 64 + x) * 4];
                if actual.abs_diff(expected) > 8 {
                    mismatches.push((frame, *codec, x, actual, expected));
                }
            }
        }
    }
    assert!(
        mismatches.is_empty(),
        "{profile:?}: (frame, codec, x, actual, expected) {mismatches:?}"
    );
}

#[test]
fn avc444_main_and_auxiliary_pictures_stay_paired_for_all_profiles() {
    for profile in [Profile::Baseline, Profile::Main, Profile::High] {
        let mut encoder = Encoder::with_api_config(
            OpenH264API::from_source(),
            EncoderConfig::new().profile(profile),
        )
        .unwrap();
        let mut decoders = [15, 14].map(|codec| {
            (
                codec,
                Avc::new(64, 32, Limits::default()).unwrap(),
                vec![0; 64 * 32 * 4],
            )
        });
        let stream = |encoded: &[u8], rect: Rect| {
            let mut bytes = 1u32.to_le_bytes().to_vec();
            for edge in [rect.left, rect.top, rect.right, rect.bottom] {
                bytes.extend(edge.to_le_bytes());
            }
            bytes.extend([0, 100]);
            bytes.extend(encoded);
            bytes
        };
        let mut previous = 0;
        for (frame, left) in [0usize, 16, 32, 48, 32, 16, 0].into_iter().enumerate() {
            let rect = if frame == 0 {
                Rect {
                    left: 0,
                    top: 0,
                    right: 64,
                    bottom: 32,
                }
            } else {
                Rect {
                    left: left.min(previous) as u16,
                    top: 8,
                    right: (left.max(previous) + 16) as u16,
                    bottom: 24,
                }
            };
            let mut main = vec![24; 64 * 32];
            for y in 8..24 {
                main[y * 64 + left..y * 64 + left + 16].fill(224);
            }
            main.extend([128; 32 * 16 * 2]);
            let main = stream(
                &encoder
                    .encode(&YUVBuffer::from_vec(main, 64, 32))
                    .unwrap()
                    .to_vec(),
                rect,
            );
            // Both auxiliary layouts reconstruct neutral chroma from this
            // picture. Its luma is deliberately unlike the main view's image.
            let auxiliary = stream(
                &encoder
                    .encode(&YUVBuffer::from_vec(vec![128; 64 * 32 * 3 / 2], 64, 32))
                    .unwrap()
                    .to_vec(),
                rect,
            );
            for (codec, decoder, pixels) in &mut decoders {
                if frame % 2 == 0 {
                    let mut wire = (main.len() as u32).to_le_bytes().to_vec();
                    wire.extend(&main);
                    wire.extend(&auxiliary);
                    decoder.decode(*codec, &wire, pixels, 64, 32).unwrap();
                } else {
                    // The same two views can also arrive in separate PDUs.
                    for (operation, bytes) in [(1u32, &main), (2, &auxiliary)] {
                        let mut wire = (operation << 30).to_le_bytes().to_vec();
                        wire.extend(bytes);
                        decoder.decode(*codec, &wire, pixels, 64, 32).unwrap();
                    }
                }
                for x in (0..64).step_by(16) {
                    let expected = if x == left { 224 } else { 24 };
                    let actual = pixels[(16 * 64 + x + 8) * 4];
                    assert!(
                        actual.abs_diff(expected) <= 8,
                        "{profile:?}, codec {codec}, frame {frame}, x={x}: {actual} != {expected}"
                    );
                }
            }
            previous = left;
        }
    }
}

#[test]
fn aligned_encoder_canvas_can_exceed_the_surface_macroblock_extent() {
    // Windows can align its canvas to 32 pixels rather than the surface's
    // nearest 16-pixel macroblock. Metadata masks still describe surface pixels.
    let mut yuv = vec![96; 64 * 32];
    yuv.extend_from_slice(&[90; 32 * 16]);
    yuv.extend_from_slice(&[200; 32 * 16]);
    let encoded = Encoder::new()
        .unwrap()
        .encode(&YUVBuffer::from_vec(yuv, 64, 32))
        .unwrap()
        .to_vec();
    let limits = Limits {
        max_width: 64,
        max_height: 32,
        max_bitmap_bytes: 64 * 32 * 4,
        ..Limits::default()
    };
    let rect = Rect {
        left: 1,
        top: 1,
        right: 17,
        bottom: 18,
    };
    let mut stream = 1u32.to_le_bytes().to_vec();
    for edge in [rect.left, rect.top, rect.right, rect.bottom] {
        stream.extend_from_slice(&edge.to_le_bytes());
    }
    stream.extend_from_slice(&[0, 100]);
    stream.extend_from_slice(&encoded);
    for codec in [11, 14, 15] {
        let mut decoder = Avc::new(18, 19, limits).unwrap();
        let mut pixels = vec![0xa5; 18 * 19 * 4];
        let mut payload = Vec::new();
        if codec != 11 {
            payload.extend_from_slice(&(0x4000_0000 | stream.len() as u32).to_le_bytes());
        }
        payload.extend_from_slice(&stream);
        assert_eq!(
            decoder
                .decode(codec, &payload, &mut pixels, 18, 19)
                .unwrap(),
            [rect]
        );
        for y in 0..19 {
            for x in 0..18 {
                let pixel = &pixels[(y * 18 + x) * 4..(y * 18 + x + 1) * 4];
                if (1..17).contains(&x) && (1..18).contains(&y) {
                    for (&actual, expected) in pixel.iter().zip([25, 69, 209, 255]) {
                        assert!(
                            actual.abs_diff(expected) <= 3,
                            "codec {codec}, ({x},{y}): {pixel:?}"
                        );
                    }
                } else {
                    assert_eq!(pixel, [0xa5; 4]);
                }
            }
        }
        if let Some(planes) = &decoder.planes {
            assert!(planes.iter().all(|plane| plane.len() == 32 * 32));
        }
    }
}

#[test]
fn auxiliary_canvas_width_is_independent_of_surface_reference_stride() {
    let fixture = PlanarFixture::new();
    let mut planes = [vec![77; 16 * 16], vec![101; 16 * 16], vec![151; 16 * 16]];
    let rect = Rect {
        left: 0,
        top: 0,
        right: 9,
        bottom: 11,
    };
    combine(&mut planes, 16, &fixture, rect, 3).unwrap();
    // Chroma halves are located using the encoded 32-pixel canvas, while
    // reference rows retain the destination surface's 16-pixel stride.
    assert_eq!((planes[1][1], planes[2][1]), (0, 48));
    assert_eq!((planes[1][16], planes[2][16]), (64, 80));
    assert!(planes.iter().all(|plane| plane.len() == 16 * 16));
}
