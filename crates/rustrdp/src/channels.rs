//! Static and dynamic virtual channels, independent of transport and the OS clipboard.

pub(crate) mod audio;
pub(crate) mod audio_input;
mod clipboard;
mod device;
mod display;
mod dynamic;

use crate::{
    Channel, ClientConfig, ClipboardContent, ClipboardFile, ClipboardFileRequest, Error, Limits,
    Result,
};

use audio::{Audio, AudioEvent, AudioToken};
use clipboard::Clipboard;
use device::Device;
use dynamic::DynamicChannels;

const FIRST: u32 = 0x01;
const LAST: u32 = 0x02;
const SHOW_PROTOCOL: u32 = 0x10;
const SUSPEND: u32 = 0x20;
const RESUME: u32 = 0x40;

/// Actions for the session driver. `Send` includes the static channel header.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ChannelEvent {
    Send {
        channel_id: u16,
        data: Vec<u8>,
    },
    SendTunnel {
        protocol: crate::multitransport::TransportProtocol,
        payloads: Vec<Vec<u8>>,
    },
    TunnelSynchronized {
        channel_id: u16,
        fragments: Vec<Vec<u8>>,
    },
    ClipboardOffer,
    ClipboardText(String),
    ClipboardContent(ClipboardContent),
    ClipboardUnavailable,
    ClipboardPublished,
    ClipboardPublishRejected,
    ClipboardFileRequest(ClipboardFileRequest),
    ClipboardFilesReleased {
        offer_id: u64,
    },
    ClipboardFiles {
        offer_id: u64,
        files: Vec<ClipboardFile>,
    },
    ClipboardFileContents {
        request: ClipboardFileRequest,
        data: Option<Vec<u8>>,
    },
    ClipboardRemoteFilesCancelled {
        offer_id: u64,
    },
    Audio(AudioEvent),
    Capture(audio_input::CaptureEvent),
    DriveRequest(crate::DriveRequest),
    DisplayReady,
    #[cfg(feature = "graphics")]
    GraphicsOpened,
    #[cfg(feature = "graphics")]
    GraphicsData(Vec<u8>),
    #[cfg(feature = "graphics")]
    GraphicsClosed,
    RequestRejected(Error),
}

pub(crate) struct Channels {
    channels: Vec<StaticChannel>,
    clipboard: Clipboard,
    dynamic: DynamicChannels,
    audio: Audio,
    device: Device,
    limits: Limits,
    chunk_size: usize,
    suspended: bool,
    queued: Vec<ChannelEvent>,
    queued_bytes: usize,
}

struct StaticChannel {
    id: u16,
    kind: Kind,
    fragment: Option<Fragment>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Clipboard,
    Dynamic,
    Audio,
    Device,
}

struct Fragment {
    length: usize,
    data: Vec<u8>,
}

/// Channel-local actions, before static virtual channel fragmentation.
enum Action {
    Send(Vec<u8>),
    Synchronize(Vec<u8>),
    Event(ChannelEvent),
}

impl Channels {
    pub fn new(channels: &[Channel], config: &ClientConfig, chunk_size: usize) -> Result<Self> {
        let limits = config.limits;
        if channels.len() > limits.max_static_channels {
            return Err(Error::Limit("static channel count"));
        }
        let mut entries = Vec::with_capacity(channels.len());
        let mut clipboard = false;
        let mut dynamic = false;
        let mut audio = false;
        let mut device = false;
        for channel in channels {
            if entries
                .iter()
                .any(|entry: &StaticChannel| entry.id == channel.id)
            {
                return Err(Error::Invalid("duplicate static channel ID"));
            }
            let (kind, seen) = match channel.name.as_str() {
                "cliprdr" => (Kind::Clipboard, &mut clipboard),
                "drdynvc" => (Kind::Dynamic, &mut dynamic),
                "rdpsnd" => (Kind::Audio, &mut audio),
                "rdpdr" => (Kind::Device, &mut device),
                _ => return Err(Error::Unsupported("static channel")),
            };
            if *seen {
                return Err(Error::Invalid("duplicate static channel name"));
            }
            *seen = true;
            entries.push(StaticChannel {
                id: channel.id,
                kind,
                fragment: None,
            });
        }
        let mut result = Self {
            channels: entries,
            clipboard: Clipboard::new(
                limits.max_clipboard_bytes,
                config.clipboard_files,
                limits.max_channel_message,
            ),
            dynamic: DynamicChannels::new(limits),
            audio: Audio::new(limits.max_channel_message),
            device: Device::new(&config.client_name, config.shared_drives.clone()),
            limits,
            chunk_size: 0,
            suspended: false,
            queued: Vec::new(),
            queued_bytes: 0,
        };
        result.dynamic.configure(
            &config.monitors,
            config.primary_scale(),
            config.audio_input,
            config.dynamic_resolution,
            config.graphics_pipeline,
        );
        result.set_chunk_size(chunk_size)?;
        Ok(result)
    }

    pub fn set_chunk_size(&mut self, size: usize) -> Result<()> {
        if !(1600..=16256).contains(&size) || size > self.limits.max_pdu.saturating_sub(8) {
            return Err(Error::Limit("virtual channel chunk size"));
        }
        self.chunk_size = size;
        Ok(())
    }

    /// Session removes bulk compression before any channel-local reassembly.
    pub(crate) fn receive_parts(
        &mut self,
        channel_id: u16,
        length: usize,
        flags: u32,
        payload: &[u8],
    ) -> Result<Vec<ChannelEvent>> {
        let index = self
            .channels
            .iter()
            .position(|channel| channel.id == channel_id)
            .ok_or(Error::Invalid("unknown static channel ID"))?;
        if flags & !(FIRST | LAST | 0x10 | SUSPEND | RESUME | 0x80) != 0 {
            return Err(Error::Invalid("static channel flags"));
        }
        if flags & (SUSPEND | RESUME) != 0 {
            if length != 0
                || !payload.is_empty()
                || flags & (FIRST | LAST) != 0
                || flags & (SUSPEND | RESUME) == (SUSPEND | RESUME)
            {
                return Err(Error::Invalid("virtual channel flow control"));
            }
            self.suspended = flags & SUSPEND != 0;
            return if self.suspended {
                Ok(Vec::new())
            } else {
                self.queued_bytes = 0;
                Ok(std::mem::take(&mut self.queued))
            };
        }
        if length > self.limits.max_channel_message {
            return Err(Error::Limit("static channel message"));
        }
        let channel = &mut self.channels[index];
        if payload.len() > self.chunk_size {
            return Err(Error::Limit("static channel chunk"));
        }
        let Some(message) = reassemble(&mut channel.fragment, length, flags, payload)? else {
            return Ok(Vec::new());
        };
        let actions = match channel.kind {
            Kind::Clipboard => self.clipboard.receive(&message)?,
            Kind::Dynamic => self.dynamic.receive(&message)?,
            Kind::Audio => self.audio.receive(&message)?,
            Kind::Device => self.device.receive(&message)?,
        };
        self.events(channel_id, actions)
    }

    /// Publishes only text explicitly supplied by the caller; never reads a system clipboard.
    pub fn set_clipboard(&mut self, text: String) -> Result<Vec<ChannelEvent>> {
        let id = self.channel_id(Kind::Clipboard)?;
        let actions = self.clipboard.set_text(text)?;
        self.events(id, actions)
    }

    pub fn clipboard_ready(&self) -> bool {
        self.clipboard.local_ready()
    }

    pub fn set_clipboard_content(
        &mut self,
        content: ClipboardContent,
    ) -> Result<Vec<ChannelEvent>> {
        let id = self.channel_id(Kind::Clipboard)?;
        let actions = self.clipboard.set_content(content)?;
        self.events(id, actions)
    }

    pub fn set_clipboard_files(
        &mut self,
        offer_id: u64,
        files: Vec<ClipboardFile>,
    ) -> Result<Vec<ChannelEvent>> {
        let id = self.channel_id(Kind::Clipboard)?;
        let actions = self.clipboard.set_files(offer_id, files)?;
        self.events(id, actions)
    }

    pub fn clipboard_file_contents(
        &mut self,
        request_id: u64,
        data: Option<Vec<u8>>,
    ) -> Result<Vec<ChannelEvent>> {
        let id = self.channel_id(Kind::Clipboard)?;
        let actions = self.clipboard.file_contents(request_id, data)?;
        self.events(id, actions)
    }

    pub fn cancel_clipboard_files(&mut self) -> Result<Vec<ChannelEvent>> {
        let id = self.channel_id(Kind::Clipboard)?;
        let actions = self.clipboard.cancel_files()?;
        self.events(id, actions)
    }

    pub fn request_clipboard_file(
        &mut self,
        offer_id: u64,
        index: u32,
        offset: u64,
        length: u32,
    ) -> Result<Vec<ChannelEvent>> {
        let id = self.channel_id(Kind::Clipboard)?;
        let actions = self
            .clipboard
            .request_file(offer_id, index, offset, length)?;
        self.events(id, actions)
    }

    pub fn cancel_remote_files(&mut self, offer_id: u64) -> Result<Vec<ChannelEvent>> {
        let id = self.channel_id(Kind::Clipboard)?;
        let actions = self.clipboard.cancel_remote_files(offer_id)?;
        self.events(id, actions)
    }

    pub fn confirm_audio(
        &mut self,
        token: AudioToken,
        elapsed_ms: u32,
    ) -> Result<Vec<ChannelEvent>> {
        let id = self.channel_id(Kind::Audio)?;
        let actions = self.audio.confirm(token, elapsed_ms)?;
        self.events(id, actions)
    }

    pub fn request_clipboard(&mut self) -> Result<Vec<ChannelEvent>> {
        let id = self.channel_id(Kind::Clipboard)?;
        let actions = self.clipboard.request();
        self.events(id, actions)
    }

    /// Queues the latest size until the server opens and initializes Display Control.
    pub fn resize(&mut self, width: u16, height: u16) -> Result<Vec<ChannelEvent>> {
        let id = self.channel_id(Kind::Dynamic)?;
        let actions = self.dynamic.resize(width, height)?;
        self.events(id, actions)
    }

    pub fn monitor_layout(&mut self, monitors: Vec<crate::Monitor>) -> Result<Vec<ChannelEvent>> {
        let id = self.channel_id(Kind::Dynamic)?;
        let actions = self.dynamic.layout(monitors)?;
        self.events(id, actions)
    }
    pub fn confirm_capture(
        &mut self,
        generation: u64,
        available: bool,
    ) -> Result<Vec<ChannelEvent>> {
        let id = self.channel_id(Kind::Dynamic)?;
        let actions = self.dynamic.confirm_capture(generation, available)?;
        self.events(id, actions)
    }
    pub fn close_capture(&mut self) -> Result<Vec<ChannelEvent>> {
        let id = self.channel_id(Kind::Dynamic)?;
        let actions = self.dynamic.close_capture();
        self.events(id, actions)
    }
    pub fn capture_samples(
        &mut self,
        generation: u64,
        samples: &[u8],
    ) -> Result<Vec<ChannelEvent>> {
        let id = self.channel_id(Kind::Dynamic)?;
        let actions = self.dynamic.capture_samples(generation, samples)?;
        self.events(id, actions)
    }
    pub fn reset_capture(&mut self, generation: u64) -> Result<Vec<ChannelEvent>> {
        self.dynamic.reset_capture(generation);
        Ok(Vec::new())
    }

    pub(crate) fn tunnel_ready(
        &mut self,
        protocol: crate::multitransport::TransportProtocol,
        soft_sync: bool,
    ) -> Result<()> {
        self.dynamic.tunnel_ready(protocol, soft_sync)
    }

    pub(crate) fn tunnel_active(&self, protocol: crate::multitransport::TransportProtocol) -> bool {
        self.dynamic.tunnel_active(protocol)
    }

    pub(crate) fn tunnel_closed(
        &mut self,
        protocol: crate::multitransport::TransportProtocol,
    ) -> Result<bool> {
        Ok(self.dynamic.tunnel_closed(protocol))
    }

    pub(crate) fn receive_tunnel(
        &mut self,
        protocol: crate::multitransport::TransportProtocol,
        data: &[u8],
    ) -> Result<Vec<ChannelEvent>> {
        let id = self.channel_id(Kind::Dynamic)?;
        let actions = self.dynamic.receive_tunnel(protocol, data)?;
        self.events(id, actions)
    }
    pub fn complete_drive(
        &mut self,
        completion: crate::DriveCompletion,
    ) -> Result<Vec<ChannelEvent>> {
        let id = self.channel_id(Kind::Device)?;
        let actions = self.device.complete(completion)?;
        self.events(id, actions)
    }

    #[cfg(feature = "graphics")]
    pub(crate) fn send_graphics(&mut self, data: Vec<u8>) -> Result<Vec<ChannelEvent>> {
        let id = self.channel_id(Kind::Dynamic)?;
        let actions = self.dynamic.send_graphics(data)?;
        self.events(id, actions)
    }

    fn channel_id(&self, kind: Kind) -> Result<u16> {
        self.channels
            .iter()
            .find(|channel| channel.kind == kind)
            .map(|channel| channel.id)
            .ok_or(Error::Unsupported("channel was not negotiated"))
    }

    fn events(&mut self, channel_id: u16, actions: Vec<Action>) -> Result<Vec<ChannelEvent>> {
        let channel = self
            .channels
            .iter()
            .find(|channel| channel.id == channel_id)
            .ok_or(Error::Invalid("unknown static channel ID"))?;
        // Windows' clipboard endpoint consumes the channel header, including
        // on single-chunk messages; the dynamic-channel endpoint does not.
        let protocol_flag = match channel.kind {
            Kind::Clipboard => SHOW_PROTOCOL,
            Kind::Dynamic | Kind::Audio | Kind::Device => 0,
        };
        let mut events = Vec::new();
        for action in actions {
            let synchronize = matches!(action, Action::Synchronize(_));
            match action {
                Action::Event(event) => events.push(event),
                Action::Send(message) | Action::Synchronize(message) => {
                    if message.len() > self.limits.max_channel_message
                        || message.len() > u32::MAX as usize
                    {
                        return Err(Error::Limit("outgoing virtual channel message"));
                    }
                    let length = message.len() as u32;
                    let count = message.len().max(1).div_ceil(self.chunk_size);
                    let mut fragments = Vec::new();
                    for index in 0..count {
                        let start = index * self.chunk_size;
                        let end = (start + self.chunk_size).min(message.len());
                        let flags = if index == 0 { FIRST } else { 0 }
                            | if index + 1 == count { LAST } else { 0 }
                            | protocol_flag;
                        let mut data = Vec::with_capacity(8 + end - start);
                        data.extend_from_slice(&length.to_le_bytes());
                        data.extend_from_slice(&flags.to_le_bytes());
                        data.extend_from_slice(&message[start..end]);
                        if self.suspended {
                            self.queued_bytes = self
                                .queued_bytes
                                .checked_add(data.len())
                                .filter(|length| *length <= self.limits.max_channel_message)
                                .ok_or(Error::Backpressure)?;
                        }
                        if synchronize {
                            fragments.push(data);
                        } else if self.suspended {
                            self.queued.push(ChannelEvent::Send { channel_id, data });
                        } else {
                            events.push(ChannelEvent::Send { channel_id, data });
                        }
                    }
                    if synchronize {
                        let event = ChannelEvent::TunnelSynchronized {
                            channel_id,
                            fragments,
                        };
                        if self.suspended {
                            self.queued.push(event);
                        } else {
                            events.push(event);
                        }
                    }
                }
            }
        }
        Ok(events)
    }
}

fn reassemble(
    fragment: &mut Option<Fragment>,
    length: usize,
    flags: u32,
    data: &[u8],
) -> Result<Option<Vec<u8>>> {
    // MS-RDPBCGR 3.1.5.2.2 also permits an unmarked standalone chunk.
    if flags & (FIRST | LAST) == 0 && fragment.is_none() {
        if length != data.len() {
            return Err(Error::Invalid("static channel message length"));
        }
        return Ok(Some(data.to_vec()));
    }
    if flags & FIRST != 0 {
        if fragment.is_some() {
            return Err(Error::Invalid("overlapping static channel messages"));
        }
        if flags & LAST != 0 {
            if length != data.len() {
                return Err(Error::Invalid("static channel message length"));
            }
            return Ok(Some(data.to_vec()));
        }
        if data.is_empty() || data.len() >= length {
            return Err(Error::Invalid("static channel first fragment length"));
        }
        *fragment = Some(Fragment {
            length,
            data: data.to_vec(),
        });
        return Ok(None);
    }
    let current = fragment
        .as_mut()
        .ok_or(Error::Invalid("static channel fragment without first"))?;
    if length != current.length || data.is_empty() || data.len() > length - current.data.len() {
        return Err(Error::Invalid("static channel continuation length"));
    }
    current.data.extend_from_slice(data);
    if flags & LAST != 0 {
        if current.data.len() != length {
            return Err(Error::Invalid("incomplete static channel message"));
        }
        return Ok(fragment.take().map(|fragment| fragment.data));
    }
    if current.data.len() == length {
        return Err(Error::Invalid("missing static channel last flag"));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channels() -> Channels {
        Channels::new(
            &[
                Channel {
                    id: 1004,
                    name: "cliprdr".into(),
                },
                Channel {
                    id: 1005,
                    name: "drdynvc".into(),
                },
            ],
            &ClientConfig::default(),
            1600,
        )
        .unwrap()
    }

    // CHANNEL_FLAG_SHOW_PROTOCOL is present even on a single-chunk message.
    #[test]
    fn clipboard_initialization_and_text_response_expose_the_channel_header() {
        let mut channels = channels();
        let caps = [
            7, 0, 0, 0, 16, 0, 0, 0, 1, 0, 0, 0, 1, 0, 12, 0, 2, 0, 0, 0, 2, 0, 0, 0,
        ];
        assert!(
            channels
                .receive_parts(1004, 24, 3, &caps)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            channels
                .receive_parts(1004, 8, 3, &[1, 0, 0, 0, 0, 0, 0, 0])
                .unwrap(),
            [
                ChannelEvent::Send {
                    channel_id: 1004,
                    data: vec![
                        24, 0, 0, 0, 0x13, 0, 0, 0, 7, 0, 0, 0, 16, 0, 0, 0, 1, 0, 0, 0, 1, 0, 12,
                        0, 2, 0, 0, 0, 2, 0, 0, 0,
                    ],
                },
                ChannelEvent::Send {
                    channel_id: 1004,
                    data: vec![8, 0, 0, 0, 0x13, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0],
                },
            ]
        );
        let ack = [3, 0, 1, 0, 0, 0, 0, 0];
        assert!(channels.receive_parts(1004, 8, 3, &ack).unwrap().is_empty());
        assert_eq!(
            channels.set_clipboard("A\u{20ac}".into()).unwrap(),
            [
                ChannelEvent::ClipboardUnavailable,
                ChannelEvent::Send {
                    channel_id: 1004,
                    data: vec![
                        14, 0, 0, 0, 0x13, 0, 0, 0, 2, 0, 0, 0, 6, 0, 0, 0, 13, 0, 0, 0, 0, 0,
                    ],
                },
            ]
        );
        assert_eq!(
            channels.receive_parts(1004, 8, 3, &ack).unwrap(),
            [ChannelEvent::ClipboardPublished]
        );
        assert_eq!(
            channels
                .receive_parts(1004, 12, 3, &[4, 0, 0, 0, 4, 0, 0, 0, 13, 0, 0, 0])
                .unwrap(),
            [ChannelEvent::Send {
                channel_id: 1004,
                data: vec![
                    14, 0, 0, 0, 0x13, 0, 0, 0, 5, 0, 1, 0, 6, 0, 0, 0, 0x41, 0, 0xac, 0x20, 0, 0,
                ],
            }]
        );
    }

    #[test]
    fn fragment_headers_preserve_the_channel_protocol_on_every_chunk() {
        let mut channels = channels();
        // FreeRDP's transport takes SHOW_PROTOCOL from the channel option,
        // not from fragment count. drdynvc does not enable that option.
        for (channel_id, flags) in [(1004, [0x11, 0x10, 0x12]), (1005, [1, 0, 2])] {
            let events = channels
                .events(channel_id, vec![Action::Send(vec![42; 3201])])
                .unwrap();
            assert_eq!(events.len(), 3);
            for ((event, flag), size) in events.iter().zip(flags).zip([1600, 1600, 1]) {
                let ChannelEvent::Send {
                    channel_id: actual_channel_id,
                    data,
                } = event
                else {
                    panic!("missing fragment")
                };
                assert_eq!(*actual_channel_id, channel_id);
                assert_eq!(&data[..8], &[0x81, 0x0c, 0, 0, flag, 0, 0, 0]);
                assert_eq!(&data[8..], vec![42; size]);
            }
        }
        assert!(channels.set_chunk_size(0).is_err());
        assert!(channels.set_chunk_size(16257).is_err());
    }

    #[test]
    fn soft_sync_response_has_a_single_barrier_after_all_its_fragments() {
        let mut channels = channels();
        let actions = channels
            .events(1005, vec![Action::Synchronize(vec![0x90; 3201])])
            .unwrap();
        let [
            ChannelEvent::TunnelSynchronized {
                channel_id,
                fragments,
            },
        ] = actions.as_slice()
        else {
            panic!("missing synchronization response");
        };
        assert_eq!(*channel_id, 1005);
        assert_eq!(fragments.len(), 3);
        assert_eq!(fragments[0][4], FIRST as u8);
        assert_eq!(fragments[2][4], LAST as u8);
        channels.suspended = true;
        assert!(
            channels
                .events(1005, vec![Action::Synchronize(vec![0x90; 3201])])
                .unwrap()
                .is_empty()
        );
        assert!(
            matches!(channels.queued.as_slice(), [ChannelEvent::TunnelSynchronized { fragments, .. }] if fragments.len() == 3)
        );
    }

    #[test]
    fn dynamic_channel_capabilities_do_not_expose_the_channel_header() {
        let mut channels = channels();
        assert_eq!(
            channels
                .receive_parts(1005, 4, 3, &[0x50, 0, 1, 0])
                .unwrap(),
            [ChannelEvent::Send {
                channel_id: 1005,
                data: vec![4, 0, 0, 0, 3, 0, 0, 0, 0x50, 0, 1, 0],
            }]
        );
    }

    #[test]
    fn static_fragments_require_consistent_length_and_boundaries() {
        let mut fragment = None;
        assert_eq!(
            reassemble(&mut fragment, 2, 0, &[1, 2]).unwrap(),
            Some(vec![1, 2])
        );
        assert!(reassemble(&mut fragment, 3, 0, &[1, 2]).is_err());
        assert!(
            reassemble(&mut fragment, 5, FIRST, &[1, 2])
                .unwrap()
                .is_none()
        );
        assert!(reassemble(&mut fragment, 5, 0, &[3]).unwrap().is_none());
        assert_eq!(
            reassemble(&mut fragment, 5, LAST, &[4, 5]).unwrap(),
            Some(vec![1, 2, 3, 4, 5])
        );
        assert!(reassemble(&mut fragment, 3, LAST, &[1, 2, 3]).is_err());
        assert!(reassemble(&mut fragment, 3, FIRST | LAST, &[1, 2]).is_err());
        assert!(reassemble(&mut fragment, 3, FIRST, &[1]).is_ok());
        assert!(reassemble(&mut fragment, 4, LAST, &[2, 3]).is_err());
    }
}
