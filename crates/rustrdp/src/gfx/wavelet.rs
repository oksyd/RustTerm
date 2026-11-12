//! Integer transforms shared by the RemoteFX and progressive bitmap codecs.

use crate::{Error, Result, wire::Reader};
#[cfg(test)]
mod tests;

pub(super) type Quant = [u8; 10];
pub(super) const QUANT_ORDER: [usize; 10] = [8, 7, 9, 5, 4, 6, 2, 1, 3, 0];

pub(super) fn quant(reader: &mut Reader<'_>) -> Result<Quant> {
    let mut values = [0; 10];
    for pair in values.as_chunks_mut::<2>().0 {
        let byte = reader.u8()?;
        pair[0] = byte & 15;
        pair[1] = byte >> 4;
    }
    Ok(values)
}

pub(super) fn bands(extrapolate: bool) -> [(usize, usize); 10] {
    let sizes = if extrapolate {
        [1023, 1023, 961, 272, 272, 256, 72, 72, 64, 81]
    } else {
        [1024, 1024, 1024, 256, 256, 256, 64, 64, 64, 64]
    };
    let mut offset = 0;
    sizes.map(|length| {
        let band = (offset, length);
        offset += length;
        band
    })
}

pub(super) struct Bits<'a> {
    data: &'a [u8],
    position: usize,
}
impl<'a> Bits<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }
    pub fn read(&mut self, count: u32) -> Result<u32> {
        if count > 31 || count as usize > self.data.len() * 8 - self.position {
            return Err(Error::Truncated);
        }
        let mut value = 0;
        for _ in 0..count {
            value = (value << 1)
                | u32::from((self.data[self.position / 8] >> (7 - self.position % 8)) & 1);
            self.position += 1;
        }
        Ok(value)
    }
    pub fn remaining(&self) -> usize {
        self.data.len() * 8 - self.position
    }
}

fn rice(bits: &mut Bits<'_>, parameter: &mut u32) -> Result<u32> {
    let k = *parameter >> 3;
    let mut quotient = 0u32;
    while bits.read(1)? == 1 {
        quotient += 1;
        if quotient > 65535 {
            return Err(Error::Invalid("RemoteFX Rice code"));
        }
    }
    let value = (quotient << k) | bits.read(k)?;
    *parameter = if quotient == 0 {
        parameter.saturating_sub(2)
    } else if quotient > 1 {
        (*parameter + quotient).min(80)
    } else {
        *parameter
    };
    if value > 65535 {
        return Err(Error::Invalid("RemoteFX coefficient"));
    }
    Ok(value)
}

fn signed(value: u32) -> i32 {
    if value & 1 == 0 {
        (value / 2) as i32
    } else {
        -(value.div_ceil(2) as i32)
    }
}

/// MS-RDPRFX 3.1.8.1.7, RLGR1 and RLGR3 with bounded output.
pub(super) fn rlgr(data: &[u8], pair_mode: bool) -> Result<Vec<i32>> {
    rlgr_values(data, pair_mode, 4096)
}

fn rlgr_values(data: &[u8], pair_mode: bool, count: usize) -> Result<Vec<i32>> {
    if data.is_empty() {
        return Err(Error::Truncated);
    }
    let mut bits = Bits::new(data);
    let mut output = vec![0; count];
    let mut index = 0usize;
    let mut run_parameter = 8u32;
    let mut rice_parameter = 8u32;
    while index < output.len() {
        let mut k = run_parameter >> 3;
        if k != 0 {
            let mut zeros = 0usize;
            loop {
                if bits.read(1)? == 1 {
                    break;
                }
                zeros += 1usize << k;
                run_parameter = (run_parameter + 4).min(80);
                k = run_parameter >> 3;
                // A final all-zero run need not encode a nonzero coefficient.
                if zeros >= output.len() - index {
                    return Ok(output);
                }
            }
            zeros += bits.read(k)? as usize;
            index = index.saturating_add(zeros).min(output.len());
            if index == output.len() {
                break;
            }
            let negative = bits.read(1)? != 0;
            let magnitude = rice(&mut bits, &mut rice_parameter)? + 1;
            if magnitude > 32768 {
                return Err(Error::Invalid("RemoteFX run coefficient"));
            }
            output[index] = if negative {
                -(magnitude as i32)
            } else {
                magnitude as i32
            };
            index += 1;
            run_parameter = run_parameter.saturating_sub(6);
        } else {
            let value = rice(&mut bits, &mut rice_parameter)?;
            if pair_mode {
                let first = bits.read(32 - value.leading_zeros())?;
                let second = value
                    .checked_sub(first)
                    .ok_or(Error::Invalid("RemoteFX coefficient pair"))?;
                run_parameter = if first == 0 && second == 0 {
                    (run_parameter + 6).min(80)
                } else if first != 0 && second != 0 {
                    run_parameter.saturating_sub(6)
                } else {
                    run_parameter
                };
                for value in [first, second] {
                    if index < output.len() {
                        output[index] = signed(value);
                        index += 1;
                    }
                }
            } else {
                run_parameter = if value == 0 {
                    (run_parameter + 3).min(80)
                } else {
                    run_parameter.saturating_sub(3)
                };
                output[index] = signed(value);
                index += 1;
            }
        }
    }
    Ok(output)
}

pub(super) fn differential(values: &mut [i32], extrapolate: bool) -> Result<()> {
    let (start, length) = bands(extrapolate)[9];
    for index in start + 1..start + length {
        values[index] = values[index]
            .checked_add(values[index - 1])
            .ok_or(Error::Invalid("RemoteFX LL differential"))?;
    }
    Ok(())
}

pub(super) fn scale(value: i32, shift: u8) -> Result<i32> {
    let scaled = i64::from(value)
        .checked_shl(u32::from(shift))
        .ok_or(Error::Invalid("RemoteFX quantization shift"))?;
    i32::try_from(scaled).map_err(|_| Error::Invalid("RemoteFX quantized coefficient"))
}

pub(super) fn dequantize(values: &mut [i32], quant: Quant, extrapolate: bool) -> Result<()> {
    for (band, (start, length)) in bands(extrapolate).into_iter().enumerate() {
        let shift = quant[QUANT_ORDER[band]]
            .checked_sub(1)
            .ok_or(Error::Invalid("RemoteFX quantization"))?;
        for value in &mut values[start..start + length] {
            *value = scale(*value, shift)?;
        }
    }
    Ok(())
}

fn narrow(value: i64) -> i32 {
    value.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

fn synthesize(low: &[i32], high: &[i32], reduced: bool, output: &mut [i32]) {
    let half = |value: i64| if reduced { value / 2 } else { (value + 1) >> 1 };
    let even_count = output.len().div_ceil(2);
    for index in 0..even_count {
        let left = high[index.saturating_sub(1)];
        let right = high.get(index).copied().unwrap_or_else(|| {
            if low.len() > high.len() + 1 {
                0
            } else {
                *high.last().unwrap()
            }
        });
        output[index * 2] =
            narrow(i64::from(low[index]) - half(i64::from(left) + i64::from(right)));
    }
    for index in 0..output.len() / 2 {
        let left = i64::from(output[index * 2]);
        if index == high.len() {
            output[index * 2 + 1] = narrow((left + i64::from(low[index + 1])) / 2);
        } else {
            let right = i64::from(
                output
                    .get(index * 2 + 2)
                    .copied()
                    .unwrap_or(output[index * 2]),
            );
            let middle = if reduced {
                (left + right) / 2
            } else {
                (left + right) >> 1
            };
            output[index * 2 + 1] = narrow(middle + 2 * i64::from(high[index]));
        }
    }
}

pub(super) fn inverse(values: &mut [i32], extrapolate: bool) {
    let stages = if extrapolate {
        [(3807, 9, 8), (3007, 17, 16), (0, 33, 31)]
    } else {
        [(3840, 8, 8), (3072, 16, 16), (0, 32, 32)]
    };
    let mut intermediate = vec![0; 4096];
    for (start, low, high) in stages {
        let size = low + high;
        let source = &values[start..start + size * size];
        let lh = low * high;
        let hh = lh * 2;
        let ll = hh + high * high;
        for y in 0..size {
            let (low_start, high_start) = if y < low {
                (ll + y * low, y * high)
            } else {
                ((y - low) * low + lh, (y - low) * high + hh)
            };
            synthesize(
                &source[low_start..low_start + low],
                &source[high_start..high_start + high],
                extrapolate,
                &mut intermediate[y * size..(y + 1) * size],
            );
        }
        synthesize_vertical(
            &intermediate[..size * size],
            low,
            high,
            extrapolate,
            &mut values[start..start + size * size],
        );
    }
}

// Apply the vertical lifting steps across contiguous rows. Each lane has the
// same arithmetic as synthesize(), without gathering and scattering columns.
fn synthesize_vertical(input: &[i32], low: usize, high: usize, reduced: bool, output: &mut [i32]) {
    let size = low + high;
    let row = |y: usize| &input[y * size..(y + 1) * size];
    let zero = [0; 64];
    for y in 0..size.div_ceil(2) {
        let left = row(low + y.saturating_sub(1));
        let right = if y < high {
            row(low + y)
        } else if low > high + 1 {
            &zero[..size]
        } else {
            row(low + high - 1)
        };
        let even = &mut output[2 * y * size..(2 * y + 1) * size];
        for (((target, value), left), right) in even.iter_mut().zip(row(y)).zip(left).zip(right) {
            let sum = i64::from(*left) + i64::from(*right);
            let half = if reduced { sum / 2 } else { (sum + 1) >> 1 };
            *target = narrow(i64::from(*value) - half);
        }
    }
    for y in 0..size / 2 {
        let (before, rest) = output.split_at_mut((2 * y + 1) * size);
        let left = &before[before.len() - size..];
        let (odd, after) = rest.split_at_mut(size);
        if y == high {
            for ((target, left), extra) in odd.iter_mut().zip(left).zip(row(y + 1)) {
                *target = narrow((i64::from(*left) + i64::from(*extra)) / 2);
            }
        } else {
            let right = if after.is_empty() {
                left
            } else {
                &after[..size]
            };
            for (((target, left), right), detail) in
                odd.iter_mut().zip(left).zip(right).zip(row(low + y))
            {
                let sum = i64::from(*left) + i64::from(*right);
                let middle = if reduced { sum / 2 } else { sum >> 1 };
                *target = narrow(middle + 2 * i64::from(*detail));
            }
        }
    }
}

pub(super) fn colors(components: &[Vec<i32>; 3]) -> Vec<u8> {
    let mut pixels = vec![0; 4096 * 4];
    for (index, pixel) in pixels.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let y = (i64::from(components[0][index]) + 4096) * 65536;
        let cb = i64::from(components[1][index]);
        let cr = i64::from(components[2][index]);
        for (target, value) in
            pixel
                .iter_mut()
                .zip([y + 116130 * cb, y - 22554 * cb - 46802 * cr, y + 91916 * cr])
        {
            *target = (value >> 21).clamp(0, 255) as u8;
        }
        pixel[3] = 255;
    }
    pixels
}

pub(super) fn block<'a>(reader: &mut Reader<'a>) -> Result<(u16, Reader<'a>)> {
    let kind = reader.u16_le()?;
    let length = reader.u32_le()? as usize;
    if length < 6 {
        return Err(Error::Invalid("RemoteFX block length"));
    }
    Ok((kind, Reader::new(reader.take(length - 6)?)))
}
