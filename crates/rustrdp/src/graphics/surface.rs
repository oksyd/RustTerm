use crate::{Error, Limits, Result, wire::Reader};

use super::{Graphics, SurfaceCallback, bitmap, nscodec};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FrameMarker {
    Begin(u32),
    End(u32),
}

enum Command {
    Bits(bitmap::Bitmap),
    Marker(FrameMarker),
}

impl Graphics {
    /// Validates and decodes the entire update before committing any pixels.
    pub(crate) fn surface_update(
        &mut self,
        data: &[u8],
        supported: u32,
        nscodec: bool,
        on_marker: &mut Option<&mut SurfaceCallback<'_>>,
    ) -> Result<bool> {
        let commands = decode(data, self.limits, &self.palette, supported, nscodec)?;
        let mut frame = self.frame;
        for command in &commands {
            match command {
                Command::Marker(FrameMarker::Begin(id)) => {
                    if frame.replace(*id).is_some() {
                        return Err(Error::Invalid("overlapping graphics frames"));
                    }
                }
                Command::Marker(FrameMarker::End(id)) => {
                    if frame.take() != Some(*id) {
                        return Err(Error::Invalid("graphics frame identifier"));
                    }
                }
                Command::Bits(_) => {}
            }
        }
        let mut changed = false;
        for command in commands {
            match command {
                Command::Bits(image) => changed |= self.apply_bitmaps([image]),
                Command::Marker(marker) => {
                    match marker {
                        FrameMarker::Begin(id) => self.frame = Some(id),
                        FrameMarker::End(_) => self.frame = None,
                    }
                    if let Some(callback) = on_marker.as_deref_mut() {
                        callback(marker, self)?;
                    }
                }
            }
        }
        Ok(changed)
    }
}

fn decode(
    data: &[u8],
    limits: Limits,
    palette: &[[u8; 4]; 256],
    supported: u32,
    nscodec: bool,
) -> Result<Vec<Command>> {
    if supported == 0 {
        return Err(Error::Unsupported("unnegotiated surface commands"));
    }
    if data.len() > limits.max_pdu {
        return Err(Error::Limit("surface update"));
    }
    let mut input = Reader::new(data);
    let mut commands = Vec::new();
    let mut decoded_bytes = 0usize;
    while !input.is_empty() {
        let kind = input.u16_le()?;
        let required = match kind {
            1 => 0x02,
            4 => 0x10,
            6 => 0x40,
            _ => return Err(Error::Unsupported("surface command type")),
        };
        if supported & required == 0 {
            return Err(Error::Unsupported("unnegotiated surface command"));
        }
        if kind == 4 {
            let action = input.u16_le()?;
            let id = input.u32_le()?;
            commands.push(Command::Marker(match action {
                0 => FrameMarker::Begin(id),
                1 => FrameMarker::End(id),
                _ => return Err(Error::Invalid("graphics frame action")),
            }));
            continue;
        }
        let left = usize::from(input.u16_le()?);
        let top = usize::from(input.u16_le()?);
        let right = usize::from(input.u16_le()?);
        let bottom = usize::from(input.u16_le()?);
        let depth = u16::from(input.u8()?);
        let flags = input.u8()?;
        input.skip(1)?;
        let codec = input.u8()?;
        let width = input.u16_le()?;
        let height = input.u16_le()?;
        let length = input.u32_le()? as usize;
        if flags & !1 != 0 {
            return Err(Error::Invalid("surface bitmap flags"));
        }
        let bytes = limits.frame_bytes(width, height)?;
        decoded_bytes = decoded_bytes
            .checked_add(bytes)
            .filter(|bytes| *bytes <= limits.max_bitmap_bytes)
            .ok_or(Error::Limit("surface bitmap update size"))?;
        if flags & 1 != 0 {
            // TS_COMPRESSED_BITMAP_HEADER_EX contains only identifiers/timestamps.
            input.skip(24)?;
        }
        let data = input.take(length)?;
        let width = usize::from(width);
        let height = usize::from(height);
        let (visible_width, visible_height) = if kind == 6 {
            if right <= left || bottom <= top {
                return Err(Error::Invalid("surface destination rectangle"));
            }
            ((right - left).min(width), (bottom - top).min(height))
        } else {
            // SET_SURFACE_BITS uses the encoded dimensions, not the right/bottom hints.
            (width, height)
        };
        let (pixels, indices) = match codec {
            0 => raw(data, width, height, depth, palette)?,
            1 if nscodec && depth == 32 => (nscodec::decode(data, width, height)?, None),
            1 if !nscodec => return Err(Error::Unsupported("unnegotiated NSCodec")),
            1 => return Err(Error::Invalid("NSCodec color depth")),
            _ => return Err(Error::Unsupported("surface bitmap codec")),
        };
        commands.push(Command::Bits(bitmap::Bitmap {
            left,
            top,
            visible_width,
            visible_height,
            width,
            pixels,
            indices,
        }));
    }
    Ok(commands)
}

fn raw(
    data: &[u8],
    width: usize,
    height: usize,
    depth: u16,
    palette: &[[u8; 4]; 256],
) -> Result<(Vec<u8>, Option<Vec<u8>>)> {
    if !matches!(depth, 8 | 15 | 16 | 24 | 32) {
        return Err(Error::Unsupported("surface bitmap color depth"));
    }
    let pixel_bytes = usize::from(depth.div_ceil(8));
    let stride = width * pixel_bytes;
    if data.len() != stride * height {
        return Err(Error::Invalid("raw surface bitmap length"));
    }
    let mut pixels = Vec::with_capacity(width * height * 4);
    if depth == 32 {
        for row in data.chunks_exact(stride).rev() {
            pixels.extend_from_slice(row);
        }
        for alpha in pixels.iter_mut().skip(3).step_by(4) {
            *alpha = 255;
        }
        return Ok((pixels, None));
    }
    let mut indices = (depth == 8).then(|| Vec::with_capacity(width * height));
    // Unencoded surface rows are packed and bottom-up, without four-byte padding.
    for row in data.chunks_exact(stride).rev() {
        for pixel in row.chunks_exact(pixel_bytes) {
            let value = pixel.iter().enumerate().fold(0, |value, (index, byte)| {
                value | (u32::from(*byte) << (index * 8))
            });
            pixels.extend_from_slice(&bitmap::color(value, depth, palette));
            if let Some(indices) = &mut indices {
                indices.push(value as u8);
            }
        }
    }
    Ok((pixels, indices))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphics::Damage;

    fn bits(kind: u16, x: u16, y: u16, width: u16, height: u16, pixels: &[u8]) -> Vec<u8> {
        let mut data = Vec::new();
        for value in [kind, x, y, x + width, y + height] {
            data.extend_from_slice(&value.to_le_bytes());
        }
        data.extend_from_slice(&[32, 0, 0, 0]);
        data.extend_from_slice(&width.to_le_bytes());
        data.extend_from_slice(&height.to_le_bytes());
        data.extend_from_slice(&(pixels.len() as u32).to_le_bytes());
        data.extend_from_slice(pixels);
        data
    }

    fn marker(action: u16, id: u32) -> Vec<u8> {
        let mut data = 4u16.to_le_bytes().to_vec();
        data.extend_from_slice(&action.to_le_bytes());
        data.extend_from_slice(&id.to_le_bytes());
        data
    }

    #[test]
    fn raw_surfaces_handle_orientation_clipping_and_optional_header() {
        let mut graphics = Graphics::new(2, 2, Limits::default()).unwrap();
        graphics.take_damage();
        let mut data = bits(1, 1, 0, 1, 2, &[1, 2, 3, 0, 4, 5, 6, 0]);
        data[11] = 1;
        data.splice(22..22, [0; 24]);
        assert!(
            graphics
                .surface_update(&data, 0x52, false, &mut None)
                .unwrap()
        );
        assert_eq!(&graphics.pixels()[4..8], &[4, 5, 6, 255]);
        assert_eq!(&graphics.pixels()[12..16], &[1, 2, 3, 255]);
        assert_eq!(
            graphics.take_damage(),
            Some(Damage {
                x: 1,
                y: 0,
                width: 1,
                height: 2
            })
        );
        let mut clipped = bits(6, 0, 0, 2, 1, &[9; 8]);
        clipped[6..8].copy_from_slice(&1u16.to_le_bytes());
        graphics
            .surface_update(&clipped, 0x52, false, &mut None)
            .unwrap();
        assert_eq!(
            graphics.take_damage(),
            Some(Damage {
                x: 0,
                y: 0,
                width: 1,
                height: 1
            })
        );
    }

    #[test]
    fn complete_frames_are_observed_before_the_next_frame_changes_pixels() {
        let mut graphics = Graphics::new(1, 1, Limits::default()).unwrap();
        graphics.take_damage();
        let mut data = marker(0, 7);
        data.extend(bits(1, 0, 0, 1, 1, &[1, 2, 3, 0]));
        data.extend(marker(1, 7));
        data.extend(marker(0, 8));
        data.extend(bits(1, 0, 0, 1, 1, &[4, 5, 6, 0]));
        let mut seen = Vec::new();
        let mut markers = Vec::new();
        let mut callback = |marker, graphics: &mut Graphics| {
            markers.push(marker);
            if let FrameMarker::End(id) = marker {
                let damage = graphics.take_damage();
                seen.push((id, graphics.pixels().to_vec(), damage));
            }
            Ok(())
        };
        graphics
            .surface_update(&data, 0x52, false, &mut Some(&mut callback))
            .unwrap();
        assert_eq!(
            markers,
            [
                FrameMarker::Begin(7),
                FrameMarker::End(7),
                FrameMarker::Begin(8)
            ]
        );
        assert_eq!(
            seen,
            [(
                7,
                vec![1, 2, 3, 255],
                Some(Damage {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1
                })
            )]
        );
        assert!(graphics.frame_in_progress());
        assert_eq!(graphics.pixels(), [4, 5, 6, 255]);
        assert!(graphics.take_damage().is_some());
        graphics
            .surface_update(&marker(1, 8), 0x52, false, &mut None)
            .unwrap();
        assert!(!graphics.frame_in_progress());
    }

    #[test]
    fn malformed_or_unnegotiated_surfaces_are_transactional() {
        let mut graphics = Graphics::new(1, 1, Limits::default()).unwrap();
        graphics.take_damage();
        let valid = bits(1, 0, 0, 1, 1, &[1; 4]);
        for end in 1..valid.len() {
            assert!(
                graphics
                    .surface_update(&valid[..end], 0x52, false, &mut None)
                    .is_err()
            );
            assert_eq!(graphics.pixels(), [0, 0, 0, 255]);
            assert_eq!(graphics.take_damage(), None);
        }
        assert!(
            graphics
                .surface_update(&valid, 0x10, false, &mut None)
                .is_err()
        );
        let mut invalid = valid.clone();
        invalid.extend(marker(1, 99));
        assert!(
            graphics
                .surface_update(&invalid, 0x52, false, &mut None)
                .is_err()
        );
        assert_eq!(graphics.pixels(), [0, 0, 0, 255]);
        assert_eq!(graphics.take_damage(), None);
        let mut unknown = valid;
        unknown[13] = 1;
        assert!(
            graphics
                .surface_update(&unknown, 0x52, false, &mut None)
                .is_err()
        );
    }

    #[test]
    fn nscodec_window_moves_repaint_the_old_location_across_frames_and_resize() {
        let mut graphics = Graphics::new(8, 4, Limits::default()).unwrap();
        let mut encoded = Vec::new();
        for count in [4u32, 4, 4, 0] {
            encoded.extend(count.to_le_bytes());
        }
        // Two distinct rows, bottom-up; neutral chroma and implicit opaque alpha.
        encoded.extend([1, 0, 0, 0, 80, 80, 200, 200]);
        encoded.extend([0; 8]);
        for (width, height) in [(8u16, 4u16), (12, 6)] {
            graphics.resize(width, height).unwrap();
            let mut previous = None;
            for (id, x) in [0, 2, width - 2].into_iter().enumerate() {
                let mut update = marker(0, id as u32);
                if let Some(old_x) = previous {
                    update.extend(bits(1, old_x, 1, 2, 2, &[0; 16]));
                }
                let mut window = bits(1, x, 1, 2, 2, &encoded);
                window[13] = 1; // Negotiated NSCodec.
                update.extend(window);
                graphics
                    .surface_update(&update, 0x52, true, &mut None)
                    .unwrap();
                assert!(graphics.frame_in_progress());

                let mut completed = None;
                graphics
                    .surface_update(
                        &marker(1, id as u32),
                        0x52,
                        true,
                        &mut Some(&mut |event, frame| {
                            assert_eq!(event, FrameMarker::End(id as u32));
                            completed = Some(frame.pixels().to_vec());
                            Ok(())
                        }),
                    )
                    .unwrap();
                let mut expected = vec![0; usize::from(width) * usize::from(height) * 4];
                for pixel in expected.as_chunks_mut::<4>().0 {
                    pixel[3] = 255;
                }
                for (y, shade) in [(1, 200), (2, 80)] {
                    for column in x..x + 2 {
                        let offset = (y * usize::from(width) + usize::from(column)) * 4;
                        expected[offset..offset + 4].copy_from_slice(&[shade, shade, shade, 255]);
                    }
                }
                assert_eq!(completed.unwrap(), expected);
                assert!(!graphics.frame_in_progress());
                previous = Some(x);
            }
        }
    }

    #[test]
    fn aggregate_surface_allocations_obey_the_update_budget() {
        let limits = Limits {
            max_bitmap_bytes: 4,
            ..Limits::default()
        };
        let mut graphics = Graphics::new(1, 1, limits).unwrap();
        graphics.take_damage();
        let mut data = bits(1, 0, 0, 1, 1, &[1; 4]);
        data.extend(bits(1, 0, 0, 1, 1, &[2; 4]));
        assert_eq!(
            graphics.surface_update(&data, 0x52, false, &mut None),
            Err(Error::Limit("surface bitmap update size"))
        );
        assert_eq!(graphics.pixels(), [0, 0, 0, 255]);
        assert_eq!(graphics.take_damage(), None);
    }
}
