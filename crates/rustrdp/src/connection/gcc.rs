//! T.125 BER connection PDUs and the T.124 conference subset used by RDP.
//!
//! Wire layouts: MS-RDPBCGR 2.2.1.3–2.2.1.4 and annotated examples 4.1.3–4.1.4.

use crate::{
    Channel, ClientConfig, ColorDepth, Error, Result,
    wire::{self, Reader},
};

use super::SecurityProtocol;

pub(super) const REQUESTED_PROTOCOLS: u32 = 0x0000_000a;

pub(super) fn requested_channels(config: &ClientConfig) -> Vec<&'static str> {
    let mut channels = Vec::with_capacity(4);
    if config.clipboard {
        channels.push("cliprdr");
    }
    if config.dynamic_resolution
        || config.audio_input
        || config.graphics_pipeline
        || config.multitransport
    {
        channels.push("drdynvc");
    }
    if config.audio_output || !config.shared_drives.is_empty() {
        channels.push("rdpdr");
    }
    if config.audio_output {
        channels.push("rdpsnd");
    }
    channels
}

pub(super) fn connect_initial(
    config: &ClientConfig,
    protocol: SecurityProtocol,
    extended_client_data_supported: bool,
) -> Result<Vec<u8>> {
    let mut data = client_core(config, protocol);
    // External TLS supplies encryption; Standard RDP Security is not requested.
    block(&mut data, 0xc002, &[0; 8])?;
    let channels = requested_channels(config);
    let mut network = Vec::with_capacity(4 + channels.len() * 12);
    network.extend_from_slice(&(channels.len() as u32).to_le_bytes());
    for name in channels {
        let mut field = [0; 8];
        field[..name.len()].copy_from_slice(name.as_bytes());
        network.extend_from_slice(&field);
        let options = 0x8000_0000u32
            | if config.bulk_compression.is_some() {
                0x0080_0000
            } else {
                0
            };
        network.extend_from_slice(&options.to_le_bytes());
    }
    block(&mut data, 0xc003, &network)?;
    if extended_client_data_supported && config.multitransport {
        block(&mut data, 0xc006, &0u32.to_le_bytes())?;
        block(
            &mut data,
            0xc00a,
            &crate::multitransport::CLIENT_FLAGS.to_le_bytes(),
        )?;
    }
    if extended_client_data_supported && !config.monitors.is_empty() {
        let mut monitors = 0u32.to_le_bytes().to_vec();
        monitors.extend_from_slice(&(config.monitors.len() as u32).to_le_bytes());
        for monitor in &config.monitors {
            for value in [
                monitor.left,
                monitor.top,
                monitor.left + i32::from(monitor.width) - 1,
                monitor.top + i32::from(monitor.height) - 1,
                i32::from(monitor.primary),
            ] {
                monitors.extend_from_slice(&value.to_le_bytes());
            }
        }
        block(&mut data, 0xc005, &monitors)?;
        let mut attributes = Vec::with_capacity(12 + config.monitors.len() * 20);
        for value in [0, 20, config.monitors.len() as u32] {
            attributes.extend_from_slice(&value.to_le_bytes());
        }
        for monitor in &config.monitors {
            // Physical dimensions are unknown; zero asks the server to ignore
            // them. Device scale 100 is valid independently of desktop scale.
            for value in [0, 0, 0, monitor.scale, 100] {
                attributes.extend_from_slice(&value.to_le_bytes());
            }
        }
        block(&mut data, 0xc008, &attributes)?;
    }

    let mut conference = vec![0, 8, 0, 16, 0, 1, 0xc0, 0];
    conference.extend_from_slice(b"Duca");
    per_length(&mut conference, data.len())?;
    conference.extend_from_slice(&data);
    let mut gcc = vec![0, 5, 0, 20, 124, 0, 1];
    per_length(&mut gcc, conference.len())?;
    gcc.extend_from_slice(&conference);

    let mut body = vec![4, 1, 1, 4, 1, 1, 1, 1, 0xff];
    domain_parameters(&mut body, [34, 2, 0, 1, 0, 1, 65535, 2]);
    domain_parameters(&mut body, [1, 1, 1, 1, 0, 1, 1056, 2]);
    domain_parameters(&mut body, [65535, 64535, 65535, 1, 0, 1, 65535, 2]);
    ber_field(&mut body, &[4], &gcc);
    let mut mcs = Vec::new();
    ber_field(&mut mcs, &[0x7f, 0x65], &body);
    wire::x224_data(&mcs)
}

fn client_core(config: &ClientConfig, protocol: SecurityProtocol) -> Vec<u8> {
    // Include the optional fields through deviceScaleFactor (234 bytes).
    let mut core = vec![0; 234];
    core[..2].copy_from_slice(&0xc001u16.to_le_bytes());
    core[2..4].copy_from_slice(&234u16.to_le_bytes());
    core[4..8].copy_from_slice(&0x0008_0004u32.to_le_bytes());
    core[8..10].copy_from_slice(&config.width.to_le_bytes());
    core[10..12].copy_from_slice(&config.height.to_le_bytes());
    core[12..14].copy_from_slice(&0xca01u16.to_le_bytes());
    core[14..16].copy_from_slice(&0xaa03u16.to_le_bytes());
    // Keyboard layout zero asks for the user's server-side layout.
    core[20..24].copy_from_slice(&1u32.to_le_bytes());
    for (unit, dest) in config
        .client_name
        .encode_utf16()
        .take(15)
        .zip(core[24..54].as_chunks_mut::<2>().0.iter_mut())
    {
        dest.copy_from_slice(&unit.to_le_bytes());
    }
    core[56..60].copy_from_slice(&4u32.to_le_bytes());
    core[64..68].copy_from_slice(&12u32.to_le_bytes());
    let (legacy_depth, high_depth, color_flags) = match config.color_depth {
        ColorDepth::Bpp16 => (0xca03u16, 16u16, 0u16),
        // highColorDepth has no 32-bit value; WANT_32BPP_SESSION selects it.
        ColorDepth::Bpp32 => (0xca04, 24, 0x0002),
    };
    core[132..134].copy_from_slice(&legacy_depth.to_le_bytes());
    core[134..136].copy_from_slice(&1u16.to_le_bytes());
    core[140..142].copy_from_slice(&high_depth.to_le_bytes());
    core[142..144].copy_from_slice(&0x000fu16.to_le_bytes());
    // ERRINFO, color depth and the optional graphics pipeline.
    core[144..146].copy_from_slice(
        &(1u16
            | color_flags
            | if config.graphics_pipeline && cfg!(feature = "graphics") {
                0x0100
            } else {
                0
            }
            | if config.monitors.is_empty() {
                0
            } else {
                0x0040
            })
        .to_le_bytes(),
    );
    core[212..216].copy_from_slice(&(protocol as u32).to_le_bytes());
    // Zero physical dimensions mean unknown; orientation zero is landscape.
    core[226..230].copy_from_slice(&config.primary_scale().to_le_bytes());
    core[230..234].copy_from_slice(&100u32.to_le_bytes());
    core
}

fn block(out: &mut Vec<u8>, kind: u16, body: &[u8]) -> Result<()> {
    let length = u16::try_from(body.len() + 4).map_err(|_| Error::Limit("GCC block"))?;
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(&length.to_le_bytes());
    out.extend_from_slice(body);
    Ok(())
}

fn domain_parameters(out: &mut Vec<u8>, values: [u16; 8]) {
    let mut parameters = Vec::new();
    for value in values {
        // MCS encodes these nonnegative values as unsigned BER integers;
        // MS-RDPBCGR 4.1.3 also encodes 65535 with two octets.
        if value < 256 {
            parameters.extend_from_slice(&[2, 1, value as u8]);
        } else {
            parameters.extend_from_slice(&[2, 2]);
            parameters.extend_from_slice(&value.to_be_bytes());
        }
    }
    ber_field(out, &[0x30], &parameters);
}

fn ber_field(out: &mut Vec<u8>, tag: &[u8], data: &[u8]) {
    out.extend_from_slice(tag);
    if data.len() < 128 {
        out.push(data.len() as u8);
    } else if data.len() < 256 {
        out.extend_from_slice(&[0x81, data.len() as u8]);
    } else {
        out.push(0x82);
        out.extend_from_slice(&(data.len() as u16).to_be_bytes());
    }
    out.extend_from_slice(data);
}

fn per_length(out: &mut Vec<u8>, length: usize) -> Result<()> {
    if length < 128 {
        out.push(length as u8);
    } else if length < 0x4000 {
        out.extend_from_slice(&(0x8000 | length as u16).to_be_bytes());
    } else {
        return Err(Error::Limit("GCC PER length"));
    }
    Ok(())
}

fn read_per_length(reader: &mut Reader<'_>) -> Result<usize> {
    let first = reader.u8()?;
    if first & 0x80 == 0 {
        Ok(usize::from(first))
    } else if first & 0xc0 == 0x80 {
        Ok((usize::from(first & 0x3f) << 8) | usize::from(reader.u8()?))
    } else {
        Err(Error::Unsupported("fragmented GCC PER length"))
    }
}

fn ber_header(reader: &mut Reader<'_>, tag: &[u8]) -> Result<usize> {
    if reader.take(tag.len())? != tag {
        return Err(Error::Invalid("MCS BER tag"));
    }
    let first = reader.u8()?;
    match first {
        0..=127 => Ok(usize::from(first)),
        0x81 => Ok(usize::from(reader.u8()?)),
        0x82 => Ok(usize::from(reader.u16_be()?)),
        _ => Err(Error::Invalid("MCS BER length")),
    }
}

fn ber_value<'a>(reader: &mut Reader<'a>, tag: &[u8]) -> Result<&'a [u8]> {
    let length = ber_header(reader, tag)?;
    reader.take(length)
}

fn ber_integer(reader: &mut Reader<'_>, tag: u8) -> Result<u32> {
    let bytes = ber_value(reader, &[tag])?;
    if bytes.is_empty() || bytes.len() > 4 {
        return Err(Error::Invalid("MCS BER integer"));
    }
    Ok(bytes
        .iter()
        .fold(0u32, |value, byte| (value << 8) | u32::from(*byte)))
}

pub(super) struct ServerData {
    pub io_channel: u16,
    pub channels: Vec<Channel>,
    pub message_channel: Option<u16>,
    pub multitransport_flags: u32,
}

pub(super) fn connect_response(packet: &[u8], config: &ClientConfig) -> Result<ServerData> {
    let mut outer = Reader::new(wire::x224_payload(packet)?);
    let mut mcs = Reader::new(ber_value(&mut outer, &[0x7f, 0x66])?);
    if !outer.is_empty() {
        return Err(Error::Invalid("trailing MCS Connect Response data"));
    }
    let result = ber_integer(&mut mcs, 0x0a)?;
    if result != 0 {
        return Err(Error::Server(result));
    }
    let _connect_id = ber_integer(&mut mcs, 2)?;
    let mut parameters = Reader::new(ber_value(&mut mcs, &[0x30])?);
    for _ in 0..8 {
        let _parameter = ber_integer(&mut parameters, 2)?;
    }
    parameters.finish()?;
    // 3.2.5.3.4 explicitly requires ignoring this length. The enclosing
    // MCS/TPKT lengths and inner GCC userData length still bound every byte.
    let _user_data_length = ber_header(&mut mcs, &[4])?;
    let mut gcc = Reader::new(mcs.remaining());
    if gcc.take(7)? != [0, 5, 0, 20, 124, 0, 1] {
        return Err(Error::Invalid("GCC conference identifier"));
    }
    // Servers deliberately use a fixed connectPDU length (42); 2.2.1.4
    // requires the client to ignore it and honor the userData length instead.
    let _connect_pdu_length = read_per_length(&mut gcc)?;
    if gcc.u8()? != 0x14 {
        return Err(Error::Invalid("GCC Conference Create Response"));
    }
    let _node_id = gcc.u16_be()?;
    let tag_length = read_per_length(&mut gcc)?;
    if tag_length == 0 || tag_length > 4 {
        return Err(Error::Invalid("GCC conference tag"));
    }
    gcc.skip(tag_length)?;
    let _result = gcc.u8()?;
    if gcc.u8()? != 1 || gcc.u8()? != 0xc0 || gcc.u8()? != 0 {
        return Err(Error::Invalid("GCC conference user data"));
    }
    if gcc.take(4)? != b"McDn" {
        return Err(Error::Invalid("GCC server H.221 key"));
    }
    let user_data_length = read_per_length(&mut gcc)?;
    let data = gcc.take(user_data_length)?;
    if !gcc.is_empty() {
        return Err(Error::Invalid("trailing GCC conference data"));
    }
    server_blocks(data, config)
}

fn server_blocks(data: &[u8], config: &ClientConfig) -> Result<ServerData> {
    let mut reader = Reader::new(data);
    let mut core_seen = false;
    let mut security_seen = false;
    let mut network = None;
    let mut message_channel = None;
    let mut multitransport_flags = None;
    let names = requested_channels(config);
    while !reader.is_empty() {
        let kind = reader.u16_le()?;
        let length = usize::from(reader.u16_le()?);
        if length < 4 {
            return Err(Error::Invalid("GCC user data block length"));
        }
        let mut block = Reader::new(reader.take(length - 4)?);
        match kind {
            0x0c01 => {
                if core_seen || block.u32_le()? < 0x0008_0004 {
                    return Err(Error::Invalid("GCC server core"));
                }
                core_seen = true;
                if block.remaining_len() >= 4 && block.u32_le()? != REQUESTED_PROTOCOLS {
                    return Err(Error::Invalid("GCC security negotiation mismatch"));
                }
                // earlyCapabilityFlags are optional; channel-join skipping
                // was not requested and cannot be enabled by the server.
                if block.remaining_len() >= 4 {
                    block.skip(4)?;
                }
            }
            0x0c02 => {
                if security_seen || block.u32_le()? != 0 || block.u32_le()? != 0 {
                    return Err(Error::Invalid("Standard RDP Security after NLA"));
                }
                security_seen = true;
            }
            0x0c03 => {
                if network.is_some() {
                    return Err(Error::Invalid("duplicate GCC network block"));
                }
                let io_channel = block.u16_le()?;
                let count = usize::from(block.u16_le()?);
                if count != names.len() || io_channel < 1001 {
                    return Err(Error::Invalid("GCC channel allocation"));
                }
                let mut channels = Vec::with_capacity(count);
                for name in &names {
                    let id = block.u16_le()?;
                    if id != 0
                        && (id < 1001
                            || id == io_channel
                            || channels.iter().any(|channel: &Channel| channel.id == id))
                    {
                        return Err(Error::Invalid("GCC static channel identifier"));
                    }
                    // Zero means the requested channel was not allocated.
                    if id != 0 {
                        channels.push(Channel {
                            id,
                            name: (*name).into(),
                        });
                    }
                }
                if count & 1 != 0 {
                    block.skip(2)?;
                }
                network = Some((io_channel, channels));
            }
            0x0c04 => {
                let id = block.u16_le()?;
                if message_channel.replace(id).is_some() || id < 1001 {
                    return Err(Error::Invalid("GCC message channel identifier"));
                }
            }
            0x0c08 => {
                if multitransport_flags.replace(block.u32_le()?).is_some() {
                    return Err(Error::Invalid("duplicate GCC multitransport block"));
                }
            }
            _ => continue,
        }
        if !block.is_empty() {
            return Err(Error::Invalid("GCC user data block size"));
        }
    }
    if !core_seen || !security_seen {
        return Err(Error::Invalid("missing GCC server core or security data"));
    }
    let (io_channel, channels) =
        network.ok_or(Error::Invalid("missing GCC server network data"))?;
    if message_channel
        .is_some_and(|id| id == io_channel || channels.iter().any(|channel| channel.id == id))
    {
        return Err(Error::Invalid("GCC message channel collision"));
    }
    let flags = if config.multitransport && message_channel.is_some() {
        multitransport_flags.unwrap_or(0) & crate::multitransport::CLIENT_FLAGS
    } else {
        0
    };
    Ok(ServerData {
        io_channel,
        channels,
        message_channel,
        multitransport_flags: flags,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocks(config: &ClientConfig, extended: bool) -> Vec<(u16, Vec<u8>)> {
        let packet = connect_initial(config, SecurityProtocol::Hybrid, extended).unwrap();
        let start = packet
            .windows(8)
            .position(|bytes| bytes == [1, 0xc0, 234, 0, 4, 0, 8, 0])
            .expect("client core block");
        let mut data = Reader::new(&packet[start..]);
        let mut blocks = Vec::new();
        while !data.is_empty() {
            let kind = data.u16_le().unwrap();
            let length = usize::from(data.u16_le().unwrap());
            blocks.push((kind, data.take(length - 4).unwrap().to_vec()));
        }
        blocks
    }

    #[test]
    fn requested_color_depth_uses_the_correct_legacy_and_32_bit_fields() {
        for (color_depth, legacy, high, want_32) in [
            (ColorDepth::Bpp16, 0xca03u16, 16u16, 0u16),
            (ColorDepth::Bpp32, 0xca04, 24, 2),
        ] {
            let config = ClientConfig {
                color_depth,
                ..ClientConfig::default()
            };
            let core = client_core(&config, SecurityProtocol::HybridEx);
            assert_eq!(core.len(), 234);
            assert_eq!(&core[2..4], &234u16.to_le_bytes());
            assert_eq!(&core[132..134], &legacy.to_le_bytes());
            assert_eq!(&core[140..142], &high.to_le_bytes());
            assert_eq!(&core[142..144], &15u16.to_le_bytes());
            assert_eq!(
                u16::from_le_bytes(core[144..146].try_into().unwrap()) & 2,
                want_32
            );
            assert_eq!(&core[212..216], &8u32.to_le_bytes());
        }
    }

    #[test]
    fn core_desktop_scale_does_not_require_extended_monitor_capabilities() {
        let config = ClientConfig {
            desktop_scale: 175,
            ..ClientConfig::default()
        };
        let blocks = blocks(&config, false);
        let core = &blocks[0].1;
        // Block payload excludes the four-byte GCC user-data header.
        assert_eq!(&core[212..222], &[0; 10]);
        assert_eq!(&core[222..226], &175u32.to_le_bytes());
        assert_eq!(&core[226..230], &100u32.to_le_bytes());
        assert!(
            !blocks
                .iter()
                .any(|(kind, _)| matches!(kind, 0xc005 | 0xc008))
        );
    }

    #[test]
    fn extended_monitor_attributes_follow_layout_order_and_primary_scale() {
        let config = ClientConfig {
            width: 2560,
            height: 720,
            monitors: vec![
                crate::Monitor {
                    scale: 150,
                    ..crate::Monitor::primary(1280, 720)
                },
                crate::Monitor {
                    left: 1280,
                    primary: false,
                    scale: 200,
                    ..crate::Monitor::primary(1280, 720)
                },
            ],
            ..ClientConfig::default()
        };
        let blocks = blocks(&config, true);
        assert_eq!(&blocks[0].1[222..226], &150u32.to_le_bytes());
        let monitor = &blocks.iter().find(|(kind, _)| *kind == 0xc005).unwrap().1;
        assert_eq!(&monitor[4..8], &2u32.to_le_bytes());
        let attributes = &blocks.iter().find(|(kind, _)| *kind == 0xc008).unwrap().1;
        assert_eq!(attributes.len(), 52);
        assert_eq!(&attributes[..4], &[0; 4]);
        assert_eq!(&attributes[4..8], &20u32.to_le_bytes());
        assert_eq!(&attributes[8..12], &2u32.to_le_bytes());
        for (index, scale) in [150u32, 200].into_iter().enumerate() {
            let monitor = &attributes[12 + index * 20..32 + index * 20];
            assert_eq!(&monitor[..12], &[0; 12]);
            assert_eq!(&monitor[12..16], &scale.to_le_bytes());
            assert_eq!(&monitor[16..20], &100u32.to_le_bytes());
        }
    }
}
