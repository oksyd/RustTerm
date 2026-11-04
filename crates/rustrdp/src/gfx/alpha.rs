use crate::{Error, Result, wire::Reader};

// MS-RDPEGFX 2.2.4.3. The caller has already bounded the destination rectangle.
pub(super) fn decode(data: &[u8], width: u16, height: u16) -> Result<Vec<u8>> {
    let size = usize::from(width) * usize::from(height);
    let mut input = Reader::new(data);
    if input.u16_le()? != 0x414c || size == 0 {
        return Err(Error::Invalid("alpha bitmap header"));
    }
    if input.u16_le()? == 0 {
        let output = input.take(size)?.to_vec();
        input.finish()?;
        return Ok(output);
    }
    let mut output = Vec::new();
    output
        .try_reserve_exact(size)
        .map_err(|_| Error::Limit("alpha bitmap allocation"))?;
    while !input.is_empty() {
        let value = input.u8()?;
        let count = run_length(&mut input)?;
        if count > size - output.len() {
            return Err(Error::Invalid("alpha bitmap run length"));
        }
        output.resize(output.len() + count, value);
    }
    if output.len() != size {
        return Err(Error::Invalid("alpha bitmap size"));
    }
    Ok(output)
}

// Residual, alpha and RLEX runs use the same cascading 8/16/32-bit length.
pub(super) fn run_length(input: &mut Reader<'_>) -> Result<usize> {
    let byte = input.u8()?;
    if byte != 255 {
        return Ok(usize::from(byte));
    }
    let word = input.u16_le()?;
    if word != 65535 {
        return Ok(usize::from(word));
    }
    Ok(input.u32_le()? as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_and_compressed_planes_have_exact_bounds() {
        assert_eq!(
            decode(&[0x4c, 0x41, 0, 0, 0, 64, 128, 255], 2, 2).unwrap(),
            [0, 64, 128, 255]
        );
        assert_eq!(
            decode(&[0x4c, 0x41, 1, 0, 42, 3, 255, 1], 2, 2).unwrap(),
            [42, 42, 42, 255]
        );
        assert_eq!(
            decode(&[0x4c, 0x41, 1, 0, 9, 255, 0, 1], 16, 16).unwrap(),
            vec![9; 256]
        );
        assert!(decode(&[0x4c, 0x41, 1, 0, 42, 5], 2, 2).is_err());
        assert!(decode(&[0x4c, 0x41, 1, 0, 42, 3], 2, 2).is_err());
        assert!(decode(&[0x4c, 0x41, 1, 0, 42, 255, 255], 2, 2).is_err());
    }
}
