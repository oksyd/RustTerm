use crate::{Error, Limits, Result, wire::Reader};

use super::{planar, rle};

pub(super) struct Bitmap {
    pub left: usize,
    pub top: usize,
    pub visible_width: usize,
    pub visible_height: usize,
    pub width: usize,
    pub pixels: Vec<u8>,
    pub indices: Option<Vec<u8>>,
}

struct Encoded<'a> {
    left: usize,
    top: usize,
    visible_width: usize,
    visible_height: usize,
    width: usize,
    height: usize,
    depth: u16,
    compressed: bool,
    data: &'a [u8],
}

pub(super) fn update(data: &[u8], limits: Limits, palette: &[[u8; 4]; 256]) -> Result<Vec<Bitmap>> {
    if data.len() > limits.max_pdu {
        return Err(Error::Limit("bitmap update"));
    }
    let mut input = Reader::new(data);
    if input.u16_le()? != 1 {
        return Err(Error::Invalid("bitmap update type"));
    }
    let count = usize::from(input.u16_le()?);
    if count > input.remaining_len() / 18 {
        return Err(Error::Truncated);
    }
    let mut encoded = Vec::with_capacity(count);
    let mut decoded_size = 0usize;
    for _ in 0..count {
        let left = usize::from(input.u16_le()?);
        let top = usize::from(input.u16_le()?);
        let right = usize::from(input.u16_le()?);
        let bottom = usize::from(input.u16_le()?);
        let width = usize::from(input.u16_le()?);
        let height = usize::from(input.u16_le()?);
        let depth = input.u16_le()?;
        let flags = input.u16_le()?;
        let length = usize::from(input.u16_le()?);
        if width == 0
            || height == 0
            || right < left
            || bottom < top
            || right - left + 1 > width
            || bottom - top + 1 > height
        {
            return Err(Error::Invalid("bitmap rectangle"));
        }
        if width > usize::from(limits.max_width) || height > usize::from(limits.max_height) {
            return Err(Error::Limit("bitmap dimensions"));
        }
        if !matches!(depth, 8 | 15 | 16 | 24 | 32) {
            return Err(Error::Unsupported("bitmap color depth"));
        }
        if flags & !0x0401 != 0 {
            return Err(Error::Invalid("bitmap flags"));
        }
        let size = width
            .checked_mul(height)
            .and_then(|n| n.checked_mul(4))
            .ok_or(Error::Limit("bitmap size"))?;
        decoded_size = decoded_size
            .checked_add(size)
            .ok_or(Error::Limit("bitmap update size"))?;
        if decoded_size > limits.max_bitmap_bytes {
            return Err(Error::Limit("bitmap update size"));
        }
        let compressed = flags & 1 != 0;
        let mut payload = Reader::new(input.take(length)?);
        if compressed && flags & 0x0400 == 0 {
            let first_row = payload.u16_le()?;
            let body_size = usize::from(payload.u16_le()?);
            payload.skip(4)?;
            if first_row != 0 || body_size != payload.remaining_len() {
                return Err(Error::Invalid("bitmap compression header"));
            }
        }
        let bitmap = Encoded {
            left,
            top,
            visible_width: right - left + 1,
            visible_height: bottom - top + 1,
            width,
            height,
            depth,
            compressed,
            data: payload.remaining(),
        };
        if !compressed {
            let stride = (width * usize::from(depth.div_ceil(8)) + 3) & !3;
            if bitmap.data.len() != stride * height {
                return Err(Error::Invalid("raw bitmap length"));
            }
        }
        encoded.push(bitmap);
    }
    if !input.is_empty() {
        return Err(Error::Invalid("trailing bitmap update data"));
    }
    encoded
        .into_iter()
        .map(|image| image.decode(palette))
        .collect()
}

impl Encoded<'_> {
    fn decode(self, palette: &[[u8; 4]; 256]) -> Result<Bitmap> {
        let (pixels, indices) = if self.compressed && self.depth == 32 {
            (planar::decode(self.data, self.width, self.height)?, None)
        } else {
            let packed = if self.compressed {
                rle::decode(self.data, self.width, self.height, self.depth)?
            } else {
                let bytes = usize::from(self.depth.div_ceil(8));
                let stride = (self.width * bytes + 3) & !3;
                self.data
                    .chunks_exact(stride)
                    .flat_map(|row| {
                        row[..self.width * bytes].chunks_exact(bytes).map(|p| {
                            p.iter().enumerate().fold(0, |value, (shift, byte)| {
                                value | u32::from(*byte) << (shift * 8)
                            })
                        })
                    })
                    .collect()
            };
            let mut pixels = Vec::with_capacity(self.width * self.height * 4);
            let mut indices = (self.depth == 8).then(|| Vec::with_capacity(packed.len()));
            for row in packed.chunks_exact(self.width).rev() {
                for &value in row {
                    pixels.extend_from_slice(&color(value, self.depth, palette));
                    if let Some(indices) = &mut indices {
                        indices.push(value as u8);
                    }
                }
            }
            (pixels, indices)
        };
        Ok(Bitmap {
            left: self.left,
            top: self.top,
            visible_width: self.visible_width,
            visible_height: self.visible_height,
            width: self.width,
            pixels,
            indices,
        })
    }
}

pub(super) fn color(value: u32, depth: u16, palette: &[[u8; 4]; 256]) -> [u8; 4] {
    let expand5 = |n: u32| ((n << 3) | (n >> 2)) as u8;
    match depth {
        8 => palette[value as u8 as usize],
        15 => [
            expand5(value & 31),
            expand5((value >> 5) & 31),
            expand5((value >> 10) & 31),
            255,
        ],
        16 => {
            let green = (value >> 5) & 63;
            [
                expand5(value & 31),
                ((green << 2) | (green >> 4)) as u8,
                expand5((value >> 11) & 31),
                255,
            ]
        }
        _ => [value as u8, (value >> 8) as u8, (value >> 16) as u8, 255],
    }
}
