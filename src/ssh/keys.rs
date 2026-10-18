//! Keystrokes as the bytes an xterm sends for them, so programs on the remote
//! (vim, nano, less, top, a shell's line editor) see the keys they expect.
//! Each frontend maps its own key events (crossterm in the CLI, egui in the
//! GUI) onto [TermKey] and lets [encode_key] do the rest, so the two agree.

/// A key, independent of the UI toolkit it came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TermKey {
    /// A character key, as typed (already shifted: `A`, not `a` + Shift).
    Char(char),
    Enter,
    Backspace,
    Tab,
    /// Shift+Tab.
    BackTab,
    Esc,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    Insert,
    Delete,
    PageUp,
    PageDown,
    /// F1 through F12.
    F(u8),
}

/// Modifier keys held with a [TermKey].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TermMods {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl TermMods {
    /// xterm's modifier parameter: 1 plus a bit per modifier, as in
    /// `ESC [ 1 ; 5 A` for Ctrl+Up. `None` with no modifiers held.
    fn param(self) -> Option<u8> {
        let m = u8::from(self.shift) + 2 * u8::from(self.alt) + 4 * u8::from(self.ctrl);
        (m != 0).then_some(1 + m)
    }
}

/// The control byte for Ctrl+`c`, as a terminal sends it.
fn ctrl_byte(c: char) -> Option<u8> {
    Some(match c {
        'a'..='z' | 'A'..='Z' => c.to_ascii_uppercase() as u8 - b'@',
        '@' | ' ' | '2' => 0,
        '[' | '3' => 0x1b,
        '\\' | '4' => 0x1c,
        ']' | '5' => 0x1d,
        '^' | '6' => 0x1e,
        '_' | '-' | '/' | '7' => 0x1f,
        '?' | '8' => 0x7f,
        _ => return None,
    })
}

/// The bytes to send for `key` with `mods` held. `app_cursor` is whether the
/// remote program has switched the cursor keys to application mode
/// (`ESC [ ? 1 h`, as vim and less do), which changes what the arrows send.
/// Returns `None` for a combination with no terminal encoding.
pub fn encode_key(key: TermKey, mods: TermMods, app_cursor: bool) -> Option<Vec<u8>> {
    let esc_prefixed = |mut bytes: Vec<u8>| {
        if mods.alt {
            bytes.insert(0, 0x1b);
        }
        bytes
    };
    // `ESC [ <final>` or `ESC O <final>` for a cursor key, or with a
    // modifier `ESC [ 1 ; <m> <final>`.
    let cursor = |fin: u8| match mods.param() {
        Some(m) => format!("\x1b[1;{m}{}", fin as char).into_bytes(),
        None if app_cursor => vec![0x1b, b'O', fin],
        None => vec![0x1b, b'[', fin],
    };
    // `ESC [ <n> ~`, or with a modifier `ESC [ <n> ; <m> ~`.
    let tilde = |n: u8| match mods.param() {
        Some(m) => format!("\x1b[{n};{m}~").into_bytes(),
        None => format!("\x1b[{n}~").into_bytes(),
    };

    Some(match key {
        TermKey::Char(c) => {
            // Windows reports AltGr as Ctrl+Alt: a character typed with it
            // (`@`, `{` on many layouts) goes as itself.
            if mods.ctrl && mods.alt && !c.is_ascii_alphabetic() {
                let mut buf = [0; 4];
                return Some(c.encode_utf8(&mut buf).as_bytes().to_vec());
            }
            let bytes = match mods.ctrl.then(|| ctrl_byte(c)).flatten() {
                Some(b) => vec![b],
                None => {
                    let mut buf = [0; 4];
                    c.encode_utf8(&mut buf).as_bytes().to_vec()
                }
            };
            esc_prefixed(bytes)
        }
        TermKey::Enter => esc_prefixed(vec![b'\r']),
        TermKey::Backspace if mods.ctrl => esc_prefixed(vec![0x08]),
        TermKey::Backspace => esc_prefixed(vec![0x7f]),
        TermKey::Tab if mods.shift => b"\x1b[Z".to_vec(),
        TermKey::Tab => esc_prefixed(vec![b'\t']),
        TermKey::BackTab => b"\x1b[Z".to_vec(),
        TermKey::Esc => esc_prefixed(vec![0x1b]),
        TermKey::Up => cursor(b'A'),
        TermKey::Down => cursor(b'B'),
        TermKey::Right => cursor(b'C'),
        TermKey::Left => cursor(b'D'),
        TermKey::Home => cursor(b'H'),
        TermKey::End => cursor(b'F'),
        TermKey::Insert => tilde(2),
        TermKey::Delete => tilde(3),
        TermKey::PageUp => tilde(5),
        TermKey::PageDown => tilde(6),
        TermKey::F(n @ 1..=4) => {
            let fin = b'P' + (n - 1);
            match mods.param() {
                Some(m) => format!("\x1b[1;{m}{}", fin as char).into_bytes(),
                None => vec![0x1b, b'O', fin],
            }
        }
        TermKey::F(n @ 5..=12) => tilde([15, 17, 18, 19, 20, 21, 23, 24][usize::from(n - 5)]),
        TermKey::F(_) => return None,
    })
}

/// Text pasted into the remote: newlines as the Enter key sends them, and
/// wrapped in bracketed-paste markers when the program asked for them, so an
/// editor inserts the text rather than running each line as typed commands.
pub fn encode_paste(text: &str, bracketed: bool) -> Vec<u8> {
    let text = text.replace("\r\n", "\r").replace('\n', "\r");
    if bracketed {
        // A paste must not be able to end the bracket early.
        let text = text.replace("\x1b[201~", "");
        format!("\x1b[200~{text}\x1b[201~").into_bytes()
    } else {
        text.into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(key: TermKey, ctrl: bool, alt: bool, shift: bool, app: bool) -> Vec<u8> {
        encode_key(key, TermMods { ctrl, alt, shift }, app).unwrap()
    }

    #[test]
    fn keys_encode_as_xterm_does() {
        assert_eq!(enc(TermKey::Char('c'), true, false, false, false), [3]);
        assert_eq!(enc(TermKey::Char('X'), true, false, true, false), [0x18]);
        assert_eq!(enc(TermKey::Char('x'), false, true, false, false), b"\x1bx");
        assert_eq!(enc(TermKey::Char('@'), true, true, false, false), b"@");
        assert_eq!(
            enc(TermKey::Char('é'), false, false, false, false),
            "é".as_bytes()
        );
        assert_eq!(enc(TermKey::Up, false, false, false, false), b"\x1b[A");
        assert_eq!(enc(TermKey::Up, false, false, false, true), b"\x1bOA");
        assert_eq!(enc(TermKey::Right, true, false, false, true), b"\x1b[1;5C");
        assert_eq!(enc(TermKey::Delete, false, false, false, false), b"\x1b[3~");
        assert_eq!(
            enc(TermKey::PageDown, true, false, false, false),
            b"\x1b[6;5~"
        );
        assert_eq!(enc(TermKey::F(1), false, false, false, false), b"\x1bOP");
        assert_eq!(enc(TermKey::F(12), false, false, false, false), b"\x1b[24~");
        assert_eq!(enc(TermKey::Tab, false, false, true, false), b"\x1b[Z");
        assert_eq!(enc(TermKey::Backspace, false, false, false, false), [0x7f]);
    }

    #[test]
    fn pastes_use_carriage_returns_and_brackets() {
        assert_eq!(encode_paste("a\r\nb\nc", false), b"a\rb\rc");
        assert_eq!(encode_paste("x\x1b[201~y", true), b"\x1b[200~xy\x1b[201~");
    }
}
