use super::*;

#[test]
fn microsoft_remotefx_tile_matches_the_decoded_reference() {
    // MS-RDPRFX 4.2.2/4.2.4 sample vectors, also carried by FreeRDP's
    // Apache-2.0 TestFreeRDPCodecRemoteFX fixture. No implementation is reused.
    let encoded = include_bytes!("../fixtures/rfx.bin");
    let expected = include_bytes!("../fixtures/rfx.bgra");
    let mut decoder = Decoder::new();
    let mut count = 0;
    decoder
        .decode(encoded, |x, y, pixels, regions| {
            assert_eq!((x, y), (0, 0));
            assert_eq!(
                regions,
                [Rect {
                    left: 0,
                    top: 0,
                    right: 64,
                    bottom: 64
                }]
            );
            assert_eq!(pixels.len(), expected.len());
            for (index, (actual, expected)) in pixels.iter().zip(expected).enumerate() {
                assert!(
                    actual.abs_diff(*expected) <= 1,
                    "component {index}: {actual} != {expected}"
                );
            }
            count += 1;
            Ok(())
        })
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn data_before_headers_and_inconsistent_tileset_codec_are_rejected() {
    let encoded = include_bytes!("../fixtures/rfx.bin");
    assert!(
        Decoder::new()
            .decode(&encoded[47..], |_, _, _, _| Ok(()))
            .is_err()
    );
    let mut invalid = encoded.to_vec();
    let position = invalid
        .windows(4)
        .position(|bytes| bytes == [0x51, 0x50, 1, 64])
        .unwrap();
    invalid[position + 1] = 0x44;
    assert!(
        Decoder::new()
            .decode(&invalid, |_, _, _, _| Ok(()))
            .is_err()
    );
}
