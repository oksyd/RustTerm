use crate::{
    Error, Limits, Monitor, Result,
    multitransport::{SoftSync, TransportProtocol},
    wire::Reader,
};
use std::collections::BTreeSet;

use super::{
    Action, ChannelEvent, Fragment,
    audio_input::{AudioInput, CaptureEvent},
    display::DisplayControl,
};

const DISPLAY_NAME: &[u8] = b"Microsoft::Windows::RDS::DisplayControl\0";
const GRAPHICS_NAME: &[u8] = b"Microsoft::Windows::RDS::Graphics\0";
const MAX_DVC_PDU: usize = 1600;
const CHANNEL_NOT_FOUND: u32 = 0x8007_0490;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Display,
    Audio,
    Graphics,
}

struct Channel {
    id: u32,
    kind: Kind,
    fragment: Option<Fragment>,
    incoming: Option<TransportProtocol>,
    outgoing: Option<TransportProtocol>,
}

pub(super) struct DynamicChannels {
    limits: Limits,
    negotiated: bool,
    channels: Vec<Channel>,
    display: DisplayControl,
    audio: AudioInput,
    display_enabled: bool,
    audio_enabled: bool,
    graphics_enabled: bool,
    ready: BTreeSet<TransportProtocol>,
    default_transport: Option<TransportProtocol>,
    migrated: BTreeSet<TransportProtocol>,
}

impl DynamicChannels {
    pub(super) fn new(limits: Limits) -> Self {
        Self {
            limits,
            negotiated: false,
            channels: Vec::new(),
            display: DisplayControl::new(limits),
            audio: AudioInput::default(),
            display_enabled: true,
            audio_enabled: false,
            graphics_enabled: false,
            ready: BTreeSet::new(),
            default_transport: None,
            migrated: BTreeSet::new(),
        }
    }

    pub(super) fn configure(
        &mut self,
        monitors: &[Monitor],
        scale: u32,
        audio: bool,
        display: bool,
        graphics: bool,
    ) {
        self.display.configure(monitors, scale);
        self.display_enabled = display;
        self.audio_enabled = audio;
        self.graphics_enabled = graphics && cfg!(feature = "graphics");
    }

    fn channel_id(&self, kind: Kind) -> Option<u32> {
        self.channels
            .iter()
            .find(|channel| channel.kind == kind)
            .map(|channel| channel.id)
    }

    pub(super) fn receive(&mut self, data: &[u8]) -> Result<Vec<Action>> {
        self.receive_from(data, None)
    }

    pub(super) fn receive_tunnel(
        &mut self,
        protocol: TransportProtocol,
        data: &[u8],
    ) -> Result<Vec<Action>> {
        if !self.ready.contains(&protocol) {
            return Err(Error::Invalid("dynamic channel on unavailable tunnel"));
        }
        self.receive_from(data, Some(protocol))
    }

    fn receive_from(
        &mut self,
        data: &[u8],
        origin: Option<TransportProtocol>,
    ) -> Result<Vec<Action>> {
        if data.first().is_some_and(|byte| byte >> 4 == 8) {
            if origin.is_some() {
                return Err(Error::Invalid("soft-sync outside TCP"));
            }
            return self.soft_sync(data);
        }
        if data.len() > MAX_DVC_PDU {
            return Err(Error::Limit("dynamic channel PDU"));
        }
        let mut reader = Reader::new(data);
        let header = reader.u8()?;
        let command = header >> 4;
        let selector = header & 3;
        let auxiliary = header >> 2 & 3;
        if origin == Some(TransportProtocol::Lossy) && !matches!(command, 1 | 3 | 4) {
            return Err(Error::Invalid(
                "fragment or compression on lossy dynamic channel",
            ));
        }
        if command == 5 {
            if self.negotiated || selector != 0 || reader.u8()? != 0 {
                return Err(Error::Invalid("dynamic channel capabilities"));
            }
            let version = reader.u16_le()?;
            match version {
                1 => {}
                2 | 3 => reader.skip(8)?,
                _ => return Err(Error::Unsupported("dynamic channel version")),
            }
            reader.finish()?;
            self.negotiated = true;
            return Ok(vec![control(
                vec![0x50, 0, version.min(2) as u8, 0],
                origin,
            )]);
        }
        if !self.negotiated {
            return Err(Error::Invalid("dynamic channel before capabilities"));
        }
        let id = variable(&mut reader, selector)?;
        if command == 1 {
            return self.create(id, reader.remaining(), origin);
        }
        if command == 4 {
            reader.finish()?;
            return Ok(self.close(id));
        }
        if !matches!(command, 2 | 3) {
            return Err(Error::Unsupported("dynamic channel command"));
        }
        let index = self
            .channels
            .iter()
            .position(|channel| channel.id == id)
            .ok_or(Error::Invalid("unknown dynamic channel ID"))?;
        if self.channels[index].incoming != origin {
            return Err(Error::Invalid("dynamic channel transport mismatch"));
        }
        if origin == Some(TransportProtocol::Lossy) && self.channels[index].fragment.is_some() {
            return Err(Error::Invalid(
                "fragment migration to lossy dynamic channel",
            ));
        }
        let kind = self.channels[index].kind;
        let maximum = match kind {
            Kind::Display => 64 * 1024,
            Kind::Audio => 256 * 1024 + 1,
            Kind::Graphics => self.limits.max_channel_message,
        }
        .min(self.limits.max_channel_message);
        let first = if command == 2 {
            Some(variable(&mut reader, auxiliary)? as usize)
        } else {
            None
        };
        let Some(message) = reassemble(
            &mut self.channels[index].fragment,
            first,
            reader.remaining(),
            maximum,
        )?
        else {
            return Ok(Vec::new());
        };
        match kind {
            Kind::Display => wrap_for(
                id,
                self.display.receive(&message)?,
                maximum,
                self.outgoing(id),
            ),
            Kind::Audio => wrap_for(
                id,
                self.audio.receive(&message)?,
                maximum,
                self.outgoing(id),
            ),
            Kind::Graphics => {
                #[cfg(feature = "graphics")]
                {
                    Ok(vec![Action::Event(ChannelEvent::GraphicsData(message))])
                }
                #[cfg(not(feature = "graphics"))]
                {
                    Err(Error::Unsupported("graphics feature is disabled"))
                }
            }
        }
    }

    fn create(
        &mut self,
        id: u32,
        name: &[u8],
        origin: Option<TransportProtocol>,
    ) -> Result<Vec<Action>> {
        if self.channels.iter().any(|channel| channel.id == id) {
            return Err(Error::Invalid("duplicate dynamic channel ID"));
        }
        if name.is_empty()
            || name.len() > 256
            || name.last() != Some(&0)
            || name[..name.len() - 1].contains(&0)
        {
            return Err(Error::Invalid("dynamic channel name"));
        }
        let kind = match name {
            DISPLAY_NAME if self.display_enabled => Some(Kind::Display),
            b"AUDIO_INPUT\0" if self.audio_enabled => Some(Kind::Audio),
            GRAPHICS_NAME if self.graphics_enabled => Some(Kind::Graphics),
            _ => None,
        }
        .filter(|kind| self.channel_id(*kind).is_none())
        // These listeners exchange stateful, potentially fragmented messages.
        // Declining CREATE still requires a response on its original transport.
        .filter(|_| origin != Some(TransportProtocol::Lossy));
        let mut response = header_with_id(1, id);
        response
            .extend_from_slice(&(if kind.is_some() { 0 } else { CHANNEL_NOT_FOUND }).to_le_bytes());
        let mut actions = Vec::with_capacity(2);
        actions.push(control(response, origin));
        if let Some(kind) = kind {
            self.channels.push(Channel {
                id,
                kind,
                fragment: None,
                incoming: origin.or(self.default_transport),
                outgoing: origin.or(self.default_transport),
            });
            match kind {
                Kind::Display => self.display.closed(),
                Kind::Audio => self.audio.close(),
                Kind::Graphics => {
                    #[cfg(feature = "graphics")]
                    actions.push(Action::Event(ChannelEvent::GraphicsOpened));
                }
            }
        }
        Ok(actions)
    }

    fn close(&mut self, id: u32) -> Vec<Action> {
        let Some(index) = self.channels.iter().position(|channel| channel.id == id) else {
            return Vec::new();
        };
        let channel = self.channels.swap_remove(index);
        let mut actions = vec![control(header_with_id(4, id), channel.outgoing)];
        match channel.kind {
            Kind::Display => self.display.closed(),
            Kind::Audio => {
                self.audio.close();
                actions.push(Action::Event(ChannelEvent::Capture(CaptureEvent::Close)));
            }
            Kind::Graphics => {
                #[cfg(feature = "graphics")]
                actions.push(Action::Event(ChannelEvent::GraphicsClosed));
            }
        }
        actions
    }

    fn outgoing(&self, id: u32) -> Option<TransportProtocol> {
        self.channels
            .iter()
            .find(|channel| channel.id == id)
            .and_then(|channel| channel.outgoing)
    }

    pub(super) fn reset_capture(&mut self, generation: u64) {
        self.audio.reset(generation);
    }

    pub(super) fn tunnel_ready(
        &mut self,
        protocol: TransportProtocol,
        soft_sync: bool,
    ) -> Result<()> {
        if !self.ready.insert(protocol) {
            return Err(Error::Invalid("duplicate ready tunnel"));
        }
        if !soft_sync && protocol == TransportProtocol::Reliable {
            let selected = TransportProtocol::Reliable;
            self.default_transport = Some(selected);
            self.migrated.insert(selected);
            for channel in &mut self.channels {
                channel.incoming = Some(selected);
                channel.outgoing = Some(selected);
            }
        }
        Ok(())
    }

    pub(super) fn tunnel_closed(&mut self, protocol: TransportProtocol) -> bool {
        self.ready.remove(&protocol);
        self.migrated.contains(&protocol)
    }

    pub(super) fn tunnel_active(&self, protocol: TransportProtocol) -> bool {
        self.migrated.contains(&protocol)
    }

    fn soft_sync(&mut self, data: &[u8]) -> Result<Vec<Action>> {
        if !self.negotiated || self.ready.is_empty() {
            return Err(Error::Invalid("soft-sync before tunnel creation"));
        }
        let sync = SoftSync::decode(data)?;
        // Client messages include audio blocks and must be delivered whole.
        // Keep TCP when reliable UDP is unavailable; UDP-L cannot carry fragments.
        let outgoing = self
            .ready
            .contains(&TransportProtocol::Reliable)
            .then_some(TransportProtocol::Reliable);
        for (protocol, ids) in &sync.channels {
            if !self.ready.contains(protocol)
                || ids
                    .iter()
                    .any(|id| !self.channels.iter().any(|channel| channel.id == *id))
                || (*protocol == TransportProtocol::Lossy
                    && self
                        .channels
                        .iter()
                        .any(|channel| ids.contains(&channel.id) && channel.fragment.is_some()))
            {
                return Err(Error::Invalid("soft-sync unestablished channel or tunnel"));
            }
        }
        if sync.channels.is_empty() {
            self.default_transport = outgoing;
            for channel in &mut self.channels {
                channel.incoming = outgoing;
                channel.outgoing = outgoing;
            }
        } else {
            for (protocol, ids) in sync.channels {
                for channel in &mut self.channels {
                    if ids.contains(&channel.id) {
                        channel.incoming = Some(protocol);
                        channel.outgoing = outgoing;
                    }
                }
                self.migrated.insert(protocol);
            }
        }
        self.migrated.extend(outgoing);
        // This TCP response precedes every following outbound tunnel event.
        Ok(vec![Action::Synchronize(SoftSync::response(
            outgoing.as_slice(),
        )?)])
    }

    pub(super) fn resize(&mut self, width: u16, height: u16) -> Result<Vec<Action>> {
        let actions = self.display.resize(width, height)?;
        self.wrap_display(actions)
    }
    pub(super) fn layout(&mut self, monitors: Vec<Monitor>) -> Result<Vec<Action>> {
        let actions = self.display.layout(monitors)?;
        self.wrap_display(actions)
    }
    fn wrap_display(&self, actions: Vec<Action>) -> Result<Vec<Action>> {
        match self.channel_id(Kind::Display) {
            Some(id) => wrap_for(
                id,
                actions,
                self.limits.max_channel_message,
                self.outgoing(id),
            ),
            None => Ok(actions),
        }
    }
    #[cfg(feature = "graphics")]
    pub(super) fn send_graphics(&self, data: Vec<u8>) -> Result<Vec<Action>> {
        let id = self
            .channel_id(Kind::Graphics)
            .ok_or(Error::Unsupported("graphics channel is not open"))?;
        wrap_for(
            id,
            vec![Action::Send(data)],
            self.limits.max_channel_message,
            self.outgoing(id),
        )
    }
    pub(super) fn confirm_capture(
        &mut self,
        generation: u64,
        available: bool,
    ) -> Result<Vec<Action>> {
        let Some(id) = self.channel_id(Kind::Audio) else {
            return Ok(Vec::new());
        };
        let actions = self.audio.confirm(generation, available)?;
        let failed = !available && !actions.is_empty();
        let mut actions = wrap_for(
            id,
            actions,
            self.limits.max_channel_message,
            self.outgoing(id),
        )?;
        if failed {
            actions.extend(self.close_capture());
        }
        Ok(actions)
    }
    pub(super) fn close_capture(&mut self) -> Vec<Action> {
        self.channel_id(Kind::Audio)
            .map(|id| self.close(id))
            .unwrap_or_default()
    }
    pub(super) fn capture_samples(
        &mut self,
        generation: u64,
        samples: &[u8],
    ) -> Result<Vec<Action>> {
        let Some(id) = self.channel_id(Kind::Audio) else {
            return Ok(Vec::new());
        };
        wrap_for(
            id,
            self.audio.samples(generation, samples)?,
            self.limits.max_channel_message,
            self.outgoing(id),
        )
    }
}

fn reassemble(
    fragment: &mut Option<Fragment>,
    first: Option<usize>,
    data: &[u8],
    maximum: usize,
) -> Result<Option<Vec<u8>>> {
    if let Some(length) = first {
        if length > maximum {
            return Err(Error::Limit("dynamic channel message"));
        }
        if fragment.is_some() || data.is_empty() || length < data.len() {
            return Err(Error::Invalid("dynamic channel first fragment"));
        }
        if length == data.len() {
            return Ok(Some(data.to_vec()));
        }
        *fragment = Some(Fragment {
            length,
            data: data.to_vec(),
        });
        return Ok(None);
    }
    if let Some(current) = fragment {
        if data.is_empty() || data.len() > current.length - current.data.len() {
            return Err(Error::Invalid("dynamic channel continuation"));
        }
        current.data.extend_from_slice(data);
        return Ok((current.data.len() == current.length)
            .then(|| fragment.take().expect("complete fragment").data));
    }
    if data.len() > maximum {
        return Err(Error::Limit("dynamic channel message"));
    }
    Ok(Some(data.to_vec()))
}

fn variable(reader: &mut Reader<'_>, selector: u8) -> Result<u32> {
    match selector {
        0 => reader.u8().map(u32::from),
        1 => reader.u16_le().map(u32::from),
        2 => reader.u32_le(),
        _ => Err(Error::Invalid("dynamic channel integer width")),
    }
}

fn header_with_id(command: u8, id: u32) -> Vec<u8> {
    let mut header = Vec::with_capacity(5);
    if let Ok(value) = u8::try_from(id) {
        header.extend_from_slice(&[command << 4, value]);
    } else if let Ok(value) = u16::try_from(id) {
        header.push(command << 4 | 1);
        header.extend_from_slice(&value.to_le_bytes());
    } else {
        header.push(command << 4 | 2);
        header.extend_from_slice(&id.to_le_bytes());
    }
    header
}

fn control(data: Vec<u8>, protocol: Option<TransportProtocol>) -> Action {
    match protocol {
        Some(protocol) => Action::Event(ChannelEvent::SendTunnel {
            protocol,
            payloads: vec![data],
        }),
        None => Action::Send(data),
    }
}

fn wrap_for(
    id: u32,
    actions: Vec<Action>,
    maximum: usize,
    protocol: Option<TransportProtocol>,
) -> Result<Vec<Action>> {
    let mut output = Vec::new();
    let limit = if protocol == Some(TransportProtocol::Lossy) {
        768
    } else {
        MAX_DVC_PDU
    };
    for action in actions {
        match action {
            Action::Event(event) => output.push(Action::Event(event)),
            Action::Synchronize(_) => {
                return Err(Error::Invalid("nested dynamic channel synchronization"));
            }
            Action::Send(bytes) => {
                if bytes.len() > maximum {
                    return Err(Error::Limit("outgoing dynamic channel message"));
                }
                let continuation = header_with_id(3, id);
                let capacity = limit - continuation.len();
                if protocol == Some(TransportProtocol::Lossy) && bytes.len() > capacity {
                    return Err(Error::Limit("fragmented lossy dynamic channel message"));
                }
                let mut payloads = Vec::new();
                if bytes.len() <= capacity {
                    let mut packet = continuation;
                    packet.extend(bytes);
                    payloads.push(packet);
                } else {
                    let mut first = header_with_id(2, id);
                    first[0] |= 2 << 2;
                    first.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
                    let initial = limit - first.len();
                    first.extend_from_slice(&bytes[..initial]);
                    payloads.push(first);
                    for chunk in bytes[initial..].chunks(capacity) {
                        let mut packet = continuation.clone();
                        packet.extend_from_slice(chunk);
                        payloads.push(packet);
                    }
                }
                if let Some(protocol) = protocol {
                    output.push(Action::Event(ChannelEvent::SendTunnel {
                        protocol,
                        payloads,
                    }));
                } else {
                    output.extend(payloads.into_iter().map(Action::Send));
                }
            }
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create(display: &mut DynamicChannels, id: u32) {
        display
            .receive(&[0x50, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0])
            .unwrap();
        let mut request = header_with_id(1, id);
        request.extend_from_slice(DISPLAY_NAME);
        let response = display.receive(&request).unwrap();
        assert!(
            matches!(&response[0], Action::Send(bytes) if bytes[bytes.len()-4..] == [0, 0, 0, 0])
        );
    }

    fn caps(id: u32) -> Vec<u8> {
        let mut caps = header_with_id(3, id);
        for value in [5_u32, 20, 1, 3840, 2160] {
            caps.extend_from_slice(&value.to_le_bytes());
        }
        caps
    }

    fn sync(id: u32, protocol: TransportProtocol) -> Vec<u8> {
        let mut packet = vec![0x80, 0, 18, 0, 0, 0, 3, 0, 1, 0];
        packet.extend_from_slice(&protocol.tunnel_type().to_le_bytes());
        packet.extend_from_slice(&1u16.to_le_bytes());
        packet.extend_from_slice(&id.to_le_bytes());
        packet
    }

    #[test]
    fn lossy_incoming_keeps_fragmented_outgoing_messages_on_tcp() {
        let mut channels = DynamicChannels::new(Limits::default());
        create(&mut channels, 7);
        channels
            .tunnel_ready(TransportProtocol::Lossy, true)
            .unwrap();
        let actions = channels
            .receive(&sync(7, TransportProtocol::Lossy))
            .unwrap();
        assert!(
            matches!(&actions[0], Action::Synchronize(bytes) if bytes == &[0x90, 0, 0, 0, 0, 0])
        );
        assert!(channels.tunnel_active(TransportProtocol::Lossy));
        assert_eq!(channels.outgoing(7), None);
        let actions = wrap_for(
            7,
            vec![Action::Send(vec![9; 3000])],
            4096,
            channels.outgoing(7),
        )
        .unwrap();
        assert_eq!(actions.len(), 2);
        assert!(
            actions
                .iter()
                .all(|action| matches!(action, Action::Send(_)))
        );
        assert!(
            channels
                .receive_tunnel(TransportProtocol::Lossy, &[0x20, 7, 20, 5])
                .is_err()
        );
        assert!(
            channels
                .receive_tunnel(TransportProtocol::Lossy, &caps(7))
                .is_ok()
        );
        assert!(channels.channels[0].fragment.is_none());
        let mut create = vec![0x10, 8];
        create.extend_from_slice(DISPLAY_NAME);
        let response = channels
            .receive_tunnel(TransportProtocol::Lossy, &create)
            .unwrap();
        assert!(
            matches!(&response[0], Action::Event(ChannelEvent::SendTunnel { protocol: TransportProtocol::Lossy, payloads })
            if payloads == &[vec![0x10, 8, 0x90, 4, 7, 0x80]])
        );
    }

    #[test]
    fn soft_sync_prefers_reliable_outgoing_and_rejects_partial_lossy_migration() {
        let mut channels = DynamicChannels::new(Limits::default());
        create(&mut channels, 7);
        channels
            .tunnel_ready(TransportProtocol::Lossy, true)
            .unwrap();
        channels
            .tunnel_ready(TransportProtocol::Reliable, true)
            .unwrap();
        channels.receive(&[0x20, 7, 20, 5]).unwrap();
        assert!(
            channels
                .receive(&sync(7, TransportProtocol::Lossy))
                .is_err()
        );
        channels.channels[0].fragment = None;
        let actions = channels
            .receive(&sync(7, TransportProtocol::Lossy))
            .unwrap();
        assert!(
            matches!(&actions[0], Action::Synchronize(bytes) if bytes == &[0x90, 0, 1, 0, 0, 0, 1, 0, 0, 0])
        );
        assert_eq!(channels.outgoing(7), Some(TransportProtocol::Reliable));
        assert!(
            wrap_for(
                7,
                vec![Action::Send(vec![0; 767])],
                4096,
                Some(TransportProtocol::Lossy)
            )
            .is_err()
        );
    }

    #[test]
    fn latest_resize_waits_for_display_capabilities() {
        let mut display = DynamicChannels::new(Limits::default());
        assert!(display.resize(1280, 720).unwrap().is_empty());
        assert!(display.resize(1920, 1080).unwrap().is_empty());
        create(&mut display, 70_000);
        let actions = display.receive(&caps(70_000)).unwrap();
        assert!(matches!(
            actions[0],
            Action::Event(ChannelEvent::DisplayReady)
        ));
        let Action::Send(layout) = &actions[1] else {
            panic!("missing layout")
        };
        assert_eq!(&layout[..5], &[0x32, 0x70, 0x11, 1, 0]);
        assert_eq!(u32::from_le_bytes(layout[33..37].try_into().unwrap()), 1920);
        assert_eq!(u32::from_le_bytes(layout[37..41].try_into().unwrap()), 1080);
        assert!(display.resize(1001, 720).is_err());
        assert!(display.resize(4000, 2160).is_err());
    }

    #[test]
    fn resizing_retains_desktop_scale_before_and_after_channel_negotiation() {
        let mut display = DynamicChannels::new(Limits::default());
        display.configure(&[], 150, false, true, false);
        assert!(display.resize(1280, 720).unwrap().is_empty());
        create(&mut display, 7);
        let actions = display.receive(&caps(7)).unwrap();
        let Action::Send(layout) = &actions[1] else {
            panic!("missing layout")
        };
        assert_eq!(&layout[50..54], &150u32.to_le_bytes());
        assert_eq!(&layout[54..58], &100u32.to_le_bytes());
        let actions = display.resize(1920, 1080).unwrap();
        let Action::Send(layout) = &actions[0] else {
            panic!("missing layout")
        };
        assert_eq!(&layout[50..54], &150u32.to_le_bytes());
        display
            .layout(vec![Monitor {
                scale: 200,
                ..Monitor::primary(1280, 720)
            }])
            .unwrap();
        let actions = display.resize(1920, 1080).unwrap();
        let Action::Send(layout) = &actions[0] else {
            panic!("missing layout")
        };
        assert_eq!(&layout[50..54], &200u32.to_le_bytes());
    }

    #[test]
    fn rejects_unregistered_channels_and_unbounded_fragments() {
        let mut display = DynamicChannels::new(Limits::default());
        create(&mut display, 7);
        let mut request = vec![0x10, 8];
        request.extend_from_slice(b"audio\x80\0"); // Unsupported ANSI name, still a valid create request.
        let response = display.receive(&request).unwrap();
        assert!(
            matches!(&response[0], Action::Send(bytes) if bytes[2..] == CHANNEL_NOT_FOUND.to_le_bytes())
        );
        assert!(
            display
                .receive(&[0x28, 7, 0xff, 0xff, 0xff, 0x7f, 0])
                .is_err()
        );
        assert!(display.receive(&[0x30, 8, 0]).is_err());
        assert!(display.receive(&[0x40, 8]).unwrap().is_empty());
    }

    #[test]
    fn server_capabilities_reject_pending_size_without_losing_the_channel() {
        let mut display = DynamicChannels::new(Limits::default());
        display.resize(1920, 1080).unwrap();
        create(&mut display, 7);
        let mut caps = header_with_id(3, 7);
        for value in [5_u32, 20, 1, 1280, 720] {
            caps.extend_from_slice(&value.to_le_bytes());
        }
        let actions = display.receive(&caps).unwrap();
        assert!(matches!(
            actions[0],
            Action::Event(ChannelEvent::DisplayReady)
        ));
        assert!(matches!(
            actions[1],
            Action::Event(ChannelEvent::RequestRejected(Error::Limit(
                "server display area"
            )))
        ));
        assert_eq!(actions.len(), 2);
        assert_eq!(display.resize(1280, 720).unwrap().len(), 1);
    }

    #[test]
    fn reassembles_display_caps_across_dynamic_fragments() {
        let mut display = DynamicChannels::new(Limits::default());
        create(&mut display, 7);
        let caps = caps(7);
        let mut first = vec![0x20, 7, 20];
        first.extend_from_slice(&caps[2..10]);
        assert!(display.receive(&first).unwrap().is_empty());
        let mut last = vec![0x30, 7];
        last.extend_from_slice(&caps[10..]);
        let actions = display.receive(&last).unwrap();
        assert!(matches!(
            actions[0],
            Action::Event(ChannelEvent::DisplayReady)
        ));
    }
    #[test]
    fn sends_two_monitors_after_capabilities_and_keeps_audio_channel_separate() {
        let mut display = DynamicChannels::new(Limits::default());
        display.configure(&[], 100, true, true, false);
        display.receive(&[0x50, 0, 1, 0]).unwrap();
        let mut create = vec![0x10, 1];
        create.extend_from_slice(DISPLAY_NAME);
        display.receive(&create).unwrap();
        let mut audio = vec![0x10, 2];
        audio.extend_from_slice(b"AUDIO_INPUT\0");
        display.receive(&audio).unwrap();
        assert_eq!(display.receive(&[0x30, 2, 1, 2, 0, 0, 0]).unwrap().len(), 1);
        display
            .layout(vec![
                Monitor::primary(1280, 720),
                Monitor {
                    left: 1280,
                    top: 0,
                    width: 1280,
                    height: 720,
                    primary: false,
                    scale: 100,
                },
            ])
            .unwrap();
        let mut caps = vec![0x30, 1];
        for value in [5u32, 20, 2, 4096, 2160] {
            caps.extend_from_slice(&value.to_le_bytes());
        }
        let actions = display.receive(&caps).unwrap();
        let Action::Send(layout) = actions.last().unwrap() else {
            panic!("monitor layout")
        };
        assert_eq!(u32::from_le_bytes(layout[14..18].try_into().unwrap()), 2);
        assert_eq!(layout.len(), 98);
    }
}
