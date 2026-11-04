//! MS-RDPEFS negotiation and bounded caller-owned drive I/O.

use crate::{Error, Result, wire::Reader};

use super::{Action, ChannelEvent};
use crate::{DriveCompletion, DriveRequest};
use std::collections::BTreeMap;

const CORE: u16 = 0x4472;
const SERVER_ANNOUNCE: u16 = 0x496e;
const CLIENT_CONFIRM: u16 = 0x4343;
const CLIENT_NAME: u16 = 0x434e;
const SERVER_CAPABILITY: u16 = 0x5350;
const CLIENT_CAPABILITY: u16 = 0x4350;
const DEVICES: u16 = 0x4441;
const LOGGED_ON: u16 = 0x554c;
const MAX_CAPABILITIES: usize = 64;

pub(super) struct Device {
    name: String,
    identity: Option<(u16, u32)>,
    capabilities: bool,
    confirmed: bool,
    announced: bool,
    drives: Vec<String>,
    pending: BTreeMap<u32, u32>,
    generation: u64,
}

impl Device {
    pub(super) fn new(client_name: &str, drives: Vec<String>) -> Self {
        Self {
            name: client_name.into(),
            identity: None,
            capabilities: false,
            confirmed: false,
            announced: false,
            drives,
            pending: BTreeMap::new(),
            generation: 0,
        }
    }

    pub(super) fn receive(&mut self, data: &[u8]) -> Result<Vec<Action>> {
        let mut reader = Reader::new(data);
        if reader.u16_le()? != CORE {
            return Err(Error::Unsupported("device channel component"));
        }
        match reader.u16_le()? {
            SERVER_ANNOUNCE => {
                let identity = identity(&mut reader)?;
                reader.finish()?;
                self.identity = Some(identity);
                self.capabilities = false;
                self.confirmed = false;
                self.announced = false;
                self.pending.clear();
                self.generation = self
                    .generation
                    .checked_add(1)
                    .ok_or(Error::Limit("device negotiation epoch"))?;
                let mut reply = 1u16.to_le_bytes().to_vec();
                reply.extend_from_slice(&identity.0.to_le_bytes());
                reply.extend_from_slice(&identity.1.to_le_bytes());
                let mut name = 1u32.to_le_bytes().to_vec();
                name.extend_from_slice(&0u32.to_le_bytes());
                let text: Vec<u8> = self
                    .name
                    .encode_utf16()
                    .chain(std::iter::once(0))
                    .flat_map(u16::to_le_bytes)
                    .collect();
                name.extend_from_slice(&(text.len() as u32).to_le_bytes());
                name.extend_from_slice(&text);
                Ok(vec![
                    Action::Send(pdu(CLIENT_CONFIRM, &reply)),
                    Action::Send(pdu(CLIENT_NAME, &name)),
                ])
            }
            SERVER_CAPABILITY => {
                let (minor, _) = self
                    .identity
                    .ok_or(Error::Invalid("device capabilities before announce"))?;
                let count = usize::from(reader.u16_le()?);
                reader.skip(2)?;
                if count > MAX_CAPABILITIES {
                    return Err(Error::Limit("device capabilities"));
                }
                let mut general = None;
                for _ in 0..count {
                    let kind = reader.u16_le()?;
                    let length = usize::from(reader.u16_le()?);
                    let version = reader.u32_le()?;
                    let body = reader.take(
                        length
                            .checked_sub(8)
                            .ok_or(Error::Invalid("device capability length"))?,
                    )?;
                    if kind == 1 {
                        if general.is_some() || !matches!((version, length), (1, 40) | (2, 44)) {
                            return Err(Error::Invalid("general device capability"));
                        }
                        let mut fields = Reader::new(body);
                        fields.skip(8)?;
                        if fields.u16_le()? != 1 {
                            return Err(Error::Invalid("device capability major version"));
                        }
                        fields.skip(body.len() - 10)?;
                        fields.finish()?;
                        general = Some(version);
                    }
                }
                reader.finish()?;
                let version = general.ok_or(Error::Invalid("missing general device capability"))?;
                let mut response = Vec::new();
                response.extend_from_slice(
                    &(if self.drives.is_empty() { 1u16 } else { 2 }).to_le_bytes(),
                );
                response.extend_from_slice(&0u16.to_le_bytes());
                response.extend_from_slice(&1u16.to_le_bytes());
                response.extend_from_slice(&(if version == 2 { 44u16 } else { 40 }).to_le_bytes());
                response.extend_from_slice(&version.to_le_bytes());
                response.extend_from_slice(&[0; 8]); // OS type/version ignored by server.
                response.extend_from_slice(&1u16.to_le_bytes());
                response.extend_from_slice(&minor.to_le_bytes());
                response.extend_from_slice(&0x3fffu32.to_le_bytes()); // Reserved mandatory I/O flags.
                response.extend_from_slice(&0u32.to_le_bytes());
                response.extend_from_slice(&6u32.to_le_bytes()); // DISPLAY_NAME | USER_LOGGEDON.
                response.extend_from_slice(&[0; 8]); // No asynchronous device I/O.
                if version == 2 {
                    response.extend_from_slice(&0u32.to_le_bytes());
                }
                if !self.drives.is_empty() {
                    response.extend_from_slice(&[4, 0, 8, 0, 2, 0, 0, 0]);
                }
                self.capabilities = true;
                let mut actions = vec![Action::Send(pdu(CLIENT_CAPABILITY, &response))];
                self.announce_devices(&mut actions);
                Ok(actions)
            }
            CLIENT_CONFIRM => {
                let actual = identity(&mut reader)?;
                reader.finish()?;
                if Some(actual) != self.identity {
                    return Err(Error::Invalid("device client identity confirmation"));
                }
                self.confirmed = true;
                let mut actions = Vec::new();
                self.announce_devices(&mut actions);
                Ok(actions)
            }
            LOGGED_ON => {
                reader.finish()?;
                if self.identity.is_none() || !self.capabilities {
                    return Err(Error::Invalid("device logon before capabilities"));
                }
                // A logon notification starts the device announcement phase even
                // if a prior logon already announced the empty device set.
                self.announced = true;
                Ok(vec![Action::Send(self.device_list())])
            }
            0x6472 => {
                let id = reader.u32_le()?;
                let status = reader.u32_le()?;
                reader.finish()?;
                if id == 0 || id as usize > self.drives.len() {
                    return Err(Error::Invalid("device announcement response"));
                }
                if status != 0 {
                    return Ok(vec![Action::Event(ChannelEvent::RequestRejected(
                        Error::Server(status),
                    ))]);
                }
                Ok(Vec::new())
            }
            0x4952 => {
                let device_id = reader.u32_le()?;
                let file_id = reader.u32_le()?;
                let completion_id = reader.u32_le()?;
                let major = reader.u32_le()?;
                let minor = reader.u32_le()?;
                if !self.announced || device_id == 0 || device_id as usize > self.drives.len() {
                    return Err(Error::Invalid("unannounced drive request"));
                }
                if reader.remaining().len() > 1024 * 1024 {
                    return Err(Error::Limit("drive request body"));
                }
                if self.pending.len() >= 256 {
                    return Err(Error::Limit("pending drive requests"));
                }
                if self.pending.insert(completion_id, device_id).is_some() {
                    return Err(Error::Invalid("duplicate drive request"));
                }
                Ok(vec![Action::Event(ChannelEvent::DriveRequest(
                    DriveRequest {
                        generation: self.generation,
                        device_id,
                        file_id,
                        completion_id,
                        major,
                        minor,
                        data: reader.remaining().to_vec(),
                    },
                ))])
            }
            _ => Err(Error::Unsupported("device redirection request")),
        }
    }

    pub fn complete(&mut self, completion: DriveCompletion) -> Result<Vec<Action>> {
        if completion.generation != self.generation {
            return Ok(Vec::new());
        }
        if self.pending.get(&completion.completion_id) != Some(&completion.device_id) {
            return Err(Error::Invalid("drive completion identity"));
        }
        if completion.data.len() > 1024 * 1024 {
            return Err(Error::Limit("drive completion"));
        }
        self.pending.remove(&completion.completion_id);
        let mut body = completion.device_id.to_le_bytes().to_vec();
        body.extend_from_slice(&completion.completion_id.to_le_bytes());
        body.extend_from_slice(&completion.status.to_le_bytes());
        body.extend(completion.data);
        Ok(vec![Action::Send(pdu(0x4943, &body))])
    }
    fn device_list(&self) -> Vec<u8> {
        let mut body = (self.drives.len() as u32).to_le_bytes().to_vec();
        for (i, name) in self.drives.iter().enumerate() {
            body.extend_from_slice(&8u32.to_le_bytes());
            body.extend_from_slice(&(i as u32 + 1).to_le_bytes());
            let mut dos_name = [0; 8];
            dos_name[..name.len()].copy_from_slice(name.as_bytes());
            body.extend_from_slice(&dos_name);
            body.extend_from_slice(&(name.len() as u32 + 1).to_le_bytes());
            body.extend_from_slice(name.as_bytes());
            body.push(0);
        }
        pdu(DEVICES, &body)
    }

    fn announce_devices(&mut self, actions: &mut Vec<Action>) {
        if self.confirmed && self.capabilities && !self.announced {
            self.announced = true;
            actions.push(Action::Send(self.device_list()));
        }
    }
}

fn identity(reader: &mut Reader<'_>) -> Result<(u16, u32)> {
    if reader.u16_le()? != 1 {
        return Err(Error::Invalid("device protocol major version"));
    }
    let minor = reader.u16_le()?;
    if !matches!(minor, 12 | 13) {
        // Pre-v12 requires caller-supplied randomness for the client ID. Modern
        // Windows supplies an identity that this transport-free engine echoes.
        return Err(Error::Unsupported("legacy device protocol version"));
    }
    Ok((minor, reader.u32_le()?))
}

fn pdu(kind: u16, body: &[u8]) -> Vec<u8> {
    let mut data = CORE.to_le_bytes().to_vec();
    data.extend_from_slice(&kind.to_le_bytes());
    data.extend_from_slice(body);
    data
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity_pdu(kind: u16, id: u32) -> Vec<u8> {
        let mut body = vec![1, 0, 12, 0];
        body.extend_from_slice(&id.to_le_bytes());
        pdu(kind, &body)
    }

    fn capabilities() -> Vec<u8> {
        let mut body = vec![1, 0, 0, 0, 1, 0, 44, 0, 2, 0, 0, 0];
        body.extend_from_slice(&[0; 8]);
        body.extend_from_slice(&[1, 0, 12, 0]);
        body.extend_from_slice(&[0; 24]);
        pdu(SERVER_CAPABILITY, &body)
    }

    #[test]
    fn negotiates_control_without_advertising_any_devices() {
        let mut device = Device::new("Linux", Vec::new());
        let reply = device.receive(&identity_pdu(SERVER_ANNOUNCE, 42)).unwrap();
        assert_eq!(reply.len(), 2);
        let Action::Send(announce) = &reply[0] else {
            panic!("expected announce")
        };
        assert_eq!(announce, &identity_pdu(CLIENT_CONFIRM, 42));
        let Action::Send(name) = &reply[1] else {
            panic!("expected name")
        };
        assert_eq!(&name[4..16], &[1, 0, 0, 0, 0, 0, 0, 0, 12, 0, 0, 0]);
        assert_eq!(&name[16..], b"L\0i\0n\0u\0x\0\0\0");
        let response = device.receive(&capabilities()).unwrap();
        assert_eq!(response.len(), 1);
        let Action::Send(caps) = &response[0] else {
            panic!("expected capabilities")
        };
        assert_eq!(caps.len(), 52);
        assert_eq!(&caps[4..8], &[1, 0, 0, 0]);
        let response = device.receive(&identity_pdu(CLIENT_CONFIRM, 42)).unwrap();
        let Action::Send(devices) = &response[0] else {
            panic!("expected empty device list")
        };
        assert_eq!(devices, &pdu(DEVICES, &[0; 4]));
    }

    #[test]
    fn rejects_mismatched_confirmation_and_truncated_capabilities() {
        let mut device = Device::new("Linux", Vec::new());
        device.receive(&identity_pdu(SERVER_ANNOUNCE, 42)).unwrap();
        assert!(device.receive(&identity_pdu(CLIENT_CONFIRM, 43)).is_err());
        let mut caps = capabilities();
        caps.pop();
        assert!(device.receive(&caps).is_err());
    }

    #[test]
    fn server_reannounce_resets_the_device_handshake() {
        let mut device = Device::new("Linux", Vec::new());
        device.receive(&identity_pdu(SERVER_ANNOUNCE, 42)).unwrap();
        device.receive(&capabilities()).unwrap();
        device.receive(&identity_pdu(CLIENT_CONFIRM, 42)).unwrap();
        device.receive(&identity_pdu(SERVER_ANNOUNCE, 43)).unwrap();
        assert!(
            device
                .receive(&identity_pdu(CLIENT_CONFIRM, 43))
                .unwrap()
                .is_empty()
        );
        assert_eq!(device.receive(&capabilities()).unwrap().len(), 2);
    }
    #[test]
    fn shared_drive_requests_require_announcement_and_matching_completion_epoch() {
        let mut device = Device::new("Linux", vec!["Local1".into()]);
        device.receive(&identity_pdu(SERVER_ANNOUNCE, 42)).unwrap();
        device.receive(&capabilities()).unwrap();
        let actions = device.receive(&identity_pdu(CLIENT_CONFIRM, 42)).unwrap();
        let Action::Send(devices) = &actions[0] else {
            panic!("device announcement")
        };
        assert_eq!(&devices[4..8], &1u32.to_le_bytes());
        assert_eq!(&devices[16..24], b"Local1\0\0");
        let mut body = Vec::new();
        for value in [1u32, 0, 23, 0, 0] {
            body.extend_from_slice(&value.to_le_bytes());
        }
        let actions = device.receive(&pdu(0x4952, &body)).unwrap();
        let Action::Event(ChannelEvent::DriveRequest(request)) = &actions[0] else {
            panic!("drive request")
        };
        let generation = request.generation;
        let completion = DriveCompletion {
            generation,
            device_id: 1,
            completion_id: 23,
            status: 0xc000000d,
            data: vec![0; 5],
        };
        assert_eq!(device.complete(completion).unwrap().len(), 1);
        device.receive(&identity_pdu(SERVER_ANNOUNCE, 43)).unwrap();
        assert!(
            device
                .complete(DriveCompletion {
                    generation,
                    device_id: 1,
                    completion_id: 23,
                    status: 0,
                    data: Vec::new()
                })
                .unwrap()
                .is_empty()
        );
    }
}
