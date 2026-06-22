//! Exec mode: run one command per channel and capture its output in a batch,
//! matching how the shell already runs local commands (`Command::output()`).
//!
//! Each SSH `exec` is an independent shell, so a remote `cd` wouldn't normally
//! persist to the next command. We work around that by tracking the remote cwd
//! ourselves: every command is wrapped so it first `cd`s into the tracked dir,
//! then — after the user's command runs — prints a marker followed by `pwd`. We
//! parse that trailing line back out of stdout to learn the new cwd, so a
//! `cd subdir` followed by `pwd` behaves as the user expects.

use std::io;

use russh::ChannelMsg;

use super::{RemoteSession, map_err};
use crate::commands::{OutKind, OutputSink};

/// Sentinel that separates the command's real stdout from the trailing `pwd`
/// we append for cwd tracking. Unlikely to collide with real output.
const CWD_MARK: &str = "\x01__SHELL_REMOTE_CWD__\x01";

impl RemoteSession {
    /// Run `command` on the remote, routing stdout/stderr through `sink`. Runs
    /// from (and updates) the tracked remote cwd so `cd` persists across calls.
    pub fn run(&mut self, command: &str, sink: OutputSink) -> io::Result<()> {
        // Build: optionally cd into the tracked dir, run the user's command,
        // then emit MARK + pwd on stdout so we can recover the resulting cwd.
        // Single-quote the tracked path (it came from a previous `pwd`, so it's
        // absolute) to tolerate spaces; an empty cwd means "first command —
        // run in the login shell's default dir".
        let cd_prefix = if self.cwd.is_empty() {
            String::new()
        } else {
            format!("cd '{}' 2>/dev/null; ", self.cwd.replace('\'', "'\\''"))
        };
        let wrapped = format!("{cd_prefix}{command}\nprintf '%s\\n' '{CWD_MARK}'; pwd");

        let handle = &self.handle;
        let mut stdout_buf: Vec<u8> = Vec::new();
        self.rt.block_on(async {
            let mut channel = handle.channel_open_session().await.map_err(map_err)?;
            channel
                .exec(true, wrapped.as_bytes())
                .await
                .map_err(map_err)?;
            // Exec mode never feeds stdin, so close the input direction right
            // away. Without this, a command that reads stdin (`python`, `cat`
            // with no args, …) blocks forever waiting for bytes that will never
            // come; EOF lets it run to completion (or exit cleanly) instead.
            // Interactive programs belong in PTY mode (`mode pty`).
            channel.eof().await.map_err(map_err)?;
            while let Some(msg) = channel.wait().await {
                match msg {
                    ChannelMsg::Data { ref data } => stdout_buf.extend_from_slice(data),
                    ChannelMsg::ExtendedData { ref data, .. } => {
                        // ext == 1 is stderr; route everything extended as stderr.
                        sink(OutKind::Stderr, String::from_utf8_lossy(data).into_owned());
                    }
                    ChannelMsg::Eof | ChannelMsg::Close => break,
                    _ => {}
                }
            }
            Ok::<_, io::Error>(())
        })?;

        // Split the captured stdout at the cwd marker: everything before is real
        // output; the line after is the remote's new cwd.
        let text = String::from_utf8_lossy(&stdout_buf);
        if let Some(idx) = text.rfind(CWD_MARK) {
            let (out, rest) = text.split_at(idx);
            let new_cwd = rest[CWD_MARK.len()..].trim();
            if !new_cwd.is_empty() {
                self.cwd = new_cwd.to_string();
            }
            let out = out.strip_suffix('\n').unwrap_or(out);
            if !out.is_empty() {
                sink(OutKind::Stdout, out.to_string());
            }
        } else if !text.is_empty() {
            // No marker (unusual — e.g. the shell died early). Emit as-is.
            sink(OutKind::Stdout, text.into_owned());
        }

        Ok(())
    }
}
