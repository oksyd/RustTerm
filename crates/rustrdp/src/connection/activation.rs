//! Capability exchange and finalization, MS-RDPBCGR 2.2.1.13–22 / 2.2.7.

use crate::{
    ClientConfig, ConnectionInfo, Error, Result,
    wire::{self, Reader},
};

struct ServerCapabilities {
    multifragment: bool,
    large_pointer: u16,
    persistent_cache: bool,
}

const NSCODEC_GUID: [u8; 16] = [
    0xb9, 0x1b, 0x8d, 0xca, 0x0f, 0x00, 0x4f, 0x15, 0x58, 0x9f, 0xae, 0x2d, 0x1a, 0x87, 0xe2, 0xd6,
];

/// Accept a Demand Active Share Control PDU, including its six-byte header.
///
/// The returned packets include Confirm Active and connection finalization.
/// Keep input disabled until [`ActivationFinalizer`] accepts the server replies.
pub(crate) fn reactivate(
    config: &ClientConfig,
    info: &mut ConnectionInfo,
    demand_active: &[u8],
) -> Result<Vec<Vec<u8>>> {
    activate(config, info, demand_active, true)
}

pub(super) fn activate(
    config: &ClientConfig,
    info: &mut ConnectionInfo,
    packet: &[u8],
    reactivation: bool,
) -> Result<Vec<Vec<u8>>> {
    if packet.len() > config.limits.max_pdu {
        return Err(Error::Limit("Demand Active PDU"));
    }
    // Commit negotiated state only after the entire PDU and outbound packets
    // have been checked, so malformed reactivation cannot resize the display.
    let mut negotiated = info.clone();
    let capabilities = demand_active(config, &mut negotiated, packet)?;
    let mut packets = vec![confirm_active(config, &negotiated, &capabilities)?];
    let mut synchronize = [1, 0, 0, 0];
    synchronize[2..4].copy_from_slice(&negotiated.server_id.to_le_bytes());
    packets.push(wire::share_data(
        negotiated.user_id,
        negotiated.io_channel_id,
        negotiated.share_id,
        31,
        &synchronize,
    )?);
    for action in [4u16, 1] {
        let mut control = [0; 8];
        control[..2].copy_from_slice(&action.to_le_bytes());
        packets.push(wire::share_data(
            negotiated.user_id,
            negotiated.io_channel_id,
            negotiated.share_id,
            20,
            &control,
        )?);
    }
    if capabilities.persistent_cache && !reactivation {
        // No persisted bitmaps. A FIRST|LAST empty list still completes the
        // server's revision-2 cache initialization (3.2.5.3.16–17).
        let mut keys = [0; 24];
        keys[20] = 3;
        packets.push(wire::share_data(
            negotiated.user_id,
            negotiated.io_channel_id,
            negotiated.share_id,
            43,
            &keys,
        )?);
    }
    packets.push(wire::share_data(
        negotiated.user_id,
        negotiated.io_channel_id,
        negotiated.share_id,
        39,
        &[0, 0, 0, 0, 3, 0, 50, 0],
    )?);
    *info = negotiated;
    Ok(packets)
}

pub(super) fn share_body(packet: &[u8], kind: u16) -> Result<&[u8]> {
    let mut reader = Reader::new(packet);
    if usize::from(reader.u16_le()?) != packet.len() || reader.u16_le()? != (kind | 0x10) {
        return Err(Error::Invalid("Share Control header"));
    }
    let _source = reader.u16_le()?;
    Ok(reader.remaining())
}

fn demand_active(
    config: &ClientConfig,
    info: &mut ConnectionInfo,
    packet: &[u8],
) -> Result<ServerCapabilities> {
    let mut reader = Reader::new(share_body(packet, 1)?);
    info.server_id = u16::from_le_bytes([packet[4], packet[5]]);
    if info.server_id < 1001 || info.server_id == info.user_id {
        return Err(Error::Invalid("server channel identifier"));
    }
    info.share_id = reader.u32_le()?;
    let descriptor_length = usize::from(reader.u16_le()?);
    let capabilities_length = usize::from(reader.u16_le()?);
    reader.skip(descriptor_length)?;
    let mut caps = Reader::new(reader.take(capabilities_length)?);
    let count = usize::from(caps.u16_le()?);
    caps.skip(2)?;
    if count > 64 {
        return Err(Error::Limit("capability count"));
    }
    let mut seen = [false; 32];
    let mut server = ServerCapabilities {
        multifragment: false,
        large_pointer: 0,
        persistent_cache: false,
    };
    info.fastpath_input = false;
    info.unicode_input = false;
    info.mouse_extended = false;
    info.horizontal_wheel = false;
    info.vc_chunk_size = 1600;
    info.surface_commands = 0;
    info.nscodec = false;
    info.frame_acknowledgements = false;
    for _ in 0..count {
        let kind = usize::from(caps.u16_le()?);
        let length = usize::from(caps.u16_le()?);
        if length < 4 {
            return Err(Error::Invalid("capability length"));
        }
        if let Some(entry) = seen.get_mut(kind) {
            if *entry {
                return Err(Error::Invalid("duplicate capability"));
            }
            *entry = true;
        }
        let mut cap = Reader::new(caps.take(length - 4)?);
        match kind {
            1 => {
                cap.skip(4)?;
                if cap.u16_le()? != 0x0200 {
                    return Err(Error::Unsupported("capability protocol version"));
                }
                cap.skip(14)?;
            }
            2 => {
                info.color_depth = cap.u16_le()?;
                if !matches!(info.color_depth, 8 | 15 | 16 | 24 | 32) {
                    return Err(Error::Unsupported("desktop color depth"));
                }
                cap.skip(6)?;
                info.width = cap.u16_le()?;
                info.height = cap.u16_le()?;
                config.limits.frame_bytes(info.width, info.height)?;
                cap.skip(12)?;
            }
            13 => {
                let flags = cap.u16_le()?;
                if flags & 1 == 0 {
                    return Err(Error::Invalid("server scancode capability"));
                }
                info.fastpath_input = flags & 0x28 != 0;
                info.unicode_input = flags & 0x10 != 0;
                info.mouse_extended = flags & 4 != 0;
                info.horizontal_wheel = flags & 0x100 != 0;
                cap.skip(82)?;
            }
            18 => {
                server.persistent_cache = cap.u8()? & 1 != 0;
                cap.skip(3)?;
            }
            20 => {
                let _compression = cap.u32_le()?;
                if cap.remaining_len() >= 4 {
                    let chunk = cap.u32_le()? as usize;
                    if !(1600..=16256).contains(&chunk) {
                        return Err(Error::Invalid("virtual channel chunk size"));
                    }
                    if chunk > config.limits.max_channel_message {
                        return Err(Error::Limit("virtual channel chunk size"));
                    }
                    info.vc_chunk_size = chunk;
                }
            }
            26 => {
                let _server_max_request = cap.u32_le()?;
                server.multifragment = true;
            }
            27 => server.large_pointer = cap.u16_le()? & 3,
            28 => {
                info.surface_commands = cap.u32_le()? & 0x52;
                cap.skip(4)?;
            }
            29 => {
                let count = cap.u8()?;
                for _ in 0..count {
                    let guid = cap.take(16)?;
                    let _server_codec_id = cap.u8()?;
                    let length = usize::from(cap.u16_le()?);
                    let properties = cap.take(length)?;
                    if guid == NSCODEC_GUID {
                        if properties.len() != 3
                            || properties[0] > 1
                            || properties[1] > 1
                            || !(1..=7).contains(&properties[2])
                        {
                            return Err(Error::Invalid("NSCodec capability"));
                        }
                        info.nscodec = true;
                    }
                }
                cap.finish()?;
            }
            30 => {
                let _server_window = cap.u32_le()?;
                info.frame_acknowledgements = true;
            }
            _ => continue,
        }
        // A capability may acquire fields in a later protocol revision.
        // Known mandatory prefixes are checked; extension bytes are opaque.
    }
    caps.finish()?;
    let _session_id = reader.u32_le()?;
    reader.finish()?;
    if !seen[1] || !seen[2] || !seen[13] {
        return Err(Error::Invalid(
            "missing general, bitmap or input capability",
        ));
    }
    // NSCodec always discards chroma bits, even at color-loss level 1.
    info.nscodec &=
        config.allow_lossy_bitmaps && info.color_depth == 32 && info.surface_commands & 0x42 != 0;
    info.frame_acknowledgements &=
        config.frame_acknowledgements.is_some() && info.surface_commands & 0x10 != 0;
    Ok(server)
}

fn capability(out: &mut Vec<u8>, count: &mut u16, kind: u16, data: &[u8]) {
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(&((data.len() + 4) as u16).to_le_bytes());
    out.extend_from_slice(data);
    *count += 1;
}

fn confirm_active(
    config: &ClientConfig,
    info: &ConnectionInfo,
    server: &ServerCapabilities,
) -> Result<Vec<u8>> {
    let mut caps = Vec::new();
    let mut count = 0;

    let mut general = [0; 20];
    general[..2].copy_from_slice(&4u16.to_le_bytes()); // UNIX
    general[4..6].copy_from_slice(&0x0200u16.to_le_bytes());
    // Microsoft servers require LONG_CREDENTIALS_SUPPORTED together with
    // FASTPATH_OUTPUT_SUPPORTED (MS-RDPBCGR 6, notes 24–25). Save Session Info
    // already accepts the long logon form during activation and active sessions.
    general[10..12].copy_from_slice(&0x0405u16.to_le_bytes());
    capability(&mut caps, &mut count, 1, &general);

    let mut bitmap = [0; 24];
    bitmap[..2].copy_from_slice(&info.color_depth.to_le_bytes());
    for offset in [2, 4, 6, 14, 16, 20] {
        bitmap[offset..offset + 2].copy_from_slice(&1u16.to_le_bytes());
    }
    bitmap[8..10].copy_from_slice(&info.width.to_le_bytes());
    bitmap[10..12].copy_from_slice(&info.height.to_le_bytes());
    bitmap[19] = 8 | if config.allow_lossy_bitmaps { 6 } else { 0 };
    capability(&mut caps, &mut count, 2, &bitmap);

    let mut orders = [0; 84];
    orders[20..22].copy_from_slice(&1u16.to_le_bytes());
    orders[22..24].copy_from_slice(&20u16.to_le_bytes());
    orders[26..28].copy_from_slice(&1u16.to_le_bytes());
    orders[30..32].copy_from_slice(&0x000au16.to_le_bytes());
    // All 32 orderSupport bytes remain zero; rendering uses bitmap updates.
    capability(&mut caps, &mut count, 3, &orders);
    capability(&mut caps, &mut count, 4, &[0; 36]); // no bitmap cache

    let cache = config.limits.max_pointer_cache as u16;
    let mut pointer = [0; 6];
    pointer[..2].copy_from_slice(&1u16.to_le_bytes());
    pointer[2..4].copy_from_slice(&cache.to_le_bytes());
    pointer[4..6].copy_from_slice(&cache.to_le_bytes());
    capability(&mut caps, &mut count, 8, &pointer);

    let mut input = [0; 84];
    let flags = 0x0001u16
        | if info.mouse_extended { 4 } else { 0 }
        | if info.horizontal_wheel { 0x100 } else { 0 }
        | if info.fastpath_input { 0x0028 } else { 0 }
        | if info.unicode_input { 0x0010 } else { 0 };
    input[..2].copy_from_slice(&flags.to_le_bytes());
    input[8..12].copy_from_slice(&4u32.to_le_bytes());
    input[16..20].copy_from_slice(&12u32.to_le_bytes());
    capability(&mut caps, &mut count, 13, &input);
    capability(&mut caps, &mut count, 15, &[0; 4]); // no cached brushes
    capability(&mut caps, &mut count, 16, &[0; 48]); // no glyph cache
    capability(&mut caps, &mut count, 17, &[0; 8]); // no offscreen cache

    let mut channel = [0; 8];
    if config.bulk_compression.is_some() {
        channel[..4].copy_from_slice(&1u32.to_le_bytes()); // VCCAPS_COMPR_SC
    }
    channel[4..8].copy_from_slice(&(info.vc_chunk_size as u32).to_le_bytes());
    capability(&mut caps, &mut count, 20, &channel);
    capability(&mut caps, &mut count, 12, &[0; 4]); // no beep
    capability(&mut caps, &mut count, 14, &[1, 0, 0, 0]);

    if info.surface_commands != 0 {
        let mut commands = [0; 8];
        commands[..4].copy_from_slice(&info.surface_commands.to_le_bytes());
        capability(&mut caps, &mut count, 28, &commands);
    }
    if info.nscodec {
        let mut codec = vec![1];
        codec.extend_from_slice(&NSCODEC_GUID);
        codec.push(1);
        codec.extend_from_slice(&3u16.to_le_bytes());
        codec.extend_from_slice(&[1, 1, 3]);
        capability(&mut caps, &mut count, 29, &codec);
    }
    if info.frame_acknowledgements
        && let Some(window) = config.frame_acknowledgements
    {
        capability(&mut caps, &mut count, 30, &window.get().to_le_bytes());
    }

    if server.multifragment {
        let maximum = u32::try_from(config.limits.max_pdu.min(config.limits.max_bitmap_bytes))
            .map_err(|_| Error::Limit("multifragment buffer"))?;
        capability(&mut caps, &mut count, 26, &maximum.to_le_bytes());
        let mut flags = 0u16;
        if config.limits.max_pointer_size >= 96 && maximum >= 38_055 {
            flags |= server.large_pointer & 1;
        }
        if config.limits.max_pointer_size >= 384 && maximum >= 608_299 {
            flags |= server.large_pointer & 2;
        }
        if flags != 0 {
            capability(&mut caps, &mut count, 27, &flags.to_le_bytes());
        }
    }

    let descriptor = b"RustRDP\0";
    let mut body = Vec::with_capacity(caps.len() + descriptor.len() + 14);
    body.extend_from_slice(&info.share_id.to_le_bytes());
    body.extend_from_slice(&1002u16.to_le_bytes());
    body.extend_from_slice(&(descriptor.len() as u16).to_le_bytes());
    body.extend_from_slice(&((caps.len() + 4) as u16).to_le_bytes());
    body.extend_from_slice(descriptor);
    body.extend_from_slice(&count.to_le_bytes());
    body.extend_from_slice(&[0, 0]);
    body.extend_from_slice(&caps);
    wire::mcs_send_data(
        info.user_id,
        info.io_channel_id,
        &wire::share_control(info.user_id, 3, &body)?,
    )
}

/// Validates server Synchronize, Cooperate, Granted Control and Font Map PDUs.
/// Receives Share Data payloads after the session has decompressed them.
#[derive(Debug)]
pub(crate) struct ActivationFinalizer {
    share_id: u32,
    stage: u8,
}

impl ActivationFinalizer {
    pub fn new(info: &ConnectionInfo) -> Self {
        Self {
            share_id: info.share_id,
            stage: 0,
        }
    }

    #[cfg(test)]
    pub fn advance(&mut self, packet: &[u8]) -> Result<bool> {
        if self.stage >= 4 {
            return Err(Error::Invalid("completed activation"));
        }
        let mut reader = Reader::new(share_body(packet, 7)?);
        let share_id = reader.u32_le()?;
        reader.skip(2)?; // pad, streamId (zero is permitted for Synchronize)
        let _uncompressed_length = reader.u16_le()?;
        let kind = reader.u8()?;
        let compression = reader.u8()?;
        let compressed_length = reader.u16_le()?;
        if compression != 0 || compressed_length != 0 {
            return Err(Error::Unsupported("compressed activation PDU"));
        }
        self.advance_data(share_id, kind, reader.remaining())
    }

    pub(crate) fn advance_data(&mut self, share_id: u32, kind: u8, payload: &[u8]) -> Result<bool> {
        if self.stage >= 4 {
            return Err(Error::Invalid("completed activation"));
        }
        let mut reader = Reader::new(payload);
        if kind == 47 {
            let code = reader.u32_le()?;
            reader.finish()?;
            if code != 0 {
                return Err(Error::Server(code));
            }
            return Ok(false);
        }
        if kind == 54 {
            let _status = reader.u32_le()?;
            reader.finish()?;
            return Ok(false);
        }
        if share_id != self.share_id {
            return Err(Error::Invalid("activation share identifier"));
        }
        if kind == 38 {
            // Login metadata is not exposed and reconnect cookies are not
            // retained. Its enclosing Share PDU already bounds the payload.
            if reader.u32_le()? > 3 {
                return Err(Error::Invalid("Save Session Info type"));
            }
            return Ok(false);
        }
        if kind == 55 {
            let count = reader.u32_le()? as usize;
            if !(1..=16).contains(&count) {
                return Err(Error::Invalid("monitor layout count"));
            }
            reader.skip(count * 20)?;
            reader.finish()?;
            return Ok(false);
        }
        match (self.stage, kind) {
            (0, 31) => {
                if reader.u16_le()? != 1 {
                    return Err(Error::Invalid("Synchronize message type"));
                }
                reader.skip(2)?;
            }
            (1 | 2, 20) => {
                let expected = if self.stage == 1 { 4 } else { 2 };
                if reader.u16_le()? != expected {
                    return Err(Error::Invalid("activation control action"));
                }
                reader.skip(6)?;
            }
            (3, 40) => reader.skip(8)?,
            _ => return Err(Error::Invalid("activation PDU sequence")),
        }
        // The synchronization target, control identifiers and all Font Map
        // fields MUST be ignored (MS-RDPBCGR 3.2.5.3.19–22).
        reader.finish()?;
        self.stage += 1;
        Ok(self.stage == 4)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> ConnectionInfo {
        let config = ClientConfig::default();
        ConnectionInfo {
            user_id: 1001,
            server_id: 1002,
            io_channel_id: 1003,
            share_id: 1,
            width: config.width,
            height: config.height,
            color_depth: 32,
            channels: vec![],
            fastpath_input: true,
            unicode_input: true,
            mouse_extended: true,
            horizontal_wheel: true,
            vc_chunk_size: 1600,
            surface_commands: 0,
            nscodec: false,
            frame_acknowledgements: false,
            message_channel_id: None,
            multitransport_flags: 0,
        }
    }

    fn demand(surface_commands: Option<u32>, nscodec: bool) -> Vec<u8> {
        demand_with_acknowledgements(surface_commands, nscodec, false)
    }

    fn demand_with_acknowledgements(
        surface_commands: Option<u32>,
        nscodec: bool,
        acknowledgements: bool,
    ) -> Vec<u8> {
        let mut caps = Vec::new();
        let mut count = 0;
        let mut general = [0; 20];
        general[4..6].copy_from_slice(&0x0200u16.to_le_bytes());
        capability(&mut caps, &mut count, 1, &general);
        let mut bitmap = [0; 24];
        bitmap[..2].copy_from_slice(&32u16.to_le_bytes());
        bitmap[8..10].copy_from_slice(&2u16.to_le_bytes());
        bitmap[10..12].copy_from_slice(&2u16.to_le_bytes());
        capability(&mut caps, &mut count, 2, &bitmap);
        let mut input = [0; 84];
        input[..2].copy_from_slice(&1u16.to_le_bytes());
        capability(&mut caps, &mut count, 13, &input);
        if let Some(flags) = surface_commands {
            let mut commands = flags.to_le_bytes().to_vec();
            commands.extend_from_slice(&[0; 4]);
            capability(&mut caps, &mut count, 28, &commands);
        }
        if nscodec {
            let mut codec = vec![1];
            codec.extend_from_slice(&NSCODEC_GUID);
            codec.push(1);
            codec.extend_from_slice(&3u16.to_le_bytes());
            codec.extend_from_slice(&[1, 1, 7]);
            capability(&mut caps, &mut count, 29, &codec);
        }
        if acknowledgements {
            capability(&mut caps, &mut count, 30, &0u32.to_le_bytes());
        }
        let mut body = 1u32.to_le_bytes().to_vec();
        body.extend_from_slice(&0u16.to_le_bytes());
        body.extend_from_slice(&((caps.len() + 4) as u16).to_le_bytes());
        body.extend_from_slice(&count.to_le_bytes());
        body.extend_from_slice(&[0; 2]);
        body.extend_from_slice(&caps);
        body.extend_from_slice(&[0; 4]);
        wire::share_control(1002, 1, &body).unwrap()
    }

    fn confirmed_caps(packet: &[u8]) -> Vec<(u16, Vec<u8>)> {
        let mut mcs = Reader::new(wire::x224_payload(packet).unwrap());
        assert_eq!(mcs.u8().unwrap(), 0x64);
        mcs.skip(5).unwrap();
        let length = wire::per_length(&mut mcs).unwrap();
        let mut body = Reader::new(share_body(mcs.take(length).unwrap(), 3).unwrap());
        mcs.finish().unwrap();
        body.skip(6).unwrap();
        let descriptor_length = usize::from(body.u16_le().unwrap());
        let capabilities_length = usize::from(body.u16_le().unwrap());
        body.skip(descriptor_length).unwrap();
        let mut caps = Reader::new(body.take(capabilities_length).unwrap());
        body.finish().unwrap();
        let count = caps.u16_le().unwrap();
        caps.skip(2).unwrap();
        let mut result = Vec::new();
        for _ in 0..count {
            let kind = caps.u16_le().unwrap();
            let length = usize::from(caps.u16_le().unwrap());
            result.push((kind, caps.take(length - 4).unwrap().to_vec()));
        }
        caps.finish().unwrap();
        result
    }

    #[test]
    fn surface_negotiation_preserves_lossless_policy_and_server_capabilities() {
        for allow_lossy_bitmaps in [false, true] {
            let config = ClientConfig {
                allow_lossy_bitmaps,
                ..ClientConfig::default()
            };
            let mut info = info();
            let packets =
                activate(&config, &mut info, &demand(Some(0xffff_ffff), true), false).unwrap();
            assert_eq!(info.surface_commands, 0x52);
            assert_eq!(info.nscodec, allow_lossy_bitmaps);
            let caps = confirmed_caps(&packets[0]);
            let commands = &caps.iter().find(|(kind, _)| *kind == 28).unwrap().1;
            assert_eq!(commands, &[0x52, 0, 0, 0, 0, 0, 0, 0]);
            let codec = caps.iter().find(|(kind, _)| *kind == 29);
            assert_eq!(codec.is_some(), allow_lossy_bitmaps);
            if let Some((_, codec)) = codec {
                assert_eq!(codec[0], 1);
                assert_eq!(&codec[1..17], &NSCODEC_GUID);
                assert_eq!(&codec[17..], &[1, 3, 0, 1, 1, 3]);
            }
            assert!(!caps.iter().any(|(kind, _)| *kind == 30));
            let packets = reactivate(&config, &mut info, &demand(None, false)).unwrap();
            assert_eq!(info.surface_commands, 0);
            assert!(!info.nscodec);
            assert!(
                !confirmed_caps(&packets[0])
                    .iter()
                    .any(|(kind, _)| matches!(kind, 28 | 29 | 30))
            );
        }
        let config = ClientConfig {
            allow_lossy_bitmaps: true,
            ..ClientConfig::default()
        };
        let mut info = info();
        activate(&config, &mut info, &demand(Some(0x10), true), false).unwrap();
        assert_eq!(info.surface_commands, 0x10);
        assert!(!info.nscodec);
    }

    #[test]
    fn frame_acknowledgements_require_opt_in_server_support_and_frame_markers() {
        for window in [None, std::num::NonZeroU32::new(3)] {
            for server_acknowledgements in [false, true] {
                for commands in [None, Some(0x42), Some(0x52)] {
                    let config = ClientConfig {
                        frame_acknowledgements: window,
                        ..ClientConfig::default()
                    };
                    let mut info = info();
                    let packets = activate(
                        &config,
                        &mut info,
                        &demand_with_acknowledgements(commands, true, server_acknowledgements),
                        false,
                    )
                    .unwrap();
                    let expected = window.is_some()
                        && server_acknowledgements
                        && commands.is_some_and(|flags| flags & 0x10 != 0);
                    assert_eq!(info.frame_acknowledgements(), expected);
                    let caps = confirmed_caps(&packets[0]);
                    let acknowledgement = caps.iter().find(|(kind, _)| *kind == 30);
                    assert_eq!(acknowledgement.is_some(), expected);
                    if let Some((_, data)) = acknowledgement {
                        assert_eq!(data, &3u32.to_le_bytes());
                    }
                    let packets = reactivate(&config, &mut info, &demand(None, false)).unwrap();
                    assert!(!info.frame_acknowledgements());
                    assert!(
                        !confirmed_caps(&packets[0])
                            .iter()
                            .any(|(kind, _)| *kind == 30)
                    );
                }
            }
        }
    }

    #[test]
    fn frame_acknowledgement_window_is_finite_and_disabled_by_default() {
        let mut config = ClientConfig::default();
        assert_eq!(config.frame_acknowledgements, None);
        config.frame_acknowledgements = std::num::NonZeroU32::new(64);
        assert!(config.validate().is_ok());
        config.frame_acknowledgements = std::num::NonZeroU32::new(65);
        assert_eq!(
            config.validate(),
            Err(Error::Invalid("frame acknowledgement window"))
        );
    }

    #[test]
    fn confirm_active_advertises_complete_fast_path_output_support() {
        let config = ClientConfig::default();
        let info = info();
        let server = ServerCapabilities {
            multifragment: true,
            large_pointer: 3,
            persistent_cache: false,
        };
        let packet = confirm_active(&config, &info, &server).unwrap();
        let caps = confirmed_caps(&packet);
        let general = &caps.iter().find(|(kind, _)| *kind == 1).unwrap().1;
        assert_eq!(general.len(), 20);
        assert_eq!(u16::from_le_bytes([general[10], general[11]]), 0x0405);
    }

    #[test]
    fn long_logon_notification_preserves_the_finalization_sequence() {
        let mut finalizer = ActivationFinalizer {
            share_id: 1,
            stage: 0,
        };
        // TS_LOGON_INFO_VERSION_2 with a user name beyond the v1 fixed field.
        let name: Vec<u8> = "a"
            .repeat(300)
            .encode_utf16()
            .chain([0])
            .flat_map(u16::to_le_bytes)
            .collect();
        let mut logon = 1u32.to_le_bytes().to_vec();
        logon.extend_from_slice(&1u16.to_le_bytes());
        for value in [576, 1, 2, name.len() as u32] {
            logon.extend_from_slice(&value.to_le_bytes());
        }
        logon.extend_from_slice(&[0; 558]);
        logon.extend_from_slice(&[0; 2]);
        logon.extend_from_slice(&name);
        assert!(!finalizer.advance_data(1, 38, &logon).unwrap());
        assert!(!finalizer.advance_data(1, 31, &[1, 0, 0, 0]).unwrap());
        assert!(
            !finalizer
                .advance_data(1, 20, &[4, 0, 0, 0, 0, 0, 0, 0])
                .unwrap()
        );
        assert!(!finalizer.advance_data(1, 38, &logon).unwrap());
        assert!(
            !finalizer
                .advance_data(1, 20, &[2, 0, 0, 0, 0, 0, 0, 0])
                .unwrap()
        );
        assert!(finalizer.advance_data(1, 40, &[0; 8]).unwrap());
    }
}
