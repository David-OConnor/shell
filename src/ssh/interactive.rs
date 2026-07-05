//! Interactive / PTY mode: one persistent channel with a server-side PTY and
//! login shell, bytes streamed both ways. Unlike exec mode this preserves shell
//! state (cwd, env, running TUIs) and supports interactive programs.
//!
//! Because output arrives unsolicited (the user may run `top`, or the remote
//! may print without input), a background task drains the channel's read half
//! and forwards bytes through an `mpsc` channel. Callers poll [`drain_output`]
//! (the GUI does this each frame; the CLI raw-mode loop does it in a tight
//! loop) and write input via [`send`]. This task runs on the session's
//! multi-threaded runtime, so it keeps streaming even between `block_on`s.
//!
//! [`drain_output`]: RemoteSession::drain_output
//! [`send`]: RemoteSession::send

use std::{
    env, io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, TryRecvError},
    },
};

use russh::ChannelMsg;

use super::{PtyChannel, RemoteSession, map_err};

impl RemoteSession {
    /// Open the PTY + shell channel and spawn the reader task. Called by
    /// `set_mode(Pty)`.
    pub(super) fn start_pty(&mut self) -> io::Result<()> {
        let term = env::var("TERM").unwrap_or_else(|_| "xterm-256color".to_string());
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        let closed = Arc::new(AtomicBool::new(false));
        let reader_closed = closed.clone();

        let handle = &self.handle;
        let writer = self.rt.block_on(async {
            let channel = handle.channel_open_session().await.map_err(map_err)?;
            // 80x24 is a reasonable default; the frontend can follow up with
            // `window_change` once it knows its real terminal size.
            channel
                .request_pty(false, &term, 80, 24, 0, 0, &[])
                .await
                .map_err(map_err)?;
            channel.request_shell(true).await.map_err(map_err)?;

            let (mut read_half, write_half) = channel.split();
            tokio::spawn(async move {
                while let Some(msg) = read_half.wait().await {
                    match msg {
                        ChannelMsg::Data { data } | ChannelMsg::ExtendedData { data, .. } => {
                            // Receiver gone (session dropped): stop reading.
                            if tx.send(data.to_vec()).is_err() {
                                break;
                            }
                        }
                        ChannelMsg::Eof | ChannelMsg::Close | ChannelMsg::ExitStatus { .. } => {
                            break;
                        }
                        _ => {}
                    }
                }
                reader_closed.store(true, Ordering::SeqCst);
            });

            Ok::<_, io::Error>(write_half)
        })?;

        self.pty = Some(PtyChannel { writer, rx, closed });
        Ok(())
    }

    /// Tear down the PTY channel (best-effort EOF on the way out). Called by
    /// `set_mode(Exec)` and `disconnect`.
    pub(super) fn stop_pty(&mut self) {
        if let Some(pty) = self.pty.take() {
            let _ = self.rt.block_on(pty.writer.eof());
        }
    }

    /// Send raw bytes (keystrokes / a line of input) to the remote shell.
    /// No-op when not in PTY mode.
    pub fn send(&self, bytes: &[u8]) -> io::Result<()> {
        if let Some(pty) = &self.pty {
            self.rt
                .block_on(pty.writer.data_bytes(bytes.to_vec()))
                .map_err(map_err)?;
        }
        Ok(())
    }

    /// Pull any output bytes received since the last call, without blocking.
    /// Returns an empty vec when idle or not in PTY mode.
    pub fn drain_output(&self) -> Vec<u8> {
        let mut out = Vec::new();
        if let Some(pty) = &self.pty {
            loop {
                match pty.rx.try_recv() {
                    Ok(chunk) => out.extend_from_slice(&chunk),
                    Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
                }
            }
        }
        out
    }

    /// Whether the remote shell has closed (user typed `exit`, connection
    /// dropped). The frontend uses this to drop back to local mode.
    pub fn pty_closed(&self) -> bool {
        self.pty
            .as_ref()
            .map(|p| p.closed.load(Ordering::SeqCst))
            .unwrap_or(false)
    }

    /// Tell the remote about a new terminal size so full-screen apps redraw
    /// correctly. No-op when not in PTY mode.
    pub fn window_change(&self, cols: u32, rows: u32) -> io::Result<()> {
        if let Some(pty) = &self.pty {
            self.rt
                .block_on(pty.writer.window_change(cols, rows, 0, 0))
                .map_err(map_err)?;
        }
        Ok(())
    }
}
