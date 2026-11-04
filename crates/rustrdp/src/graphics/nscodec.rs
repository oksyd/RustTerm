use std::borrow::Cow;

use crate::{Error, Result, wire::Reader};

// MS-RDPNSC 2.2.2 and 3.1.8.4. The caller validates the bitmap against its
// dimension and decoded-byte limits before entering this codec.
pub(crate) fn decode(data: &[u8], width: usize, height: usize) -> Result<Vec<u8>> {
    if width == 0 || height == 0 {
        return Err(Error::Invalid("NSCodec bitmap dimensions"));
    }
    let area = |width: usize, height: usize| {
        width
            .checked_mul(height)
            .ok_or(Error::Limit("NSCodec bitmap dimensions"))
    };
    let pixel_count = area(width, height)?;
    let output_size = area(pixel_count, 4)?;
    let mut input = Reader::new(data);
    let lengths = [
        input.u32_le()? as usize,
        input.u32_le()? as usize,
        input.u32_le()? as usize,
        input.u32_le()? as usize,
    ];
    let loss = input.u8()?;
    let subsampled = match input.u8()? {
        0 => false,
        1 => true,
        _ => return Err(Error::Invalid("NSCodec subsampling level")),
    };
    input.skip(2)?;
    if !(1..=7).contains(&loss) {
        return Err(Error::Invalid("NSCodec color loss level"));
    }
    let luma_stride = if subsampled {
        width
            .checked_add(7)
            .ok_or(Error::Limit("NSCodec padded width"))?
            & !7
    } else {
        width
    };
    let chroma_stride = if subsampled { luma_stride / 2 } else { width };
    let chroma_height = if subsampled {
        height.div_ceil(2)
    } else {
        height
    };
    let luma_size = area(luma_stride, height)?;
    let chroma_size = area(chroma_stride, chroma_height)?;
    let expected = [luma_size, chroma_size, chroma_size, pixel_count];
    for (index, (&length, &size)) in lengths.iter().zip(&expected).enumerate() {
        if length > size || (length == 0 && index != 3) {
            return Err(Error::Invalid("NSCodec plane length"));
        }
    }
    // Validate all input slices before allocating decoded planes. Each RLE
    // decoder is confined to its own declared plane, including the raw tail.
    let encoded = [
        input.take(lengths[0])?,
        input.take(lengths[1])?,
        input.take(lengths[2])?,
        input.take(lengths[3])?,
    ];
    input.finish()?;
    let luma = plane(encoded[0], luma_size)?;
    let orange = plane(encoded[1], chroma_size)?;
    let green = plane(encoded[2], chroma_size)?;
    let alpha = if encoded[3].is_empty() {
        None
    } else {
        Some(plane(encoded[3], pixel_count)?)
    };
    let mut pixels = Vec::new();
    pixels
        .try_reserve_exact(output_size)
        .map_err(|_| Error::Limit("NSCodec bitmap allocation"))?;
    pixels.resize(output_size, 0);
    let chroma_shift = usize::from(subsampled);
    let recover = |value: u8| i16::from(value.wrapping_shl(u32::from(loss - 1)) as i8);
    // Wire planes are bottom-up. Combine and flip in one pass; the alpha plane
    // is tightly packed even when the color planes have subsampling padding.
    for (y, row) in pixels.chunks_exact_mut(width * 4).rev().enumerate() {
        let chroma_row = (y >> chroma_shift) * chroma_stride;
        for (x, target) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let luma = i16::from(luma[y * luma_stride + x]);
            let chroma = chroma_row + (x >> chroma_shift);
            let co = recover(orange[chroma]);
            let cg = recover(green[chroma]);
            // NSCodec uses the normal inverse YCoCg transform, unlike the
            // legacy planar codec's R/B compatibility interchange.
            target[0] = (luma - co - cg).clamp(0, 255) as u8;
            target[1] = (luma + cg).clamp(0, 255) as u8;
            target[2] = (luma + co - cg).clamp(0, 255) as u8;
            target[3] = alpha.as_ref().map_or(255, |plane| plane[y * width + x]);
        }
    }
    Ok(pixels)
}

fn plane(data: &[u8], size: usize) -> Result<Cow<'_, [u8]>> {
    if data.len() == size {
        return Ok(Cow::Borrowed(data));
    }
    if size <= 4 || data.len() >= size {
        return Err(Error::Invalid("NSCodec plane length"));
    }
    let mut input = Reader::new(data);
    let mut decoded = Vec::new();
    decoded
        .try_reserve_exact(size)
        .map_err(|_| Error::Limit("NSCodec plane allocation"))?;
    let prefix = size - 4;
    while decoded.len() < prefix {
        let value = input.u8()?;
        let remaining = prefix - decoded.len();
        // The final prefix byte is a literal even if the raw tail starts with
        // the same value. Runs must never consume those final four pixels.
        if remaining > 1 && input.remaining().first() == Some(&value) {
            input.skip(1)?;
            let factor = input.u8()?;
            let count = if factor == 255 {
                input.u32_le()? as usize
            } else {
                usize::from(factor) + 2
            };
            if count < 2 || count > remaining {
                return Err(Error::Invalid("NSCodec RLE run length"));
            }
            decoded.resize(decoded.len() + count, value);
        } else {
            decoded.push(value);
        }
    }
    decoded.extend_from_slice(input.take(4)?);
    input.finish()?;
    Ok(Cow::Owned(decoded))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bitmap(planes: [&[u8]; 4], loss: u8, subsampled: u8) -> Vec<u8> {
        let mut encoded = Vec::new();
        for plane in planes {
            encoded.extend_from_slice(&(plane.len() as u32).to_le_bytes());
        }
        encoded.extend_from_slice(&[loss, subsampled, 0, 0]);
        for plane in planes {
            encoded.extend_from_slice(plane);
        }
        encoded
    }

    #[test]
    fn microsoft_protocol_example_decodes_exactly() {
        // MS-RDPNSC section 4: 15x10 image, all four planes RLE compressed.
        // https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpnsc/e4d67d59-b541-4546-9a1b-1966da6cfee4
        let wire = include_bytes!("fixtures/nscodec.bin");
        let expected: Vec<u8> = include_bytes!("fixtures/nscodec.bgra")
            .chunks_exact(15 * 4)
            .rev()
            .flatten()
            .copied()
            .collect();
        assert_eq!(decode(wire, 15, 10).unwrap(), expected);
    }

    #[test]
    fn raw_planes_preserve_alpha_and_flip_rows() {
        let encoded = bitmap(
            [
                &[10, 20, 30, 40],
                &[1, 0, 255, 2],
                &[0, 1, 255, 254],
                &[11, 22, 33, 44],
            ],
            1,
            0,
        );
        assert_eq!(
            decode(&encoded, 2, 2).unwrap(),
            [
                32, 29, 30, 33, 40, 38, 44, 44, 9, 10, 11, 11, 19, 21, 19, 22
            ]
        );
    }

    #[test]
    fn all_color_loss_levels_restore_signed_chroma_and_clamp() {
        for loss in 1..=7 {
            let encoded = bitmap(
                [&[128], &[1 << (7 - loss)], &[3 << (7 - loss)], &[]],
                loss,
                0,
            );
            assert_eq!(decode(&encoded, 1, 1).unwrap(), [128, 64, 255, 255]);
        }
        let encoded = bitmap([&[128], &[2], &[1], &[]], 7, 0);
        assert_eq!(decode(&encoded, 1, 1).unwrap(), [192, 192, 0, 255]);
    }

    #[test]
    fn odd_subsampled_dimensions_ignore_padding_and_use_packed_alpha() {
        let encoded = bitmap(
            [
                &[
                    10, 20, 30, 222, 222, 222, 222, 222, 40, 50, 60, 222, 222, 222, 222, 222, 70,
                    80, 90, 222, 222, 222, 222, 222,
                ],
                &[1, 2, 222, 222, 3, 4, 222, 222],
                &[0, 0, 222, 222, 0, 0, 222, 222],
                &[1, 2, 3, 4, 5, 6, 7, 8, 9],
            ],
            1,
            1,
        );
        assert_eq!(
            decode(&encoded, 3, 3).unwrap(),
            [
                67, 70, 73, 7, 77, 80, 83, 8, 86, 90, 94, 9, 39, 40, 41, 4, 49, 50, 51, 5, 58, 60,
                62, 6, 9, 10, 11, 1, 19, 20, 21, 2, 28, 30, 32, 3
            ]
        );
    }

    #[test]
    fn rle_literals_short_and_long_runs_preserve_the_raw_tail() {
        assert_eq!(
            plane(&[7, 7, 2, 8, 8, 9, 10, 11], 9).unwrap().as_ref(),
            &[7, 7, 7, 7, 8, 8, 9, 10, 11]
        );
        let mut expected = vec![42; 256];
        expected.extend_from_slice(&[1, 2, 3, 4]);
        assert_eq!(
            plane(&[42, 42, 255, 0, 1, 0, 0, 1, 2, 3, 4], 260)
                .unwrap()
                .as_ref(),
            expected
        );
        assert_eq!(
            plane(&[42, 42, 254, 1, 2, 3, 4], 260).unwrap().as_ref(),
            expected
        );
    }

    #[test]
    fn malformed_rle_cannot_overrun_planes_or_borrow_the_next_plane() {
        for encoded in [
            &[1, 1, 255, 255, 255, 255, 255, 0, 0, 0, 0][..],
            &[1, 1, 255, 0, 0, 0, 0, 0, 0, 0, 0],
            &[1, 1, 9, 0, 0, 0, 0],
            &[1, 1, 6, 1, 2, 3],
            &[1, 1, 6, 1, 2, 3, 4, 99],
        ] {
            assert!(plane(encoded, 12).is_err());
        }
        assert!(plane(&[1, 2, 3], 4).is_err());
    }

    #[test]
    fn truncated_and_invalid_streams_are_rejected() {
        let wire = include_bytes!("fixtures/nscodec.bin");
        for end in 0..wire.len() {
            assert!(decode(&wire[..end], 15, 10).is_err(), "prefix {end}");
        }
        for (offset, value) in [(16, 0), (16, 8), (17, 2)] {
            let mut invalid = wire.to_vec();
            invalid[offset] = value;
            assert!(decode(&invalid, 15, 10).is_err());
        }
        for length in [0u32, u32::MAX] {
            let mut invalid = wire.to_vec();
            invalid[..4].copy_from_slice(&length.to_le_bytes());
            assert!(decode(&invalid, 15, 10).is_err());
        }
        let mut trailing = wire.to_vec();
        trailing.push(0);
        assert!(decode(&trailing, 15, 10).is_err());
        assert!(decode(wire, 0, 10).is_err());
        assert!(decode(wire, 15, 0).is_err());
        assert!(decode(wire, usize::MAX, 2).is_err());
        assert!(decode(wire, usize::MAX / 2, 1).is_err());
    }
}
