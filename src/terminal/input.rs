use super::Modes;

#[derive(Clone, Copy, Debug)]
pub struct MouseEvent {
    pub button: u8,
    pub column: u16,
    pub row: u16,
    pub pressed: bool,
    pub motion: bool,
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

pub(super) fn key(key: &str, ctrl: bool, alt: bool, shift: bool, app_cursor: bool) -> Vec<u8> {
    let modifier = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl);
    let arrow = match key {
        "up" => Some('A'),
        "down" => Some('B'),
        "right" => Some('C'),
        "left" => Some('D'),
        "home" => Some('H'),
        "end" => Some('F'),
        _ => None,
    };
    if let Some(final_byte) = arrow {
        return if modifier > 1 {
            format!("\x1b[1;{modifier}{final_byte}").into_bytes()
        } else {
            format!("\x1b{}{final_byte}", if app_cursor { 'O' } else { '[' }).into_bytes()
        };
    }
    let function = match key {
        "f1" => Some('P'),
        "f2" => Some('Q'),
        "f3" => Some('R'),
        "f4" => Some('S'),
        _ => None,
    };
    if let Some(last) = function {
        return if modifier > 1 {
            format!("\x1b[1;{modifier}{last}").into_bytes()
        } else {
            format!("\x1bO{last}").into_bytes()
        };
    }
    let tilde = match key {
        "insert" => Some(2),
        "delete" => Some(3),
        "pageup" | "page-up" => Some(5),
        "pagedown" | "page-down" => Some(6),
        "f5" => Some(15),
        "f6" => Some(17),
        "f7" => Some(18),
        "f8" => Some(19),
        "f9" => Some(20),
        "f10" => Some(21),
        "f11" => Some(23),
        "f12" => Some(24),
        _ => None,
    };
    if let Some(number) = tilde {
        return if modifier > 1 {
            format!("\x1b[{number};{modifier}~").into_bytes()
        } else {
            format!("\x1b[{number}~").into_bytes()
        };
    }
    let mut bytes = match key {
        "enter" | "return" => vec![b'\r'],
        "backspace" => vec![if ctrl { 8 } else { 127 }],
        "escape" | "esc" => vec![27],
        "tab" if shift => b"\x1b[Z".to_vec(),
        "tab" => vec![b'\t'],
        "space" if ctrl => vec![0],
        _ if ctrl => {
            let mut chars = key.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) if c.is_ascii() => match c.to_ascii_lowercase() {
                    'a'..='z' => vec![(c.to_ascii_lowercase() as u8) - b'a' + 1],
                    '@' | '`' | ' ' | '2' => vec![0],
                    '[' | '3' => vec![27],
                    '\\' | '4' => vec![28],
                    ']' | '5' => vec![29],
                    '^' | '~' | '6' => vec![30],
                    '_' | '/' | '7' => vec![31],
                    '?' | '8' => vec![127],
                    _ => Vec::new(),
                },
                _ => Vec::new(),
            }
        }
        "space" if alt => vec![b' '],
        _ if alt && key.len() == 1 && key.is_ascii() => {
            vec![if shift {
                key.as_bytes()[0].to_ascii_uppercase()
            } else {
                key.as_bytes()[0]
            }]
        }
        // Ordinary text belongs to the IME/text commit path, not key-down.
        _ => Vec::new(),
    };
    if alt && !bytes.is_empty() {
        bytes.insert(0, 27);
    }
    bytes
}

pub(super) fn paste(text: &str, bracketed: bool) -> Vec<u8> {
    let mut output = String::with_capacity(text.len() + if bracketed { 12 } else { 0 });
    if bracketed {
        output.push_str("\x1b[200~");
    }
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\r' | '\n' => {
                if ch == '\r' && chars.peek() == Some(&'\n') {
                    chars.next();
                }
                output.push(if bracketed { '\n' } else { '\r' });
            }
            '\t' => output.push(ch),
            ch if !ch.is_control() => output.push(ch),
            _ => {}
        }
    }
    if bracketed {
        output.push_str("\x1b[201~");
    }
    output.into_bytes()
}

pub(super) fn mouse(e: MouseEvent, modes: Modes, columns: u16, rows: u16) -> Vec<u8> {
    if e.shift
        || !matches!(e.button, 0..=3 | 64..=67)
        || (e.button >= 64 && (!e.pressed || e.motion))
        || !(modes.mouse_click || modes.mouse_drag || modes.mouse_motion)
        || e.column >= columns
        || e.row >= rows
    {
        return Vec::new();
    }
    if e.motion && !(modes.mouse_motion || (modes.mouse_drag && e.pressed)) {
        return Vec::new();
    }
    let mut button = if !e.pressed && !modes.sgr_mouse {
        3
    } else {
        e.button
    };
    if e.motion {
        button |= 32;
    }
    if e.alt {
        button |= 8;
    }
    if e.ctrl {
        button |= 16;
    }
    if modes.sgr_mouse {
        format!(
            "\x1b[<{button};{};{}{}",
            e.column + 1,
            e.row + 1,
            if e.pressed || e.motion { 'M' } else { 'm' }
        )
        .into_bytes()
    } else if modes.utf8_mouse && e.column < 2015 && e.row < 2015 {
        let mut bytes = vec![27, b'[', b'M'];
        for value in [
            u32::from(button) + 32,
            u32::from(e.column) + 33,
            u32::from(e.row) + 33,
        ] {
            let mut buffer = [0; 4];
            bytes.extend_from_slice(
                char::from_u32(value)
                    .expect("bounded mouse coordinate")
                    .encode_utf8(&mut buffer)
                    .as_bytes(),
            );
        }
        bytes
    } else if !modes.utf8_mouse && e.column < 223 && e.row < 223 {
        vec![
            27,
            b'[',
            b'M',
            button + 32,
            e.column as u8 + 33,
            e.row as u8 + 33,
        ]
    } else {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::key;

    #[test]
    fn control_aliases_reach_the_shell_with_optional_meta_prefix() {
        for (alias, canonical, code) in [("/", "_", 31), ("`", "@", 0), ("~", "^", 30)] {
            assert_eq!(key(alias, true, false, false, false), vec![code]);
            assert_eq!(
                key(alias, true, false, false, false),
                key(canonical, true, false, false, false)
            );
            assert_eq!(key(alias, true, true, false, false), vec![27, code]);
        }
    }
}
