//! Encodes mouse events for programs in a pane that asked for them, the way
//! xterm would send them.

use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
use vt100::{MouseProtocolEncoding, MouseProtocolMode};

/// The bytes for a mouse event at `col`, `row` (0-based, inside the pane),
/// or `None` if the program did not ask for this kind of event.
pub fn encode(
    kind: MouseEventKind,
    mods: KeyModifiers,
    col: u16,
    row: u16,
    mode: MouseProtocolMode,
    encoding: MouseProtocolEncoding,
) -> Option<Vec<u8>> {
    use MouseProtocolMode as M;

    let (mut code, release) = match kind {
        MouseEventKind::Down(button) => (button_code(button), false),
        MouseEventKind::Up(button) => {
            if mode == M::Press {
                return None;
            }
            (button_code(button), true)
        }
        MouseEventKind::Drag(button) => {
            if !matches!(mode, M::ButtonMotion | M::AnyMotion) {
                return None;
            }
            (button_code(button) + 32, false)
        }
        MouseEventKind::Moved => {
            if mode != M::AnyMotion {
                return None;
            }
            (3 + 32, false)
        }
        MouseEventKind::ScrollUp => (64, false),
        MouseEventKind::ScrollDown => (65, false),
        MouseEventKind::ScrollLeft => (66, false),
        MouseEventKind::ScrollRight => (67, false),
    };
    if mode == M::None {
        return None;
    }
    if mods.contains(KeyModifiers::SHIFT) {
        code += 4;
    }
    if mods.contains(KeyModifiers::ALT) {
        code += 8;
    }
    if mods.contains(KeyModifiers::CONTROL) {
        code += 16;
    }

    let (x, y) = (u32::from(col) + 1, u32::from(row) + 1);
    match encoding {
        MouseProtocolEncoding::Sgr => {
            let end = if release { 'm' } else { 'M' };
            Some(format!("\x1b[<{code};{x};{y}{end}").into_bytes())
        }
        MouseProtocolEncoding::Default | MouseProtocolEncoding::Utf8 => {
            // The old encodings cannot tell buttons apart on release.
            if release {
                code = (code & !3) | 3;
            }
            let mut out = b"\x1b[M".to_vec();
            for value in [code as u32, x, y] {
                let value = value + 32;
                if encoding == MouseProtocolEncoding::Utf8 {
                    let c = char::from_u32(value)?;
                    let mut buf = [0; 4];
                    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                } else {
                    out.push(u8::try_from(value).ok()?);
                }
            }
            Some(out)
        }
    }
}

fn button_code(button: MouseButton) -> u8 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: KeyModifiers = KeyModifiers::NONE;

    #[test]
    fn nothing_without_a_mouse_mode() {
        let kind = MouseEventKind::Down(MouseButton::Left);
        let enc = MouseProtocolEncoding::Sgr;
        assert_eq!(encode(kind, NONE, 0, 0, MouseProtocolMode::None, enc), None);
    }

    #[test]
    fn sgr_press_release_and_wheel() {
        let (mode, enc) = (MouseProtocolMode::PressRelease, MouseProtocolEncoding::Sgr);
        let down = MouseEventKind::Down(MouseButton::Left);
        let up = MouseEventKind::Up(MouseButton::Left);
        assert_eq!(
            encode(down, NONE, 4, 9, mode, enc).unwrap(),
            b"\x1b[<0;5;10M"
        );
        assert_eq!(encode(up, NONE, 4, 9, mode, enc).unwrap(), b"\x1b[<0;5;10m");
        assert_eq!(
            encode(MouseEventKind::ScrollDown, NONE, 0, 0, mode, enc).unwrap(),
            b"\x1b[<65;1;1M"
        );
        assert_eq!(
            encode(down, KeyModifiers::CONTROL, 0, 0, mode, enc).unwrap(),
            b"\x1b[<16;1;1M"
        );
    }

    #[test]
    fn motion_only_when_asked_for() {
        let enc = MouseProtocolEncoding::Sgr;
        let drag = MouseEventKind::Drag(MouseButton::Left);
        assert_eq!(
            encode(drag, NONE, 0, 0, MouseProtocolMode::PressRelease, enc),
            None
        );
        assert_eq!(
            encode(drag, NONE, 0, 0, MouseProtocolMode::ButtonMotion, enc).unwrap(),
            b"\x1b[<32;1;1M"
        );
        assert_eq!(
            encode(
                MouseEventKind::Moved,
                NONE,
                0,
                0,
                MouseProtocolMode::ButtonMotion,
                enc
            ),
            None
        );
        assert_eq!(
            encode(
                MouseEventKind::Up(MouseButton::Left),
                NONE,
                0,
                0,
                MouseProtocolMode::Press,
                enc
            ),
            None
        );
    }

    #[test]
    fn x10_encoding() {
        let (mode, enc) = (
            MouseProtocolMode::PressRelease,
            MouseProtocolEncoding::Default,
        );
        let down = MouseEventKind::Down(MouseButton::Right);
        assert_eq!(
            encode(down, NONE, 0, 0, mode, enc).unwrap(),
            b"\x1b[M\x22\x21\x21"
        );
        let up = MouseEventKind::Up(MouseButton::Right);
        assert_eq!(
            encode(up, NONE, 0, 0, mode, enc).unwrap(),
            b"\x1b[M\x23\x21\x21"
        );
        // Beyond column 223 the old encoding has no byte left.
        assert_eq!(encode(down, NONE, 230, 0, mode, enc), None);
    }
}
