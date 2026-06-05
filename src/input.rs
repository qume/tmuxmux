use egui::{Key, Modifiers};

/// Translate a non-text key event into the byte sequence a terminal expects.
///
/// Printable characters arrive separately as `Event::Text` (which preserves
/// shift/uppercase/symbols — the old key→char table could only type
/// lowercase), so this only handles special keys and Ctrl combinations.
///
/// Note: Ctrl+C / Ctrl+X / Ctrl+V never reach this function — egui-winit
/// converts them to Event::Copy / Cut / Paste. They are handled in app.rs.
pub fn key_event_to_bytes(key: Key, mods: Modifiers, app_cursor: bool) -> Vec<u8> {
    // xterm modifier parameter: 1 + shift(1) + alt(2) + ctrl(4)
    let modcode = 1
        + if mods.shift { 1 } else { 0 }
        + if mods.alt { 2 } else { 0 }
        + if mods.ctrl { 4 } else { 0 };

    let csi_mod = |ch: char| -> Vec<u8> {
        if modcode > 1 {
            format!("\x1b[1;{}{}", modcode, ch).into_bytes()
        } else if app_cursor {
            format!("\x1bO{}", ch).into_bytes()
        } else {
            format!("\x1b[{}", ch).into_bytes()
        }
    };
    let csi_tilde = |n: u8| -> Vec<u8> {
        if modcode > 1 {
            format!("\x1b[{};{}~", n, modcode).into_bytes()
        } else {
            format!("\x1b[{}~", n).into_bytes()
        }
    };

    match key {
        Key::Enter => vec![b'\r'],
        Key::Backspace => {
            if mods.ctrl {
                vec![0x08]
            } else {
                vec![0x7f]
            }
        }
        Key::Tab => {
            if mods.shift {
                b"\x1b[Z".to_vec()
            } else {
                vec![b'\t']
            }
        }
        Key::Escape => vec![0x1b],
        Key::ArrowUp => csi_mod('A'),
        Key::ArrowDown => csi_mod('B'),
        Key::ArrowRight => csi_mod('C'),
        Key::ArrowLeft => csi_mod('D'),
        Key::Home => csi_mod('H'),
        Key::End => csi_mod('F'),
        Key::PageUp => csi_tilde(5),
        Key::PageDown => csi_tilde(6),
        Key::Insert => csi_tilde(2),
        Key::Delete => csi_tilde(3),
        Key::F1 => {
            if modcode > 1 {
                format!("\x1b[1;{}P", modcode).into_bytes()
            } else {
                b"\x1bOP".to_vec()
            }
        }
        Key::F2 => {
            if modcode > 1 {
                format!("\x1b[1;{}Q", modcode).into_bytes()
            } else {
                b"\x1bOQ".to_vec()
            }
        }
        Key::F3 => {
            if modcode > 1 {
                format!("\x1b[1;{}R", modcode).into_bytes()
            } else {
                b"\x1bOR".to_vec()
            }
        }
        Key::F4 => {
            if modcode > 1 {
                format!("\x1b[1;{}S", modcode).into_bytes()
            } else {
                b"\x1bOS".to_vec()
            }
        }
        Key::F5 => csi_tilde(15),
        Key::F6 => csi_tilde(17),
        Key::F7 => csi_tilde(18),
        Key::F8 => csi_tilde(19),
        Key::F9 => csi_tilde(20),
        Key::F10 => csi_tilde(21),
        Key::F11 => csi_tilde(23),
        Key::F12 => csi_tilde(24),
        // Ctrl combinations. Plain printables come through Event::Text instead.
        Key::Space if mods.ctrl => vec![0x00],
        Key::OpenBracket if mods.ctrl => vec![0x1b],
        Key::Backslash if mods.ctrl => vec![0x1c],
        Key::CloseBracket if mods.ctrl => vec![0x1d],
        Key::Minus if mods.ctrl => vec![0x1f],
        _ if mods.ctrl => ctrl_letter(key, mods),
        _ => Vec::new(),
    }
}

fn ctrl_letter(key: Key, mods: Modifiers) -> Vec<u8> {
    let byte = match key {
        Key::A => 1,
        Key::B => 2,
        Key::C => 3,
        Key::D => 4,
        Key::E => 5,
        Key::F => 6,
        Key::G => 7,
        Key::H => 8,
        Key::I => 9,
        Key::J => 10,
        Key::K => 11,
        Key::L => 12,
        Key::M => 13,
        Key::N => 14,
        Key::O => 15,
        Key::P => 16,
        Key::Q => 17,
        Key::R => 18,
        Key::S => 19,
        Key::T => 20,
        Key::U => 21,
        Key::V => 22,
        Key::W => 23,
        Key::X => 24,
        Key::Y => 25,
        Key::Z => 26,
        _ => return Vec::new(),
    };
    if mods.alt {
        vec![0x1b, byte]
    } else {
        vec![byte]
    }
}

/// Parse escape sequences in `--script keys:` payloads: \n \r \t \e \\ \xNN
pub fn unescape_keys(s: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        match chars.next() {
            Some('n') => out.push(b'\n'),
            Some('r') => out.push(b'\r'),
            Some('t') => out.push(b'\t'),
            Some('e') => out.push(0x1b),
            Some('\\') => out.push(b'\\'),
            Some('x') => {
                let hi = chars.next().and_then(|c| c.to_digit(16));
                let lo = chars.next().and_then(|c| c.to_digit(16));
                if let (Some(hi), Some(lo)) = (hi, lo) {
                    out.push((hi * 16 + lo) as u8);
                }
            }
            Some(other) => {
                out.push(b'\\');
                let mut buf = [0u8; 4];
                out.extend_from_slice(other.encode_utf8(&mut buf).as_bytes());
            }
            None => out.push(b'\\'),
        }
    }
    out
}
