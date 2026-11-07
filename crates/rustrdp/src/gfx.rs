//! Graphics Pipeline Extension, MS-RDPEGFX. The channel owns no OS resources.

mod alpha;
mod avc;
mod clear;
mod codecs;
mod progressive;
mod rfx;
#[cfg(test)]
mod tests;
mod wavelet;
mod zgfx;

use std::collections::BTreeMap;

use crate::{Error, Limits, Result, graphics::Graphics, wire::Reader};

const CAPABILITIES: [(u32, u32); 7] = [
    (0x000a_0701, 0xc2),
    (0x000a_0400, 0x42),
    (0x000a_0301, 0x40),
    (0x000a_0200, 2),
    (0x000a_0002, 2),
    (0x0008_0105, 0x12),
    (0x0008_0004, 2),
];
const MAX_SURFACES: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Rect {
    pub left: u16,
    pub top: u16,
    pub right: u16,
    pub bottom: u16,
}

impl Rect {
    fn read(reader: &mut Reader<'_>) -> Result<Self> {
        let rect = Self {
            left: reader.u16_le()?,
            top: reader.u16_le()?,
            right: reader.u16_le()?,
            bottom: reader.u16_le()?,
        };
        if rect.left > rect.right || rect.top > rect.bottom {
            return Err(Error::Invalid("graphics rectangle"));
        }
        Ok(rect)
    }
    pub(crate) fn width(self) -> usize {
        usize::from(self.right - self.left)
    }
    pub(crate) fn height(self) -> usize {
        usize::from(self.bottom - self.top)
    }
    fn checked(self, width: u16, height: u16) -> Result<Self> {
        if self.right > width || self.bottom > height {
            return Err(Error::Invalid("graphics rectangle bounds"));
        }
        Ok(self)
    }
    fn intersect(self, other: Self) -> Option<Self> {
        let result = Self {
            left: self.left.max(other.left),
            top: self.top.max(other.top),
            right: self.right.min(other.right),
            bottom: self.bottom.min(other.bottom),
        };
        (result.left < result.right && result.top < result.bottom).then_some(result)
    }
    fn union(self, other: Self) -> Self {
        Self {
            left: self.left.min(other.left),
            top: self.top.min(other.top),
            right: self.right.max(other.right),
            bottom: self.bottom.max(other.bottom),
        }
    }
}

struct Surface {
    width: u16,
    height: u16,
    pixels: Vec<u8>,
    origin: Option<(usize, usize)>,
    damage: Option<Rect>,
    codecs: codecs::Codecs,
}

impl Surface {
    fn bounds(&self) -> Rect {
        Rect {
            left: 0,
            top: 0,
            right: self.width,
            bottom: self.height,
        }
    }
    fn dirty(&mut self, rect: Rect) {
        if rect.width() != 0 && rect.height() != 0 {
            self.damage = Some(self.damage.map_or(rect, |previous| previous.union(rect)));
        }
    }
    fn copy(&self, rect: Rect) -> Vec<u8> {
        let mut pixels = Vec::with_capacity(rect.width() * rect.height() * 4);
        for y in rect.top..rect.bottom {
            let start = (usize::from(y) * usize::from(self.width) + usize::from(rect.left)) * 4;
            pixels.extend_from_slice(&self.pixels[start..start + rect.width() * 4]);
        }
        pixels
    }
    fn put(&mut self, rect: Rect, pixels: &[u8], stride: usize) {
        for y in 0..rect.height() {
            let start = ((usize::from(rect.top) + y) * usize::from(self.width)
                + usize::from(rect.left))
                * 4;
            self.pixels[start..start + rect.width() * 4]
                .copy_from_slice(&pixels[y * stride * 4..(y * stride + rect.width()) * 4]);
        }
        self.dirty(rect);
    }
}

struct CachedBitmap {
    width: u16,
    height: u16,
    pixels: Vec<u8>,
}

pub(crate) enum FrameEvent {
    Begin(u32),
    End { id: u32, total: u32 },
    Reset { width: u16, height: u16 },
}

pub(crate) struct Gfx {
    limits: Limits,
    allow_avc: bool,
    version: Option<u32>,
    confirmed_flags: u32,
    reset: bool,
    surfaces: BTreeMap<u16, Surface>,
    cache: BTreeMap<u16, CachedBitmap>,
    cache_bytes: usize,
    surface_bytes: usize,
    cache_limit: usize,
    zgfx: zgfx::Decoder,
    progressive: progressive::Decoder,
    clear: clear::Decoder,
    rfx: rfx::Decoder,
    decoded_frames: u32,
    codec: Option<crate::GraphicsCodec>,
    retired_avc_audit: (u64, u64),
}

impl Gfx {
    pub(crate) fn new(limits: Limits, allow_avc: bool) -> Self {
        Self {
            limits,
            allow_avc,
            version: None,
            confirmed_flags: 0,
            reset: false,
            surfaces: BTreeMap::new(),
            cache: BTreeMap::new(),
            cache_bytes: 0,
            surface_bytes: 0,
            cache_limit: 16 * 1024 * 1024,
            zgfx: zgfx::Decoder::new(),
            progressive: progressive::Decoder::new(limits.max_bitmap_bytes),
            clear: clear::Decoder::new(),
            rfx: rfx::Decoder::new(),
            decoded_frames: 0,
            codec: None,
            retired_avc_audit: (0, 0),
        }
    }
    pub(crate) fn take_avc_audit(&mut self) -> (u64, u64) {
        let mut counts = std::mem::take(&mut self.retired_avc_audit);
        for surface in self.surfaces.values_mut() {
            let (decoded, empty) = surface.codecs.take_avc_audit();
            counts.0 = counts.0.saturating_add(decoded);
            counts.1 = counts.1.saturating_add(empty);
        }
        counts
    }
    pub(crate) fn codec(&self) -> Option<crate::GraphicsCodec> {
        self.codec
    }
    fn offered_capabilities(&self) -> impl Iterator<Item = (u32, u32)> + '_ {
        CAPABILITIES.into_iter().map(|(version, flags)| {
            let flags = if self.allow_avc {
                flags
            } else if version >= 0x000a_0002 {
                // AVC_DISABLED and AVC_THINCLIENT are mutually exclusive.
                (flags & !0x40) | 0x20
            } else {
                flags & !0x10 // AVC420_ENABLED in RDP 8.1
            };
            (version, flags)
        })
    }
    pub(crate) fn capabilities(&self) -> Vec<u8> {
        let mut body = (CAPABILITIES.len() as u16).to_le_bytes().to_vec();
        for (version, flags) in self.offered_capabilities() {
            for field in [version, 4, flags] {
                body.extend_from_slice(&field.to_le_bytes());
            }
        }
        pdu(0x12, &body)
    }
    pub(crate) fn deactivate(&mut self) {
        // Share reactivation does not close a dynamic channel. Its surfaces,
        // caches and decoder references remain valid until their own release.
        self.progressive.end_frame();
    }
    pub(crate) fn acknowledge(id: u32, total: u32) -> Vec<u8> {
        let mut body = Vec::with_capacity(12);
        for field in [0, id, total] {
            body.extend_from_slice(&field.to_le_bytes());
        }
        pdu(0x0d, &body)
    }
    pub(crate) fn receive(
        &mut self,
        data: &[u8],
        graphics: &mut Graphics,
        mut on_frame: impl FnMut(FrameEvent, &mut Graphics) -> Result<()>,
    ) -> Result<()> {
        if data.len() > self.limits.max_channel_message {
            return Err(Error::Limit("graphics channel message"));
        }
        let decoded = self.zgfx.decode(data, self.limits.max_bitmap_bytes)?;
        let mut reader = Reader::new(&decoded);
        while !reader.is_empty() {
            let kind = reader.u16_le()?;
            let flags = reader.u16_le()?;
            let length = reader.u32_le()? as usize;
            if flags != 0 || length < 8 || length > self.limits.max_bitmap_bytes {
                return Err(Error::Invalid("graphics PDU header"));
            }
            let mut body = Reader::new(reader.take(length - 8)?);
            if self.version.is_none() && kind != 0x13 {
                return Err(Error::Invalid("graphics before capabilities"));
            }
            match kind {
                0x13 => {
                    let version = body.u32_le()?;
                    let length = body.u32_le()? as usize;
                    let caps = body.take(length)?;
                    body.finish()?;
                    // Unknown capability sets are ignored, not interpreted as
                    // a supported version (MS-RDPEGFX 3.3.5.19).
                    let offered = self
                        .offered_capabilities()
                        .find(|(value, _)| *value == version);
                    let Some((_, offered)) = offered else {
                        if matches!(version, 0x000a_0100 | 0x000a_0502 | 0x000a_0600) {
                            return Err(Error::Invalid("unoffered graphics capability version"));
                        }
                        continue;
                    };
                    if self.version.is_some() {
                        return Err(Error::Invalid("duplicate graphics capabilities"));
                    }
                    if length != 4 {
                        return Err(Error::Invalid("graphics capabilities size"));
                    }
                    let flags = Reader::new(caps).u32_le()?;
                    // Restrictions must survive confirmation: clearing
                    // AVC_DISABLED or SCALEDMAP_DISABLE would enable operations
                    // that were explicitly excluded from our capabilities.
                    if flags & !offered != 0 || flags & (offered & 0xa0) != offered & 0xa0 {
                        return Err(Error::Invalid("unoffered graphics capabilities"));
                    }
                    self.version = Some(version);
                    self.confirmed_flags = flags;
                    self.cache_limit = if flags & 3 != 0 || version == 0x000a_0301 {
                        16 * 1024 * 1024
                    } else {
                        100 * 1024 * 1024
                    };
                }
                0x0e => {
                    if length != 340 {
                        return Err(Error::Invalid("graphics reset length"));
                    }
                    let width = u16::try_from(body.u32_le()?)
                        .map_err(|_| Error::Limit("graphics desktop width"))?;
                    let height = u16::try_from(body.u32_le()?)
                        .map_err(|_| Error::Limit("graphics desktop height"))?;
                    self.limits.frame_bytes(width, height)?;
                    let monitors = body.u32_le()?;
                    if monitors > 16 {
                        return Err(Error::Limit("graphics monitor count"));
                    }
                    body.skip(monitors as usize * 20)?;
                    body.skip(body.remaining_len())?;
                    graphics.resize_output(width, height)?;
                    for surface in self.surfaces.values_mut() {
                        surface.dirty(surface.bounds());
                    }
                    self.reset = true;
                    on_frame(FrameEvent::Reset { width, height }, graphics)?;
                }
                _ if !self.reset => return Err(Error::Invalid("graphics before reset")),
                1 => self.wire_to_surface(&mut body)?,
                2 => {
                    let id = body.u16_le()?;
                    let codec = body.u16_le()?;
                    let context = body.u32_le()?;
                    pixel_format(body.u8()?)?;
                    let length = body.u32_le()? as usize;
                    let bytes = body.take(length)?;
                    if codec != 9 {
                        return Err(Error::Unsupported("progressive graphics codec"));
                    }
                    let surface = self
                        .surfaces
                        .get_mut(&id)
                        .ok_or(Error::Invalid("graphics surface ID"))?;
                    let tiles = self.progressive.decode_bitmap(
                        id,
                        context,
                        surface.width,
                        surface.height,
                        bytes,
                    )?;
                    self.codec = Some(crate::GraphicsCodec::Progressive);
                    for tile in tiles {
                        let left = usize::from(tile.x_idx) * 64;
                        let top = usize::from(tile.y_idx) * 64;
                        for region in tile.update_rectangles {
                            let rect = Rect {
                                left: region.left,
                                top: region.top,
                                right: region.right,
                                bottom: region.bottom,
                            }
                            .checked(surface.width, surface.height)?;
                            codecs::put_bgra_tile(surface, rect, left, top, &tile.pixels)?;
                        }
                    }
                }
                3 => {
                    let id = body.u16_le()?;
                    let context = body.u32_le()?;
                    self.require_surface(id)?;
                    self.progressive.delete_context(id, context);
                }
                4 => {
                    let id = body.u16_le()?;
                    let pixel: [u8; 4] = body.take(4)?.try_into().map_err(|_| Error::Truncated)?;
                    let count = usize::from(body.u16_le()?);
                    let surface = self
                        .surfaces
                        .get_mut(&id)
                        .ok_or(Error::Invalid("graphics surface ID"))?;
                    for _ in 0..count {
                        let rect = Rect::read(&mut body)?.checked(surface.width, surface.height)?;
                        for y in rect.top..rect.bottom {
                            for x in rect.left..rect.right {
                                let offset = (usize::from(y) * usize::from(surface.width)
                                    + usize::from(x))
                                    * 4;
                                surface.pixels[offset..offset + 4].copy_from_slice(&pixel);
                            }
                        }
                        surface.dirty(rect);
                    }
                }
                5 => {
                    let source = body.u16_le()?;
                    let destination = body.u16_le()?;
                    let source = self.require_surface(source)?;
                    let rect = Rect::read(&mut body)?.checked(source.width, source.height)?;
                    let bitmap = CachedBitmap {
                        width: rect.width() as u16,
                        height: rect.height() as u16,
                        pixels: source.copy(rect),
                    };
                    let surface = self
                        .surfaces
                        .get_mut(&destination)
                        .ok_or(Error::Invalid("graphics surface ID"))?;
                    copy_points(surface, &bitmap, &mut body)?;
                }
                6 => {
                    let id = body.u16_le()?;
                    body.skip(8)?;
                    let slot = body.u16_le()?;
                    let max_slots = if self.cache_limit == 16 * 1024 * 1024 {
                        4096
                    } else {
                        25600
                    };
                    if slot == 0 || usize::from(slot) > max_slots {
                        return Err(Error::Invalid("graphics cache slot"));
                    }
                    let source = self.require_surface(id)?;
                    let rect = Rect::read(&mut body)?.checked(source.width, source.height)?;
                    let bytes = rect.width() * rect.height() * 4;
                    let previous = self.cache.get(&slot).map_or(0, |entry| entry.pixels.len());
                    if self.cache_bytes - previous + bytes > self.cache_limit {
                        return Err(Error::Limit("graphics bitmap cache"));
                    }
                    let bitmap = CachedBitmap {
                        width: rect.width() as u16,
                        height: rect.height() as u16,
                        pixels: source.copy(rect),
                    };
                    self.cache.insert(slot, bitmap);
                    self.cache_bytes = self.cache_bytes - previous + bytes;
                }
                7 => {
                    let slot = body.u16_le()?;
                    let id = body.u16_le()?;
                    let bitmap = self
                        .cache
                        .get(&slot)
                        .ok_or(Error::Invalid("graphics cache miss"))?;
                    let surface = self
                        .surfaces
                        .get_mut(&id)
                        .ok_or(Error::Invalid("graphics surface ID"))?;
                    copy_points(surface, bitmap, &mut body)?;
                }
                8 => {
                    let slot = body.u16_le()?;
                    if let Some(bitmap) = self.cache.remove(&slot) {
                        self.cache_bytes -= bitmap.pixels.len();
                    }
                }
                9 => {
                    let id = body.u16_le()?;
                    let width = body.u16_le()?;
                    let height = body.u16_le()?;
                    pixel_format(body.u8()?)?;
                    let bytes = self.limits.frame_bytes(width, height)?;
                    if self.surfaces.contains_key(&id) {
                        return Err(Error::Invalid("duplicate graphics surface"));
                    }
                    if self.surfaces.len() == MAX_SURFACES
                        || self.surface_bytes.saturating_add(bytes)
                            > self.limits.max_bitmap_bytes.saturating_mul(4)
                    {
                        return Err(Error::Limit("graphics surfaces"));
                    }
                    let mut pixels = Vec::new();
                    pixels
                        .try_reserve_exact(bytes)
                        .map_err(|_| Error::Limit("graphics surface allocation"))?;
                    pixels.resize(bytes, 0);
                    for alpha in pixels.iter_mut().skip(3).step_by(4) {
                        *alpha = 255;
                    }
                    self.surfaces.insert(
                        id,
                        Surface {
                            width,
                            height,
                            pixels,
                            origin: None,
                            damage: None,
                            codecs: codecs::Codecs::default(),
                        },
                    );
                    self.surface_bytes += bytes;
                }
                10 => {
                    let id = body.u16_le()?;
                    if let Some(mut surface) = self.surfaces.remove(&id) {
                        self.surface_bytes -= surface.pixels.len();
                        let (decoded, empty) = surface.codecs.take_avc_audit();
                        self.retired_avc_audit.0 = self.retired_avc_audit.0.saturating_add(decoded);
                        self.retired_avc_audit.1 = self.retired_avc_audit.1.saturating_add(empty);
                    }
                    self.progressive.delete_surface(id);
                }
                11 => {
                    let _timestamp = body.u32_le()?;
                    let id = body.u32_le()?;
                    graphics.begin_frame(id)?;
                    self.progressive.begin_frame();
                    on_frame(FrameEvent::Begin(id), graphics)?;
                }
                12 => {
                    let id = body.u32_le()?;
                    graphics.end_frame(id)?;
                    self.progressive.end_frame();
                    self.compose(graphics);
                    self.decoded_frames = self.decoded_frames.wrapping_add(1);
                    on_frame(
                        FrameEvent::End {
                            id,
                            total: self.decoded_frames,
                        },
                        graphics,
                    )?;
                }
                15 => {
                    let id = body.u16_le()?;
                    body.skip(2)?;
                    let x = body.u32_le()? as usize;
                    let y = body.u32_le()? as usize;
                    let surface = self
                        .surfaces
                        .get_mut(&id)
                        .ok_or(Error::Invalid("graphics surface ID"))?;
                    surface.origin = Some((x, y));
                    surface.dirty(surface.bounds());
                }
                17 => {
                    if body.u16_le()? != 0 {
                        return Err(Error::Invalid("unoffered persistent graphics cache"));
                    }
                }
                _ => return Err(Error::Unsupported("graphics PDU type")),
            }
            body.finish()?;
        }
        if !graphics.frame_in_progress() {
            self.compose(graphics);
        }
        Ok(())
    }

    fn require_surface(&self, id: u16) -> Result<&Surface> {
        self.surfaces
            .get(&id)
            .ok_or(Error::Invalid("graphics surface ID"))
    }

    fn compose(&mut self, graphics: &mut Graphics) {
        for surface in self.surfaces.values_mut() {
            if let (Some(rect), Some((x, y))) = (surface.damage.take(), surface.origin) {
                graphics.compose(
                    &surface.pixels,
                    usize::from(surface.width),
                    rect,
                    x.saturating_add(usize::from(rect.left)),
                    y.saturating_add(usize::from(rect.top)),
                );
            }
        }
    }

    fn wire_to_surface(&mut self, reader: &mut Reader<'_>) -> Result<()> {
        let id = reader.u16_le()?;
        let codec = reader.u16_le()?;
        if matches!(codec, 11 | 14 | 15) && !self.avc_codec_negotiated(codec) {
            return Err(Error::Invalid("unnegotiated graphics AVC codec"));
        }
        pixel_format(reader.u8()?)?;
        let surface = self
            .surfaces
            .get_mut(&id)
            .ok_or(Error::Invalid("graphics surface ID"))?;
        let rect = Rect::read(reader)?.checked(surface.width, surface.height)?;
        let length = reader.u32_le()? as usize;
        let bytes = reader.take(length)?;
        codecs::decode(
            surface,
            codec,
            rect,
            bytes,
            self.limits,
            &mut self.clear,
            &mut self.rfx,
        )?;
        use crate::GraphicsCodec;
        self.codec = Some(match codec {
            0 => GraphicsCodec::Raw,
            3 => GraphicsCodec::RemoteFx,
            8 => GraphicsCodec::Clear,
            10 => GraphicsCodec::Planar,
            11 => GraphicsCodec::Avc420,
            12 => GraphicsCodec::Alpha,
            14 => GraphicsCodec::Avc444,
            15 => GraphicsCodec::Avc444V2,
            _ => return Err(Error::Unsupported("graphics bitmap codec")),
        });
        Ok(())
    }

    fn avc_codec_negotiated(&self, codec: u16) -> bool {
        if !self.allow_avc {
            return false;
        }
        match self.version {
            Some(0x0008_0105) => codec == 11 && self.confirmed_flags & 0x10 != 0,
            Some(version) if version >= 0x000a_0002 => {
                self.confirmed_flags & 0x20 == 0 && (codec != 15 || version >= 0x000a_0200)
            }
            _ => false,
        }
    }
}

fn copy_points(
    surface: &mut Surface,
    bitmap: &CachedBitmap,
    reader: &mut Reader<'_>,
) -> Result<()> {
    let count = reader.u16_le()?;
    for _ in 0..count {
        let x = reader.u16_le()?;
        let y = reader.u16_le()?;
        let right = x
            .checked_add(bitmap.width)
            .ok_or(Error::Invalid("graphics copy rectangle"))?;
        let bottom = y
            .checked_add(bitmap.height)
            .ok_or(Error::Invalid("graphics copy rectangle"))?;
        let rect = Rect {
            left: x,
            top: y,
            right,
            bottom,
        }
        .checked(surface.width, surface.height)?;
        surface.put(rect, &bitmap.pixels, usize::from(bitmap.width));
    }
    Ok(())
}

fn pixel_format(value: u8) -> Result<()> {
    if matches!(value, 0x20 | 0x21) {
        Ok(())
    } else {
        Err(Error::Unsupported("graphics pixel format"))
    }
}

fn pdu(kind: u16, body: &[u8]) -> Vec<u8> {
    let mut data = Vec::with_capacity(body.len() + 8);
    data.extend_from_slice(&kind.to_le_bytes());
    data.extend_from_slice(&0u16.to_le_bytes());
    data.extend_from_slice(&((body.len() + 8) as u32).to_le_bytes());
    data.extend_from_slice(body);
    data
}
