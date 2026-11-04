//! MS-RDPBCGR §2.2.8.1: bounded fast-path and slow-path client input.
use std::collections::BTreeSet;

use crate::{ConnectionInfo, Error, Result, wire};

/// Logical button independent of the client's native window system.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum MouseButton {
    /// Primary button.
    Left,
    /// Secondary button.
    Right,
    /// Middle or wheel button.
    Middle,
    /// Back button; requires negotiated extended mouse support.
    Back,
    /// Forward button; requires negotiated extended mouse support.
    Forward,
}

/// One local input operation in remote desktop coordinates.
///
/// Coordinates are clamped to the current desktop. Keyboard events preserve
/// press/release ordering; use [`Input::ReleaseAll`] on local focus loss.
#[derive(Clone, Debug)]
pub enum Input {
    /// Set-1 scan code, optionally prefixed by E0 or E1.
    Key {
        /// Scan code in the low byte; optional prefix in the high byte.
        scancode: u16,
        /// True for key-down (including repeat), false for key-up.
        pressed: bool,
    },
    /// Text or IME commit, at most 4096 UTF-8 bytes, requiring Unicode support.
    /// Each character is encoded as UTF-16 key-down and key-up events.
    Unicode(String),
    /// Send a temporary Ctrl+V chord while preserving already held keys.
    ///
    /// This only encodes keyboard input. The caller must first publish the
    /// clipboard and wait for the server to accept it. Held Shift, Alt and
    /// Windows modifiers are temporarily released and then restored; keys
    /// introduced by the chord are released before returning.
    Paste,
    /// Move the remote pointer.
    Pointer {
        /// Horizontal coordinate in remote desktop pixels.
        x: u16,
        /// Vertical coordinate in remote desktop pixels.
        y: u16,
    },
    /// Move the pointer and change a button's pressed state.
    MouseButton {
        /// Button to update.
        button: MouseButton,
        /// True for button-down, false for button-up.
        pressed: bool,
        /// Horizontal coordinate in remote desktop pixels.
        x: u16,
        /// Vertical coordinate in remote desktop pixels.
        y: u16,
    },
    /// Signed Windows wheel units: 120 per notch.
    Wheel {
        /// True for horizontal scrolling; requires negotiated support.
        horizontal: bool,
        /// Signed rotation; positive values scroll up or right.
        delta: i16,
        /// Horizontal coordinate in remote desktop pixels.
        x: u16,
        /// Vertical coordinate in remote desktop pixels.
        y: u16,
    },
    /// Release all keys and buttons tracked by the session, once each.
    ReleaseAll,
}

#[derive(Default)]
pub(crate) struct InputState {
    keys: BTreeSet<u16>,
    buttons: BTreeSet<MouseButton>,
    position: (u16, u16),
}

enum Event {
    Key {
        code: u8,
        flags: u8,
    },
    Unicode {
        code: u16,
        released: bool,
    },
    Mouse {
        extended: bool,
        flags: u16,
        x: u16,
        y: u16,
    },
}

impl InputState {
    pub fn position(&self) -> (u16, u16) {
        self.position
    }

    pub fn encode(&mut self, input: Input, info: &ConnectionInfo) -> Result<Vec<Vec<u8>>> {
        if info.width == 0 || info.height == 0 {
            return Err(Error::Invalid("input dimensions"));
        }
        let mut events = Vec::new();
        match input {
            Input::Key { scancode, pressed } => {
                if !matches!(scancode >> 8, 0 | 0xe0 | 0xe1) {
                    return Err(Error::Invalid("keyboard scan code"));
                }
                if pressed {
                    self.keys.insert(scancode);
                } else if !self.keys.remove(&scancode) {
                    return Ok(Vec::new());
                }
                events.push(key(scancode, pressed));
            }
            Input::Unicode(text) => {
                if !info.unicode_input {
                    return Err(Error::Unsupported("Unicode input"));
                }
                if text.len() > 4096 {
                    return Err(Error::Limit("input text"));
                }
                for character in text.chars() {
                    let mut buffer = [0; 2];
                    let units = character.encode_utf16(&mut buffer);
                    for released in [false, true] {
                        for &code in units.iter() {
                            events.push(Event::Unicode { code, released });
                        }
                    }
                }
            }
            Input::Paste => self.paste(&mut events),
            Input::Pointer { x, y } => self.move_to(x, y, info, &mut events),
            Input::MouseButton {
                button,
                pressed,
                x,
                y,
            } => {
                if matches!(button, MouseButton::Back | MouseButton::Forward)
                    && !info.mouse_extended
                {
                    return Err(Error::Unsupported("extended mouse buttons"));
                }
                self.move_to(x, y, info, &mut events);
                let changed = if pressed {
                    self.buttons.insert(button)
                } else {
                    self.buttons.remove(&button)
                };
                if changed {
                    events.push(mouse_button(button, pressed, self.position));
                }
            }
            Input::Wheel {
                horizontal,
                delta,
                x,
                y,
            } => {
                if horizontal && !info.horizontal_wheel {
                    return Err(Error::Unsupported("horizontal mouse wheel"));
                }
                self.move_to(x, y, info, &mut events);
                let mut remaining = i32::from(delta);
                while remaining != 0 {
                    let rotation = remaining.clamp(-256, 255);
                    remaining -= rotation;
                    events.push(Event::Mouse {
                        extended: false,
                        flags: (if horizontal { 0x0400 } else { 0x0200 })
                            | (rotation as u16 & 0x01ff),
                        x: 0,
                        y: 0,
                    });
                }
            }
            Input::ReleaseAll => {
                events.extend(
                    std::mem::take(&mut self.keys)
                        .into_iter()
                        .map(|code| key(code, false)),
                );
                events.extend(
                    std::mem::take(&mut self.buttons)
                        .into_iter()
                        .map(|button| mouse_button(button, false, self.position)),
                );
            }
        }
        events
            .chunks(255)
            .map(|events| {
                if info.fastpath_input {
                    fast_path(events)
                } else {
                    slow_path(events, info)
                }
            })
            .collect()
    }

    fn move_to(&mut self, x: u16, y: u16, info: &ConnectionInfo, events: &mut Vec<Event>) {
        self.position = (x.min(info.width - 1), y.min(info.height - 1));
        events.push(Event::Mouse {
            extended: false,
            flags: 0x0800,
            x: self.position.0,
            y: self.position.1,
        });
    }

    fn paste(&self, events: &mut Vec<Event>) {
        const INCOMPATIBLE: [u16; 6] = [0x2a, 0x36, 0x38, 0xe038, 0xe05b, 0xe05c];
        for scancode in INCOMPATIBLE {
            if self.keys.contains(&scancode) {
                events.push(key(scancode, false));
            }
        }
        let control_held = self.keys.contains(&0x1d) || self.keys.contains(&0xe01d);
        if !control_held {
            events.push(key(0x1d, true));
        }
        // If V is physically held, this is one repeat, not a release/repress
        // cycle whose restoration would generate an additional character.
        events.push(key(0x2f, true));
        if !self.keys.contains(&0x2f) {
            events.push(key(0x2f, false));
        }
        if !control_held {
            events.push(key(0x1d, false));
        }
        for scancode in INCOMPATIBLE {
            if self.keys.contains(&scancode) {
                events.push(key(scancode, true));
            }
        }
    }
}

fn key(scancode: u16, pressed: bool) -> Event {
    let extended = match scancode >> 8 {
        0xe0 => 2,
        0xe1 => 4,
        _ => 0,
    };
    Event::Key {
        code: scancode as u8,
        flags: extended | u8::from(!pressed),
    }
}

fn mouse_button(button: MouseButton, pressed: bool, (x, y): (u16, u16)) -> Event {
    let (extended, flag) = match button {
        MouseButton::Left => (false, 0x1000),
        MouseButton::Right => (false, 0x2000),
        MouseButton::Middle => (false, 0x4000),
        MouseButton::Back => (true, 1),
        MouseButton::Forward => (true, 2),
    };
    Event::Mouse {
        extended,
        flags: flag | if pressed { 0x8000 } else { 0 },
        x,
        y,
    }
}

fn fast_path(events: &[Event]) -> Result<Vec<u8>> {
    let count = u8::try_from(events.len()).map_err(|_| Error::Limit("input event count"))?;
    let mut body = Vec::new();
    if count > 15 {
        body.push(count);
    }
    for event in events {
        match *event {
            Event::Key { code, flags } => body.extend_from_slice(&[flags, code]),
            Event::Unicode { code, released } => {
                body.push(0x80 | u8::from(released));
                body.extend_from_slice(&code.to_le_bytes());
            }
            Event::Mouse {
                extended,
                flags,
                x,
                y,
            } => {
                body.push(if extended { 0x40 } else { 0x20 });
                for value in [flags, x, y] {
                    body.extend_from_slice(&value.to_le_bytes());
                }
            }
        }
    }
    let mut packet = vec![if count <= 15 { count << 2 } else { 0 }];
    if body.len() + 2 < 128 {
        packet.push((body.len() + 2) as u8);
    } else {
        wire::put_per_length(&mut packet, body.len() + 3)?;
    }
    packet.extend_from_slice(&body);
    Ok(packet)
}

fn slow_path(events: &[Event], info: &ConnectionInfo) -> Result<Vec<u8>> {
    let count = u16::try_from(events.len()).map_err(|_| Error::Limit("input event count"))?;
    let mut body = Vec::with_capacity(4 + events.len() * 12);
    body.extend_from_slice(&count.to_le_bytes());
    body.extend_from_slice(&[0, 0]);
    for event in events {
        let (kind, flags, first, second) = match *event {
            Event::Key { code, flags } => (
                4_u16,
                (u16::from(flags & 1) << 15)
                    | (u16::from(flags & 2) << 7)
                    | (u16::from(flags & 4) << 7),
                u16::from(code),
                0,
            ),
            Event::Unicode { code, released } => (5, if released { 0x8000 } else { 0 }, code, 0),
            Event::Mouse {
                extended,
                flags,
                x,
                y,
            } => (if extended { 0x8002 } else { 0x8001 }, flags, x, y),
        };
        body.extend_from_slice(&[0; 4]);
        for value in [kind, flags, first, second] {
            body.extend_from_slice(&value.to_le_bytes());
        }
    }
    wire::share_data(info.user_id, info.io_channel_id, info.share_id, 28, &body)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn info(fastpath_input: bool) -> ConnectionInfo {
        ConnectionInfo {
            user_id: 1001,
            server_id: 1002,
            io_channel_id: 1003,
            share_id: 1,
            width: 320,
            height: 200,
            color_depth: 32,
            channels: vec![],
            fastpath_input,
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

    fn decode_keys(packets: &[Vec<u8>], fast: bool) -> Vec<(u16, bool)> {
        let mut keys = Vec::new();
        for packet in packets {
            if fast {
                let mut reader = wire::Reader::new(packet);
                let header = reader.u8().unwrap();
                assert_eq!(wire::per_length(&mut reader).unwrap(), packet.len());
                let count = (header >> 2) & 15;
                let count = if count == 0 {
                    reader.u8().unwrap()
                } else {
                    count
                };
                for _ in 0..count {
                    let flags = reader.u8().unwrap();
                    let code = u16::from(reader.u8().unwrap());
                    assert_eq!(flags & !7, 0);
                    let prefix = if flags & 2 != 0 {
                        0xe000
                    } else if flags & 4 != 0 {
                        0xe100
                    } else {
                        0
                    };
                    keys.push((prefix | code, flags & 1 == 0));
                }
                reader.finish().unwrap();
            } else {
                let mut indication = packet.clone();
                indication[7] = 0x68;
                let mut reader = wire::Reader::new(wire::mcs_indication(&indication).unwrap().2);
                reader.skip(18).unwrap();
                let count = reader.u16_le().unwrap();
                reader.skip(2).unwrap();
                for _ in 0..count {
                    reader.skip(4).unwrap();
                    assert_eq!(reader.u16_le().unwrap(), 4);
                    let flags = reader.u16_le().unwrap();
                    let code = reader.u16_le().unwrap();
                    assert_eq!(reader.u16_le().unwrap(), 0);
                    let prefix = if flags & 0x100 != 0 {
                        0xe000
                    } else if flags & 0x200 != 0 {
                        0xe100
                    } else {
                        0
                    };
                    keys.push((prefix | code, flags & 0x8000 == 0));
                }
                reader.finish().unwrap();
            }
        }
        keys
    }

    #[test]
    fn paste_releases_only_synthetic_keys_in_both_input_paths() {
        for fast in [false, true] {
            let info = info(fast);
            let mut state = InputState::default();
            assert_eq!(
                decode_keys(&state.encode(Input::Paste, &info).unwrap(), fast),
                [(0x1d, true), (0x2f, true), (0x2f, false), (0x1d, false)],
            );
            assert!(state.encode(Input::ReleaseAll, &info).unwrap().is_empty());

            state
                .encode(
                    Input::Key {
                        scancode: 0xe01d,
                        pressed: true,
                    },
                    &info,
                )
                .unwrap();
            assert_eq!(
                decode_keys(&state.encode(Input::Paste, &info).unwrap(), fast),
                [(0x2f, true), (0x2f, false)],
            );
            assert_eq!(
                decode_keys(&state.encode(Input::ReleaseAll, &info).unwrap(), fast),
                [(0xe01d, false)],
            );
        }
    }

    #[test]
    fn paste_preserves_held_v_modifiers_buttons_and_pointer() {
        for fast in [false, true] {
            let info = info(fast);
            for held_control in [false, true] {
                let mut state = InputState::default();
                for scancode in [0x2a, 0x36, 0x38, 0xe038, 0xe05b, 0xe05c, 0x2f, 0x1e] {
                    state
                        .encode(
                            Input::Key {
                                scancode,
                                pressed: true,
                            },
                            &info,
                        )
                        .unwrap();
                }
                if held_control {
                    state
                        .encode(
                            Input::Key {
                                scancode: 0xe01d,
                                pressed: true,
                            },
                            &info,
                        )
                        .unwrap();
                }
                state
                    .encode(
                        Input::MouseButton {
                            button: MouseButton::Left,
                            pressed: true,
                            x: 30,
                            y: 20,
                        },
                        &info,
                    )
                    .unwrap();
                let original = state.keys.clone();
                let mut remote_keys = original.clone();
                let mut paste_presses = 0;
                for (scancode, pressed) in
                    decode_keys(&state.encode(Input::Paste, &info).unwrap(), fast)
                {
                    if pressed {
                        if scancode == 0x2f {
                            assert!(remote_keys.contains(&0x1d) || remote_keys.contains(&0xe01d));
                            assert!(
                                [0x2a, 0x36, 0x38, 0xe038, 0xe05b, 0xe05c]
                                    .iter()
                                    .all(|key| !remote_keys.contains(key))
                            );
                            paste_presses += 1;
                        }
                        remote_keys.insert(scancode);
                    } else {
                        assert!(remote_keys.remove(&scancode));
                    }
                }
                assert_eq!(paste_presses, 1);
                assert_eq!(remote_keys, original);
                assert_eq!(state.keys, original);
                assert_eq!(state.buttons, BTreeSet::from([MouseButton::Left]));
                assert_eq!(state.position(), (30, 20));
                assert!(!state.encode(Input::ReleaseAll, &info).unwrap().is_empty());
                assert!(state.encode(Input::ReleaseAll, &info).unwrap().is_empty());
            }
        }
    }

    #[test]
    fn unicode_surrogates_and_releases_keep_their_order() {
        let packets = InputState::default()
            .encode(Input::Unicode("😀".into()), &info(true))
            .unwrap();
        assert_eq!(
            packets,
            vec![vec![
                16, 14, 0x80, 0x3d, 0xd8, 0x80, 0, 0xde, 0x81, 0x3d, 0xd8, 0x81, 0, 0xde
            ]]
        );
    }
    #[test]
    fn blur_releases_extended_keys_and_buttons_once() {
        let mut state = InputState::default();
        let info = info(true);
        state
            .encode(
                Input::Key {
                    scancode: 0xe01d,
                    pressed: true,
                },
                &info,
            )
            .unwrap();
        state
            .encode(
                Input::MouseButton {
                    button: MouseButton::Back,
                    pressed: true,
                    x: 900,
                    y: 900,
                },
                &info,
            )
            .unwrap();
        assert_eq!(state.position(), (319, 199));
        assert_eq!(
            state.encode(Input::ReleaseAll, &info).unwrap(),
            vec![vec![8, 11, 3, 0x1d, 0x40, 1, 0, 0x3f, 1, 0xc7, 0]]
        );
        assert!(state.encode(Input::ReleaseAll, &info).unwrap().is_empty());
    }
    #[test]
    fn large_pastes_are_split_into_bounded_slow_path_pdus() {
        let packets = InputState::default()
            .encode(Input::Unicode("x".repeat(4096)), &info(false))
            .unwrap();
        assert_eq!(packets.len(), 33);
        let mut total = 0;
        for packet in packets {
            let mut mcs = wire::Reader::new(wire::x224_payload(&packet).unwrap());
            assert_eq!(mcs.u8().unwrap(), 0x64);
            mcs.skip(5).unwrap();
            let length = wire::per_length(&mut mcs).unwrap();
            let mut share = wire::Reader::new(mcs.take(length).unwrap());
            share.skip(18).unwrap();
            let count = usize::from(share.u16_le().unwrap());
            assert!(count <= 255);
            assert_eq!(share.remaining_len(), 2 + count * 12);
            total += count;
        }
        assert_eq!(total, 8192);
    }

    #[test]
    fn wheel_rotation_is_split_without_losing_units() {
        for delta in [i16::MIN, -360, 360, i16::MAX] {
            let packets = InputState::default()
                .encode(
                    Input::Wheel {
                        horizontal: false,
                        delta,
                        x: 0,
                        y: 0,
                    },
                    &info(false),
                )
                .unwrap();
            let mut total = 0i32;
            for packet in packets {
                let mut mcs = wire::Reader::new(wire::x224_payload(&packet).unwrap());
                mcs.skip(6).unwrap();
                let length = wire::per_length(&mut mcs).unwrap();
                let mut share = wire::Reader::new(mcs.take(length).unwrap());
                share.skip(18).unwrap();
                let count = share.u16_le().unwrap();
                share.skip(2).unwrap();
                for _ in 0..count {
                    share.skip(6).unwrap();
                    let flags = share.u16_le().unwrap();
                    share.skip(4).unwrap();
                    if flags & 0x200 != 0 {
                        let magnitude = i32::from(flags & 0x1ff);
                        total += if flags & 0x100 != 0 {
                            magnitude - 512
                        } else {
                            magnitude
                        };
                    }
                }
            }
            assert_eq!(total, i32::from(delta));
        }
    }

    #[test]
    fn unnegotiated_mouse_extensions_do_not_change_input_state() {
        let mut connection = info(true);
        connection.mouse_extended = false;
        connection.horizontal_wheel = false;
        let mut state = InputState::default();
        assert!(matches!(
            state.encode(
                Input::MouseButton {
                    button: MouseButton::Back,
                    pressed: true,
                    x: 42,
                    y: 42,
                },
                &connection
            ),
            Err(Error::Unsupported(_))
        ));
        assert!(matches!(
            state.encode(
                Input::Wheel {
                    horizontal: true,
                    delta: 120,
                    x: 42,
                    y: 42,
                },
                &connection
            ),
            Err(Error::Unsupported(_))
        ));
        assert_eq!(state.position(), (0, 0));
        assert!(
            state
                .encode(Input::ReleaseAll, &connection)
                .unwrap()
                .is_empty()
        );
    }
}
