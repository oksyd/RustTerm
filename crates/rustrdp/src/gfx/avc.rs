use openh264::{
    OpenH264API,
    decoder::{Decoder, DecoderConfig, Flush},
    formats::YUVSource,
};

use super::Rect;
use crate::{Error, Limits, Result, wire::Reader};
#[cfg(test)]
mod tests;

pub(super) struct Avc {
    decoder: Decoder,
    width: usize,
    height: usize,
    limits: Limits,
    planes: Option<[Vec<u8>; 3]>,
    audit: (u64, u64),
}

struct Stream<'a> {
    regions: Vec<Rect>,
    bytes: &'a [u8],
}

impl<'a> Stream<'a> {
    fn parse(data: &'a [u8], width: u16, height: u16) -> Result<Self> {
        let mut input = Reader::new(data);
        let count = input.u32_le()? as usize;
        if count > input.remaining_len() / 10 {
            return Err(Error::Truncated);
        }
        let mut regions = Vec::with_capacity(count);
        let mut pixel_work = 0usize;
        let work_limit =
            usize::from(width).div_ceil(16) * usize::from(height).div_ceil(16) * 256 * 4;
        for _ in 0..count {
            let rect = Rect::read(&mut input)?.checked(width, height)?;
            if rect.width() == 0 || rect.height() == 0 {
                continue;
            }
            // Reconstruction visits complete macroblocks. Bound overlapping
            // region amplification as well as the decoded picture allocation.
            let columns = usize::from(rect.right).div_ceil(16) - usize::from(rect.left) / 16;
            let rows = usize::from(rect.bottom).div_ceil(16) - usize::from(rect.top) / 16;
            pixel_work = pixel_work.saturating_add(columns * rows * 256);
            if pixel_work > work_limit {
                return Err(Error::Limit("H.264 region reconstruction"));
            }
            regions.push(rect);
        }
        input.skip(count * 2)?;
        Ok(Self {
            regions,
            bytes: input.remaining(),
        })
    }
}

impl Avc {
    pub(super) fn new(width: u16, height: u16, limits: Limits) -> Result<Self> {
        let decoder = Decoder::with_api_config(
            OpenH264API::from_source(),
            // Each RDP bitstream comes with rectangles for that exact picture.
            // OpenH264 buffers Main/High pictures even for I/P-only streams;
            // retrieve them now so the next update cannot receive an older
            // picture (or the other AVC444 view) with the wrong rectangles.
            DecoderConfig::new().flush_after_decode(Flush::Flush),
        )
        .map_err(|_| Error::Unsupported("H.264 decoder initialization"))?;
        Ok(Self {
            decoder,
            width: usize::from(width).div_ceil(16) * 16,
            height: usize::from(height).div_ceil(16) * 16,
            limits,
            planes: None,
            audit: (0, 0),
        })
    }

    pub(super) fn take_audit(&mut self) -> (u64, u64) {
        std::mem::take(&mut self.audit)
    }

    pub(super) fn decode(
        &mut self,
        codec: u16,
        data: &[u8],
        pixels: &mut [u8],
        width: u16,
        height: u16,
    ) -> Result<Vec<Rect>> {
        if codec == 11 {
            let stream = Stream::parse(data, width, height)?;
            self.process(&stream, 0, pixels, width)?;
            return Ok(stream.regions);
        }
        let mut input = Reader::new(data);
        let flags = input.u32_le()?;
        let operation = flags >> 30;
        if operation == 3 {
            return Err(Error::Invalid("AVC444 operation"));
        }
        let length = (flags & 0x3fff_ffff) as usize;
        let first = if operation == 0 {
            input.take(length)?
        } else {
            input.remaining()
        };
        let first = Stream::parse(first, width, height)?;
        let auxiliary = if operation == 0 {
            Some(Stream::parse(input.remaining(), width, height)?)
        } else {
            None
        };
        let chroma = if codec == 14 { 2 } else { 3 };
        if self.planes.is_none() {
            self.planes = Some([
                vec![0; self.width * self.height],
                vec![128; self.width * self.height],
                vec![128; self.width * self.height],
            ]);
        }
        self.process(
            &first,
            if operation == 2 { chroma } else { 1 },
            pixels,
            width,
        )?;
        let mut regions = first.regions;
        if let Some(auxiliary) = auxiliary {
            self.process(&auxiliary, chroma, pixels, width)?;
            regions.extend(auxiliary.regions);
        }
        Ok(regions)
    }

    fn process(
        &mut self,
        stream: &Stream<'_>,
        operation: u8,
        pixels: &mut [u8],
        width: u16,
    ) -> Result<()> {
        validate_parameter_sets(stream.bytes, self.limits)?;
        self.audit.0 = self.audit.0.saturating_add(1);
        let Some(frame) = self
            .decoder
            .decode(stream.bytes)
            .map_err(|_| Error::Invalid("H.264 bitstream"))?
        else {
            self.audit.1 = self.audit.1.saturating_add(1);
            return Ok(());
        };
        let (decoded_width, decoded_height) = frame.dimensions();
        validate_canvas(decoded_width as u64, decoded_height as u64, self.limits)?;
        for rect in &stream.regions {
            if usize::from(rect.right) > decoded_width || usize::from(rect.bottom) > decoded_height
            {
                return Err(Error::Invalid("H.264 region bounds"));
            }
        }
        if operation == 0 {
            let (ys, us, vs) = frame.strides();
            for rect in &stream.regions {
                for y in usize::from(rect.top)..usize::from(rect.bottom) {
                    for x in usize::from(rect.left)..usize::from(rect.right) {
                        let color = to_bgra(
                            frame.y()[y * ys + x],
                            frame.u()[(y / 2) * us + x / 2],
                            frame.v()[(y / 2) * vs + x / 2],
                        );
                        let offset = (y * usize::from(width) + x) * 4;
                        pixels[offset..offset + 4].copy_from_slice(&color);
                    }
                }
            }
        } else {
            let planes = self
                .planes
                .as_mut()
                .ok_or(Error::Invalid("AVC444 reconstruction state"))?;
            for rect in &stream.regions {
                combine(planes, self.width, &frame, *rect, operation)?;
                for y in usize::from(rect.top)..usize::from(rect.bottom) {
                    for x in usize::from(rect.left)..usize::from(rect.right) {
                        let offset = y * self.width + x;
                        let (u, v) = if operation >= 2 && x % 2 == 0 && y % 2 == 0 {
                            let restore = |plane: &[u8]| {
                                (4 * i32::from(plane[offset])
                                    - i32::from(plane[offset + 1])
                                    - i32::from(plane[offset + self.width])
                                    - i32::from(plane[offset + self.width + 1]))
                                .clamp(0, 255) as u8
                            };
                            (restore(&planes[1]), restore(&planes[2]))
                        } else {
                            (planes[1][offset], planes[2][offset])
                        };
                        let destination = (y * usize::from(width) + x) * 4;
                        pixels[destination..destination + 4].copy_from_slice(&to_bgra(
                            planes[0][offset],
                            u,
                            v,
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}

fn combine(
    planes: &mut [Vec<u8>; 3],
    stride: usize,
    frame: &impl YUVSource,
    rect: Rect,
    operation: u8,
) -> Result<()> {
    let (width, height) = frame.dimensions();
    let (ys, us, vs) = frame.strides();
    if width % 16 != 0 || height % 16 != 0 {
        return Err(Error::Invalid("AVC444 macroblock alignment"));
    }
    let left = usize::from(rect.left) / 16 * 16;
    let top = usize::from(rect.top) / 16 * 16;
    let right = usize::from(rect.right).div_ceil(16) * 16;
    let bottom = usize::from(rect.bottom).div_ceil(16) * 16;
    // Each main-view chroma sample belongs to a 2x2 block. Retain samples
    // outside this region's footprint for later chroma-only updates there.
    let chroma_left = usize::from(rect.left) / 2 * 2;
    let chroma_top = usize::from(rect.top) / 2 * 2;
    let chroma_right = usize::from(rect.right).div_ceil(2) * 2;
    let chroma_bottom = usize::from(rect.bottom).div_ceil(2) * 2;
    for y in top..bottom {
        for x in left..right {
            let index = y * stride + x;
            if operation == 1 {
                // Keep the last main-view luma for each declared region.
                // Chroma reconstruction visits whole macroblocks, but an
                // adjacent region can still refer to an older main view.
                if x >= usize::from(rect.left)
                    && x < usize::from(rect.right)
                    && y >= usize::from(rect.top)
                    && y < usize::from(rect.bottom)
                {
                    planes[0][index] = frame.y()[y * ys + x];
                }
                if x >= chroma_left && x < chroma_right && y >= chroma_top && y < chroma_bottom {
                    planes[1][index] = frame.u()[(y / 2) * us + x / 2];
                    planes[2][index] = frame.v()[(y / 2) * vs + x / 2];
                }
            } else if operation == 2 {
                if y % 2 == 1 {
                    let row = (y / 16) * 16 + (y % 16) / 2;
                    planes[1][index] = frame.y()[row * ys + x];
                    planes[2][index] = frame.y()[(row + 8) * ys + x];
                } else if x % 2 == 1 {
                    planes[1][index] = frame.u()[(y / 2) * us + x / 2];
                    planes[2][index] = frame.v()[(y / 2) * vs + x / 2];
                }
            } else if x % 2 == 1 {
                planes[1][index] = frame.y()[y * ys + x / 2];
                planes[2][index] = frame.y()[y * ys + width / 2 + x / 2];
            } else if y % 2 == 1 {
                let (plane, step) = if x % 4 == 0 {
                    (frame.u(), us)
                } else {
                    (frame.v(), vs)
                };
                planes[1][index] = plane[(y / 2) * step + x / 4];
                planes[2][index] = plane[(y / 2) * step + width / 4 + x / 4];
            }
        }
    }
    Ok(())
}

fn to_bgra(y: u8, u: u8, v: u8) -> [u8; 4] {
    // MS-RDPEGFX 3.3.8.3.1 specifies full-range BT.709, not video-range BT.601.
    let y = i32::from(y) << 16;
    let u = i32::from(u) - 128;
    let v = i32::from(v) - 128;
    let component = |value: i32| ((value + 32768) >> 16).clamp(0, 255) as u8;
    [
        component(y + 121609 * u),
        component(y - 12277 * u - 30679 * v),
        component(y + 103206 * v),
        255,
    ]
}

// Validate only SPS allocation parameters before handing the stream to the
// maintained codec. Entropy decoding, references and reconstruction remain there.
fn validate_parameter_sets(data: &[u8], limits: Limits) -> Result<()> {
    let mut found = false;
    for nal in openh264::nal_units(data) {
        found = true;
        let header = nal
            .iter()
            .position(|byte| *byte != 0)
            .ok_or(Error::Truncated)?
            + 1;
        let kind = *nal.get(header).ok_or(Error::Truncated)? & 0x1f;
        if kind != 7 {
            continue;
        }
        let mut rbsp = Vec::with_capacity(nal.len());
        let mut zeros = 0;
        for &byte in &nal[header + 1..] {
            if zeros >= 2 && byte == 3 {
                zeros = 0;
                continue;
            }
            rbsp.push(byte);
            zeros = if byte == 0 { zeros + 1 } else { 0 };
        }
        let mut bits = Bits {
            data: &rbsp,
            position: 0,
        };
        let profile = bits.read(8)?;
        bits.read(16)?;
        bits.ue()?;
        if matches!(
            profile,
            100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
        ) {
            if bits.ue()? != 1 || bits.ue()? != 0 || bits.ue()? != 0 {
                return Err(Error::Unsupported("H.264 pixel format"));
            }
            bits.read(1)?;
            if bits.read(1)? != 0 {
                for index in 0..8 {
                    if bits.read(1)? != 0 {
                        let mut last = 8i32;
                        let mut next = 8i32;
                        for _ in 0..if index < 6 { 16 } else { 64 } {
                            if next != 0 {
                                next = ((i64::from(last) + i64::from(bits.se()?)).rem_euclid(256))
                                    as i32;
                            }
                            if next != 0 {
                                last = next;
                            }
                        }
                    }
                }
            }
        }
        bits.ue()?;
        match bits.ue()? {
            0 => {
                bits.ue()?;
            }
            1 => {
                bits.read(1)?;
                bits.se()?;
                bits.se()?;
                let count = bits.ue()?;
                if count > 255 {
                    return Err(Error::Limit("H.264 picture order cycle"));
                }
                for _ in 0..count {
                    bits.se()?;
                }
            }
            2 => {}
            _ => return Err(Error::Invalid("H.264 picture order type")),
        }
        if bits.ue()? > 16 {
            return Err(Error::Limit("H.264 reference pictures"));
        }
        bits.read(1)?;
        let columns = bits
            .ue()?
            .checked_add(1)
            .ok_or(Error::Limit("H.264 width"))?;
        let rows = bits
            .ue()?
            .checked_add(1)
            .ok_or(Error::Limit("H.264 height"))?;
        let frame = bits.read(1)?;
        validate_canvas(
            u64::from(columns) * 16,
            u64::from(rows) * 16 * u64::from(2 - frame),
            limits,
        )?;
    }
    if !found {
        return Err(Error::Invalid("H.264 Annex B stream"));
    }
    Ok(())
}

fn validate_canvas(width: u64, height: u64, limits: Limits) -> Result<()> {
    // Encoders can use a larger aligned canvas than the destination surface.
    // Only metadata regions are reconstructed into the surface-sized planes.
    if width == 0
        || height == 0
        || width > u64::from(limits.max_width)
        || height > u64::from(limits.max_height)
    {
        return Err(Error::Limit("H.264 picture dimensions"));
    }
    if width * height * 4 > limits.max_bitmap_bytes as u64 {
        return Err(Error::Limit("H.264 picture allocation"));
    }
    Ok(())
}

struct Bits<'a> {
    data: &'a [u8],
    position: usize,
}
impl Bits<'_> {
    fn read(&mut self, count: usize) -> Result<u32> {
        if count > 32 || self.position + count > self.data.len() * 8 {
            return Err(Error::Truncated);
        }
        let mut result = 0;
        for _ in 0..count {
            result = (result << 1)
                | u32::from((self.data[self.position / 8] >> (7 - self.position % 8)) & 1);
            self.position += 1;
        }
        Ok(result)
    }
    fn ue(&mut self) -> Result<u32> {
        let mut zeros = 0;
        while self.read(1)? == 0 {
            zeros += 1;
            if zeros > 31 {
                return Err(Error::Limit("H.264 Golomb value"));
            }
        }
        Ok(((1u32 << zeros) - 1) + self.read(zeros)?)
    }
    fn se(&mut self) -> Result<i32> {
        let value = self.ue()?;
        let magnitude =
            i32::try_from(value.div_ceil(2)).map_err(|_| Error::Limit("H.264 signed value"))?;
        Ok(if value & 1 == 0 {
            -magnitude
        } else {
            magnitude
        })
    }
}
