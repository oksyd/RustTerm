use super::{CONTENT_MIME_TYPES, MAX_CONTENT_BYTES, ReadError};
use image::{DynamicImage, ImageDecoder, ImageEncoder, RgbaImage};
use rustrdp::ClipboardContent;
use std::{io::Cursor, sync::Arc};

const MAX_PIXELS: u64 = 8 * 1024 * 1024;
pub(crate) type MimeData = Vec<(&'static str, Arc<[u8]>)>;

pub(crate) fn from_mime(formats: Vec<(&str, Vec<u8>)>) -> Result<ClipboardContent, ReadError> {
    let mut content = ClipboardContent::default();
    for (mime, bytes) in formats {
        match mime {
            "text/html" => content.html = Some(utf8(bytes)?),
            "text/rtf" | "application/rtf" if content.rtf.is_none() => content.rtf = Some(bytes),
            "image/png" => {
                let image = decode_png(&bytes)?;
                content.dib = Some(encode_dib(&image));
                content.png = Some(bytes);
            }
            "text/plain;charset=utf-8" | "UTF8_STRING" | "text/plain" if content.text.is_none() => {
                content.text = Some(utf8(bytes)?);
            }
            _ => {}
        }
    }
    if content.len() > MAX_CONTENT_BYTES {
        return Err(ReadError::TooLarge);
    }
    Ok(content)
}

pub(crate) fn to_mime(content: &ClipboardContent) -> Result<MimeData, ReadError> {
    if content.len() > MAX_CONTENT_BYTES {
        return Err(ReadError::TooLarge);
    }
    let image = if let Some(bytes) = content.png.as_ref() {
        Some(decode_png(bytes)?)
    } else if let Some(bytes) = content.dib.as_ref() {
        Some(decode_dib(bytes)?)
    } else {
        None
    };
    let mut formats = Vec::new();
    if let Some(text) = &content.text {
        let bytes: Arc<[u8]> = text.as_bytes().into();
        for mime in &CONTENT_MIME_TYPES[4..] {
            formats.push((*mime, bytes.clone()));
        }
    }
    if let Some(html) = &content.html {
        formats.push(("text/html", html.as_bytes().into()));
    }
    if let Some(rtf) = &content.rtf {
        let bytes: Arc<[u8]> = rtf.as_slice().into();
        formats.push(("text/rtf", bytes.clone()));
        formats.push(("application/rtf", bytes));
    }
    if let Some(image) = image {
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(
                image.as_raw(),
                image.width(),
                image.height(),
                image::ExtendedColorType::Rgba8,
            )
            .map_err(|_| ReadError::Invalid)?;
        if png.len() > MAX_CONTENT_BYTES {
            return Err(ReadError::TooLarge);
        }
        formats.push(("image/png", png.into()));
    }
    if formats.is_empty() {
        return Err(ReadError::Invalid);
    }
    Ok(formats)
}

fn utf8(bytes: Vec<u8>) -> Result<String, ReadError> {
    let text = String::from_utf8(bytes).map_err(|_| ReadError::Invalid)?;
    if text.contains('\0') {
        return Err(ReadError::Invalid);
    }
    Ok(text)
}

fn limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(16384);
    limits.max_image_height = Some(16384);
    limits.max_alloc = Some(MAX_CONTENT_BYTES as u64);
    limits
}

fn decode_png(bytes: &[u8]) -> Result<RgbaImage, ReadError> {
    let decoder = image::codecs::png::PngDecoder::with_limits(Cursor::new(bytes), limits())
        .map_err(|_| ReadError::Invalid)?;
    decode(decoder)
}

fn decode_dib(bytes: &[u8]) -> Result<RgbaImage, ReadError> {
    let word = |offset: usize| -> Result<u32, ReadError> {
        Ok(u32::from_le_bytes(
            bytes
                .get(offset..offset + 4)
                .ok_or(ReadError::Invalid)?
                .try_into()
                .unwrap(),
        ))
    };
    let header = word(0)?;
    if !matches!(header, 40 | 52 | 56 | 108 | 124) || bytes.len() < header as usize {
        return Err(ReadError::Invalid);
    }
    let bit_count = u16::from_le_bytes(bytes[14..16].try_into().unwrap());
    let colors = match (word(32)?, bit_count) {
        (0, 1 | 4 | 8) => 1u32 << bit_count,
        (colors, _) => colors,
    };
    // Only BITMAPINFOHEADER stores BI_BITFIELDS masks after the header.
    // V2 through V5 contain their masks within the header itself.
    let masks = if header == 40 && word(16)? == 3 {
        12
    } else {
        0
    };
    let offset = u64::from(header) + masks + u64::from(colors) * 4;
    if offset >= bytes.len() as u64 {
        return Err(ReadError::Invalid);
    }
    // A file header supplies the exact pixel offset, avoiding the decoder's
    // headerless V4/V5 path incorrectly skipping another three mask DWORDs.
    let mut bmp = Vec::with_capacity(14 + bytes.len());
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&((14 + bytes.len()) as u32).to_le_bytes());
    bmp.extend_from_slice(&[0; 4]);
    bmp.extend_from_slice(&((14 + offset) as u32).to_le_bytes());
    bmp.extend_from_slice(bytes);
    let mut decoder =
        image::codecs::bmp::BmpDecoder::new(Cursor::new(bmp)).map_err(|_| ReadError::Invalid)?;
    decoder
        .set_limits(limits())
        .map_err(|_| ReadError::TooLarge)?;
    decode(decoder)
}

fn decode(decoder: impl ImageDecoder) -> Result<RgbaImage, ReadError> {
    let (width, height) = decoder.dimensions();
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(ReadError::TooLarge);
    }
    DynamicImage::from_decoder(decoder)
        .map(|image| image.into_rgba8())
        .map_err(|_| ReadError::Invalid)
}

fn encode_dib(image: &RgbaImage) -> Vec<u8> {
    // CF_DIBV5 preserves alpha, with explicit BGRA masks and top-down rows.
    let mut dib = vec![0u8; 124];
    for (offset, value) in [
        (0, 124),
        (4, image.width()),
        (8, (-(image.height() as i32)) as u32),
        (16, 3),
        (20, image.as_raw().len() as u32),
        (40, 0x00ff_0000),
        (44, 0x0000_ff00),
        (48, 0x0000_00ff),
        (52, 0xff00_0000),
        (56, 0x7352_4742),
        (108, 4),
    ] {
        dib[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    dib[12..14].copy_from_slice(&1u16.to_le_bytes());
    dib[14..16].copy_from_slice(&32u16.to_le_bytes());
    dib.reserve(image.as_raw().len());
    for pixel in image.pixels() {
        dib.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
    }
    dib
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_to_windows_bitmap_and_back_preserves_rows_and_alpha() {
        let image = RgbaImage::from_raw(
            2,
            2,
            vec![255, 0, 0, 255, 0, 255, 0, 64, 0, 0, 255, 0, 19, 23, 29, 127],
        )
        .unwrap();
        let formats = to_mime(&ClipboardContent {
            dib: Some(encode_dib(&image)),
            ..Default::default()
        })
        .unwrap();
        let png = formats
            .iter()
            .find(|(mime, _)| *mime == "image/png")
            .unwrap();
        assert_eq!(decode_png(&png.1).unwrap(), image);
    }

    #[test]
    fn bitmap_info_masks_and_palette_precede_pixels() {
        let mut dib = vec![0; 40];
        dib[0..4].copy_from_slice(&40u32.to_le_bytes());
        dib[4..8].copy_from_slice(&1u32.to_le_bytes());
        dib[8..12].copy_from_slice(&1u32.to_le_bytes());
        dib[12..14].copy_from_slice(&1u16.to_le_bytes());
        dib[14..16].copy_from_slice(&16u16.to_le_bytes());
        dib[16..20].copy_from_slice(&3u32.to_le_bytes());
        for mask in [0xf800u32, 0x07e0, 0x001f] {
            dib.extend_from_slice(&mask.to_le_bytes());
        }
        dib.extend_from_slice(&[0xe0, 0x07, 0, 0]);
        assert_eq!(decode_dib(&dib).unwrap().as_raw(), &[0, 255, 0, 255]);

        dib.truncate(40);
        dib[4..8].copy_from_slice(&2u32.to_le_bytes());
        dib[14..16].copy_from_slice(&1u16.to_le_bytes());
        dib[16..20].copy_from_slice(&0u32.to_le_bytes());
        dib.extend_from_slice(&[255, 0, 0, 0, 0, 0, 255, 0]);
        dib.extend_from_slice(&[0x40, 0, 0, 0]);
        assert_eq!(
            decode_dib(&dib).unwrap().as_raw(),
            &[0, 0, 255, 255, 255, 0, 0, 255]
        );
        dib[32..36].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(decode_dib(&dib), Err(ReadError::Invalid));
    }

    #[test]
    fn rich_text_keeps_all_representations_and_rejects_invalid_utf8() {
        let content = from_mime(vec![
            ("text/plain", b"hello".to_vec()),
            ("text/html", b"<b>hello</b>".to_vec()),
            ("text/rtf", br"{\rtf1 hello}".to_vec()),
        ])
        .unwrap();
        let mime = to_mime(&content).unwrap();
        assert!(
            mime.iter()
                .any(|(mime, bytes)| *mime == "text/html" && bytes.as_ref() == b"<b>hello</b>")
        );
        assert!(mime.iter().any(|(mime, _)| *mime == "application/rtf"));
        assert_eq!(
            from_mime(vec![("text/html", vec![0xff])]),
            Err(ReadError::Invalid)
        );
    }
}
