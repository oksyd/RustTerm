use crate::{Error, Limits, Result, wire::Reader};

use super::{PointerKind, bitmap};

pub(super) struct Shape {
    pub hotspot_x: u16,
    pub hotspot_y: u16,
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<Pixel>,
    alpha: bool,
}

pub(super) struct Pixel {
    color: [u8; 4],
    and: bool,
}

impl Shape {
    pub fn decode(
        kind: PointerKind,
        data: &[u8],
        limits: Limits,
        palette: &[[u8; 4]; 256],
    ) -> Result<(u16, Self)> {
        if data.len() > limits.max_pdu {
            return Err(Error::Limit("pointer update"));
        }
        let mut input = Reader::new(data);
        let depth = if kind == PointerKind::Color {
            24
        } else {
            input.u16_le()?
        };
        if !matches!(depth, 1 | 4 | 8 | 15 | 16 | 24 | 32) {
            return Err(Error::Unsupported("pointer color depth"));
        }
        let index = input.u16_le()?;
        if usize::from(index) >= limits.max_pointer_cache {
            return Err(Error::Limit("pointer cache index"));
        }
        let hotspot_x = input.u16_le()?;
        let hotspot_y = input.u16_le()?;
        let width = input.u16_le()?;
        let height = input.u16_le()?;
        if width == 0 || height == 0 || hotspot_x >= width || hotspot_y >= height {
            return Err(Error::Invalid("pointer dimensions or hotspot"));
        }
        if width > limits.max_pointer_size || height > limits.max_pointer_size {
            return Err(Error::Limit("pointer dimensions"));
        }
        let width = usize::from(width);
        let height = usize::from(height);
        let and_length = if kind == PointerKind::Large {
            input.u32_le()? as usize
        } else {
            usize::from(input.u16_le()?)
        };
        let xor_length = if kind == PointerKind::Large {
            input.u32_le()? as usize
        } else {
            usize::from(input.u16_le()?)
        };
        let xor_depth = if depth == 15 { 16 } else { usize::from(depth) };
        let xor_stride = (width * xor_depth).div_ceil(16) * 2;
        let and_stride = width.div_ceil(16) * 2;
        let xor_size = xor_stride
            .checked_mul(height)
            .ok_or(Error::Limit("pointer mask size"))?;
        let and_size = and_stride
            .checked_mul(height)
            .ok_or(Error::Limit("pointer mask size"))?;
        if xor_length != xor_size || (and_length != and_size && !(and_length == 0 && depth == 32)) {
            return Err(Error::Invalid("pointer mask length"));
        }
        let pixel_count = width
            .checked_mul(height)
            .filter(|count| {
                count
                    .checked_mul(size_of::<Pixel>())
                    .is_some_and(|bytes| bytes <= limits.max_bitmap_bytes)
            })
            .ok_or(Error::Limit("pointer allocation"))?;
        let xor = input.take(xor_length)?;
        let and = input.take(and_length)?;
        if input.remaining_len() == 1 {
            input.skip(1)?;
        }
        if !input.is_empty() {
            return Err(Error::Invalid("trailing pointer data"));
        }
        let mut pixels = Vec::with_capacity(pixel_count);
        for y in 0..height {
            // Monochrome pointers use the T.128 top-down representation.
            let row = if depth == 1 { y } else { height - y - 1 };
            let xor_row = &xor[row * xor_stride..(row + 1) * xor_stride];
            for x in 0..width {
                let color = match depth {
                    1 => {
                        let value = if bit(xor_row, x) { 255 } else { 0 };
                        [value, value, value, 255]
                    }
                    4 => {
                        let shift = if x.is_multiple_of(2) { 4 } else { 0 };
                        palette[usize::from((xor_row[x / 2] >> shift) & 15)]
                    }
                    _ => {
                        let bytes = xor_depth / 8;
                        let value = xor_row[x * bytes..(x + 1) * bytes]
                            .iter()
                            .enumerate()
                            .fold(0, |n, (shift, byte)| n | u32::from(*byte) << (shift * 8));
                        let mut color = bitmap::color(value, depth, palette);
                        if depth == 32 {
                            color[3] = (value >> 24) as u8;
                        }
                        color
                    }
                };
                let and = !and.is_empty() && bit(&and[row * and_stride..(row + 1) * and_stride], x);
                pixels.push(Pixel { color, and });
            }
        }
        Ok((
            index,
            Self {
                hotspot_x,
                hotspot_y,
                width,
                height,
                pixels,
                alpha: depth == 32,
            },
        ))
    }

    pub fn bytes(&self) -> usize {
        self.pixels.len() * size_of::<Pixel>()
    }

    pub fn composite(&self, index: usize, destination: &mut [u8]) {
        let pixel = &self.pixels[index];
        if !self.alpha || pixel.and && pixel.color[3] == 255 {
            for (dest, &source) in destination[..3].iter_mut().zip(&pixel.color[..3]) {
                *dest = (if pixel.and { *dest } else { 0 }) ^ source;
            }
        } else {
            let alpha = u32::from(pixel.color[3]);
            for (dest, &source) in destination[..3].iter_mut().zip(&pixel.color[..3]) {
                *dest = ((u32::from(source) * alpha + u32::from(*dest) * (255 - alpha) + 127) / 255)
                    as u8;
            }
        }
        destination[3] = 255;
    }
}

fn bit(mask: &[u8], index: usize) -> bool {
    mask[index / 8] & (0x80 >> (index % 8)) != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_pointer_headers_are_rejected_before_mask_allocation() {
        let limits = Limits {
            max_pointer_size: u16::MAX,
            ..Limits::default()
        };
        let mut data = Vec::new();
        for field in [1u16, 0, 0, 0, u16::MAX, u16::MAX] {
            data.extend_from_slice(&field.to_le_bytes());
        }
        // A monochrome mask of this size fits u32; its decoded pixels do not
        // fit the configured budget, or even usize on a 32-bit target.
        let mask_length = 8192u32 * u32::from(u16::MAX);
        data.extend_from_slice(&mask_length.to_le_bytes());
        data.extend_from_slice(&mask_length.to_le_bytes());
        assert!(matches!(
            Shape::decode(PointerKind::Large, &data, limits, &[[0; 4]; 256]),
            Err(Error::Limit("pointer allocation"))
        ));
    }
}
