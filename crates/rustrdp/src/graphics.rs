//! Bounded bitmap decoding and software pointer composition.

mod bitmap;
pub(crate) mod nscodec;
pub(crate) mod planar;
mod pointer;
mod rle;
mod surface;

use std::{collections::BTreeMap, sync::Arc};

use crate::{Error, Limits, Result, wire::Reader};
use pointer::Shape;

pub(crate) use surface::FrameMarker;

pub(crate) type SurfaceCallback<'a> = dyn FnMut(FrameMarker, &mut Graphics) -> Result<()> + 'a;

/// A rectangle containing framebuffer changes since the previous damage read.
///
/// Coordinates are top-left based and clipped to the current desktop. A damage
/// rectangle always has nonzero dimensions and may include unchanged pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Damage {
    /// Left edge in desktop pixels.
    pub x: u16,
    /// Top edge in desktop pixels.
    pub y: u16,
    /// Width in desktop pixels.
    pub width: u16,
    /// Height in desktop pixels.
    pub height: u16,
}

/// A pointer update, with its transport header already removed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PointerKind {
    Default,
    Hidden,
    Position,
    Color,
    Cached,
    New,
    Large,
}

enum Pointer {
    Default,
    Hidden,
    Custom(Arc<Shape>),
}

#[derive(Default)]
struct Underlay {
    left: usize,
    top: usize,
    width: usize,
    pixels: Vec<u8>,
}

/// A top-down, packed BGRA desktop including the remote software pointer.
///
/// Obtain this read-only view from [`crate::Session::graphics`]. Its dimensions
/// and pixels remain valid until the session processes another update. Copy only
/// when the renderer needs ownership beyond that point; reading the view itself
/// does not allocate or schedule a redraw.
pub struct Graphics {
    width: u16,
    height: u16,
    limits: Limits,
    pixels: Vec<u8>,
    damage: Option<Damage>,
    frame: Option<u32>,
    palette: [[u8; 4]; 256],
    indices: Option<Vec<u16>>,
    pointer: Pointer,
    pointer_x: u16,
    pointer_y: u16,
    pointer_cache: BTreeMap<u16, Arc<Shape>>,
    pointer_cache_bytes: usize,
    underlay: Underlay,
}

impl Graphics {
    pub(crate) fn new(width: u16, height: u16, limits: Limits) -> Result<Self> {
        Ok(Self {
            width,
            height,
            limits,
            pixels: framebuffer(width, height, limits)?,
            damage: Some(Damage {
                x: 0,
                y: 0,
                width,
                height,
            }),
            frame: None,
            palette: [[0, 0, 0, 255]; 256],
            indices: None,
            pointer: Pointer::Default,
            pointer_x: 0,
            pointer_y: 0,
            pointer_cache: BTreeMap::new(),
            pointer_cache_bytes: 0,
            underlay: Underlay::default(),
        })
    }

    pub(crate) fn resize(&mut self, width: u16, height: u16) -> Result<()> {
        let pixels = framebuffer(width, height, self.limits)?;
        self.width = width;
        self.height = height;
        self.pixels = pixels;
        self.frame = None;
        self.damage = Some(Damage {
            x: 0,
            y: 0,
            width,
            height,
        });
        self.indices = None;
        self.underlay.pixels.clear();
        self.pointer_x = self.pointer_x.min(width - 1);
        self.pointer_y = self.pointer_y.min(height - 1);
        self.draw_pointer();
        Ok(())
    }

    /// Desktop width in pixels.
    pub fn width(&self) -> u16 {
        self.width
    }
    /// Desktop height in pixels.
    pub fn height(&self) -> u16 {
        self.height
    }
    /// Packed top-down BGRA pixels, with a row stride of `width() * 4` bytes.
    /// The buffer length is exactly `width() * height() * 4` bytes.
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }
    /// Whether the renderer should hide its local pointer over the remote image.
    ///
    /// A remote software pointer is already composited into [`Self::pixels`].
    /// This also returns `true` when the server explicitly hides the pointer.
    /// A server request for the system default pointer returns `false`.
    pub fn hide_local_pointer(&self) -> bool {
        !matches!(self.pointer, Pointer::Default)
    }

    pub(crate) fn pointer_position(&self) -> (u16, u16) {
        (self.pointer_x, self.pointer_y)
    }

    pub(crate) fn take_damage(&mut self) -> Option<Damage> {
        self.damage.take()
    }

    pub(crate) fn frame_in_progress(&self) -> bool {
        self.frame.is_some()
    }

    pub(crate) fn cancel_frame(&mut self) {
        self.frame = None;
    }

    #[cfg(feature = "graphics")]
    pub(crate) fn begin_frame(&mut self, id: u32) -> Result<()> {
        if self.frame.is_some() {
            return Err(Error::Invalid("overlapping graphics frames"));
        }
        self.frame = Some(id);
        Ok(())
    }

    #[cfg(feature = "graphics")]
    pub(crate) fn end_frame(&mut self, id: u32) -> Result<()> {
        if self.frame != Some(id) {
            return Err(Error::Invalid("graphics frame identifier"));
        }
        self.frame = None;
        Ok(())
    }

    #[cfg(feature = "graphics")]
    pub(crate) fn resize_output(&mut self, width: u16, height: u16) -> Result<()> {
        let frame = self.frame;
        self.resize(width, height)?;
        self.frame = frame;
        Ok(())
    }

    #[cfg(feature = "graphics")]
    pub(crate) fn compose(
        &mut self,
        pixels: &[u8],
        stride: usize,
        rect: crate::gfx::Rect,
        x: usize,
        y: usize,
    ) {
        let width = rect.width().min(usize::from(self.width).saturating_sub(x));
        let height = rect
            .height()
            .min(usize::from(self.height).saturating_sub(y));
        if width == 0 || height == 0 {
            return;
        }
        self.restore_pointer();
        let desktop_width = usize::from(self.width);
        for row in 0..height {
            let source = (usize::from(rect.top) + row) * stride + usize::from(rect.left);
            let destination = (y + row) * desktop_width + x;
            self.pixels[destination * 4..(destination + width) * 4]
                .copy_from_slice(&pixels[source * 4..(source + width) * 4]);
            // Output surfaces are composited onto an opaque desktop. Alpha is
            // retained in the pipeline surface/cache for subsequent operations.
            for alpha in self.pixels[destination * 4..(destination + width) * 4]
                .iter_mut()
                .skip(3)
                .step_by(4)
            {
                *alpha = 255;
            }
            if let Some(indices) = &mut self.indices {
                indices[destination..destination + width].fill(256);
            }
        }
        self.mark_damage(x, y, width, height);
        self.draw_pointer();
    }

    fn mark_damage(&mut self, x: usize, y: usize, width: usize, height: usize) {
        let mut right = x.saturating_add(width).min(usize::from(self.width));
        let mut bottom = y.saturating_add(height).min(usize::from(self.height));
        if x >= right || y >= bottom {
            return;
        }
        let mut left = x;
        let mut top = y;
        if let Some(previous) = self.damage {
            left = left.min(usize::from(previous.x));
            top = top.min(usize::from(previous.y));
            right = right.max(usize::from(previous.x) + usize::from(previous.width));
            bottom = bottom.max(usize::from(previous.y) + usize::from(previous.height));
        }
        self.damage = Some(Damage {
            x: left as u16,
            y: top as u16,
            width: (right - left) as u16,
            height: (bottom - top) as u16,
        });
    }

    /// Decodes TS_UPDATE_BITMAP_DATA, including updateType and rectangle count.
    /// Invalid updates leave the framebuffer and pointer unchanged.
    pub(crate) fn bitmap_update(&mut self, data: &[u8]) -> Result<bool> {
        let images = bitmap::update(data, self.limits, &self.palette)?;
        Ok(self.apply_bitmaps(images))
    }

    fn apply_bitmaps(&mut self, images: impl IntoIterator<Item = bitmap::Bitmap>) -> bool {
        let mut changed = false;
        let desktop_width = usize::from(self.width);
        let desktop_height = usize::from(self.height);
        for image in images {
            let width = image
                .visible_width
                .min(desktop_width.saturating_sub(image.left));
            let height = image
                .visible_height
                .min(desktop_height.saturating_sub(image.top));
            if width == 0 || height == 0 {
                continue;
            }
            if !changed {
                self.restore_pointer();
            }
            if image.indices.is_some() && self.indices.is_none() {
                self.indices = Some(vec![256; desktop_width * desktop_height]);
            }
            for y in 0..height {
                let source = y * image.width;
                let destination = (image.top + y) * desktop_width + image.left;
                self.pixels[destination * 4..(destination + width) * 4]
                    .copy_from_slice(&image.pixels[source * 4..(source + width) * 4]);
                if let Some(indices) = &mut self.indices {
                    let row = &mut indices[destination..destination + width];
                    if let Some(source_indices) = &image.indices {
                        for (target, &index) in
                            row.iter_mut().zip(&source_indices[source..source + width])
                        {
                            *target = u16::from(index);
                        }
                    } else {
                        row.fill(256);
                    }
                }
            }
            self.mark_damage(image.left, image.top, width, height);
            changed = true;
        }
        if changed {
            self.draw_pointer();
        }
        changed
    }

    /// Decodes TS_UPDATE_PALETTE_DATA, including updateType and padding.
    pub(crate) fn palette_update(&mut self, data: &[u8]) -> Result<bool> {
        let mut input = Reader::new(data);
        if input.u16_le()? != 2 {
            return Err(Error::Invalid("palette update type"));
        }
        input.skip(2)?;
        let count = input.u32_le()? as usize;
        if count == 0 || count > 256 {
            return Err(Error::Invalid("palette entry count"));
        }
        let entries = input.take(count * 3)?;
        if !input.is_empty() {
            return Err(Error::Invalid("trailing palette data"));
        }
        let mut palette_changed = false;
        for (target, rgb) in self
            .palette
            .iter_mut()
            .zip(entries.as_chunks::<3>().0.iter())
        {
            let color = [rgb[2], rgb[1], rgb[0], 255];
            palette_changed |= *target != color;
            *target = color;
        }
        if !palette_changed || self.indices.is_none() {
            return Ok(false);
        }
        self.restore_pointer();
        let mut changed = false;
        if let Some(indices) = &self.indices {
            for (&index, target) in indices
                .iter()
                .zip(self.pixels.as_chunks_mut::<4>().0.iter_mut())
            {
                if index < 256 && *target != self.palette[usize::from(index)] {
                    *target = self.palette[usize::from(index)];
                    changed = true;
                }
            }
        }
        if changed {
            self.mark_damage(0, 0, usize::from(self.width), usize::from(self.height));
        }
        self.draw_pointer();
        Ok(changed)
    }

    /// Applies a pointer update and reports changes to pixels or cursor visibility.
    pub(crate) fn pointer_update(&mut self, kind: PointerKind, data: &[u8]) -> Result<bool> {
        let pointer = match kind {
            PointerKind::Default | PointerKind::Hidden => {
                if !data.is_empty() {
                    return Err(Error::Invalid("system pointer payload"));
                }
                if kind == PointerKind::Default {
                    Pointer::Default
                } else {
                    Pointer::Hidden
                }
            }
            PointerKind::Position => {
                let mut input = Reader::new(data);
                let x = input.u16_le()?;
                let y = input.u16_le()?;
                if !input.is_empty() {
                    return Err(Error::Invalid("pointer position length"));
                }
                return self.move_pointer(x, y);
            }
            PointerKind::Cached => {
                let mut input = Reader::new(data);
                let index = input.u16_le()?;
                if !input.is_empty() {
                    return Err(Error::Invalid("cached pointer length"));
                }
                let shape = self
                    .pointer_cache
                    .get(&index)
                    .ok_or(Error::Invalid("uncached pointer index"))?;
                Pointer::Custom(Arc::clone(shape))
            }
            PointerKind::Color | PointerKind::New | PointerKind::Large => {
                let (index, shape) = Shape::decode(kind, data, self.limits, &self.palette)?;
                let previous_size = self
                    .pointer_cache
                    .get(&index)
                    .map_or(0, |shape| shape.bytes());
                let cache_bytes = (self.pointer_cache_bytes - previous_size)
                    .checked_add(shape.bytes())
                    .filter(|size| *size <= self.limits.max_bitmap_bytes)
                    .ok_or(Error::Limit("pointer cache allocation"))?;
                let shape = Arc::new(shape);
                self.pointer_cache.insert(index, Arc::clone(&shape));
                self.pointer_cache_bytes = cache_bytes;
                Pointer::Custom(shape)
            }
        };
        let unchanged = match (&self.pointer, &pointer) {
            (Pointer::Default, Pointer::Default) | (Pointer::Hidden, Pointer::Hidden) => true,
            (Pointer::Custom(current), Pointer::Custom(next)) => Arc::ptr_eq(current, next),
            _ => false,
        };
        if unchanged {
            return Ok(false);
        }
        self.restore_pointer();
        self.pointer = pointer;
        self.draw_pointer();
        Ok(true)
    }

    pub(crate) fn move_pointer(&mut self, x: u16, y: u16) -> Result<bool> {
        if (self.pointer_x, self.pointer_y) == (x, y) {
            return Ok(false);
        }
        self.restore_pointer();
        self.pointer_x = x;
        self.pointer_y = y;
        self.draw_pointer();
        Ok(matches!(self.pointer, Pointer::Custom(_)))
    }

    fn restore_pointer(&mut self) {
        let underlay = &mut self.underlay;
        if underlay.pixels.is_empty() {
            return;
        }
        for (y, row) in underlay.pixels.chunks_exact(underlay.width * 4).enumerate() {
            let offset = ((underlay.top + y) * usize::from(self.width) + underlay.left) * 4;
            self.pixels[offset..offset + row.len()].copy_from_slice(row);
        }
        let (left, top, width, height) = (
            underlay.left,
            underlay.top,
            underlay.width,
            underlay.pixels.len() / (underlay.width * 4),
        );
        // Retain the cursor-sized allocation across mouse moves.
        underlay.pixels.clear();
        self.mark_damage(left, top, width, height);
    }

    fn draw_pointer(&mut self) {
        let Pointer::Custom(shape) = &self.pointer else {
            return;
        };
        let origin_x = i32::from(self.pointer_x) - i32::from(shape.hotspot_x);
        let origin_y = i32::from(self.pointer_y) - i32::from(shape.hotspot_y);
        let left = origin_x.max(0) as usize;
        let top = origin_y.max(0) as usize;
        let right = (origin_x + shape.width as i32)
            .min(i32::from(self.width))
            .max(0) as usize;
        let bottom = (origin_y + shape.height as i32)
            .min(i32::from(self.height))
            .max(0) as usize;
        if right <= left || bottom <= top {
            return;
        }
        let underlay = &mut self.underlay;
        underlay.left = left;
        underlay.top = top;
        underlay.width = right - left;
        let pixels = &mut underlay.pixels;
        pixels.clear();
        pixels.reserve((right - left) * (bottom - top) * 4);
        for y in top..bottom {
            let row = (y * usize::from(self.width) + left) * 4;
            pixels.extend_from_slice(&self.pixels[row..row + (right - left) * 4]);
            for x in left..right {
                let source =
                    (y as i32 - origin_y) as usize * shape.width + (x as i32 - origin_x) as usize;
                let destination = (y * usize::from(self.width) + x) * 4;
                shape.composite(source, &mut self.pixels[destination..destination + 4]);
            }
        }
        self.mark_damage(left, top, right - left, bottom - top);
    }
}

fn framebuffer(width: u16, height: u16, limits: Limits) -> Result<Vec<u8>> {
    let bytes = limits.frame_bytes(width, height)?;
    let mut pixels = Vec::new();
    pixels
        .try_reserve_exact(bytes)
        .map_err(|_| Error::Limit("framebuffer allocation"))?;
    pixels.resize(bytes, 0);
    for alpha in pixels.iter_mut().skip(3).step_by(4) {
        *alpha = 255;
    }
    Ok(pixels)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bitmap(
        left: u16,
        top: u16,
        width: u16,
        height: u16,
        depth: u16,
        flags: u16,
        data: &[u8],
    ) -> Vec<u8> {
        let mut update = Vec::new();
        for value in [
            1,
            1,
            left,
            top,
            left + width - 1,
            top + height - 1,
            width,
            height,
            depth,
            flags,
            data.len() as u16,
        ] {
            update.extend_from_slice(&value.to_le_bytes());
        }
        update.extend_from_slice(data);
        update
    }

    fn color_pointer(hotspot_x: u16, width: u16, xor: &[u8], and: &[u8]) -> Vec<u8> {
        let mut update = Vec::new();
        for value in [
            0,
            hotspot_x,
            0,
            width,
            1,
            and.len() as u16,
            xor.len() as u16,
        ] {
            update.extend_from_slice(&value.to_le_bytes());
        }
        update.extend_from_slice(xor);
        update.extend_from_slice(and);
        update
    }

    #[test]
    fn damage_accumulates_clipped_updates_and_resets_after_resize() {
        let mut graphics = Graphics::new(8, 5, Limits::default()).unwrap();
        assert_eq!(
            graphics.take_damage(),
            Some(Damage {
                x: 0,
                y: 0,
                width: 8,
                height: 5
            })
        );
        assert_eq!(graphics.take_damage(), None);
        graphics
            .bitmap_update(&bitmap(1, 1, 1, 1, 24, 0, &[1, 2, 3, 0]))
            .unwrap();
        graphics
            .bitmap_update(&bitmap(6, 4, 3, 2, 24, 0, &[0; 24]))
            .unwrap();
        assert_eq!(
            graphics.take_damage(),
            Some(Damage {
                x: 1,
                y: 1,
                width: 7,
                height: 4
            })
        );
        assert_eq!(graphics.take_damage(), None);
        assert!(
            !graphics
                .bitmap_update(&bitmap(8, 0, 1, 1, 24, 0, &[0; 4]))
                .unwrap()
        );
        assert_eq!(graphics.take_damage(), None);
        graphics
            .bitmap_update(&bitmap(7, 4, 1, 1, 24, 0, &[0; 4]))
            .unwrap();
        graphics.resize(3, 2).unwrap();
        assert_eq!(
            graphics.take_damage(),
            Some(Damage {
                x: 0,
                y: 0,
                width: 3,
                height: 2
            })
        );
    }

    #[test]
    fn pointer_damage_covers_old_and_new_clipped_bounds() {
        let mut graphics = Graphics::new(8, 2, Limits::default()).unwrap();
        graphics.take_damage();
        let shape = color_pointer(1, 2, &[255; 6], &[0; 2]);
        graphics.pointer_update(PointerKind::Color, &shape).unwrap();
        assert_eq!(
            graphics.take_damage(),
            Some(Damage {
                x: 0,
                y: 0,
                width: 1,
                height: 1
            })
        );
        graphics.move_pointer(4, 1).unwrap();
        assert_eq!(
            graphics.take_damage(),
            Some(Damage {
                x: 0,
                y: 0,
                width: 5,
                height: 2
            })
        );
        assert!(
            !graphics
                .bitmap_update(&bitmap(8, 0, 1, 1, 24, 0, &[0; 4]))
                .unwrap()
        );
        assert_eq!(graphics.take_damage(), None);
        graphics.move_pointer(12, 1).unwrap();
        assert_eq!(
            graphics.take_damage(),
            Some(Damage {
                x: 3,
                y: 1,
                width: 2,
                height: 1
            })
        );
        assert!(graphics.pointer_update(PointerKind::Hidden, &[]).unwrap());
        assert_eq!(graphics.take_damage(), None);
        assert!(graphics.pointer_update(PointerKind::Default, &[]).unwrap());
        assert_eq!(graphics.take_damage(), None);
    }

    #[test]
    fn raw_bitmaps_respect_padding_orientation_and_clipping() {
        let mut graphics = Graphics::new(2, 2, Limits::default()).unwrap();
        let update = bitmap(
            1,
            0,
            2,
            2,
            24,
            0,
            &[1, 2, 3, 4, 5, 6, 0, 0, 10, 20, 30, 40, 50, 60, 0, 0],
        );
        assert!(graphics.bitmap_update(&update).unwrap());
        assert_eq!(&graphics.pixels()[4..8], &[10, 20, 30, 255]);
        assert_eq!(&graphics.pixels()[12..16], &[1, 2, 3, 255]);
        assert!(
            !graphics
                .bitmap_update(&bitmap(5, 0, 1, 1, 24, 0, &[1, 2, 3, 0]))
                .unwrap()
        );
    }

    #[test]
    fn compressed_bitmaps_decode_through_the_update_header() {
        let mut graphics = Graphics::new(2, 1, Limits::default()).unwrap();
        graphics
            .bitmap_update(&bitmap(0, 0, 2, 1, 16, 0x401, &[0x82, 0, 0xf8, 0xe0, 7]))
            .unwrap();
        assert_eq!(graphics.pixels(), &[0, 0, 255, 255, 0, 255, 0, 255]);
        graphics
            .bitmap_update(&bitmap(
                0,
                0,
                2,
                1,
                32,
                0x401,
                &[0x30, 0x20, 1, 2, 0x20, 3, 4, 0x20, 5, 6],
            ))
            .unwrap();
        assert_eq!(graphics.pixels(), &[5, 3, 1, 255, 6, 4, 2, 255]);
        graphics
            .bitmap_update(&bitmap(0, 0, 2, 1, 32, 0x401, &[0x29, 100, 110, 10, 5]))
            .unwrap();
        assert_eq!(graphics.pixels(), &[105, 105, 85, 255, 115, 115, 95, 255]);
    }

    #[test]
    fn indexed_pixels_follow_palette_changes() {
        let mut graphics = Graphics::new(1, 1, Limits::default()).unwrap();
        graphics
            .bitmap_update(&bitmap(0, 0, 1, 1, 8, 0, &[1, 0, 0, 0]))
            .unwrap();
        graphics.take_damage();
        assert!(
            graphics
                .palette_update(&[2, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 1, 2, 3])
                .unwrap()
        );
        assert_eq!(graphics.pixels(), &[3, 2, 1, 255]);
        assert_eq!(
            graphics.take_damage(),
            Some(Damage {
                x: 0,
                y: 0,
                width: 1,
                height: 1
            })
        );
        assert!(
            !graphics
                .palette_update(&[2, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 1, 2, 3])
                .unwrap()
        );
        assert_eq!(graphics.take_damage(), None);
    }

    #[test]
    fn pointer_restore_tracks_bitmap_updates_and_visibility() {
        let mut graphics = Graphics::new(2, 1, Limits::default()).unwrap();
        let shape = color_pointer(0, 1, &[255, 255, 255, 0], &[0, 0]);
        assert!(!graphics.hide_local_pointer());
        graphics.pointer_update(PointerKind::Color, &shape).unwrap();
        assert!(graphics.hide_local_pointer());
        assert_eq!(&graphics.pixels()[..4], &[255; 4]);
        graphics
            .bitmap_update(&bitmap(0, 0, 1, 1, 24, 0, &[1, 2, 3, 0]))
            .unwrap();
        assert_eq!(&graphics.pixels()[..4], &[255; 4]);
        graphics.move_pointer(1, 0).unwrap();
        assert_eq!(graphics.pixels(), &[1, 2, 3, 255, 255, 255, 255, 255]);
        graphics.pointer_update(PointerKind::Hidden, &[]).unwrap();
        assert!(graphics.hide_local_pointer());
        assert_eq!(&graphics.pixels()[4..], &[0, 0, 0, 255]);
        graphics
            .pointer_update(PointerKind::Cached, &[0, 0])
            .unwrap();
        assert_eq!(&graphics.pixels()[4..], &[255; 4]);
        graphics.pointer_update(PointerKind::Default, &[]).unwrap();
        assert!(!graphics.hide_local_pointer());
        assert_eq!(&graphics.pixels()[4..], &[0, 0, 0, 255]);
    }

    #[test]
    fn pointer_hotspot_clipping_and_xor_restore_are_exact() {
        let mut graphics = Graphics::new(1, 1, Limits::default()).unwrap();
        graphics
            .bitmap_update(&bitmap(0, 0, 1, 1, 24, 0, &[1, 2, 3, 0]))
            .unwrap();
        let shape = color_pointer(1, 2, &[0, 0, 0, 255, 255, 255], &[0xc0, 0]);
        graphics.pointer_update(PointerKind::Color, &shape).unwrap();
        assert_eq!(graphics.pixels(), &[254, 253, 252, 255]);
        graphics.move_pointer(2, 0).unwrap();
        assert_eq!(graphics.pixels(), &[1, 2, 3, 255]);
    }

    #[test]
    fn pointer_moves_reuse_background_storage_and_repeated_shapes_do_not_redraw() {
        let mut graphics = Graphics::new(3, 1, Limits::default()).unwrap();
        let shape = color_pointer(0, 1, &[255, 255, 255, 0], &[0, 0]);
        graphics.pointer_update(PointerKind::Color, &shape).unwrap();
        let allocation = graphics.underlay.pixels.as_ptr();
        assert!(
            !graphics
                .pointer_update(PointerKind::Cached, &[0, 0])
                .unwrap()
        );
        assert!(!graphics.move_pointer(0, 0).unwrap());
        for x in [1, 2, 4, 0] {
            assert!(graphics.move_pointer(x, 0).unwrap());
            assert_eq!(graphics.underlay.pixels.as_ptr(), allocation);
            for pixel in 0..3 {
                let expected = if pixel == usize::from(x) {
                    [255; 4]
                } else {
                    [0, 0, 0, 255]
                };
                assert_eq!(&graphics.pixels()[pixel * 4..pixel * 4 + 4], &expected);
            }
        }
        assert!(graphics.pointer_update(PointerKind::Hidden, &[]).unwrap());
        assert!(!graphics.pointer_update(PointerKind::Hidden, &[]).unwrap());
        assert!(graphics.pointer_update(PointerKind::Default, &[]).unwrap());
        assert!(!graphics.pointer_update(PointerKind::Default, &[]).unwrap());
    }

    #[test]
    fn alpha_pointer_accepts_absent_and_mask() {
        let mut graphics = Graphics::new(1, 1, Limits::default()).unwrap();
        graphics
            .bitmap_update(&bitmap(0, 0, 1, 1, 24, 0, &[20, 40, 60, 0]))
            .unwrap();
        let mut shape = 32u16.to_le_bytes().to_vec();
        shape.extend(color_pointer(0, 1, &[100, 120, 140, 128], &[]));
        graphics.pointer_update(PointerKind::New, &shape).unwrap();
        assert_eq!(graphics.pixels(), &[60, 80, 100, 255]);
    }

    #[test]
    fn malformed_updates_leave_desktop_unchanged() {
        let mut graphics = Graphics::new(1, 1, Limits::default()).unwrap();
        graphics.take_damage();
        let update = bitmap(0, 0, 1, 1, 24, 0, &[1, 2, 3, 0]);
        let shape = color_pointer(0, 1, &[255, 255, 255, 0], &[0, 0]);
        for end in 0..update.len() {
            assert!(graphics.bitmap_update(&update[..end]).is_err());
            assert_eq!(graphics.pixels(), &[0, 0, 0, 255]);
            assert_eq!(graphics.take_damage(), None);
        }
        let mut multiple = update.clone();
        multiple[2..4].copy_from_slice(&2u16.to_le_bytes());
        multiple.extend_from_slice(&update[4..update.len() - 1]);
        assert!(graphics.bitmap_update(&multiple).is_err());
        assert_eq!(graphics.pixels(), &[0, 0, 0, 255]);
        assert_eq!(graphics.take_damage(), None);
        for end in 0..shape.len() {
            assert!(
                graphics
                    .pointer_update(PointerKind::Color, &shape[..end])
                    .is_err()
            );
            assert_eq!(graphics.pixels(), &[0, 0, 0, 255]);
            assert_eq!(graphics.take_damage(), None);
        }
        assert!(
            graphics
                .pointer_update(PointerKind::Cached, &[1, 0])
                .is_err()
        );
        assert!(graphics.resize(0, 1).is_err());
        assert_eq!((graphics.width(), graphics.height()), (1, 1));
        assert_eq!(graphics.take_damage(), None);
    }
}
