use crate::{Error, Result, wire::Reader};

// MS-RDPEGDI 2.2.2.5.1 and 3.1.9: planar RGB/AYCoCg with optional
// chroma reduction, 2x2 subsampling and per-plane RLE/delta coding.
pub(crate) fn decode(data: &[u8], width: usize, height: usize) -> Result<Vec<u8>> {
    decode_rows(data, width, height, true)
}

#[cfg(feature = "graphics")]
pub(crate) fn decode_top_down(data: &[u8], width: usize, height: usize) -> Result<Vec<u8>> {
    decode_rows(data, width, height, false)
}

fn decode_rows(data: &[u8], width: usize, height: usize, bottom_up: bool) -> Result<Vec<u8>> {
    let bytes = width
        .checked_mul(height)
        .and_then(|n| n.checked_mul(4))
        .filter(|n| *n > 0)
        .ok_or(Error::Invalid("planar bitmap dimensions"))?;
    let mut input = Reader::new(data);
    let format = input.u8()?;
    if format & 0xc0 != 0 {
        return Err(Error::Invalid("planar bitmap flags"));
    }
    let loss = format & 7;
    let subsampled = format & 8 != 0;
    if subsampled && loss == 0 {
        return Err(Error::Invalid("chroma subsampling without YCoCg"));
    }
    let rle = format & 0x10 != 0;
    let alpha = if format & 0x20 == 0 {
        Some(plane(&mut input, width, height, rle)?)
    } else {
        None
    };
    let (chroma_width, chroma_height) = if subsampled {
        (width.div_ceil(2), height.div_ceil(2))
    } else {
        (width, height)
    };
    let red_or_luma = plane(&mut input, width, height, rle)?;
    let green_or_co = plane(&mut input, chroma_width, chroma_height, rle)?;
    let blue_or_cg = plane(&mut input, chroma_width, chroma_height, rle)?;
    if !rle && input.remaining_len() == 1 {
        input.skip(1)?;
    }
    if !input.is_empty() {
        return Err(Error::Invalid("trailing planar bitmap data"));
    }
    let mut pixels = vec![0; bytes];
    for y in 0..height {
        for x in 0..width {
            let source = y * width + x;
            let target = ((if bottom_up { height - 1 - y } else { y }) * width + x) * 4;
            let color = if loss == 0 {
                [blue_or_cg[source], green_or_co[source], red_or_luma[source]]
            } else {
                // Expand the chroma lookup, rather than allocating two full-size
                // supersampled planes. The edge cells may contain just one pixel.
                let chroma = if subsampled {
                    (y / 2) * chroma_width + x / 2
                } else {
                    source
                };
                ycocg(
                    red_or_luma[source],
                    green_or_co[chroma],
                    blue_or_cg[chroma],
                    loss,
                )
            };
            pixels[target..target + 3].copy_from_slice(&color);
            pixels[target + 3] = alpha.as_ref().map_or(255, |a| a[source]);
        }
    }
    Ok(pixels)
}

fn ycocg(luma: u8, co: u8, cg: u8, loss: u8) -> [u8; 3] {
    // Recover the two's-complement nine-bit chroma before the inverse matrix.
    // CLL >= 1 makes both values even, so division by two is exact.
    let recover = |value: u8| {
        let bits = (i16::from(value) << loss) & 0x1ff;
        (if bits & 0x100 == 0 {
            bits
        } else {
            bits - 0x200
        }) / 2
    };
    let co = recover(co);
    let cg = recover(cg);
    let y = i16::from(luma);
    // Apply the Microsoft R/B interchange required for RDP bitmap YCoCg
    // by MS-RDPEGDI 3.1.9.1.2, then store the corrected color as BGR.
    [y + co - cg, y + cg, y - co - cg].map(|value| value.clamp(0, 255) as u8)
}

fn plane(input: &mut Reader<'_>, width: usize, height: usize, rle: bool) -> Result<Vec<u8>> {
    let count = width
        .checked_mul(height)
        .ok_or(Error::Limit("planar bitmap size"))?;
    if !rle {
        return Ok(input.take(count)?.to_vec());
    }
    let mut plane = Vec::with_capacity(count);
    for y in 0..height {
        let end = (y + 1) * width;
        let mut last = 0;
        while plane.len() < end {
            let control = input.u8()?;
            let mut raw = usize::from(control >> 4);
            let mut run = usize::from(control & 15);
            if run == 1 || run == 2 {
                run = run * 16 + raw;
                raw = 0;
            }
            if raw + run == 0 || raw + run > end - plane.len() {
                return Err(Error::Invalid("planar RLE scanline length"));
            }
            for &byte in input.take(raw)? {
                last = byte;
                append(&mut plane, width, y, last);
            }
            for _ in 0..run {
                append(&mut plane, width, y, last);
            }
        }
    }
    Ok(plane)
}

fn append(plane: &mut Vec<u8>, width: usize, row: usize, encoded: u8) {
    let value = if row == 0 {
        encoded
    } else {
        let delta = if encoded & 1 == 0 {
            encoded >> 1
        } else {
            (encoded >> 1).wrapping_add(1).wrapping_neg()
        };
        plane[plane.len() - width].wrapping_add(delta)
    };
    plane.push(value);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_planes_are_bgra_and_bottom_up() {
        assert_eq!(
            decode(&[0x20, 10, 11, 20, 21, 30, 31, 0], 1, 2).unwrap(),
            [31, 21, 11, 255, 30, 20, 10, 255]
        );
    }

    #[test]
    fn rle_delta_and_extended_runs() {
        let mut input = Reader::new(&[0x13, 255, 0x40, 1, 125, 245, 194]);
        assert_eq!(
            plane(&mut input, 4, 2, true).unwrap(),
            [255, 255, 255, 255, 254, 192, 132, 96]
        );
        let mut input = Reader::new(&[0x10, 42, 0x01]);
        assert_eq!(plane(&mut input, 17, 1, true).unwrap(), [42; 17]);
    }

    #[test]
    fn invalid_planes_are_rejected() {
        assert!(decode(&[0x30, 0], 1, 1).is_err());
        assert!(decode(&[0x30, 0x20, 1, 2], 1, 1).is_err());
        assert!(decode(&[0x20, 1, 2], 1, 1).is_err());
        assert_eq!(decode(&[0x21], 1, 1), Err(Error::Truncated));
        assert!(matches!(
            decode(&[0x28, 0, 0, 0], 1, 1),
            Err(Error::Invalid(_))
        ));
        assert!(decode(&[0x60, 0, 0, 0], 1, 1).is_err());
        assert!(decode(&[0x20, 0, 0, 0, 0, 0], 1, 1).is_err());
        assert!(decode(&[0x20], 0, 1).is_err());
        assert!(decode(&[0x20], usize::MAX, 2).is_err());
    }

    #[test]
    fn ycocg_recovers_signed_chroma_and_clamps_without_changing_alpha() {
        let data = [
            0x01, 17, 34, 51, 68, // alpha
            100, 100, 0, 250, // luma
            10, 246, 127, 128, // Co / 2, in two's complement
            5, 251, 128, 127, // Cg / 2
        ];
        assert_eq!(
            decode(&data, 4, 1).unwrap(),
            [
                105, 105, 85, 17, 95, 95, 115, 34, 255, 0, 1, 51, 0, 255, 251, 68,
            ]
        );
        // Co=128 and Cg=-128 survive every specified loss level exactly.
        for loss in 1..=7 {
            let data = [0x20 | loss, 128, 1 << (7 - loss), 3 << (7 - loss)];
            assert_eq!(decode(&data, 1, 1).unwrap(), [255, 64, 128, 255]);
        }
    }

    #[test]
    fn odd_chroma_planes_expand_before_bottom_up_conversion() {
        let raw = [
            0x29, 10, 20, 30, 40, 50, 60, 70, 80, 90, // 3x3 luma, bottom up
            1, 2, 3, 4, // 2x2 Co plane
            0, 0, 0, 0, // 2x2 Cg plane
        ];
        let expected = [
            73, 70, 67, 255, 83, 80, 77, 255, 94, 90, 86, 255, 41, 40, 39, 255, 51, 50, 49, 255,
            62, 60, 58, 255, 11, 10, 9, 255, 21, 20, 19, 255, 32, 30, 28, 255,
        ];
        assert_eq!(decode(&raw, 3, 3).unwrap(), expected);
        let rle = [
            0x39, 0x30, 10, 20, 30, 0x30, 60, 60, 60, 0x30, 60, 60, 60, 0x20, 1, 2, 0x20, 4, 4,
            0x20, 0, 0, 0x20, 0, 0,
        ];
        assert_eq!(decode(&rle, 3, 3).unwrap(), expected);
        assert!(decode(&raw[..raw.len() - 1], 3, 3).is_err());
        assert!(decode(&rle[..rle.len() - 1], 3, 3).is_err());
    }
}
