//! Translates crossterm key events into the byte sequences an xterm-compatible
//! terminal would send to the program running in it.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Returns the bytes for `key`, or `None` for keys that send nothing.
/// `app_cursor` is the DECCKM mode of the pane: when set, unmodified arrow
/// keys use SS3 (`ESC O A`) instead of CSI (`ESC [ A`).
pub fn encode(key: KeyEvent, app_cursor: bool) -> Option<Vec<u8>> {
    let mods = key.modifiers;
    let alt = mods.contains(KeyModifiers::ALT);
    let ctrl = mods.contains(KeyModifiers::CONTROL);

    let mut bytes = match key.code {
        KeyCode::Char(c) if ctrl => vec![ctrl_byte(c)?],
        KeyCode::Char(c) => c.to_string().into_bytes(),
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => return Some(b"\x1b[Z".to_vec()),
        KeyCode::Backspace if ctrl => vec![0x08],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],

        KeyCode::Up => return Some(cursor_key(b'A', mods, app_cursor)),
        KeyCode::Down => return Some(cursor_key(b'B', mods, app_cursor)),
        KeyCode::Right => return Some(cursor_key(b'C', mods, app_cursor)),
        KeyCode::Left => return Some(cursor_key(b'D', mods, app_cursor)),
        KeyCode::Home => return Some(cursor_key(b'H', mods, app_cursor)),
        KeyCode::End => return Some(cursor_key(b'F', mods, app_cursor)),

        KeyCode::Insert => return Some(tilde_key(2, mods)),
        KeyCode::Delete => return Some(tilde_key(3, mods)),
        KeyCode::PageUp => return Some(tilde_key(5, mods)),
        KeyCode::PageDown => return Some(tilde_key(6, mods)),

        KeyCode::F(n @ 1..=4) => {
            let final_byte = b'P' + (n - 1);
            return Some(match modifier_param(mods) {
                1 => vec![0x1b, b'O', final_byte],
                m => format!("\x1b[1;{m}{}", final_byte as char).into_bytes(),
            });
        }
        KeyCode::F(n) => {
            let code = match n {
                5 => 15,
                6 => 17,
                7 => 18,
                8 => 19,
                9 => 20,
                10 => 21,
                11 => 23,
                12 => 24,
                _ => return None,
            };
            return Some(tilde_key(code, mods));
        }
        _ => return None,
    };

    if alt {
        bytes.insert(0, 0x1b);
    }
    Some(bytes)
}

/// The control byte for Ctrl+`c`, e.g. Ctrl+C -> 0x03.
fn ctrl_byte(c: char) -> Option<u8> {
    match c.to_ascii_lowercase() {
        c @ 'a'..='z' => Some(c as u8 - b'a' + 1),
        ' ' | '@' | '2' => Some(0x00),
        '[' | '3' => Some(0x1b),
        '\\' | '4' => Some(0x1c),
        ']' | '5' => Some(0x1d),
        '^' | '6' => Some(0x1e),
        '_' | '7' | '-' => Some(0x1f),
        '?' | '8' => Some(0x7f),
        _ => None,
    }
}

/// xterm's modifier parameter: 1 + shift(1) + alt(2) + ctrl(4).
fn modifier_param(mods: KeyModifiers) -> u8 {
    let mut m = 1;
    if mods.contains(KeyModifiers::SHIFT) {
        m += 1;
    }
    if mods.contains(KeyModifiers::ALT) {
        m += 2;
    }
    if mods.contains(KeyModifiers::CONTROL) {
        m += 4;
    }
    m
}

fn cursor_key(final_byte: u8, mods: KeyModifiers, app_cursor: bool) -> Vec<u8> {
    match modifier_param(mods) {
        1 if app_cursor => vec![0x1b, b'O', final_byte],
        1 => vec![0x1b, b'[', final_byte],
        m => format!("\x1b[1;{m}{}", final_byte as char).into_bytes(),
    }
}

fn tilde_key(code: u8, mods: KeyModifiers) -> Vec<u8> {
    match modifier_param(mods) {
        1 => format!("\x1b[{code}~").into_bytes(),
        m => format!("\x1b[{code};{m}~").into_bytes(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn plain_and_unicode_chars() {
        let none = KeyModifiers::NONE;
        assert_eq!(
            encode(key(KeyCode::Char('a'), none), false),
            Some(b"a".to_vec())
        );
        assert_eq!(
            encode(key(KeyCode::Char('ä'), none), false),
            Some("ä".as_bytes().to_vec())
        );
    }

    #[test]
    fn control_and_alt() {
        assert_eq!(
            encode(key(KeyCode::Char('c'), KeyModifiers::CONTROL), false),
            Some(vec![0x03])
        );
        assert_eq!(
            encode(key(KeyCode::Char('x'), KeyModifiers::ALT), false),
            Some(b"\x1bx".to_vec())
        );
        assert_eq!(
            encode(
                key(
                    KeyCode::Char('d'),
                    KeyModifiers::CONTROL | KeyModifiers::ALT
                ),
                false
            ),
            Some(vec![0x1b, 0x04])
        );
    }

    #[test]
    fn arrows_respect_application_cursor_mode() {
        let none = KeyModifiers::NONE;
        assert_eq!(
            encode(key(KeyCode::Up, none), false),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            encode(key(KeyCode::Up, none), true),
            Some(b"\x1bOA".to_vec())
        );
        assert_eq!(
            encode(key(KeyCode::Left, KeyModifiers::CONTROL), true),
            Some(b"\x1b[1;5D".to_vec())
        );
    }

    #[test]
    fn tilde_and_function_keys() {
        let none = KeyModifiers::NONE;
        assert_eq!(
            encode(key(KeyCode::Delete, none), false),
            Some(b"\x1b[3~".to_vec())
        );
        assert_eq!(
            encode(key(KeyCode::PageUp, KeyModifiers::SHIFT), false),
            Some(b"\x1b[5;2~".to_vec())
        );
        assert_eq!(
            encode(key(KeyCode::F(1), none), false),
            Some(b"\x1bOP".to_vec())
        );
        assert_eq!(
            encode(key(KeyCode::F(5), none), false),
            Some(b"\x1b[15~".to_vec())
        );
    }
}
