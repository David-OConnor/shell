//! The CLI's side of a remote session. Commands are typed at the shell's own
//! prompt (rustyline: history, highlighting, the shell's built-ins), then the
//! terminal is handed straight to the remote while each one runs: raw mode,
//! keystrokes out, output in, as with `ssh`. So `vim`, `top`, `sudo`, a
//! Python REPL, or a nested shell behave as they would over plain `ssh`.
//!
//! As in OpenSSH, Enter then `~.` drops a connection that has stopped
//! responding.

use std::{
    io::{self, Write},
    time::Duration,
};

use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    terminal,
};

use super::{
    keys::{TermKey, TermMods, encode_key},
    session::{ConnectError, Connecting, Phase, RemoteEvent, RemoteSession},
};

/// Output taken from the session per pass of the loop.
const POLL_BYTES: usize = 1024 * 1024;

/// How a stretch of passthrough ended.
pub(crate) enum Outcome {
    /// The remote shell is back at its prompt.
    Ready,
    /// The remote shell exited or the connection dropped.
    Closed,
    /// The user typed the `~.` escape.
    Dropped,
}

/// Raw mode for the life of the guard, restored however we leave.
struct RawMode;

impl RawMode {
    fn new() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        enable_vt_output();
        Ok(Self)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

/// Make sure the Windows console interprets the escape sequences the remote
/// sends, rather than printing them. A no-op elsewhere.
fn enable_vt_output() {
    #[cfg(windows)]
    // SAFETY: plain console-mode calls on our own standard output handle.
    unsafe {
        use windows_sys::Win32::System::Console::{
            ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, GetStdHandle, STD_OUTPUT_HANDLE,
            SetConsoleMode,
        };
        let handle = GetStdHandle(STD_OUTPUT_HANDLE);
        let mut mode = 0;
        if GetConsoleMode(handle, &mut mode) != 0 {
            SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
        }
    }
}

/// The terminal's size, for the remote PTY.
pub(crate) fn terminal_size() -> (u16, u16) {
    terminal::size().unwrap_or((80, 24))
}

/// Wait for a connection attempt, which Ctrl+C or Esc cancels.
pub(crate) fn wait_connect(mut connecting: Connecting) -> Result<RemoteSession, ConnectError> {
    let _raw = RawMode::new().map_err(|e| ConnectError::Other(format!("ssh: {e}")))?;
    loop {
        if let Some(result) = connecting.poll_timeout(Duration::from_millis(50)) {
            return result;
        }
        while event::poll(Duration::ZERO).unwrap_or(false) {
            if let Ok(Event::Key(key)) = event::read()
                && key.kind != KeyEventKind::Release
                && (key.code == KeyCode::Esc
                    || (key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL)))
            {
                return Err(ConnectError::Cancelled);
            }
        }
    }
}

/// Map a crossterm key onto the shared encoding.
fn term_key(key: KeyEvent) -> Option<(TermKey, TermMods)> {
    let mods = TermMods {
        ctrl: key.modifiers.contains(KeyModifiers::CONTROL),
        alt: key.modifiers.contains(KeyModifiers::ALT),
        shift: key.modifiers.contains(KeyModifiers::SHIFT),
    };
    let key = match key.code {
        KeyCode::Char(c) => TermKey::Char(c),
        KeyCode::Enter => TermKey::Enter,
        KeyCode::Backspace => TermKey::Backspace,
        KeyCode::Tab => TermKey::Tab,
        KeyCode::BackTab => TermKey::BackTab,
        KeyCode::Esc => TermKey::Esc,
        KeyCode::Up => TermKey::Up,
        KeyCode::Down => TermKey::Down,
        KeyCode::Left => TermKey::Left,
        KeyCode::Right => TermKey::Right,
        KeyCode::Home => TermKey::Home,
        KeyCode::End => TermKey::End,
        KeyCode::Insert => TermKey::Insert,
        KeyCode::Delete => TermKey::Delete,
        KeyCode::PageUp => TermKey::PageUp,
        KeyCode::PageDown => TermKey::PageDown,
        KeyCode::F(n) => TermKey::F(n),
        KeyCode::Null => TermKey::Char('@'),
        _ => return None,
    };
    // Crossterm reports Shift on characters it has already shifted.
    let mods = match key {
        TermKey::Char(_) => TermMods {
            shift: false,
            ..mods
        },
        _ => mods,
    };
    Some((key, mods))
}

/// The remote's output, on its way to the terminal.
struct Screen {
    out: io::Stdout,
    /// The start of a UTF-8 character cut off at the end of the last write.
    pending: Vec<u8>,
    /// Whether the last byte shown ended a line, so the next prompt can
    /// start on a fresh one.
    line_done: bool,
}

impl Screen {
    fn new() -> Self {
        Self {
            out: io::stdout(),
            pending: Vec::new(),
            line_done: true,
        }
    }

    /// Show `bytes`. Rust's stdout takes only valid UTF-8 for a Windows
    /// console, and a remote can send anything (vim probes the terminal with
    /// a lone 0xBD byte) or split a character across reads; so there, it's
    /// decoded first, holding back a split character and replacing invalid
    /// bytes.
    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        if let Some(&last) = bytes.last() {
            self.line_done = last == b'\n';
        }
        if !cfg!(windows) {
            return self.out.write_all(bytes);
        }
        self.pending.extend_from_slice(bytes);
        let cut_off = match std::str::from_utf8(&self.pending) {
            Err(e) if e.error_len().is_none() => self.pending.len() - e.valid_up_to(),
            _ => 0,
        };
        let end = self.pending.len() - cut_off;
        let text = String::from_utf8_lossy(&self.pending[..end]).into_owned();
        self.pending.drain(..end);
        self.out.write_all(text.as_bytes())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

/// Hand the terminal to the remote: its output to the screen, keystrokes to
/// it. Returns once it's back at its prompt, or the connection ends.
pub(crate) fn passthrough(session: &mut RemoteSession) -> io::Result<Outcome> {
    let raw = RawMode::new()?;
    let mut screen = Screen::new();
    // For the `~.` escape: whether the next key starts a line, and whether a
    // `~` there is being held back to see what follows it.
    let mut line_start = true;
    let mut tilde = false;

    let size = terminal_size();
    session.resize(size.0, size.1);

    let outcome = 'pump: loop {
        let mut wrote = false;
        for ev in session.poll(POLL_BYTES) {
            match ev {
                RemoteEvent::Output(bytes) => {
                    screen.write(&bytes)?;
                    wrote = true;
                }
                RemoteEvent::Ready { .. } => break 'pump Outcome::Ready,
                RemoteEvent::Closed => break 'pump Outcome::Closed,
            }
        }
        if wrote {
            screen.flush()?;
        }

        // Idle: wait briefly for a key. Busy: just check, then read more.
        let wait = if wrote {
            Duration::ZERO
        } else {
            Duration::from_millis(8)
        };
        while event::poll(wait)? {
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    let Some((k, mods)) = term_key(key) else {
                        continue;
                    };
                    let plain = mods == TermMods::default();
                    if tilde {
                        tilde = false;
                        match k {
                            TermKey::Char('.') if plain => break 'pump Outcome::Dropped,
                            TermKey::Char('~') if plain => {
                                session.send(b"~");
                                line_start = false;
                                continue;
                            }
                            _ => session.send(b"~"),
                        }
                    } else if line_start && plain && k == TermKey::Char('~') {
                        tilde = true;
                        continue;
                    }
                    line_start = k == TermKey::Enter;
                    if let Some(bytes) = encode_key(k, mods, session.app_cursor()) {
                        session.send(&bytes);
                    }
                }
                Event::Paste(text) => session.send_paste(&text),
                Event::Resize(cols, rows) => session.resize(cols, rows),
                _ => {}
            }
            if wait.is_zero() {
                break;
            }
        }
    };

    screen.flush()?;
    drop(raw);
    if !screen.line_done {
        // Output with no final newline: keep it, and start the prompt below.
        println!();
    }
    Ok(outcome)
}

/// Wait for a freshly connected session to finish starting: [Phase::Idle]
/// once the prompt hook is in. When it can't go in, the session is a plain
/// terminal ([Phase::Raw]), handed to the user until it ends.
pub(crate) fn finish_starting(session: &mut RemoteSession) -> io::Result<Outcome> {
    let mut screen = Screen::new();
    {
        let _raw = RawMode::new()?;
        while session.phase() == Phase::Starting {
            if event::poll(Duration::from_millis(20))?
                && let Event::Key(key) = event::read()?
                && key.kind != KeyEventKind::Release
                && key.code == KeyCode::Char('c')
                && key.modifiers.contains(KeyModifiers::CONTROL)
            {
                return Ok(Outcome::Dropped);
            }
            for ev in session.poll(POLL_BYTES) {
                match ev {
                    RemoteEvent::Output(bytes) => screen.write(&bytes)?,
                    RemoteEvent::Ready { .. } => {}
                    RemoteEvent::Closed => return Ok(Outcome::Closed),
                }
            }
            screen.flush()?;
        }
    }
    if session.phase() == Phase::Raw {
        println!(
            "(ssh: couldn't hook into the remote shell's prompt, so this is a plain terminal, without the shell's own commands)"
        );
        return passthrough(session);
    }
    Ok(Outcome::Ready)
}
