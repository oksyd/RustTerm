//! RemoteFX image streams (MS-RDPRFX), independent of the graphics transport.

use super::{Rect, wavelet};
use crate::{Error, Result, wire::Reader};
#[cfg(test)]
mod tests;

#[derive(Default)]
pub(super) struct Decoder {
    headers: u8,
    pair_mode: bool,
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn decode(
        &mut self,
        data: &[u8],
        mut tile: impl FnMut(u16, u16, &[u8], &[Rect]) -> Result<()>,
    ) -> Result<()> {
        let mut stream = Reader::new(data);
        let mut in_frame = false;
        let mut region = None;
        let mut tile_sets = 0u16;
        let mut regions_remaining = 0u16;
        let mut work = 0usize;
        while !stream.is_empty() {
            let (kind, mut body) = wavelet::block(&mut stream)?;
            if matches!(kind, 0xccc3..=0xccc7)
                && (body.u8()? != 1 || body.u8()? != if kind == 0xccc3 { 255 } else { 0 })
            {
                return Err(Error::Invalid("RemoteFX channel header"));
            }
            match kind {
                0xccc0 => {
                    if body.u32_le()? != 0xcacc_acca || body.u16_le()? != 0x100 {
                        return Err(Error::Invalid("RemoteFX synchronization"));
                    }
                    self.headers |= 1;
                }
                0xccc1 => {
                    if body.u8()? != 1 || body.u8()? != 1 || body.u16_le()? != 0x100 {
                        return Err(Error::Invalid("RemoteFX version"));
                    }
                    self.headers |= 2;
                }
                0xccc2 => {
                    let count = body.u8()?;
                    if count == 0 {
                        return Err(Error::Invalid("RemoteFX channels"));
                    }
                    for _ in 0..count {
                        body.u8()?;
                        if body.u16_le()? == 0 || body.u16_le()? == 0 {
                            return Err(Error::Invalid("RemoteFX dimensions"));
                        }
                    }
                    self.headers |= 4;
                }
                0xccc3 => {
                    body.u8()?;
                    if body.u16_le()? != 64 {
                        return Err(Error::Unsupported("RemoteFX tile size"));
                    }
                    let properties = body.u16_le()?;
                    if (properties >> 3) & 3 != 1
                        || (properties >> 5) & 15 != 1
                        || (properties >> 13) & 3 != 1
                    {
                        return Err(Error::Unsupported("RemoteFX transform"));
                    }
                    self.pair_mode = match (properties >> 9) & 15 {
                        1 => false,
                        4 => true,
                        _ => return Err(Error::Unsupported("RemoteFX entropy codec")),
                    };
                    self.headers |= 8;
                }
                0xccc4 => {
                    if in_frame || self.headers != 15 {
                        return Err(Error::Invalid("RemoteFX frame order"));
                    }
                    body.u32_le()?;
                    regions_remaining = body.u16_le()?;
                    in_frame = true;
                }
                0xccc6 => {
                    if !in_frame || tile_sets != 0 || regions_remaining == 0 {
                        return Err(Error::Invalid("RemoteFX region order"));
                    }
                    regions_remaining -= 1;
                    body.u8()?;
                    let count = usize::from(body.u16_le()?);
                    if count > body.remaining_len() / 8 {
                        return Err(Error::Truncated);
                    }
                    let mut rectangles = Vec::with_capacity(count);
                    for _ in 0..count {
                        rectangles.push(rectangle(&mut body)?);
                    }
                    if body.u16_le()? != 0xcac1 {
                        return Err(Error::Invalid("RemoteFX region type"));
                    }
                    tile_sets = body.u16_le()?;
                    if tile_sets != 1 {
                        return Err(Error::Invalid("RemoteFX tileset count"));
                    }
                    region = Some(rectangles);
                }
                0xccc7 => {
                    if tile_sets == 0 {
                        return Err(Error::Invalid("RemoteFX tileset order"));
                    }
                    tile_sets -= 1;
                    if body.u16_le()? != 0xcac2 {
                        return Err(Error::Invalid("RemoteFX tileset type"));
                    }
                    body.u16_le()?;
                    let properties = body.u16_le()?;
                    let entropy = if self.pair_mode { 4 } else { 1 };
                    if (properties >> 10) & 15 != entropy
                        || (properties >> 4) & 3 != 1
                        || (properties >> 6) & 15 != 1
                        || (properties >> 14) & 3 != 1
                    {
                        return Err(Error::Unsupported("RemoteFX tileset transform"));
                    }
                    let count = body.u8()?;
                    if body.u8()? != 64 {
                        return Err(Error::Unsupported("RemoteFX tile size"));
                    }
                    let tiles = body.u16_le()?;
                    let length = body.u32_le()? as usize;
                    let mut quants = Vec::with_capacity(usize::from(count));
                    for _ in 0..count {
                        quants.push(wavelet::quant(&mut body)?);
                    }
                    let mut encoded = Reader::new(body.take(length)?);
                    let rectangles = region.as_deref().ok_or(Error::Invalid("RemoteFX region"))?;
                    work = work.saturating_add(usize::from(tiles) * rectangles.len().max(1));
                    if work > 1 << 20 {
                        return Err(Error::Limit("RemoteFX region visits"));
                    }
                    for _ in 0..tiles {
                        let (kind, mut tile_data) = wavelet::block(&mut encoded)?;
                        if kind != 0xcac3 {
                            return Err(Error::Invalid("RemoteFX tile type"));
                        }
                        let tables = [tile_data.u8()?, tile_data.u8()?, tile_data.u8()?];
                        let x = tile_data.u16_le()?;
                        let y = tile_data.u16_le()?;
                        let lengths = [
                            tile_data.u16_le()?,
                            tile_data.u16_le()?,
                            tile_data.u16_le()?,
                        ];
                        let mut components: [Vec<i32>; 3] = std::array::from_fn(|_| Vec::new());
                        for component in 0..3 {
                            let quant = *quants
                                .get(usize::from(tables[component]))
                                .ok_or(Error::Invalid("RemoteFX quantization index"))?;
                            let coefficients = &mut components[component];
                            *coefficients = wavelet::rlgr(
                                tile_data.take(usize::from(lengths[component]))?,
                                self.pair_mode,
                            )?;
                            wavelet::differential(coefficients, false)?;
                            wavelet::dequantize(coefficients, quant, false)?;
                            wavelet::inverse(coefficients, false);
                        }
                        tile_data.finish()?;
                        tile(x, y, &wavelet::colors(&components), rectangles)?;
                    }
                    encoded.finish()?;
                }
                0xccc5 => {
                    if !in_frame || tile_sets != 0 || regions_remaining != 0 {
                        return Err(Error::Invalid("RemoteFX frame end"));
                    }
                    in_frame = false;
                    region = None;
                }
                _ => return Err(Error::Unsupported("RemoteFX block type")),
            }
            body.finish()?;
        }
        if in_frame {
            return Err(Error::Truncated);
        }
        Ok(())
    }
}

pub(super) fn rectangle(reader: &mut Reader<'_>) -> Result<Rect> {
    let left = reader.u16_le()?;
    let top = reader.u16_le()?;
    let right = left
        .checked_add(reader.u16_le()?)
        .ok_or(Error::Invalid("RemoteFX rectangle"))?;
    let bottom = top
        .checked_add(reader.u16_le()?)
        .ok_or(Error::Invalid("RemoteFX rectangle"))?;
    Ok(Rect {
        left,
        top,
        right,
        bottom,
    })
}
