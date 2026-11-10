//! In-process SSH client, so the shell handles `ssh` itself instead of leaking
//! the command through to the OS's native `ssh` binary.
//!
//! Built on [`russh`] (pure-Rust, no OpenSSL/libssh2 C build on Windows).
//! [session] holds the connection itself, shared with `peregrine`: one
//! persistent PTY running the remote login shell, so state (cwd, env,
//! running programs) carries over between commands and interactive programs
//! work. [cli] is the CLI's frontend for it; [keys] encodes keystrokes for
//! both frontends.
//!
//! Credentials are never held here long-term: the caller passes the password
//! in (from the OS keyring; see [secrets]), and SSH keys are read from an
//! agent or `~/.ssh` as needed.

use std::{io, path::Path};

use crate::{
    RemoteTerminal,
    commands::{OutKind, OutputSink},
    state::State,
};

mod cli;
pub mod keys;
pub mod secrets;
mod session;

pub use session::{
    ConnectError, ConnectRequest, Connecting, Phase, RemoteEvent, RemoteSession, Waker, connect,
    no_waker,
};

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
/// session. SSH keys (an agent's, or `~/.ssh/id_*`) are tried, and the
/// keyring's saved password; failing those we ask for a password, and save it
/// once it has worked.
pub fn cmd_ssh(state: &mut State, state_path: &Path, args: &str) {
    let Some((host, port, user)) = resolve_ssh_target(state, args) else {
        return;
    };

    let mut password = secrets::get_password(&host, port, &user);
    // Set once the user has typed a password, to save it if it works.
    let mut typed = false;
    for _ in 0..3 {
        println!("Connecting to {user}@{host}:{port} … (Esc cancels)");
        let (cols, rows) = cli::terminal_size();
        let request = ConnectRequest {
            host: host.clone(),
            port,
            user: user.clone(),
            password: password.clone(),
            cols,
            rows,
        };
        match cli::wait_connect(connect(request, no_waker())) {
            Ok(session) => {
                if typed
                    && let Some(p) = &password
                    && let Err(e) = secrets::set_password(&host, port, &user, p)
                {
                    eprintln!("ssh: warning: couldn't save password to keyring: {e}");
                }
                record_remote(state, &host, port, &user);
                if let Err(e) = state.save(state_path) {
                    eprintln!("warning: failed to save state: {e}");
                }
                state.active_remote = Some(session);
                start_session(state);
                return;
            }
            Err(ConnectError::Auth { password_tried }) => {
                if password_tried {
                    eprintln!("ssh: wrong password for {user}@{host}");
                }
                match read_password(&format!("Password for {user}@{host}: ")) {
                    Ok(p) => {
                        password = Some(p);
                        typed = true;
                    }
                    Err(e) => {
                        eprintln!("ssh: {e}");
                        return;
                    }
                }
            }
            Err(e) => {
                eprintln!("ssh: {e}");
                return;
            }
        }
    }
    eprintln!("ssh: giving up after 3 tries");
}

/// Wait for a new session's shell to come up, then report it.
fn start_session(state: &mut State) {
    let Some(session) = state.active_remote.as_mut() else {
        return;
    };
    let label = session.label();
    let outcome = cli::finish_starting(session);
    if !end_if_over(state, outcome) {
        println!(
            "Connected to {label}. Commands run there now; `exit` logs out. If it stops responding, Enter then ~. disconnects."
        );
    }
}

/// Run `line` on the remote, handing it the terminal until it's done.
pub(crate) fn run_remote(state: &mut State, line: &str) {
    let Some(session) = state.active_remote.as_mut() else {
        return;
    };
    session.run(line);
    let outcome = cli::passthrough(session);
    end_if_over(state, outcome);
}

/// Disconnect after a session's terminal reports it's over. Returns whether
/// it was.
fn end_if_over(state: &mut State, outcome: io::Result<cli::Outcome>) -> bool {
    let message = match outcome {
        Ok(cli::Outcome::Ready) => return false,
        Ok(cli::Outcome::Closed) => "ssh: connection closed".to_string(),
        Ok(cli::Outcome::Dropped) => "ssh: disconnected".to_string(),
        Err(e) => format!("ssh: terminal error: {e}; disconnected"),
    };
    if let Some(session) = state.active_remote.take() {
        session.disconnect();
    }
    println!("{message}");
    true
}

/// Render the saved-remotes list in the same paginated frame as the
/// history / recent-directories / bookmarks lists.
pub fn render_remotes(remotes: &[RemoteTerminal], page: usize) -> String {
    crate::render_page(
        "Remotes",
        "Ctrl+5 again: older page",
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
/// single `&&`-chained line so it stops on the first failure. Returns `None`
/// (after emitting a diagnostic via `sink`) when the commit message is empty,
/// matching the local guard.
pub fn remote_sync_command(message: &str, sink: OutputSink) -> Option<String> {
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

/// Build the remote shell command that `logs <service>` maps to over SSH,
/// matching the local [`crate::commands::logs`]: a live tail with `follow`
/// (Ctrl+C ends it), otherwise a bounded snapshot. It runs in the remote
/// terminal, so `sudo` can ask for its password. Returns `None` (after
/// emitting a diagnostic via `sink`) when the service name is empty, matching
/// the local guard.
pub fn remote_logs_command(service: &str, follow: bool, sink: OutputSink) -> Option<String> {
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
    Some(if follow {
        format!("sudo journalctl -u '{escaped}' -f")
    } else {
        format!("sudo journalctl -u '{escaped}' -n 200 --no-pager")
    })
}
