use crate::{Error, Result, wire::Reader};

// MS-RDPBCGR 2.2.9.1.1.3.1.2.4. Pixels stay in the wire color space until
// decoding completes, since XOR prediction operates on packed pixel values.
pub(super) fn decode(data: &[u8], width: usize, height: usize, bpp: u16) -> Result<Vec<u32>> {
    let count = width
        .checked_mul(height)
        .ok_or(Error::Limit("bitmap size"))?;
    let mut input = Reader::new(data);
    let mut pixels = Vec::with_capacity(count);
    let white = match bpp {
        8 => 0xff,
        15 => 0x7fff,
        16 => 0xffff,
        24 => 0xff_ffff,
        _ => return Err(Error::Unsupported("interleaved bitmap depth")),
    };
    let bytes = usize::from(bpp.div_ceil(8));
    let mut foreground = white;
    let mut previous_background = false;
    let mut first_row = true;
    while !input.is_empty() {
        if first_row && pixels.len() >= width {
            first_row = false;
            previous_background = false;
        }
        let header = input.u8()?;
        let code = if header < 0xc0 {
            header >> 5
        } else if header < 0xf0 {
            header >> 4
        } else {
            header
        };
        let length = match code {
            2 | 13 => {
                let short = usize::from(header & if code == 2 { 31 } else { 15 });
                if short == 0 {
                    usize::from(input.u8()?) + 1
                } else {
                    short * 8
                }
            }
            0..=4 | 12 | 14 => {
                let mask = if code <= 4 { 31 } else { 15 };
                let short = usize::from(header & mask);
                if short == 0 {
                    usize::from(input.u8()?) + usize::from(mask) + 1
                } else {
                    short
                }
            }
            0xf0..=0xf4 | 0xf6..=0xf8 => usize::from(input.u16_le()?),
            0xf9 | 0xfa => 8,
            0xfd | 0xfe => 1,
            _ => return Err(Error::Invalid("interleaved RLE order")),
        };
        let dither = matches!(code, 14 | 0xf8);
        let output_length = if dither { length * 2 } else { length };
        if length == 0 || output_length > count.saturating_sub(pixels.len()) {
            return Err(Error::Invalid("interleaved RLE run length"));
        }
        let background = matches!(code, 0 | 0xf0);
        match code {
            0 | 0xf0 => {
                for i in 0..length {
                    let prior = predecessor(&pixels, width);
                    pixels.push(
                        prior
                            ^ if i == 0 && previous_background {
                                foreground
                            } else {
                                0
                            },
                    );
                }
            }
            1 | 12 | 0xf1 | 0xf6 => {
                if matches!(code, 12 | 0xf6) {
                    foreground = pixel(&mut input, bytes)?;
                }
                for _ in 0..length {
                    pixels.push(predecessor(&pixels, width) ^ foreground);
                }
            }
            2 | 13 | 0xf2 | 0xf7 | 0xf9 | 0xfa => {
                if matches!(code, 13 | 0xf7) {
                    foreground = pixel(&mut input, bytes)?;
                }
                let mut mask = 0;
                for i in 0..length {
                    if i.is_multiple_of(8) {
                        mask = match code {
                            0xf9 => 0x03,
                            0xfa => 0x05,
                            _ => input.u8()?,
                        };
                    }
                    pixels.push(
                        predecessor(&pixels, width)
                            ^ if mask & (1 << (i % 8)) != 0 {
                                foreground
                            } else {
                                0
                            },
                    );
                }
            }
            3 | 0xf3 => {
                let color = pixel(&mut input, bytes)?;
                pixels.resize(pixels.len() + length, color);
            }
            4 | 0xf4 => {
                for _ in 0..length {
                    pixels.push(pixel(&mut input, bytes)?);
                }
            }
            14 | 0xf8 => {
                let a = pixel(&mut input, bytes)?;
                let b = pixel(&mut input, bytes)?;
                for _ in 0..length {
                    pixels.extend_from_slice(&[a, b]);
                }
            }
            0xfd => pixels.push(white),
            0xfe => pixels.push(0),
            _ => unreachable!(),
        }
        previous_background = background;
    }
    if pixels.len() != count {
        return Err(Error::Truncated);
    }
    Ok(pixels)
}

fn predecessor(pixels: &[u32], width: usize) -> u32 {
    pixels
        .len()
        .checked_sub(width)
        .map_or(0, |index| pixels[index])
}

fn pixel(input: &mut Reader<'_>, bytes: usize) -> Result<u32> {
    let mut value = 0;
    for (shift, byte) in input.take(bytes)?.iter().enumerate() {
        value |= u32::from(*byte) << (shift * 8);
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_background_and_foreground_prediction() {
        assert_eq!(
            decode(&[0x82, 10, 20, 0x02], 2, 2, 8).unwrap(),
            [10, 20, 10, 20]
        );
        assert_eq!(decode(&[0xc2, 7, 0x22], 2, 2, 8).unwrap(), [7, 7, 0, 0]);
    }

    #[test]
    fn fg_bg_masks_dither_and_mega_orders() {
        assert_eq!(
            decode(&[0xd0, 3, 0x7f, 0x05], 4, 1, 8).unwrap(),
            [127, 0, 127, 0]
        );
        assert_eq!(decode(&[0xe2, 1, 2], 4, 1, 8).unwrap(), [1, 2, 1, 2]);
        assert_eq!(decode(&[0xf3, 3, 0, 9], 3, 1, 8).unwrap(), [9; 3]);
        assert_eq!(decode(&[0x01, 0x02], 3, 1, 8).unwrap(), [0, 255, 0]);
    }

    #[test]
    fn rejects_invalid_orders_and_lengths() {
        for bytes in [
            &[0xff][..],
            &[0x63, 1],
            &[0xf3, 0, 0, 1],
            &[0x82, 1],
            &[0x61, 1, 0xfe],
        ] {
            assert!(decode(bytes, 1, 1, 8).is_err());
        }
    }
}
