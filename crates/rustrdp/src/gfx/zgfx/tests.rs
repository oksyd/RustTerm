use super::*;

fn compressed(bits: &str) -> Vec<u8> {
    let bits: Vec<_> = bits
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect();
    let mut bytes = vec![0xe0, 0x24];
    for chunk in bits.chunks(8) {
        let mut value = 0;
        for (index, bit) in chunk.iter().enumerate() {
            assert!(matches!(bit, b'0' | b'1'));
            value |= (bit - b'0') << (7 - index);
        }
        bytes.push(value);
    }
    bytes.push(((8 - bits.len() % 8) % 8) as u8);
    bytes
}

#[test]
fn microsoft_compression_examples_decode_without_external_codecs() {
    // Published wire fixtures from MS-RDPEGFX 4.2.1.1.1 through 4.2.1.1.4.
    let first = [0xe0, 0x24, 0xce, 0x9b, 0x19, 0x62, 0x18, 0];
    assert_eq!(
        Decoder::new().decode(&first, 8).unwrap(),
        [1, 2, 255, 101, 101, 101, 101, 101]
    );
    let sentence = b"The quick brown fox jumps over the lazy dog";
    let mut raw = vec![0xe0, 4];
    raw.extend(sentence);
    assert_eq!(
        Decoder::new().decode(&raw, sentence.len()).unwrap(),
        sentence
    );
    let overlapping = [0xe0, 0x24, 0x20, 0x90, 0x88, 0x71, 0x1f, 0xb2, 1];
    assert_eq!(
        Decoder::new().decode(&overlapping, 60).unwrap(),
        b"ABC".repeat(20)
    );
    let multipart = [
        0xe1, 3, 0, 0x2b, 0, 0, 0, 0x11, 0, 0, 0, 4, 0x54, 0x68, 0x65, 0x20, 0x71, 0x75, 0x69,
        0x63, 0x6b, 0x20, 0x62, 0x72, 0x6f, 0x77, 0x6e, 0x20, 0x0e, 0, 0, 0, 4, 0x66, 0x6f, 0x78,
        0x20, 0x6a, 0x75, 0x6d, 0x70, 0x73, 0x20, 0x6f, 0x76, 0x65, 0x10, 0, 0, 0, 0x24, 0x39,
        0x08, 0x0e, 0x91, 0xf8, 0xd8, 0x61, 0x3d, 0x1e, 0x44, 0x06, 0x43, 0x79, 0x9c, 2,
    ];
    assert_eq!(
        Decoder::new().decode(&multipart, sentence.len()).unwrap(),
        sentence
    );
}

#[test]
fn raw_runs_resume_at_byte_boundaries_and_populate_later_history() {
    // The raw-run header in MS-RDPEGFX 4.2.1.1.5 declares 1,000 bytes.
    let mut raw = vec![0xe0, 0x24, 0x88, 0x01, 0xf4, 0];
    let expected: Vec<u8> = (0..1000).map(|index| (index % 251) as u8).collect();
    raw.extend(&expected);
    raw.push(0);
    let mut decoder = Decoder::new();
    assert_eq!(decoder.decode(&raw, 1000).unwrap(), expected);
    let distance_three = compressed("10001 00011 0");
    assert_eq!(decoder.decode(&distance_three, 3).unwrap(), expected[997..]);

    let mut raw_then_literal = compressed("10001 00000 000000000000011 0000000");
    raw_then_literal.pop();
    raw_then_literal.extend(b"ABC");
    let literal = compressed("0 01000100");
    raw_then_literal.extend(&literal[2..]);
    assert_eq!(
        Decoder::new().decode(&raw_then_literal, 4).unwrap(),
        b"ABCD"
    );
}

#[test]
fn every_short_literal_prefix_and_reserved_trailer_bits_are_handled() {
    let codes = [
        "11000", "11001", "110100", "110101", "110110", "1101110", "1101111", "1110000", "1110001",
        "1110010", "1110011", "1110100", "1110101", "1110110", "1110111", "1111000", "1111001",
        "1111010", "1111011", "1111100", "1111101", "11111100", "11111101", "11111110", "11111111",
    ];
    let expected = [
        0, 1, 2, 3, 255, 4, 5, 6, 7, 8, 9, 10, 11, 58, 59, 60, 61, 62, 63, 64, 128, 12, 56, 57, 102,
    ];
    let mut packet = compressed(&codes.join(""));
    *packet.last_mut().unwrap() |= 0xf8;
    assert_eq!(
        Decoder::new().decode(&packet, expected.len()).unwrap(),
        expected
    );
}

#[test]
fn overlapping_matches_can_cross_message_boundaries() {
    let mut decoder = Decoder::new();
    decoder.decode(&[0xe0, 4, b'A', b'B', b'C'], 3).unwrap();
    assert_eq!(
        decoder
            .decode(&compressed("10001 00011 110 100"), 12)
            .unwrap(),
        b"ABCABCABCABC"
    );
    assert_eq!(
        decoder
            .decode(&compressed("0 01000100 10001 00100 110 100"), 13)
            .unwrap(),
        b"DABCDABCDABCD"
    );
}

#[test]
fn history_wrap_and_the_largest_defined_distance_preserve_order() {
    let mut decoder = Decoder::new();
    let mut history = vec![0; HISTORY_BYTES];
    history[..5].copy_from_slice(&[1, 2, 3, 4, 5]);
    decoder.remember(&history);
    decoder.decode(&[0xe0, 4, 90, 91], 2).unwrap();
    let farthest = compressed(&format!("10111101 {:021b} 0", HISTORY_BYTES - 2_414_240));
    assert_eq!(decoder.decode(&farthest, 3).unwrap(), [3, 4, 5]);
    let outside = compressed(&format!(
        "10111101 {:021b} 0",
        HISTORY_BYTES - 2_414_240 + 1
    ));
    assert!(matches!(
        decoder.decode(&outside, 3),
        Err(Error::Invalid("ZGFX match distance"))
    ));
}

#[test]
fn malformed_messages_are_bounded_and_do_not_commit_partial_history() {
    let mut decoder = Decoder::new();
    decoder.decode(&[0xe0, 4, b'A'], 1).unwrap();
    let expansion = compressed("0 01000010 10001 00001 111111111111110 111111111111111");
    assert!(matches!(
        decoder.decode(&expansion, 16),
        Err(Error::Limit(_))
    ));
    assert_eq!(
        decoder.decode(&compressed("10001 00001 0"), 3).unwrap(),
        b"AAA"
    );
    let malformed: &[&[u8]] = &[
        &[],
        &[0xe0],
        &[0xe0, 0x24],
        &[0xe0, 0x24, 7],
        &[0xe2, 4],
        &[0xe0, 0x25, 0],
        &[0xe1, 1, 0, 1, 0, 0, 0, 2, 0, 0, 0, 4],
        &[0xe1, 0, 0, 0, 0, 0, 0, 1],
    ];
    for packet in malformed {
        assert!(
            Decoder::new().decode(packet, 16).is_err(),
            "packet {packet:?}"
        );
    }
    for bits in ["10000000", "101111100", "10001 00001 0", "0 00000001"] {
        assert!(Decoder::new().decode(&compressed(bits), 16).is_err());
    }
    let mut huge = vec![0xe1, 1, 0];
    huge.extend(u32::MAX.to_le_bytes());
    let mut decoder = Decoder::new();
    assert!(matches!(decoder.decode(&huge, 1024), Err(Error::Limit(_))));
    assert!(decoder.history.is_empty());
}
