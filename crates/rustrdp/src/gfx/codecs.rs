use crate::{Error, Limits, Result};

use super::{Rect, Surface, avc::Avc};

#[derive(Default)]
pub(super) struct Codecs {
    avc: Option<Avc>,
}

impl Codecs {
    pub(super) fn take_avc_audit(&mut self) -> (u64, u64) {
        self.avc.as_mut().map_or((0, 0), Avc::take_audit)
    }
}

pub(super) fn decode(
    surface: &mut Surface,
    codec: u16,
    rect: Rect,
    data: &[u8],
    limits: Limits,
    clear: &mut super::clear::Decoder,
    rfx: &mut super::rfx::Decoder,
) -> Result<()> {
    let size = rect.width() * rect.height() * 4;
    if size == 0 {
        return Err(Error::Invalid("empty graphics bitmap"));
    }
    if size > limits.max_bitmap_bytes || data.len() > limits.max_channel_message {
        return Err(Error::Limit("graphics bitmap"));
    }
    match codec {
        0 => {
            if data.len() != size {
                return Err(Error::Invalid("uncompressed graphics bitmap length"));
            }
            surface.put(rect, data, rect.width());
        }
        3 => {
            rfx.decode(data, |x, y, pixels, regions| {
                let left = usize::from(rect.left) + usize::from(x) * 64;
                let top = usize::from(rect.top) + usize::from(y) * 64;
                if left >= usize::from(rect.right) || top >= usize::from(rect.bottom) {
                    return Ok(());
                }
                let tile = Rect {
                    left: left as u16,
                    top: top as u16,
                    right: (left + 64).min(usize::from(rect.right)) as u16,
                    bottom: (top + 64).min(usize::from(rect.bottom)) as u16,
                };
                if regions.is_empty() {
                    put_bgra_tile(surface, tile, left, top, pixels)?;
                }
                for region in regions {
                    let region = Rect {
                        left: rect.left.saturating_add(region.left),
                        top: rect.top.saturating_add(region.top),
                        right: rect.left.saturating_add(region.right),
                        bottom: rect.top.saturating_add(region.bottom),
                    };
                    if let Some(part) = tile.intersect(region) {
                        put_bgra_tile(surface, part, left, top, pixels)?;
                    }
                }
                Ok(())
            })?;
        }
        8 => {
            let pixels = clear.decode(data, rect.width() as u16, rect.height() as u16)?;
            if pixels.len() != size {
                return Err(Error::Invalid("ClearCodec bitmap dimensions"));
            }
            surface.put(rect, &pixels, rect.width());
        }
        10 => {
            let pixels =
                crate::graphics::planar::decode_top_down(data, rect.width(), rect.height())?;
            surface.put(rect, &pixels, rect.width());
        }
        11 | 14 | 15 => {
            if surface.codecs.avc.is_none() {
                surface.codecs.avc = Some(Avc::new(surface.width, surface.height, limits)?);
            }
            let decoder = surface
                .codecs
                .avc
                .as_mut()
                .ok_or(Error::Invalid("graphics AVC decoder"))?;
            let regions = decoder.decode(
                codec,
                data,
                &mut surface.pixels,
                surface.width,
                surface.height,
            )?;
            for region in regions {
                surface.dirty(region);
            }
        }
        12 => {
            let alpha = super::alpha::decode(data, rect.width() as u16, rect.height() as u16)?;
            let width = rect.width();
            let stride = usize::from(surface.width);
            for (index, value) in alpha.into_iter().enumerate() {
                let destination = ((usize::from(rect.top) + index / width) * stride
                    + usize::from(rect.left)
                    + index % width)
                    * 4
                    + 3;
                surface.pixels[destination] = value;
            }
            surface.dirty(rect);
        }
        _ => return Err(Error::Unsupported("graphics bitmap codec")),
    }
    Ok(())
}

pub(super) fn put_bgra_tile(
    surface: &mut Surface,
    rect: Rect,
    left: usize,
    top: usize,
    pixels: &[u8],
) -> Result<()> {
    blit_bgra(
        &mut surface.pixels,
        usize::from(surface.width),
        rect,
        left,
        top,
        pixels,
    )?;
    surface.dirty(rect);
    Ok(())
}

fn blit_bgra(
    target: &mut [u8],
    stride: usize,
    rect: Rect,
    left: usize,
    top: usize,
    pixels: &[u8],
) -> Result<()> {
    if usize::from(rect.left) < left
        || usize::from(rect.top) < top
        || usize::from(rect.right) > left + 64
        || usize::from(rect.bottom) > top + 64
        || pixels.len() != 64 * 64 * 4
    {
        return Err(Error::Invalid("graphics tile bounds"));
    }
    let width = rect.width() * 4;
    for y in rect.top..rect.bottom {
        let source = ((usize::from(y) - top) * 64 + usize::from(rect.left) - left) * 4;
        let destination = (usize::from(y) * stride + usize::from(rect.left)) * 4;
        target[destination..destination + width].copy_from_slice(&pixels[source..source + width]);
    }
    Ok(())
}
