//! In-process SSH client, so the shell handles `ssh` itself instead of leaking
//! the command through to the OS's native `ssh` binary.
//!
//! Built on [`russh`] (pure-Rust, no OpenSSL/libssh2 C build on Windows). russh
//! is async, so a [`RemoteSession`] owns a small multi-threaded Tokio runtime
//! and `block_on`s each operation — the brief block mirrors how the rest of the
//! shell shells out with `Command::output()`. The runtime is multi-threaded (one
//! worker) rather than current-thread so russh's background session task — and,
//! in PTY mode, our channel reader task — keep making progress between our
//! `block_on` calls (important for live PTY output streaming into the GUI).
//!
//! Two interaction modes, each in its own submodule, both reused by `shell_gui`:
//!   * [`exec`] — one channel per command, output captured in a batch. The
//!     default; fits the existing output-pane model.
//!   * [`interactive`] — a single persistent PTY + shell channel, bytes streamed
//!     both ways. The user toggles modes at runtime via [`RemoteSession::set_mode`].
//!
//! Credentials are never held here long-term: [`connect`] takes the password by
//! value (the caller pulls it from the OS keyring — see `crate::secrets`).

use std::{
    io,
    path::Path,
    sync::{Arc, atomic::AtomicBool, mpsc::Receiver},
    time::Duration,
};

use russh::{
    ChannelWriteHalf,
    client::{self, Handle, Msg},
    keys::ssh_key,
};
use tokio::runtime::Runtime;

use crate::{
    RemoteTerminal,
    commands::{OutKind, OutputSink},
    state::State,
};

pub mod secrets;

mod exec;
mod interactive;

/// Which interaction mode a live session is in. Toggled at runtime; new
/// sessions start in the `#[default]` mode ([`SshMode::Pty`]) on connect.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SshMode {
    /// One channel per typed command, output captured in a batch.
    Exec,
    #[default]
    /// A persistent PTY + login shell with bytes streamed both ways.
    Pty,
}

/// russh event handler. We only need server-key verification; everything else
/// uses the trait defaults.
struct Handler;

impl client::Handler for Handler {
    type Error = russh::Error;

    // todo: Verify the key against a known_hosts store and prompt on mismatch.
    // For now we trust-on-first-use (accept any key), matching the pragmatic
    // posture of the rest of the shell's SSH MVP.
    async fn check_server_key(
        &mut self,
        _server_public_key: &ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

/// State for the PTY/interactive mode: the channel's write half (for sending
/// user input), a receiver fed by a background reader task (output bytes), and
/// a flag the reader sets when the remote shell closes.
struct PtyChannel {
    writer: ChannelWriteHalf<Msg>,
    rx: Receiver<Vec<u8>>,
    closed: Arc<AtomicBool>,
}

/// A live, authenticated SSH connection to one remote. Holds the Tokio runtime
/// and russh client handle for its whole lifetime; each command (exec mode)
/// opens a fresh channel on the same connection, so the password is only ever
/// entered once.
pub struct RemoteSession {
    rt: Arc<Runtime>,
    handle: Handle<Handler>,
    user: String,
    host: String,
    port: u16,
    mode: SshMode,
    /// Remote working directory tracked across exec-mode commands so a remote
    /// `cd` persists (each exec is otherwise an independent shell). Empty until
    /// the first command reports it; see [`exec`].
    cwd: String,
    /// `Some` only while in PTY mode.
    pty: Option<PtyChannel>,
}

/// Map a russh error into an `io::Error` so callers stay on the shell's
/// `io::Result`-based plumbing. Shared with the `exec` / `interactive` submodules.
pub(crate) fn map_err(e: russh::Error) -> io::Error {
    io::Error::other(format!("ssh: {e}"))
}

/// Open and authenticate a connection. Blocks until the TCP connect + password
/// auth complete. Returns an `io::Error` on connect failure, and a
/// `PermissionDenied` error specifically when auth is rejected (so callers can
/// tell "wrong password" from "host unreachable").
pub fn connect(host: &str, port: u16, user: &str, password: &str) -> io::Result<RemoteSession> {
    let rt = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .map_err(|e| io::Error::other(format!("ssh: runtime: {e}")))?,
    );

    let config = Arc::new(client::Config {
        // Keep idle connections alive for a long session; the user disconnects
        // explicitly (or closes the tab / shell).
        inactivity_timeout: Some(Duration::from_secs(3600)),
        ..Default::default()
    });

    let handle = rt.block_on(async {
        let mut handle = client::connect(config, (host, port), Handler)
            .await
            .map_err(map_err)?;
        let auth = handle
            .authenticate_password(user, password)
            .await
            .map_err(map_err)?;
        if !auth.success() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "ssh: authentication failed (bad username or password)",
            ));
        }
        Ok::<_, io::Error>(handle)
    })?;

    // Built in Exec first because it's the base state with no PTY channel;
    // `set_mode` below transitions from here, opening the PTY/shell channel
    // when the default mode is Pty.
    let mut session = RemoteSession {
        rt,
        handle,
        user: user.to_string(),
        host: host.to_string(),
        port,
        mode: SshMode::Exec,
        cwd: String::new(),
        pty: None,
    };

    // Honour the configured default interaction mode (see `SshMode`'s
    // `#[default]`, currently Pty/interactive) rather than hardcoding one here.
    session.set_mode(SshMode::default())?;

    Ok(session)
}

impl RemoteSession {
    /// `user@host`, for prompts and the remote-panel status line.
    pub fn label(&self) -> String {
        format!("{}@{}", self.user, self.host)
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn user(&self) -> &str {
        &self.user
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn mode(&self) -> SshMode {
        self.mode
    }

    /// Remote working directory tracked in exec mode (empty before the first
    /// command). Used to render the prompt.
    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    /// Switch interaction mode. Going Exec→Pty opens the PTY/shell channel;
    /// Pty→Exec tears it down. The underlying authenticated connection is kept
    /// either way, so toggling is cheap and doesn't re-prompt for a password.
    pub fn set_mode(&mut self, mode: SshMode) -> io::Result<()> {
        if mode == self.mode {
            return Ok(());
        }
        match mode {
            SshMode::Pty => self.start_pty()?,
            SshMode::Exec => self.stop_pty(),
        }
        self.mode = mode;
        Ok(())
    }

    /// Close the connection. Best-effort: any error during the polite
    /// disconnect is ignored since we're tearing down anyway.
    pub fn disconnect(mut self) {
        if self.mode == SshMode::Pty {
            self.stop_pty();
        }
        let rt = self.rt.clone();
        let _ = rt.block_on(self.handle.disconnect(
            russh::Disconnect::ByApplication,
            "",
            "English",
        ));
    }
}

/// Resolve the argument to `ssh` into a concrete `(host, port, user)`: either a
/// saved-remote index, or a freeform `[user@]host[:port]` spec. Prints a usage
/// / error message and returns `None` on failure.
pub fn resolve_ssh_target(state: &State, args: &str) -> Option<(String, u16, String)> {
    let args = args.trim();
    if args.is_empty() {
        eprintln!("ssh: usage: ssh [user@]host [port]  |  ssh <saved-index>  (see `remote list`)");
        return None;
    }
    // A bare number selects a saved remote by its `remote list` index.
    if let Ok(idx) = args.parse::<usize>() {
        let resolved = state.remote_terminals.lock().ok().and_then(|l| {
            l.get(idx)
                .map(|r| (r.host.clone(), r.port, r.username.clone()))
        });
        if resolved.is_none() {
            eprintln!("ssh: no saved remote at index {idx} (see `remote list`)");
        }
        return resolved;
    }
    Some(parse_remote_spec(args))
}

/// Dedupe-record a remote into the saved list (newest last), mirroring
/// `record_recent_dir`.
pub fn record_remote(state: &State, host: &str, port: u16, user: &str) {
    if let Ok(mut list) = state.remote_terminals.lock() {
        list.retain(|r| !(r.host == host && r.port == port && r.username == user));
        list.push(RemoteTerminal {
            host: host.to_string(),
            port,
            username: user.to_string(),
        });
    }
}

/// `ssh [user@]host [port]` / `ssh <index>` — connect and enter a remote
/// session. The password comes from the keyring if saved; otherwise we prompt
/// (hidden) and save it for next time.
pub fn cmd_ssh(state: &mut State, state_path: &Path, args: &str) {
    let Some((host, port, user)) = resolve_ssh_target(state, args) else {
        return;
    };

    let password = match secrets::get_password(&host, port, &user) {
        Some(p) => p,
        None => match read_password(&format!("Password for {user}@{host}: ")) {
            Ok(p) => {
                // Save for next time so future connects need no prompt.
                if let Err(e) = secrets::set_password(&host, port, &user, &p) {
                    eprintln!("ssh: warning: couldn't save password to keyring: {e}");
                }
                p
            }
            Err(e) => {
                eprintln!("ssh: {e}");
                return;
            }
        },
    };

    println!("Connecting to {user}@{host}:{port} …");
    match connect(&host, port, &user, &password) {
        Ok(session) => {
            println!(
                "Connected to {user}@{host}. Commands run remotely; `mode pty` opens an interactive shell (python, vim, …), `exit` disconnects."
            );
            record_remote(state, &host, port, &user);
            if let Err(e) = state.save(state_path) {
                eprintln!("warning: failed to save state: {e}");
            }
            state.active_remote = Some(session);

            // Stay in the shell's own command interface — prompt, history,
            // highlighting, completion — and route each typed command to the
            // remote via exec mode. (The session connects in PTY mode by
            // default for the GUI's sake; the CLI switches to exec so it keeps
            // its rich line editing instead of handing the terminal to a raw
            // remote shell.) The user opts into a raw interactive shell with
            // `mode pty` when one is actually needed — e.g. python, vim, top.
            if let Some(session) = state.active_remote.as_mut()
                && let Err(e) = session.set_mode(SshMode::Exec)
            {
                eprintln!("ssh: {e}");
            }
        }
        Err(e) => eprintln!("ssh: {e}"),
    }
}

/// Render the saved-remotes list in the same paginated frame as the
/// history / recent-directories / bookmarks lists.
pub fn render_remotes(remotes: &[RemoteTerminal], page: usize) -> String {
    crate::render_page(
        "Remotes",
        "Ctrl+R again: older page",
        "Use `ssh <number>` to connect, `remote del <number>` to delete; e.g. `ssh 0`",
        "(no saved remotes — add one with `remote add user@host`)",
        remotes,
        page,
        crate::DISP_PAGE_LEN,
        |i, r| format!("{i}:  {}@{}:{}", r.username, r.host, r.port),
    )
}

/// `remote list | add <[user@]host[:port]> | del <index>` — manage saved
/// remotes and their keyring passwords.
pub fn cmd_remote(state: &mut State, state_path: &Path, args: &str) {
    let (sub, rest) = split_first_word(args);
    match sub {
        "" | "list" | "ls" => {
            let Ok(list) = state.remote_terminals.lock() else {
                eprintln!("remote: list lock poisoned");
                return;
            };
            print!("{}", render_remotes(&list, 0));
        }
        "add" => {
            if rest.is_empty() {
                eprintln!("remote: usage: remote add [user@]host[:port] [port]");
                return;
            }
            let (host, port, user) = parse_remote_spec(rest);
            match read_password(&format!("Password for {user}@{host} (blank to skip): ")) {
                Ok(pw) if !pw.is_empty() => {
                    if let Err(e) = secrets::set_password(&host, port, &user, &pw) {
                        eprintln!("remote: warning: couldn't save password to keyring: {e}");
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!("remote: {e}");
                    return;
                }
            }
            record_remote(state, &host, port, &user);
            if let Err(e) = state.save(state_path) {
                eprintln!("remote: failed to save state: {e}");
            }
            println!("Saved remote {user}@{host}:{port}");
        }
        "del" | "rm" | "remove" => match rest.parse::<usize>() {
            Ok(idx) => {
                let removed = match state.remote_terminals.lock() {
                    Ok(mut list) if idx < list.len() => Some(list.remove(idx)),
                    Ok(_) => {
                        eprintln!("remote: no saved remote at index {idx}");
                        None
                    }
                    Err(_) => {
                        eprintln!("remote: list lock poisoned");
                        None
                    }
                };
                if let Some(r) = removed {
                    let _ = secrets::delete_password(&r.host, r.port, &r.username);
                    if let Err(e) = state.save(state_path) {
                        eprintln!("remote: failed to save state: {e}");
                    }
                    println!("Removed remote {}@{}:{}", r.username, r.host, r.port);
                }
            }
            Err(_) => eprintln!("remote: usage: remote del <index>"),
        },
        other => {
            eprintln!("remote: unknown subcommand `{other}` (expected list/add/del)");
        }
    }
}

/// The OS account name, used as the default SSH username when the user writes
/// just `ssh host` (no `user@`).
fn default_user() -> String {
    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "root".to_string())
}

/// Split `args` into its first whitespace-delimited word and the trimmed rest.
fn split_first_word(args: &str) -> (&str, &str) {
    match args.find(char::is_whitespace) {
        Some(i) => (&args[..i], args[i..].trim()),
        None => (args, ""),
    }
}

/// Parse a `[user@]host[:port] [port]` spec into `(host, port, user)`. The
/// optional trailing port argument wins over a `:port` suffix. Defaults: user =
/// the OS account, port = 22.
fn parse_remote_spec(spec: &str) -> (String, u16, String) {
    let (target, rest) = split_first_word(spec);
    let port_arg = rest.split_whitespace().next().and_then(|p| p.parse().ok());

    let (user, hostport) = match target.split_once('@') {
        Some((u, hp)) => (u.to_string(), hp),
        None => (default_user(), target),
    };
    let (host, port) = match hostport.split_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().unwrap_or(22)),
        None => (hostport.to_string(), 22),
    };
    (host, port_arg.unwrap_or(port), user)
}

/// Prompt for a password on the CLI without echoing it. Uses crossterm raw mode
/// to read characters silently; Enter finishes, Esc / Ctrl+C aborts.
fn read_password(prompt: &str) -> io::Result<String> {
    use std::io::Write;

    use crossterm::{
        event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
        terminal,
    };

    print!("{prompt}");
    io::stdout().flush()?;

    terminal::enable_raw_mode()?;
    let mut pw = String::new();
    let mut read = || -> io::Result<bool> {
        loop {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Release {
                    continue;
                }
                if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                    return Ok(false); // aborted
                }
                match key.code {
                    KeyCode::Enter => return Ok(true),
                    KeyCode::Esc => return Ok(false),
                    KeyCode::Backspace => {
                        pw.pop();
                    }
                    KeyCode::Char(c) => pw.push(c),
                    _ => {}
                }
            }
        }
    };
    let finished = read();
    terminal::disable_raw_mode()?;
    println!();

    match finished {
        Ok(true) => Ok(pw),
        Ok(false) => Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "password entry cancelled",
        )),
        Err(e) => Err(e),
    }
}

/// Build the remote shell command that `sync <message>` maps to over SSH.
/// Mirrors the local [`crate::commands::sync`] steps (add → commit → push) as a
/// single `&&`-chained line so it stops on the first failure, run from the
/// session's tracked remote cwd. Returns `None` (after emitting a diagnostic
/// via `sink`) when the commit message is empty, matching the local guard.
pub(crate) fn remote_sync_command(message: &str, sink: OutputSink) -> Option<String> {
    let message = message.trim().trim_matches('"');
    if message.is_empty() {
        sink(
            OutKind::Stderr,
            "sync: commit message required, e.g. sync \"my commit message\"".to_string(),
        );
        return None;
    }
    // Single-quote the message so spaces/specials survive the remote shell;
    // escape any embedded single quotes the usual `'\''` way.
    let escaped = message.replace('\'', "'\\''");
    Some(format!(
        "git add . && git commit -am '{escaped}' && git push"
    ))
}

/// Build the remote shell command that `logs <service>` maps to over SSH.
/// Mirrors the non-follow local path ([`crate::commands::logs`] with
/// `follow = false`): a bounded, non-paged snapshot, since exec-mode SSH has no
/// tty to host a live `-f` tail. `journalctl` runs under `sudo` to match the
/// local built-in — over exec mode (no tty) that needs passwordless sudo on the
/// remote, otherwise sudo's prompt surfaces as a stderr diagnostic. Returns
/// `None` (after emitting a diagnostic via `sink`) when the service name is
/// empty, matching the local guard.
pub(crate) fn remote_logs_command(service: &str, sink: OutputSink) -> Option<String> {
    let service = service.trim().trim_matches('"');
    if service.is_empty() {
        sink(
            OutKind::Stderr,
            "logs: service required, e.g. logs gunicorn".to_string(),
        );
        return None;
    }
    // Single-quote the unit name so it survives the remote shell intact.
    let escaped = service.replace('\'', "'\\''");
    Some(format!("sudo journalctl -u '{escaped}' -n 200 --no-pager"))
}

/// `mode exec|pty` while connected: switch the live session's interaction mode.
/// Entering PTY mode runs the raw-terminal interactive loop until the remote
/// shell exits (then we disconnect) or the user detaches with Ctrl+] (then we
/// return to exec mode, keeping the connection).
pub(crate) fn remote_set_mode(state: &mut State, args: &str) {
    let target = match args.trim() {
        "pty" | "interactive" | "shell" => SshMode::Pty,
        "exec" | "cmd" | "" => SshMode::Exec,
        other => {
            eprintln!("mode: expected `exec` or `pty`, got `{other}`");
            return;
        }
    };

    if target == SshMode::Exec {
        if let Some(session) = state.active_remote.as_mut()
            && let Err(e) = session.set_mode(SshMode::Exec)
        {
            eprintln!("mode: {e}");
        }
        return;
    }

    // Entering PTY mode: open the shell channel (a no-op if we're already in
    // PTY mode, e.g. straight after connect), then drive the interactive loop.
    if let Some(session) = state.active_remote.as_mut()
        && let Err(e) = session.set_mode(SshMode::Pty)
    {
        eprintln!("mode: {e}");
        return;
    }
    enter_pty_loop(state);
}

/// Drive the raw-terminal interactive loop for the active session's PTY,
/// returning to the caller when the remote shell exits (we disconnect) or the
/// user detaches with Ctrl+] (we drop back to exec mode, keeping the
/// connection). The session must already be in [`SshMode::Pty`]. Shared by the
/// initial connect (which lands in PTY mode by default) and `mode pty`.
pub fn enter_pty_loop(state: &mut State) {
    if let Some(session) = state.active_remote.as_mut() {
        println!("-- interactive shell (Ctrl+] to detach to command mode) --");
        if let Err(e) = cli_pty_loop(session) {
            eprintln!("mode: pty: {e}");
        }
    }

    // The loop ended: either the remote shell exited (disconnect entirely) or
    // the user detached (drop back to exec mode, keeping the connection).
    let closed = state
        .active_remote
        .as_ref()
        .map(|s| s.pty_closed())
        .unwrap_or(false);
    if closed {
        if let Some(session) = state.active_remote.take() {
            session.disconnect();
        }
        println!("ssh: remote shell exited; disconnected");
    } else if let Some(session) = state.active_remote.as_mut() {
        let _ = session.set_mode(SshMode::Exec);
        println!(
            "ssh: detached to command mode (`mode pty` resumes the interactive shell, `exit` disconnects)"
        );
    }
}

/// Translate a crossterm key event into the bytes to feed the remote PTY.
/// Covers printable chars, Ctrl-letters, and the common navigation/edit keys.
pub fn key_to_bytes(key: crossterm::event::KeyEvent) -> Option<Vec<u8>> {
    use crossterm::event::{KeyCode, KeyModifiers};

    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                let upper = c.to_ascii_uppercase();
                if upper.is_ascii_alphabetic() {
                    return Some(vec![upper as u8 - b'A' + 1]);
                }
                match c {
                    ' ' => return Some(vec![0]),
                    '\\' => return Some(vec![0x1c]),
                    _ => {}
                }
            }
            let mut buf = [0u8; 4];
            Some(c.encode_utf8(&mut buf).as_bytes().to_vec())
        }
        KeyCode::Enter => Some(vec![b'\r']),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Tab => Some(vec![b'\t']),
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Up => Some(b"\x1b[A".to_vec()),
        KeyCode::Down => Some(b"\x1b[B".to_vec()),
        KeyCode::Right => Some(b"\x1b[C".to_vec()),
        KeyCode::Left => Some(b"\x1b[D".to_vec()),
        KeyCode::Home => Some(b"\x1b[H".to_vec()),
        KeyCode::End => Some(b"\x1b[F".to_vec()),
        KeyCode::Delete => Some(b"\x1b[3~".to_vec()),
        KeyCode::PageUp => Some(b"\x1b[5~".to_vec()),
        KeyCode::PageDown => Some(b"\x1b[6~".to_vec()),
        _ => None,
    }
}

/// Raw-terminal interactive loop for PTY mode. Puts the terminal into raw mode,
/// pumps keystrokes to the remote and remote output to stdout, and returns when
/// the remote shell closes or the user presses Ctrl+] to detach. Always
/// restores cooked mode on the way out.
fn cli_pty_loop(session: &mut RemoteSession) -> io::Result<()> {
    use std::io::Write;

    use crossterm::{
        event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
        terminal,
    };

    terminal::enable_raw_mode()?;
    if let Ok((cols, rows)) = terminal::size() {
        let _ = session.window_change(cols as u32, rows as u32);
    }

    // Inner closure so we can always disable raw mode afterwards, even on error.
    let pump = || -> io::Result<()> {
        let mut stdout = io::stdout();
        loop {
            if session.pty_closed() {
                break;
            }
            let out = session.drain_output();
            if !out.is_empty() {
                stdout.write_all(&out)?;
                stdout.flush()?;
            }
            // Short poll so we keep draining output even while idle.
            if event::poll(std::time::Duration::from_millis(10))? {
                match event::read()? {
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        // Ctrl+] detaches back to exec mode without closing the shell.
                        if key.modifiers.contains(KeyModifiers::CONTROL)
                            && key.code == KeyCode::Char(']')
                        {
                            break;
                        }
                        if let Some(bytes) = key_to_bytes(key) {
                            session.send(&bytes)?;
                        }
                    }
                    Event::Resize(cols, rows) => {
                        let _ = session.window_change(cols as u32, rows as u32);
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    };

    let result = pump();
    terminal::disable_raw_mode()?;

    // Flush any trailing output produced after the loop broke.
    let out = session.drain_output();
    if !out.is_empty() {
        let mut stdout = io::stdout();
        let _ = stdout.write_all(&out);
        let _ = stdout.flush();
    }
    println!();
    result
}
