//! Stateful RemoteFX Progressive decoding, MS-RDPEGFX 2.2.4.2 and 3.3.8.2.

use std::collections::{BTreeMap, BTreeSet, btree_map::Entry};

use super::{
    Rect,
    wavelet::{self, Bits, QUANT_ORDER, Quant},
};
use crate::{Error, Result, wire::Reader};

const TILE_BYTES: usize = 4096 * (3 * 4 + 4);
const PASS_BYTES: usize = 4096 * 3;
// Account for each persistent tile-to-codec association and its map entry.
const TILE_CONTEXT_BYTES: usize = 128;
const MAX_MASKS: usize = 65_536;
type Position = (u16, u16);

pub(super) struct Tile {
    pub x_idx: u16,
    pub y_idx: u16,
    pub pixels: Vec<u8>,
    pub update_rectangles: Vec<Rect>,
}

struct Pass {
    signs: [Vec<i8>; 3],
    positions: [Quant; 3],
    contexts: BTreeSet<u32>,
}

struct StoredTile {
    // Q6 retains half-Q5 values when the base quantization factor is zero.
    // Reference coefficients must survive successive difference/upgrade passes.
    current: [Vec<i32>; 3],
    pixels: Vec<u8>,
    pass: Option<Pass>,
    extrapolate: bool,
    frame: u64,
}

impl StoredTile {
    fn render(&mut self) {
        let mut components = self.current.clone();
        for values in &mut components {
            for value in values.iter_mut() {
                *value >>= 1;
            }
            wavelet::inverse(values, self.extrapolate);
        }
        self.pixels = wavelet::colors(&components);
    }

    fn bytes(&self) -> usize {
        TILE_BYTES
            + self
                .pass
                .as_ref()
                .map_or(0, |pass| PASS_BYTES + pass.contexts.len() * 64)
    }
}

struct Surface {
    width: u16,
    height: u16,
    tiles: BTreeMap<Position, StoredTile>,
    contexts: BTreeSet<u32>,
    masks: BTreeMap<Position, Vec<Rect>>,
    // Regions establish tile associations for the lifetime of a codec context.
    // A tile may be supplied by another context in a later graphics frame;
    // clipping masks and frame boundaries must not erase those associations.
    tile_contexts: BTreeMap<Position, BTreeSet<u32>>,
}

impl Surface {
    fn tile_context_bytes(&self) -> usize {
        self.tile_contexts
            .values()
            .map(BTreeSet::len)
            .sum::<usize>()
            * TILE_CONTEXT_BYTES
    }
}

pub(super) struct Decoder {
    surfaces: BTreeMap<u16, Surface>,
    bytes: usize,
    max_bytes: usize,
    masks: usize,
    frame: u64,
    in_frame: bool,
}

impl Decoder {
    pub(super) fn new(max_bitmap_bytes: usize) -> Self {
        Self {
            surfaces: BTreeMap::new(),
            bytes: 0,
            // References, pixels and refinement signs need 19 bytes per pixel.
            // Five times the BGRA budget includes tile metadata; one additional
            // tile allows partial edge tiles even for a very small surface.
            max_bytes: max_bitmap_bytes
                .saturating_mul(5)
                .saturating_add(TILE_BYTES + PASS_BYTES + 256),
            masks: 0,
            frame: 1,
            in_frame: false,
        }
    }

    pub(super) fn begin_frame(&mut self) {
        self.clear_masks();
        self.frame = self.frame.wrapping_add(1);
        self.in_frame = true;
    }

    pub(super) fn end_frame(&mut self) {
        self.clear_masks();
        self.in_frame = false;
    }

    fn clear_masks(&mut self) {
        for surface in self.surfaces.values_mut() {
            surface.masks.clear();
        }
        self.masks = 0;
    }

    pub(super) fn delete_surface(&mut self, id: u16) {
        if let Some(surface) = self.surfaces.remove(&id) {
            self.bytes -= surface.tiles.values().map(StoredTile::bytes).sum::<usize>()
                + surface.contexts.len() * 64
                + surface.tile_context_bytes();
            self.masks -= surface.masks.values().map(Vec::len).sum::<usize>();
        }
    }

    pub(super) fn delete_context(&mut self, id: u16, context: u32) {
        if let Some(surface) = self.surfaces.get_mut(&id) {
            if surface.contexts.remove(&context) {
                self.bytes -= 64;
            }
            surface.tile_contexts.retain(|_, contexts| {
                if contexts.remove(&context) {
                    self.bytes -= TILE_CONTEXT_BYTES;
                }
                !contexts.is_empty()
            });
            for tile in surface.tiles.values_mut() {
                if let Some(pass) = &mut tile.pass {
                    if pass.contexts.remove(&context) {
                        self.bytes -= 64;
                    }
                    if pass.contexts.is_empty() {
                        tile.pass = None;
                        self.bytes -= PASS_BYTES;
                    }
                }
            }
        }
    }

    pub(super) fn decode_bitmap(
        &mut self,
        id: u16,
        context: u32,
        width: u16,
        height: u16,
        data: &[u8],
    ) -> Result<Vec<Tile>> {
        // A region mask applies to this bitmap only. Tile references survive
        // across frames, but replaying an earlier bitmap's mask here
        // would overwrite intervening updates made by other codecs.
        self.clear_masks();
        if !self.in_frame {
            self.frame = self.frame.wrapping_add(1);
        }
        if width == 0 || height == 0 {
            return Err(Error::Invalid("progressive surface dimensions"));
        }
        if !self.surfaces.contains_key(&id) && self.surfaces.len() >= 64 {
            return Err(Error::Limit("progressive surfaces"));
        }
        let mut surface = self.surfaces.remove(&id).unwrap_or_else(|| Surface {
            width,
            height,
            tiles: BTreeMap::new(),
            contexts: BTreeSet::new(),
            masks: BTreeMap::new(),
            tile_contexts: BTreeMap::new(),
        });
        let known_context = surface.contexts.contains(&context);
        let result = self.decode_surface(&mut surface, context, width, height, data);
        self.surfaces.insert(id, surface);
        if result.is_err() && !known_context {
            // A rejected first upgrade must not make that ID valid on retry.
            self.delete_context(id, context);
        }
        result
    }

    fn associate_tile(
        &mut self,
        surface: &mut Surface,
        position: Position,
        context: u32,
    ) -> Result<()> {
        if !surface
            .tile_contexts
            .get(&position)
            .is_some_and(|ids| ids.contains(&context))
        {
            self.reserve(TILE_CONTEXT_BYTES)?;
            surface
                .tile_contexts
                .entry(position)
                .or_default()
                .insert(context);
        }
        self.link_tile_contexts(surface, position)
    }

    fn link_tile_contexts(&mut self, surface: &mut Surface, position: Position) -> Result<()> {
        if let Some(pass) = surface
            .tiles
            .get_mut(&position)
            .and_then(|tile| tile.pass.as_mut())
            && let Some(contexts) = surface.tile_contexts.get(&position)
        {
            let missing = contexts.difference(&pass.contexts).count();
            self.reserve(missing * 64)?;
            pass.contexts.extend(contexts);
        }
        Ok(())
    }

    fn reserve(&mut self, bytes: usize) -> Result<()> {
        if bytes > self.max_bytes - self.bytes {
            return Err(Error::Limit("progressive tile state"));
        }
        self.bytes += bytes;
        Ok(())
    }

    fn decode_surface(
        &mut self,
        surface: &mut Surface,
        context: u32,
        width: u16,
        height: u16,
        data: &[u8],
    ) -> Result<Vec<Tile>> {
        if (surface.width, surface.height) != (width, height) {
            return Err(Error::Invalid("progressive surface dimensions changed"));
        }
        let mut known_context = surface.contexts.contains(&context);
        if !known_context {
            if surface.contexts.len() >= 4096 {
                return Err(Error::Limit("progressive codec contexts"));
            }
            self.reserve(64)?;
            surface.contexts.insert(context);
        }
        let mut input = Reader::new(data);
        let mut inside = false;
        let mut changed = BTreeSet::new();
        while !input.is_empty() {
            let (kind, mut block) = wavelet::block(&mut input)?;
            match kind {
                // MS-RDPEGFX explicitly says to ignore sync magic/version and
                // repeated end blocks, while validating their fixed lengths.
                0xccc0 => {
                    block.skip(6)?;
                }
                0xccc1 => {
                    if inside {
                        return Err(Error::Invalid("overlapping progressive frames"));
                    }
                    block.skip(6)?;
                    inside = true;
                }
                0xccc2 => {
                    inside = false;
                }
                0xccc3 => {
                    block.skip(1)?;
                    if block.u16_le()? != 64 {
                        return Err(Error::Invalid("progressive tile size"));
                    }
                    block.skip(1)?;
                }
                0xccc4 if inside => self.region(
                    surface,
                    context,
                    &mut known_context,
                    &mut block,
                    &mut changed,
                )?,
                _ => {
                    block.skip(block.remaining_len())?;
                }
            }
            block.finish()?;
        }
        let mut output = Vec::with_capacity(changed.len());
        for position in changed {
            if let (Some(tile), Some(rectangles)) =
                (surface.tiles.get(&position), surface.masks.get(&position))
            {
                output.push(Tile {
                    x_idx: position.0,
                    y_idx: position.1,
                    pixels: tile.pixels.clone(),
                    update_rectangles: rectangles.clone(),
                });
            }
        }
        Ok(output)
    }

    fn region(
        &mut self,
        surface: &mut Surface,
        context: u32,
        known_context: &mut bool,
        input: &mut Reader<'_>,
        changed: &mut BTreeSet<Position>,
    ) -> Result<()> {
        if input.u8()? != 64 {
            return Err(Error::Invalid("progressive region tile size"));
        }
        let count = input.u16_le()?;
        let quant_count = input.u8()?;
        let progressive_count = input.u8()?;
        let extrapolate = input.u8()? & 1 != 0;
        let tiles = input.u16_le()?;
        let tile_bytes = input.u32_le()? as usize;
        if count == 0 || quant_count > 7 {
            return Err(Error::Invalid("progressive region tables"));
        }
        for _ in 0..count {
            let left = input.u16_le()?;
            let top = input.u16_le()?;
            let right = left
                .checked_add(input.u16_le()?)
                .ok_or(Error::Invalid("progressive rectangle"))?;
            let bottom = top
                .checked_add(input.u16_le()?)
                .ok_or(Error::Invalid("progressive rectangle"))?;
            let rect = Rect {
                left,
                top,
                right,
                bottom,
            }
            .checked(surface.width, surface.height)?;
            if rect.width() == 0 || rect.height() == 0 {
                return Err(Error::Invalid("empty progressive rectangle"));
            }
            for y in top / 64..=(bottom - 1) / 64 {
                for x in left / 64..=(right - 1) / 64 {
                    let position = (x, y);
                    let tile_rect = Rect {
                        left: x * 64,
                        top: y * 64,
                        right: (u32::from(x) * 64 + 64).min(u32::from(surface.width)) as u16,
                        bottom: (u32::from(y) * 64 + 64).min(u32::from(surface.height)) as u16,
                    };
                    let mask = rect
                        .intersect(tile_rect)
                        .ok_or(Error::Invalid("progressive tile mask"))?;
                    let masks = surface.masks.entry(position).or_default();
                    if masks
                        .iter()
                        .any(|previous| previous.intersect(mask).is_some())
                    {
                        return Err(Error::Invalid("overlapping progressive regions"));
                    }
                    if self.masks == MAX_MASKS {
                        return Err(Error::Limit("progressive frame regions"));
                    }
                    masks.push(mask);
                    self.masks += 1;
                    self.associate_tile(surface, position, context)?;
                    if surface
                        .tiles
                        .get(&position)
                        .is_some_and(|tile| tile.frame == self.frame)
                    {
                        changed.insert(position);
                    }
                }
            }
        }
        let tables = (0..quant_count)
            .map(|_| quant(input))
            .collect::<Result<Vec<_>>>()?;
        let mut progressive = Vec::with_capacity(usize::from(progressive_count));
        for _ in 0..progressive_count {
            input.skip(1)?; // The quality percentage is advisory.
            let values = [quant(input)?, quant(input)?, quant(input)?];
            if values.iter().flatten().any(|value| *value > 8) {
                return Err(Error::Invalid("progressive quantization table"));
            }
            progressive.push(values);
        }
        let mut payload = Reader::new(input.take(tile_bytes)?);
        for _ in 0..tiles {
            let (kind, mut block) = wavelet::block(&mut payload)?;
            if !matches!(kind, 0xccc5..=0xccc7) {
                return Err(Error::Invalid("progressive tile block"));
            }
            let indices = [block.u8()?, block.u8()?, block.u8()?];
            let base = [
                table(&tables, indices[0])?,
                table(&tables, indices[1])?,
                table(&tables, indices[2])?,
            ];
            let position = (block.u16_le()?, block.u16_le()?);
            if u32::from(position.0) * 64 >= u32::from(surface.width)
                || u32::from(position.1) * 64 >= u32::from(surface.height)
            {
                return Err(Error::Invalid("progressive tile coordinates"));
            }
            let difference = kind != 0xccc7 && block.u8()? & 1 != 0;
            let quality = if kind == 0xccc5 { 255 } else { block.u8()? };
            let extra = if quality == 255 {
                [[0; 10]; 3]
            } else {
                *progressive
                    .get(usize::from(quality))
                    .ok_or(Error::Invalid("progressive quality index"))?
            };
            let mut positions = base;
            for component in 0..3 {
                for band in 0..10 {
                    positions[component][band] += extra[component][band];
                }
            }
            if kind == 0xccc7 {
                // MS-RDPEGFX 3.3.5.2 requires a known codec context, not that
                // this context was the one that originally encoded this tile.
                if !*known_context {
                    return Err(Error::Invalid("progressive upgrade without codec context"));
                }
                let tile = surface
                    .tiles
                    .get_mut(&position)
                    .ok_or(Error::Invalid("progressive upgrade without tile"))?;
                if tile.extrapolate != extrapolate {
                    return Err(Error::Invalid("progressive transform changed"));
                }
                let pass = tile
                    .pass
                    .as_ref()
                    .ok_or(Error::Invalid("progressive upgrade without tile state"))?;
                for (old, new) in pass
                    .positions
                    .iter()
                    .flatten()
                    .zip(positions.iter().flatten())
                {
                    if new > old || old - new > 15 {
                        return Err(Error::Invalid("progressive bit position"));
                    }
                }
                upgrade(tile, &mut block, positions)?;
            } else {
                let tile = match surface.tiles.entry(position) {
                    Entry::Occupied(entry) => entry.into_mut(),
                    Entry::Vacant(entry) => {
                        if difference {
                            return Err(Error::Invalid("progressive difference without reference"));
                        }
                        self.reserve(TILE_BYTES)?;
                        entry.insert(StoredTile {
                            current: std::array::from_fn(|_| vec![0; 4096]),
                            pixels: Vec::new(),
                            pass: None,
                            extrapolate,
                            frame: self.frame,
                        })
                    }
                };
                if difference && tile.extrapolate != extrapolate {
                    return Err(Error::Invalid("progressive difference transform"));
                }
                if tile.pass.is_none() {
                    self.reserve(PASS_BYTES)?;
                    tile.pass = Some(Pass {
                        signs: std::array::from_fn(|_| vec![0; 4096]),
                        positions,
                        contexts: BTreeSet::new(),
                    });
                }
                first(tile, &mut block, positions, difference, extrapolate)?;
                *known_context = true;
            }
            block.finish()?;
            self.associate_tile(surface, position, context)?;
            let tile = surface.tiles.get_mut(&position).unwrap();
            tile.frame = self.frame;
            tile.render();
            changed.insert(position);
        }
        payload.finish()?;
        Ok(())
    }
}

fn quant(input: &mut Reader<'_>) -> Result<Quant> {
    let mut values = wavelet::quant(input)?;
    for (a, b) in [(1, 2), (4, 5), (7, 8)] {
        values.swap(a, b);
    }
    Ok(values)
}

fn table(values: &[Quant], index: u8) -> Result<Quant> {
    values
        .get(usize::from(index))
        .copied()
        .ok_or(Error::Invalid("progressive quantization index"))
}

fn scaled(value: i32, position: u8) -> Result<i32> {
    wavelet::scale(value, position)
}

fn first(
    tile: &mut StoredTile,
    input: &mut Reader<'_>,
    positions: [Quant; 3],
    difference: bool,
    extrapolate: bool,
) -> Result<()> {
    let lengths = [input.u16_le()?, input.u16_le()?, input.u16_le()?];
    let tail = input.u16_le()?;
    let pass = tile.pass.as_mut().unwrap();
    for (component, length) in lengths.into_iter().enumerate() {
        let mut values = wavelet::rlgr(input.take(usize::from(length))?, false)?;
        for (sign, value) in pass.signs[component].iter_mut().zip(&values) {
            *sign = value.signum() as i8;
        }
        wavelet::differential(&mut values, extrapolate)?;
        for (band, (start, length)) in wavelet::bands(extrapolate).into_iter().enumerate() {
            let position = positions[component][QUANT_ORDER[band]];
            for (current, value) in tile.current[component][start..start + length]
                .iter_mut()
                .zip(&values[start..start + length])
            {
                let decoded = scaled(*value, position)?;
                *current = if difference {
                    current
                        .checked_add(decoded)
                        .ok_or(Error::Invalid("progressive coefficient overflow"))?
                } else {
                    decoded
                };
            }
        }
    }
    input.skip(usize::from(tail))?;
    pass.positions = positions;
    tile.extrapolate = extrapolate;
    Ok(())
}

fn upgrade(tile: &mut StoredTile, input: &mut Reader<'_>, positions: [Quant; 3]) -> Result<()> {
    let lengths = [
        (input.u16_le()?, input.u16_le()?),
        (input.u16_le()?, input.u16_le()?),
        (input.u16_le()?, input.u16_le()?),
    ];
    let pass = tile.pass.as_mut().unwrap();
    for (component, (srl_length, raw_length)) in lengths.into_iter().enumerate() {
        let mut srl = Srl::new(input.take(usize::from(srl_length))?);
        let mut raw = Bits::new(input.take(usize::from(raw_length))?);
        for (band, (start, length)) in wavelet::bands(tile.extrapolate).into_iter().enumerate() {
            let index = QUANT_ORDER[band];
            let next = positions[component][index];
            let count = u32::from(pass.positions[component][index] - next);
            if count == 0 {
                continue;
            }
            for offset in start..start + length {
                let sign = &mut pass.signs[component][offset];
                let value = if band == 9 {
                    raw.read(count)? as i32
                } else if *sign != 0 {
                    (raw.read(count)? as i32) * i32::from(*sign)
                } else {
                    let value = srl.next(count)?;
                    *sign = value.signum() as i8;
                    value
                };
                tile.current[component][offset] = tile.current[component][offset]
                    .checked_add(scaled(value, next)?)
                    .ok_or(Error::Invalid("progressive upgrade overflow"))?;
            }
        }
        if raw.remaining() > 7 || srl.bits.remaining() > 15 {
            return Err(Error::Invalid("progressive trailing entropy data"));
        }
    }
    pass.positions = positions;
    Ok(())
}

struct Srl<'a> {
    bits: Bits<'a>,
    parameter: u32,
    zeros: usize,
    unary: bool,
}

impl<'a> Srl<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            bits: Bits::new(data),
            parameter: 8,
            zeros: 0,
            unary: false,
        }
    }

    fn next(&mut self, count: u32) -> Result<i32> {
        if self.zeros != 0 {
            self.zeros -= 1;
            return Ok(0);
        }
        if !self.unary {
            let k = self.parameter / 8;
            if self.bits.read(1)? == 0 {
                self.zeros = (1 << k) - 1;
                self.parameter = (self.parameter + 4).min(80);
                return Ok(0);
            }
            self.zeros = self.bits.read(k)? as usize;
            self.unary = true;
            if self.zeros != 0 {
                self.zeros -= 1;
                return Ok(0);
            }
        }
        self.unary = false;
        self.parameter = self.parameter.saturating_sub(6);
        let negative = self.bits.read(1)? != 0;
        let maximum = (1u32 << count) - 1;
        let mut magnitude = 1;
        while magnitude < maximum && self.bits.read(1)? == 0 {
            magnitude += 1;
        }
        Ok(if negative {
            -(magnitude as i32)
        } else {
            magnitude as i32
        })
    }
}

#[cfg(test)]
mod tests;
