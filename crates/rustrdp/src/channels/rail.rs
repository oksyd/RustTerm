//! MS-RDPERP 2.2.2 RAIL virtual-channel messages.
use super::{Action, ChannelEvent};
use crate::{
    ClientConfig, Error, Result,
    remote_app::{RemoteApp, RemoteAppCommand, RemoteAppEvent},
    wire::Reader,
};

pub(super) struct Rail {
    application: Option<RemoteApp>,
    dimensions: (u16, u16),
    started: bool,
    result_received: bool,
}

impl Rail {
    pub fn new(config: &ClientConfig) -> Self {
        Self {
            application: config.remote_app.clone(),
            dimensions: (config.width, config.height),
            started: false,
            result_received: false,
        }
    }

    pub fn receive(&mut self, data: &[u8]) -> Result<Vec<Action>> {
        if data.len() > 65535 {
            return Err(Error::Limit("RAIL channel message"));
        }
        if self.application.is_none() {
            return Err(Error::Invalid("unrequested RAIL channel"));
        }
        let mut reader = Reader::new(data);
        let mut actions = Vec::new();
        while !reader.is_empty() {
            let kind = reader.u16_le()?;
            let length = usize::from(reader.u16_le()?);
            if length < 4 {
                return Err(Error::Invalid("RAIL order length"));
            }
            let mut body = Reader::new(reader.take(length - 4)?);
            if !self.started && kind != 5 {
                return Err(Error::Invalid("RAIL message before handshake"));
            }
            match kind {
                5 => {
                    let _server_build = body.u32_le()?;
                    body.finish()?;
                    if self.started {
                        return Err(Error::Invalid("duplicate RAIL handshake"));
                    }
                    // The first client message MUST be a handshake. Build 0
                    // identifies a non-Windows implementation; no host data.
                    actions.push(Action::Send(pdu(5, &0u32.to_le_bytes())));
                    // Standard RAIL only. Do not claim local move/size, Enhanced
                    // RemoteApp, IME synchronization or shell integration.
                    let flags = 0x410u32
                        | u32::from(
                            self.application
                                .as_ref()
                                .is_some_and(|app| app.local_move_size),
                        );
                    actions.push(Action::Send(pdu(0x0b, &flags.to_le_bytes())));
                    for (param, value) in [(0x25u32, 1), (0x100b, 1), (0x45, 0), (0x21, 0)] {
                        let mut bytes = param.to_le_bytes().to_vec();
                        bytes.push(value);
                        actions.push(Action::Send(pdu(3, &bytes)));
                    }
                    let mut contrast = 0x43u32.to_le_bytes().to_vec();
                    contrast.extend_from_slice(&0x7eu32.to_le_bytes()); // available, disabled
                    contrast.extend_from_slice(&2u32.to_le_bytes()); // empty color scheme
                    contrast.extend_from_slice(&0u16.to_le_bytes());
                    actions.push(Action::Send(pdu(3, &contrast)));
                    let mut work = 0x2fu32.to_le_bytes().to_vec();
                    for n in [0, 0, self.dimensions.0, self.dimensions.1] {
                        work.extend_from_slice(&n.to_le_bytes());
                    }
                    actions.push(Action::Send(pdu(3, &work)));
                    actions.push(Action::Send(execute(self.application.as_ref().unwrap())?));
                    self.started = true;
                    continue;
                }
                0x80 => {
                    if self.result_received {
                        return Err(Error::Invalid("duplicate RemoteApp launch result"));
                    }
                    let _flags = body.u16_le()?;
                    let result = body.u16_le()?;
                    let status = body.u32_le()?;
                    body.skip(2)?;
                    let _program = crate::remote_app::unicode(&mut body)?;
                    actions.push(Action::Event(ChannelEvent::RemoteApp(
                        RemoteAppEvent::ExecuteResult { result, status },
                    )));
                    self.result_received = true;
                }
                3 => {
                    // Server system parameters are notifications, not instructions
                    // to change the local screen saver or lock policy.
                    match body.u32_le()? {
                        0x11 | 0x77 => {
                            if body.u8()? > 1 {
                                return Err(Error::Invalid("RAIL boolean"));
                            }
                        }
                        _ => return Err(Error::Unsupported("RAIL server system parameter")),
                    }
                }
                9 => {
                    let window_id = body.u32_le()?;
                    let start = body.u16_le()?;
                    let kind = body.u16_le()?;
                    let position = (body.u16_le()? as i16, body.u16_le()? as i16);
                    if start > 1 || !(1..=11).contains(&kind) {
                        return Err(Error::Invalid("RAIL move/size"));
                    }
                    actions.push(Action::Event(ChannelEvent::RemoteApp(
                        RemoteAppEvent::LocalMoveSize {
                            window_id,
                            start: start != 0,
                            kind,
                            position,
                        },
                    )));
                }
                0x0a => {
                    let window_id = body.u32_le()?;
                    body.skip(8)?; // maximized size/position
                    let min = (body.u16_le()?, body.u16_le()?);
                    let max = (body.u16_le()?, body.u16_le()?);
                    actions.push(Action::Event(ChannelEvent::RemoteApp(
                        RemoteAppEvent::MinMax {
                            window_id,
                            min,
                            max,
                        },
                    )));
                }
                _ => return Err(Error::Unsupported("RAIL server order")),
            }
            body.finish()?;
        }
        Ok(actions)
    }

    pub fn command(&self, command: RemoteAppCommand) -> Result<Vec<Action>> {
        if !self.started {
            return Err(Error::Invalid("RAIL channel is not ready"));
        }
        let mut body = Vec::with_capacity(12);
        let kind = match command {
            RemoteAppCommand::Activate { window_id, enabled } => {
                body.extend_from_slice(&window_id.to_le_bytes());
                body.push(u8::from(enabled));
                2
            }
            RemoteAppCommand::System { window_id, command } => {
                body.extend_from_slice(&window_id.to_le_bytes());
                body.extend_from_slice(&(command as u16).to_le_bytes());
                4
            }
            RemoteAppCommand::Move { window_id, bounds } => {
                if bounds.right <= bounds.left || bounds.bottom <= bounds.top {
                    return Err(Error::Invalid("RemoteApp window bounds"));
                }
                body.extend_from_slice(&window_id.to_le_bytes());
                for value in [bounds.left, bounds.top, bounds.right, bounds.bottom] {
                    body.extend_from_slice(
                        &i16::try_from(value)
                            .map_err(|_| Error::Invalid("RemoteApp window bounds"))?
                            .to_le_bytes(),
                    );
                }
                8
            }
        };
        Ok(vec![Action::Send(pdu(kind, &body))])
    }
}

fn pdu(kind: u16, body: &[u8]) -> Vec<u8> {
    let mut data = Vec::with_capacity(body.len() + 4);
    data.extend_from_slice(&kind.to_le_bytes());
    data.extend_from_slice(&((body.len() + 4) as u16).to_le_bytes());
    data.extend_from_slice(body);
    data
}

fn execute(app: &RemoteApp) -> Result<Vec<u8>> {
    app.validate()?;
    let strings: Vec<Vec<u8>> = [&app.program, &app.working_directory, &app.arguments]
        .iter()
        .map(|text| text.encode_utf16().flat_map(u16::to_le_bytes).collect())
        .collect();
    let mut body = vec![0, 0]; // arguments are literal; no expansion or file translation
    for bytes in &strings {
        body.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
    }
    for bytes in strings {
        body.extend_from_slice(&bytes);
    }
    Ok(pdu(1, &body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote_app::{Rect, SystemCommand};

    fn config() -> ClientConfig {
        ClientConfig {
            remote_app: Some(RemoteApp {
                program: "||calc".into(),
                ..Default::default()
            }),
            dynamic_resolution: false,
            ..Default::default()
        }
    }

    #[test]
    fn handshake_initializes_parameters_before_a_single_execute() {
        let mut rail = Rail::new(&config());
        let actions = rail.receive(&[5, 0, 8, 0, 0xb1, 0x1d, 0, 0]).unwrap();
        let packets: Vec<_> = actions
            .iter()
            .map(|action| match action {
                Action::Send(packet) => packet.as_slice(),
                _ => panic!("expected an outbound RAIL packet"),
            })
            .collect();
        assert_eq!(packets[0], [5, 0, 8, 0, 0, 0, 0, 0]);
        assert_eq!(packets[1], [11, 0, 8, 0, 0x10, 4, 0, 0]);
        // MS-RDPERP 4.4.1: empty high-contrast color scheme includes its terminator.
        assert_eq!(
            packets[6],
            [3, 0, 18, 0, 0x43, 0, 0, 0, 0x7e, 0, 0, 0, 2, 0, 0, 0, 0, 0]
        );
        assert_eq!(
            packets.last().unwrap(),
            &&[
                1, 0, 24, 0, 0, 0, 12, 0, 0, 0, 0, 0, b'|', 0, b'|', 0, b'c', 0, b'a', 0, b'l', 0,
                b'c', 0
            ][..]
        );
        assert!(rail.receive(&[5, 0, 8, 0, 0, 0, 0, 0]).is_err());
    }

    #[test]
    fn launch_failure_keeps_both_protocol_and_windows_status() {
        let mut rail = Rail::new(&config());
        rail.receive(&[5, 0, 8, 0, 0, 0, 0, 0]).unwrap();
        let actions = rail
            .receive(&[0x80, 0, 16, 0, 0, 0, 3, 0, 0x15, 0, 0, 0, 0, 0, 0, 0])
            .unwrap();
        assert!(matches!(
            &actions[..],
            [Action::Event(ChannelEvent::RemoteApp(
                RemoteAppEvent::ExecuteResult {
                    result: 3,
                    status: 0x15
                }
            ))]
        ));
        assert!(
            rail.receive(&[0x80, 0, 16, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])
                .is_err()
        );
    }

    #[test]
    fn execute_encodes_non_ascii_lengths_in_bytes_without_terminators() {
        let app = RemoteApp {
            program: "||应用".into(),
            working_directory: "C:\\".into(),
            arguments: "文😀".into(),
            ..Default::default()
        };
        let packet = execute(&app).unwrap();
        assert_eq!(&packet[4..12], &[0, 0, 8, 0, 6, 0, 6, 0]);
        assert_eq!(packet.len(), 32);
        assert_eq!(&packet[26..], &[0x87, 0x65, 0x3d, 0xd8, 0, 0xde]);
    }

    #[test]
    fn malformed_orders_and_illegal_launch_parameters_are_rejected() {
        let handshake = [5, 0, 8, 0, 1, 0, 0, 0];
        for length in 1..handshake.len() {
            assert!(Rail::new(&config()).receive(&handshake[..length]).is_err());
        }
        for packet in [&[5, 0, 3, 0][..], &[3, 0, 9, 0, 0x11, 0, 0, 0, 0][..]] {
            assert!(Rail::new(&config()).receive(packet).is_err());
        }
        for program in [String::new(), " ".into(), "||x\0y".into(), "😀".repeat(131)] {
            assert!(
                execute(&RemoteApp {
                    program,
                    ..Default::default()
                })
                .is_err()
            );
        }
        let mut config = config();
        config.graphics_pipeline = true;
        assert!(config.validate().is_err());
    }

    #[test]
    fn window_actions_use_signed_coordinates_and_do_not_close_the_channel() {
        let mut rail = Rail::new(&config());
        rail.receive(&[5, 0, 8, 0, 0, 0, 0, 0]).unwrap();
        let close = rail
            .command(RemoteAppCommand::System {
                window_id: 42,
                command: SystemCommand::Close,
            })
            .unwrap();
        assert!(
            matches!(&close[0], Action::Send(bytes) if bytes == &[4, 0, 10, 0, 42, 0, 0, 0, 0x60, 0xf0])
        );
        let moved = rail
            .command(RemoteAppCommand::Move {
                window_id: 42,
                bounds: Rect {
                    left: -10,
                    top: -20,
                    right: 300,
                    bottom: 200,
                },
            })
            .unwrap();
        assert!(
            matches!(&moved[0], Action::Send(bytes) if bytes == &[8, 0, 16, 0, 42, 0, 0, 0, 0xf6, 0xff, 0xec, 0xff, 0x2c, 1, 200, 0])
        );
        assert!(
            rail.command(RemoteAppCommand::Move {
                window_id: 42,
                bounds: Rect {
                    right: 32768,
                    bottom: 200,
                    ..Default::default()
                }
            })
            .is_err()
        );
    }
}
