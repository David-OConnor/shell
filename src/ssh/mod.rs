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
    sync::{Arc, atomic::AtomicBool, mpsc::Receiver},
    time::Duration,
};

use russh::{
    ChannelWriteHalf,
    client::{self, Handle, Msg},
    keys::ssh_key,
};
use tokio::runtime::Runtime;

mod exec;
mod interactive;

/// Which interaction mode a live session is in. Toggled at runtime; defaults to
/// [`SshMode::Exec`] on connect.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SshMode {
    /// One channel per typed command, output captured in a batch.
    Exec,
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

    Ok(RemoteSession {
        rt,
        handle,
        user: user.to_string(),
        host: host.to_string(),
        port,
        mode: SshMode::Exec,
        cwd: String::new(),
        pty: None,
    })
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
