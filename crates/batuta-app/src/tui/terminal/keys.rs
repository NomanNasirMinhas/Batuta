//! Turning key presses into the bytes a program expects to read.
//!
//! This is the other half of being a terminal. The grid interprets what the
//! program writes; this encodes what the user types, and getting it wrong is
//! just as visible — arrows that print `[A` instead of moving, a Backspace
//! that does nothing, Ctrl+C that fails to interrupt.
//!
//! The encodings are the DEC/xterm ones every shell and editor already
//! expects. There is nothing to invent here, only to get right.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// The modifier parameter xterm uses in `CSI 1 ; n <letter>`.
///
/// One plus a bitmask: shift 1, alt 2, control 4. A plain key has no
/// parameter at all rather than `1`, which is why this returns an `Option`.
fn modifier_param(m: KeyModifiers) -> Option<u8> {
    let mut bits = 0;
    if m.contains(KeyModifiers::SHIFT) {
        bits |= 1;
    }
    if m.contains(KeyModifiers::ALT) {
        bits |= 2;
    }
    if m.contains(KeyModifiers::CONTROL) {
        bits |= 4;
    }
    (bits != 0).then_some(bits + 1)
}

/// A cursor-style key: `CSI A` plainly, `CSI 1 ; 5 A` with control held.
fn csi_letter(letter: char, m: KeyModifiers) -> Vec<u8> {
    match modifier_param(m) {
        Some(p) => format!("\x1b[1;{p}{letter}").into_bytes(),
        None => format!("\x1b[{letter}").into_bytes(),
    }
}

/// A tilde-style key: `CSI 3 ~` plainly, `CSI 3 ; 5 ~` with modifiers.
fn csi_tilde(number: u8, m: KeyModifiers) -> Vec<u8> {
    match modifier_param(m) {
        Some(p) => format!("\x1b[{number};{p}~").into_bytes(),
        None => format!("\x1b[{number}~").into_bytes(),
    }
}

/// The bytes to send for one key press, or `None` if it means nothing here.
pub fn encode(key: KeyEvent) -> Option<Vec<u8>> {
    let m = key.modifiers;
    let ctrl = m.contains(KeyModifiers::CONTROL);
    let alt = m.contains(KeyModifiers::ALT);

    let bytes = match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                // Control codes: Ctrl+A is 1, Ctrl+Z is 26, and the handful
                // above Z follow on. Ctrl+C landing here as 0x03 is what makes
                // it interrupt the running program rather than doing whatever
                // Ctrl+C means elsewhere in this app.
                let upper = c.to_ascii_uppercase();
                let code = match upper {
                    'A'..='Z' => Some(upper as u8 - b'A' + 1),
                    '@' => Some(0),
                    '[' => Some(27),
                    '\\' => Some(28),
                    ']' => Some(29),
                    '^' => Some(30),
                    '_' => Some(31),
                    ' ' => Some(0),
                    _ => None,
                };
                match code {
                    Some(b) => vec![b],
                    // Not a control combination the terminal has a code for;
                    // send the character rather than nothing.
                    None => c.to_string().into_bytes(),
                }
            } else {
                let mut out = Vec::new();
                // Alt is an escape prefix, which is how every terminal has
                // encoded it since it was called Meta.
                if alt {
                    out.push(0x1b);
                }
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                out
            }
        }

        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        // DEL, not backspace. Sending 0x08 leaves readline-style editors
        // deleting forwards or doing nothing at all.
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],

        KeyCode::Up => csi_letter('A', m),
        KeyCode::Down => csi_letter('B', m),
        KeyCode::Right => csi_letter('C', m),
        KeyCode::Left => csi_letter('D', m),
        KeyCode::Home => csi_letter('H', m),
        KeyCode::End => csi_letter('F', m),

        KeyCode::Insert => csi_tilde(2, m),
        KeyCode::Delete => csi_tilde(3, m),
        KeyCode::PageUp => csi_tilde(5, m),
        KeyCode::PageDown => csi_tilde(6, m),

        KeyCode::F(n) => match n {
            // The first four are SS3-style; the rest are tilde codes, and the
            // numbering skips values for historical reasons rather than any
            // pattern worth deriving.
            1 => b"\x1bOP".to_vec(),
            2 => b"\x1bOQ".to_vec(),
            3 => b"\x1bOR".to_vec(),
            4 => b"\x1bOS".to_vec(),
            5 => csi_tilde(15, m),
            6 => csi_tilde(17, m),
            7 => csi_tilde(18, m),
            8 => csi_tilde(19, m),
            9 => csi_tilde(20, m),
            10 => csi_tilde(21, m),
            11 => csi_tilde(23, m),
            12 => csi_tilde(24, m),
            _ => return None,
        },

        _ => return None,
    };

    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn with(code: KeyCode, m: KeyModifiers) -> Vec<u8> {
        encode(KeyEvent::new(code, m)).expect("should encode")
    }

    #[test]
    fn ordinary_typing_is_sent_as_itself() {
        assert_eq!(encode(key(KeyCode::Char('a'))).unwrap(), b"a");
        assert_eq!(encode(key(KeyCode::Char('Z'))).unwrap(), b"Z");
        // Multi-byte characters go as UTF-8, not as a lossy single byte.
        assert_eq!(
            encode(key(KeyCode::Char('é'))).unwrap(),
            "é".as_bytes().to_vec()
        );
    }

    #[test]
    fn ctrl_c_is_an_interrupt_and_not_a_letter() {
        // The whole reason Ctrl+C cannot also mean something in the app while
        // the terminal has focus.
        assert_eq!(with(KeyCode::Char('c'), KeyModifiers::CONTROL), vec![3]);
        assert_eq!(with(KeyCode::Char('C'), KeyModifiers::CONTROL), vec![3]);
        assert_eq!(with(KeyCode::Char('d'), KeyModifiers::CONTROL), vec![4]);
        assert_eq!(with(KeyCode::Char('z'), KeyModifiers::CONTROL), vec![26]);
    }

    #[test]
    fn backspace_sends_delete_not_backspace() {
        // 0x08 leaves shells deleting the wrong direction or ignoring it.
        assert_eq!(encode(key(KeyCode::Backspace)).unwrap(), vec![0x7f]);
    }

    #[test]
    fn enter_sends_a_carriage_return() {
        // Not a line feed: a shell reading a raw line wants CR.
        assert_eq!(encode(key(KeyCode::Enter)).unwrap(), b"\r");
    }

    #[test]
    fn arrows_are_escape_sequences_not_letters() {
        assert_eq!(encode(key(KeyCode::Up)).unwrap(), b"\x1b[A");
        assert_eq!(encode(key(KeyCode::Down)).unwrap(), b"\x1b[B");
        assert_eq!(encode(key(KeyCode::Right)).unwrap(), b"\x1b[C");
        assert_eq!(encode(key(KeyCode::Left)).unwrap(), b"\x1b[D");
    }

    #[test]
    fn modifiers_on_arrows_use_the_xterm_parameter() {
        // Ctrl+Left is word-left in every shell, and it only works if the
        // modifier is encoded rather than dropped.
        assert_eq!(with(KeyCode::Left, KeyModifiers::CONTROL), b"\x1b[1;5D");
        assert_eq!(with(KeyCode::Right, KeyModifiers::SHIFT), b"\x1b[1;2C");
        assert_eq!(with(KeyCode::Up, KeyModifiers::ALT), b"\x1b[1;3A");

        let both = KeyModifiers::CONTROL | KeyModifiers::SHIFT;
        assert_eq!(with(KeyCode::Down, both), b"\x1b[1;6B");
    }

    #[test]
    fn a_plain_arrow_carries_no_modifier_parameter() {
        // `CSI 1 ; 1 A` is not what an unmodified arrow looks like, and some
        // programs treat it differently.
        assert_eq!(encode(key(KeyCode::Up)).unwrap(), b"\x1b[A");
    }

    #[test]
    fn editing_keys_use_their_tilde_codes() {
        assert_eq!(encode(key(KeyCode::Delete)).unwrap(), b"\x1b[3~");
        assert_eq!(encode(key(KeyCode::Insert)).unwrap(), b"\x1b[2~");
        assert_eq!(encode(key(KeyCode::PageUp)).unwrap(), b"\x1b[5~");
        assert_eq!(encode(key(KeyCode::PageDown)).unwrap(), b"\x1b[6~");
        assert_eq!(with(KeyCode::Delete, KeyModifiers::CONTROL), b"\x1b[3;5~");
    }

    #[test]
    fn alt_is_an_escape_prefix() {
        assert_eq!(with(KeyCode::Char('b'), KeyModifiers::ALT), b"\x1bb");
    }

    #[test]
    fn escape_sends_escape() {
        // It must reach the program: this is how anyone leaves insert mode.
        assert_eq!(encode(key(KeyCode::Esc)).unwrap(), vec![0x1b]);
    }

    #[test]
    fn function_keys_are_encoded() {
        assert_eq!(encode(key(KeyCode::F(1))).unwrap(), b"\x1bOP");
        assert_eq!(encode(key(KeyCode::F(5))).unwrap(), b"\x1b[15~");
        assert_eq!(encode(key(KeyCode::F(12))).unwrap(), b"\x1b[24~");
        assert!(
            encode(key(KeyCode::F(20))).is_none(),
            "no encoding to guess"
        );
    }

    #[test]
    fn keys_with_no_meaning_send_nothing_rather_than_rubbish() {
        assert!(encode(key(KeyCode::Null)).is_none());
    }
}
