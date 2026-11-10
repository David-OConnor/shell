//! OS keyring access for SSH passwords. We don't store SSH passwords in the
//! plaintext state file anymore (see `save_data`); they live in the platform
//! credential store instead (Windows Credential Manager / macOS Keychain /
//! Linux Secret Service), keyed by `<user>@<host>:<port>`.
//!
//! Both the CLI and GUI frontends call these helpers: when a remote is saved
//! they `set_password`, when connecting they `get_password`, and when a remote
//! is deleted they `delete_password`. All failures are surfaced as `io::Error`
//! and treated as non-fatal by callers (a missing password just means we
//! prompt / fail the connect, not that the app breaks).

use std::io;

use keyring::{Entry, Error as KeyringError};

/// Service name under which all of this app's SSH credentials are grouped in
/// the OS store. Browsable e.g. via `cmdkey /list` on Windows.
const SERVICE: &str = "shell-ssh";

/// The per-remote account key. Matches the human-readable `user@host:port`
/// form so credentials are easy to recognise in the OS credential manager.
fn account(host: &str, port: u16, user: &str) -> String {
    format!("{user}@{host}:{port}")
}

/// Map a keyring error into an `io::Error` so callers can use `?` alongside
/// the rest of the shell's `io::Result`-based plumbing.
fn to_io(e: KeyringError) -> io::Error {
    io::Error::other(format!("keyring: {e}"))
}

/// Store (or overwrite) the password for a remote in the OS credential store.
pub fn set_password(host: &str, port: u16, user: &str, password: &str) -> io::Result<()> {
    let entry = Entry::new(SERVICE, &account(host, port, user)).map_err(to_io)?;
    entry.set_password(password).map_err(to_io)
}

/// Look up a stored password. Returns `None` when there's no saved credential
/// (or the store is unavailable) rather than erroring — callers treat a
/// missing password as "ask the user / can't auto-connect".
pub fn get_password(host: &str, port: u16, user: &str) -> Option<String> {
    let entry = Entry::new(SERVICE, &account(host, port, user)).ok()?;
    entry.get_password().ok()
}

/// Remove a stored password. A missing entry is treated as success — deleting
/// a remote that never had a saved password is not an error.
pub fn delete_password(host: &str, port: u16, user: &str) -> io::Result<()> {
    let entry = Entry::new(SERVICE, &account(host, port, user)).map_err(to_io)?;
    match entry.delete_credential() {
        Ok(()) | Err(KeyringError::NoEntry) => Ok(()),
        Err(e) => Err(to_io(e)),
    }
}
