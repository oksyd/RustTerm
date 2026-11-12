use super::*;

#[test]
fn microsoft_first_progressive_pass_rlgr_vector() {
    // MS-RDPEGFX 4.1.2.1.1, original encoded bytes before LL reconstruction.
    let bytes = [0xa8, 0x62, 0x6d, 0xff, 0xf7, 0];
    assert_eq!(
        rlgr_values(&bytes, false, 14).unwrap(),
        [-2, 0, 0, 0, 0, 0, 0, 0, 0, 1, 3, 1, -7, 6]
    );
}

#[test]
fn inverse_lifting_handles_symmetric_and_extrapolated_edges() {
    let mut symmetric = [0; 4];
    synthesize(&[10, 20], &[2, 4], false, &mut symmetric);
    assert_eq!(symmetric, [8, 16, 17, 25]);
    let mut odd = [0; 5];
    synthesize(&[10, 20, 30], &[2, 4], true, &mut odd);
    assert_eq!(odd, [8, 16, 17, 29, 26]);
    let mut extrapolated = [0; 6];
    synthesize(&[10, 20, 30, 40], &[2, 4], true, &mut extrapolated);
    assert_eq!(extrapolated, [8, 16, 17, 30, 28, 34]);
}

#[test]
fn row_wise_vertical_lifting_matches_column_reference_at_every_codec_stage() {
    let mut seed = 0x76e8_19a4u32;
    for (low, high, reduced) in [
        (8, 8, false),
        (16, 16, false),
        (32, 32, false),
        (9, 8, true),
        (17, 16, true),
        (33, 31, true),
    ] {
        let size = low + high;
        for full_range in [false, true] {
            for _ in 0..16 {
                let input: Vec<i32> = (0..size * size)
                    .map(|_| {
                        seed ^= seed << 13;
                        seed ^= seed >> 17;
                        seed ^= seed << 5;
                        if full_range {
                            seed as i32
                        } else {
                            (seed as i32) >> 16
                        }
                    })
                    .collect();
                let mut expected = vec![0; size * size];
                for x in 0..size {
                    let low_line: Vec<_> = (0..low).map(|y| input[y * size + x]).collect();
                    let high_line: Vec<_> =
                        (0..high).map(|y| input[(y + low) * size + x]).collect();
                    let mut line = vec![0; size];
                    synthesize(&low_line, &high_line, reduced, &mut line);
                    for (y, value) in line.into_iter().enumerate() {
                        expected[y * size + x] = value;
                    }
                }
                let mut actual = vec![0; size * size];
                synthesize_vertical(&input, low, high, reduced, &mut actual);
                assert_eq!(
                    actual, expected,
                    "stage {low}/{high}, full_range={full_range}"
                );
            }
        }
    }
}

#[test]
fn inverse_constant_tiles_and_full_range_color() {
    for reduced in [false, true] {
        let mut coefficients = vec![0; 4096];
        let (start, count) = bands(reduced)[9];
        coefficients[start..start + count].fill(-4096);
        inverse(&mut coefficients, reduced);
        assert!(coefficients.iter().all(|value| *value == -4096));
        let pixels = colors(&[coefficients, vec![0; 4096], vec![0; 4096]]);
        assert!(
            pixels
                .as_chunks::<4>()
                .0
                .iter()
                .all(|pixel| *pixel == [0, 0, 0, 255])
        );
    }
}

#[test]
fn quantization_uses_wire_band_order_and_bounded_coefficients() {
    let mut coefficients = vec![1; 4096];
    dequantize(&mut coefficients, [1, 2, 3, 4, 5, 6, 7, 8, 9, 10], false).unwrap();
    for (band, (start, count)) in bands(false).into_iter().enumerate() {
        assert!(
            coefficients[start..start + count]
                .iter()
                .all(|value| *value == 1 << QUANT_ORDER[band])
        );
    }
    assert!(scale(i32::MAX, 1).is_err());
    assert!(rlgr(&[], false).is_err());
}
