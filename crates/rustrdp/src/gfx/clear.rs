use super::alpha::run_length;
use crate::{Error, Result, wire::Reader};

type Pixel = [u8; 4];
type Column = Box<[Pixel]>;

// MS-RDPEGFX 2.2.4.1 and 3.3.1.9-13. Caches and the sequence number belong to
// the graphics connection, not to an individual offscreen surface.
pub(super) struct Decoder {
    sequence: u8,
    glyphs: Vec<Option<Box<[u8]>>>,
    bars: Vec<Option<Column>>,
    short_bars: Vec<Option<Column>>,
    next_bar: usize,
    next_short: usize,
}

impl Decoder {
    pub fn new() -> Self {
        Self {
            sequence: 0,
            glyphs: vec![None; 4000],
            bars: vec![None; 32768],
            short_bars: vec![None; 16384],
            next_bar: 0,
            next_short: 0,
        }
    }

    pub fn decode(&mut self, data: &[u8], width: u16, height: u16) -> Result<Vec<u8>> {
        let width = usize::from(width);
        let height = usize::from(height);
        let count = width * height;
        let size = count
            .checked_mul(4)
            .ok_or(Error::Limit("ClearCodec bitmap"))?;
        let mut input = Reader::new(data);
        let flags = input.u8()?;
        if flags & !7 != 0 || flags & 3 == 2 || count == 0 {
            return Err(Error::Invalid("ClearCodec bitmap flags"));
        }
        if input.u8()? != self.sequence {
            return Err(Error::Invalid("ClearCodec sequence"));
        }
        let glyph = if flags & 1 != 0 {
            let index = usize::from(input.u16_le()?);
            if index >= self.glyphs.len() || count > 1024 {
                return Err(Error::Invalid("ClearCodec glyph index or size"));
            }
            Some(index)
        } else {
            None
        };
        if flags & 4 != 0 {
            // Reset cursors only: previously cached glyphs and bars remain valid.
            self.next_bar = 0;
            self.next_short = 0;
        }
        let pixels = if flags & 2 != 0 {
            input.finish()?;
            let stored = glyph
                .and_then(|i| self.glyphs[i].as_ref())
                .filter(|pixels| pixels.len() == size)
                .ok_or(Error::Invalid("ClearCodec glyph cache miss"))?;
            stored.to_vec()
        } else {
            let residual_size = input.u32_le()? as usize;
            let bands_size = input.u32_le()? as usize;
            let subcodecs_size = input.u32_le()? as usize;
            let residual = input.take(residual_size)?;
            let bands = input.take(bands_size)?;
            let subcodecs = input.take(subcodecs_size)?;
            input.finish()?;
            let mut pixels = Vec::new();
            pixels
                .try_reserve_exact(size)
                .map_err(|_| Error::Limit("ClearCodec bitmap allocation"))?;
            pixels.resize(size, 0);
            for pixel in pixels.as_chunks_mut::<4>().0 {
                pixel[3] = 255;
            }
            decode_residual(residual, &mut pixels)?;
            self.decode_bands(bands, &mut pixels, width, height)?;
            decode_subcodecs(subcodecs, &mut pixels, width, height)?;
            if let Some(index) = glyph {
                self.glyphs[index] = Some(pixels.clone().into_boxed_slice());
            }
            pixels
        };
        self.sequence = self.sequence.wrapping_add(1);
        Ok(pixels)
    }

    fn decode_bands(
        &mut self,
        data: &[u8],
        pixels: &mut [u8],
        width: usize,
        height: usize,
    ) -> Result<()> {
        let mut input = Reader::new(data);
        while !input.is_empty() {
            let left = usize::from(input.u16_le()?);
            let right = usize::from(input.u16_le()?);
            let top = usize::from(input.u16_le()?);
            let bottom = usize::from(input.u16_le()?);
            if left > right
                || top > bottom
                || right >= width
                || bottom >= height
                || bottom - top >= 52
            {
                return Err(Error::Invalid("ClearCodec band rectangle"));
            }
            let background = pixel(&mut input)?;
            let length = bottom - top + 1;
            for x in left..=right {
                let header = input.u16_le()?;
                let index = if header & 0x8000 != 0 {
                    usize::from(header & 0x7fff)
                } else {
                    let (on, short) = if header & 0x4000 != 0 {
                        let index = usize::from(header & 0x3fff);
                        let on = usize::from(input.u8()?);
                        let column = self.short_bars[index]
                            .as_deref()
                            .ok_or(Error::Invalid("ClearCodec short bar cache miss"))?;
                        (on, column)
                    } else {
                        let on = usize::from(header & 0xff);
                        let off = usize::from(header >> 8);
                        if on > off || off > length {
                            return Err(Error::Invalid("ClearCodec short bar range"));
                        }
                        let mut column = Vec::with_capacity(off - on);
                        for _ in on..off {
                            column.push(pixel(&mut input)?);
                        }
                        let index = self.next_short;
                        self.short_bars[index] = Some(column.into_boxed_slice());
                        self.next_short = (index + 1) % self.short_bars.len();
                        (on, self.short_bars[index].as_deref().unwrap())
                    };
                    if on > length || short.len() > length - on {
                        return Err(Error::Invalid("ClearCodec short bar height"));
                    }
                    let mut column = vec![background; length];
                    column[on..on + short.len()].copy_from_slice(short);
                    let index = self.next_bar;
                    self.bars[index] = Some(column.into_boxed_slice());
                    self.next_bar = (index + 1) % self.bars.len();
                    index
                };
                let column = self.bars[index]
                    .as_deref()
                    .filter(|c| c.len() == length)
                    .ok_or(Error::Invalid("ClearCodec bar cache miss or height"))?;
                for (y, color) in column.iter().enumerate() {
                    let offset = ((top + y) * width + x) * 4;
                    pixels[offset..offset + 4].copy_from_slice(color);
                }
            }
        }
        Ok(())
    }
}

fn pixel(input: &mut Reader<'_>) -> Result<Pixel> {
    let bytes = input.take(3)?;
    Ok([bytes[0], bytes[1], bytes[2], 255])
}

fn decode_residual(data: &[u8], pixels: &mut [u8]) -> Result<()> {
    let mut input = Reader::new(data);
    let mut offset = 0;
    while !input.is_empty() {
        let color = pixel(&mut input)?;
        let count = run_length(&mut input)?;
        if count == 0 || count > (pixels.len() - offset) / 4 {
            return Err(Error::Invalid("ClearCodec residual run length"));
        }
        for out in pixels[offset..offset + count * 4].as_chunks_mut::<4>().0 {
            out.copy_from_slice(&color);
        }
        offset += count * 4;
    }
    Ok(())
}

fn decode_subcodecs(data: &[u8], pixels: &mut [u8], width: usize, height: usize) -> Result<()> {
    let mut input = Reader::new(data);
    while !input.is_empty() {
        let x = usize::from(input.u16_le()?);
        let y = usize::from(input.u16_le()?);
        let w = usize::from(input.u16_le()?);
        let h = usize::from(input.u16_le()?);
        let size = input.u32_le()? as usize;
        let codec = input.u8()?;
        if w == 0 || h == 0 || x + w > width || y + h > height || size > w * h * 3 {
            return Err(Error::Invalid("ClearCodec subcodec bounds"));
        }
        let data = input.take(size)?;
        let bitmap = match codec {
            0 => {
                if data.len() != w * h * 3 {
                    return Err(Error::Invalid("ClearCodec raw subcodec size"));
                }
                data.as_chunks::<3>()
                    .0
                    .iter()
                    .flat_map(|p| [p[0], p[1], p[2], 255])
                    .collect()
            }
            1 => crate::graphics::nscodec::decode_top_down(data, w, h)?,
            2 => decode_rlex(data, w * h)?,
            _ => return Err(Error::Invalid("ClearCodec subcodec identifier")),
        };
        for (row, source) in bitmap.chunks_exact(w * 4).enumerate() {
            let offset = ((y + row) * width + x) * 4;
            pixels[offset..offset + source.len()].copy_from_slice(source);
        }
    }
    Ok(())
}

fn decode_rlex(data: &[u8], count: usize) -> Result<Vec<u8>> {
    let mut input = Reader::new(data);
    let colors = input.u8()?;
    if colors == 0 || colors > 127 {
        return Err(Error::Invalid("ClearCodec RLEX palette"));
    }
    let bits = 8 - (colors - 1).leading_zeros();
    let mask = (1u16 << bits) - 1;
    let palette = (0..colors)
        .map(|_| pixel(&mut input))
        .collect::<Result<Vec<_>>>()?;
    let size = count * 4;
    let mut output = Vec::with_capacity(size);
    while !input.is_empty() {
        let header = u16::from(input.u8()?);
        let stop = usize::from(header & mask);
        let depth = usize::from(header >> bits);
        if stop >= palette.len() || depth > stop {
            return Err(Error::Invalid("ClearCodec RLEX suite"));
        }
        let start = stop - depth;
        let run = run_length(&mut input)?;
        let remaining = count - output.len() / 4;
        if run > remaining || depth + 1 > remaining - run {
            return Err(Error::Invalid("ClearCodec RLEX run length"));
        }
        for _ in 0..run {
            output.extend_from_slice(&palette[start]);
        }
        for color in &palette[start..=stop] {
            output.extend_from_slice(color);
        }
    }
    if output.len() != size {
        return Err(Error::Invalid("ClearCodec RLEX output size"));
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn composite(
        flags: u8,
        sequence: u8,
        glyph: u16,
        residual: &[u8],
        bands: &[u8],
        subcodecs: &[u8],
    ) -> Vec<u8> {
        let mut packet = vec![flags, sequence];
        if flags & 1 != 0 {
            packet.extend_from_slice(&glyph.to_le_bytes());
        }
        for layer in [residual, bands, subcodecs] {
            packet.extend_from_slice(&(layer.len() as u32).to_le_bytes());
        }
        for layer in [residual, bands, subcodecs] {
            packet.extend_from_slice(layer);
        }
        packet
    }

    #[test]
    fn microsoft_rlex_example_has_the_documented_pixels_and_size() {
        // MS-RDPEGFX 4.1.1.2, captured packet (sequence 13), not our encoder.
        let wire: Vec<u8> = "
            00 0d 00 00 00 00 00 00 00 00 82 00 00 00 00 00
            00 00 4e 00 11 00 75 00 00 00 02 0e ff ff ff 00
            00 00 db ff ff 00 3a 90 ff b6 66 66 b6 ff b6 66
            00 90 db ff 00 00 3a db 90 3a 3a 90 db 66 00 00
            ff ff b6 64 64 64 11 04 11 4c 11 4c 11 4c 11 4c
            11 4c 00 47 13 00 01 01 04 00 01 00 00 47 16 00
            11 02 00 47 29 00 11 01 00 49 0a 00 01 00 04 00
            01 00 00 4a 0a 00 09 00 01 00 00 47 05 00 01 01
            1c 00 01 00 11 4c 11 4c 11 4c 00 47 0d 4d 00 4d
        "
        .split_whitespace()
        .map(|byte| u8::from_str_radix(byte, 16).unwrap())
        .collect();
        let mut decoder = Decoder::new();
        decoder.sequence = 13;
        let pixels = decoder.decode(&wire, 78, 17).unwrap();
        assert_eq!(pixels.len(), 78 * 17 * 4);
        assert!(pixels[..20].iter().all(|&v| v == 255));
        assert_eq!(&pixels[20..24], &[0, 0, 0, 255]);
        assert!(pixels[24..83 * 4].iter().all(|&v| v == 255));
        assert_eq!(&pixels[83 * 4..84 * 4], &[0, 0, 0, 255]);
        assert!(pixels.as_chunks::<4>().0.iter().all(|p| p[3] == 255));
    }

    #[test]
    fn glyphs_survive_cursor_reset_and_sequence_wrap() {
        let mut decoder = Decoder::new();
        decoder.sequence = 255;
        let first = composite(1, 255, 17, &[9, 8, 7, 2], &[], &[]);
        let expected = vec![9, 8, 7, 255, 9, 8, 7, 255];
        assert_eq!(decoder.decode(&first, 2, 1).unwrap(), expected);
        assert_eq!(decoder.decode(&[7, 0, 17, 0], 2, 1).unwrap(), expected);
        assert!(decoder.decode(&[3, 1, 17, 0], 3, 1).is_err());
        assert!(decoder.decode(&[3, 2, 17, 0], 2, 1).is_err());
    }

    #[test]
    fn bands_reuse_both_caches_across_bitmaps_and_validate_height() {
        let mut decoder = Decoder::new();
        // Three columns: raw short bar, full-bar hit, short-bar hit at y=0.
        let bands = [
            0, 0, 2, 0, 0, 0, 2, 0, 10, 20, 30, 1, 2, 1, 2, 3, 0, 128, 0, 64, 0,
        ];
        let pixels = decoder
            .decode(&composite(0, 0, 0, &[], &bands, &[]), 3, 3)
            .unwrap();
        assert_eq!(
            &pixels[..12],
            &[10, 20, 30, 255, 10, 20, 30, 255, 1, 2, 3, 255]
        );
        assert_eq!(
            &pixels[12..24],
            &[1, 2, 3, 255, 1, 2, 3, 255, 10, 20, 30, 255]
        );
        let next = [0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 1, 128];
        assert_eq!(
            decoder
                .decode(&composite(0, 1, 0, &[], &next, &[]), 1, 3)
                .unwrap(),
            [1, 2, 3, 255, 10, 20, 30, 255, 10, 20, 30, 255]
        );
        let wrong_height = [0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 128];
        assert!(
            decoder
                .decode(&composite(0, 2, 0, &[], &wrong_height, &[]), 1, 2)
                .is_err()
        );
    }

    #[test]
    fn rlex_suites_and_zero_bit_palette_have_exact_output_bounds() {
        assert_eq!(
            decode_rlex(&[1, 9, 8, 7, 0, 2], 3).unwrap(),
            [9, 8, 7, 255].repeat(3)
        );
        // Two colors, stop=1, depth=1 and run=2: indexes 0,0,0,1.
        assert_eq!(
            decode_rlex(&[2, 9, 8, 7, 1, 2, 3, 3, 2], 4).unwrap(),
            [9, 8, 7, 255, 9, 8, 7, 255, 9, 8, 7, 255, 1, 2, 3, 255]
        );
        assert!(decode_rlex(&[1, 9, 8, 7, 0, 255, 0, 1], 2).is_err());
        assert!(decode_rlex(&[2, 9, 8, 7, 1, 2, 3, 2, 0], 1).is_err());
        assert!(decode_rlex(&[0], 1).is_err());
    }

    #[test]
    fn layers_overlay_in_order_and_reject_truncation_and_overflow() {
        let sub = [1, 0, 0, 0, 1, 0, 1, 0, 3, 0, 0, 0, 0, 4, 5, 6];
        let packet = composite(0, 0, 0, &[9, 8, 7, 2], &[], &sub);
        assert_eq!(
            Decoder::new().decode(&packet, 2, 1).unwrap(),
            [9, 8, 7, 255, 4, 5, 6, 255]
        );
        for length in 0..packet.len() {
            assert!(Decoder::new().decode(&packet[..length], 2, 1).is_err());
        }
        assert!(
            Decoder::new()
                .decode(&composite(0, 0, 0, &[9, 8, 7, 3], &[], &[]), 2, 1)
                .is_err()
        );
        assert!(
            Decoder::new()
                .decode(&composite(0, 0, 0, &[9, 8, 7, 0], &[], &[]), 2, 1)
                .is_err()
        );
    }

    #[test]
    fn nscodec_subcodec_and_cached_glyph_keep_top_down_rows() {
        // MS-RDPNSC section 4's asymmetric 15x10 bitmap, carried inside
        // MS-RDPEGFX 2.2.4.1.1.3.1. FreeRDP 3.32.1 clear_decompress returns
        // these exact pixels without the vertical flip used by legacy surfaces.
        let wire = include_bytes!("../graphics/fixtures/nscodec.bin");
        let bitmap = include_bytes!("../graphics/fixtures/nscodec.bgra");
        let mut subcodec = Vec::new();
        for value in [2u16, 1, 15, 10] {
            subcodec.extend_from_slice(&value.to_le_bytes());
        }
        subcodec.extend_from_slice(&(wire.len() as u32).to_le_bytes());
        subcodec.push(1);
        subcodec.extend_from_slice(wire);
        let mut expected = [9, 8, 7, 255].repeat(19 * 13);
        for (y, row) in bitmap.as_chunks::<{ 15 * 4 }>().0.iter().enumerate() {
            let offset = ((y + 1) * 19 + 2) * 4;
            expected[offset..offset + row.len()].copy_from_slice(row);
        }
        let mut decoder = Decoder::new();
        let packet = composite(1, 0, 42, &[9, 8, 7, 247], &[], &subcodec);
        for packet in [packet, vec![3, 1, 42, 0]] {
            let pixels = decoder.decode(&packet, 19, 13).unwrap();
            for (y, (actual, expected)) in pixels
                .as_chunks::<{ 19 * 4 }>()
                .0
                .iter()
                .zip(expected.as_chunks::<{ 19 * 4 }>().0)
                .enumerate()
            {
                assert_eq!(actual, expected, "row {y}");
            }
        }
    }
}
