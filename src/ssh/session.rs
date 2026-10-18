//! One SSH connection running the user's login shell in a remote PTY, as
//! `ssh host` does. Both frontends drive it the same way: [connect] starts the
//! connection in the background and returns at once; [Connecting::poll] hands
//! over the [RemoteSession] once it's up; from then on, lines go in with
//! [RemoteSession::run], keystrokes with [RemoteSession::send], and output
//! comes back from [RemoteSession::poll]. Nothing here blocks the caller.
//!
//! ## Knowing when a command is done
//!
//! The frontends keep their own command line (history, completion, the
//! shell's built-ins), so they need to know when the remote shell is back at
//! its prompt. On connect we install a small prompt hook in the remote shell
//! (bash, zsh, or any POSIX `sh`; see [bootstrap]): before each prompt it
//! turns the terminal's echo off (the frontend shows what's typed) and prints
//! an OSC escape carrying the exit status, `$PWD`, and `$HOME`. The reader
//! task strips those out of the output stream and reports them as
//! [RemoteEvent::Ready]. While a command runs the terminal is a normal one,
//! echo on, so `vim`, `top`, `sudo`'s password prompt, a Python REPL, or a
//! nested shell work as they would over plain `ssh`.
//!
//! When the hook can't be installed (fish, a restricted shell, a network
//! device's CLI), no marker ever arrives; after [BOOTSTRAP_TIMEOUT] the
//! session falls back to [Phase::Raw], a plain terminal with the remote's own
//! prompt.
//!
//! russh is async: each connection owns a small Tokio runtime, whose tasks
//! read the channel, write to it, and keep the connection alive.

use std::{
    fmt,
    path::PathBuf,
    sync::{Arc, mpsc as std_mpsc},
    time::Duration,
};

use russh::{
    ChannelMsg, ChannelReadHalf, ChannelWriteHalf, MethodKind, Pty,
    client::{self, AuthResult, Handle, KeyboardInteractiveAuthResponse},
    keys::{self, PrivateKeyWithHashAlg, agent::client::AgentClient, ssh_key},
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    runtime::Runtime,
    sync::mpsc,
    time::{Instant, timeout, timeout_at},
};

use super::keys::encode_paste;

/// Called from the connection's background tasks whenever there's news (it
/// connected, output arrived, it closed), so a GUI can repaint.
pub type Waker = Arc<dyn Fn() + Send + Sync>;

/// A [Waker] that does nothing, for frontends that poll anyway.
pub fn no_waker() -> Waker {
    Arc::new(|| {})
}

/// Reaching the host: DNS, TCP, and the SSH handshake. An unreachable host
/// otherwise takes as long as the OS's TCP timeout to fail.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const AUTH_TIMEOUT: Duration = Duration::from_secs(30);
const OPEN_TIMEOUT: Duration = Duration::from_secs(15);
/// Each SSH agent operation.
const AGENT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long to wait for the first prompt marker before treating the remote
/// as a plain terminal. Slow shell startup files (nvm, conda) count against
/// it; a marker that arrives later still switches the session over.
const BOOTSTRAP_TIMEOUT: Duration = Duration::from_secs(6);
/// A connection that stops answering is closed after about
/// `KEEPALIVE_INTERVAL * (KEEPALIVE_MAX + 1)`, rather than hanging forever.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);
const KEEPALIVE_MAX: usize = 3;
/// The terminal type we emulate. Both frontends understand xterm sequences.
const TERM: &str = "xterm-256color";
/// Output chunks queued for the frontend. When it falls behind, the reader
/// waits, which (through SSH flow control) slows the remote down instead of
/// growing memory.
const EVENT_QUEUE: usize = 64;

/// Introduces a prompt marker; see [bootstrap]. An OSC with an unassigned
/// number, which terminals ignore should one ever slip through.
const MARKER: &[u8] = b"\x1b]7770;";
/// Separates the marker's fields.
const FIELD_SEP: u8 = 0x1f;
/// A longer "marker" isn't one of ours.
const MARKER_MAX: usize = 8192;

/// Where to connect, and as whom.
#[derive(Clone)]
pub struct ConnectRequest {
    pub host: String,
    pub port: u16,
    pub user: String,
    /// Tried first when given. Without one, only SSH keys (from an agent, or
    /// `~/.ssh/id_*` without a passphrase) are tried.
    pub password: Option<String>,
    /// The terminal size to start the remote PTY at.
    pub cols: u16,
    pub rows: u16,
}

/// Why a connection attempt failed.
#[derive(Debug)]
pub enum ConnectError {
    /// The host didn't answer within [CONNECT_TIMEOUT].
    Timeout,
    /// The host couldn't be reached: unknown name, refused, unreachable.
    Network(String),
    /// The server turned down every way we had to log in. `password_tried`
    /// says whether a password was among them, so the caller can tell
    /// "wrong password" from "needs a password".
    Auth {
        password_tried: bool,
    },
    Cancelled,
    Other(String),
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout => write!(
                f,
                "no answer from the host within {} s",
                CONNECT_TIMEOUT.as_secs()
            ),
            Self::Network(e) | Self::Other(e) => f.write_str(e),
            Self::Auth {
                password_tried: true,
            } => f.write_str("authentication failed: wrong password?"),
            Self::Auth {
                password_tried: false,
            } => f.write_str(
                "authentication failed: no SSH key was accepted, and no password is saved",
            ),
            Self::Cancelled => f.write_str("cancelled"),
        }
    }
}

impl std::error::Error for ConnectError {}

/// Where the remote shell is at.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    /// Connected; the shell is starting and the prompt hook going in.
    Starting,
    /// At its prompt, waiting for a command.
    Idle,
    /// Running a command sent with [RemoteSession::run].
    Busy,
    /// The prompt hook couldn't be installed: a plain terminal, with the
    /// remote's own prompt and line editing.
    Raw,
}

/// Something from the remote, from [RemoteSession::poll].
#[derive(Debug)]
pub enum RemoteEvent {
    /// Terminal output, prompt markers removed.
    Output(Vec<u8>),
    /// The shell is at its prompt: the last command finished (with this exit
    /// status, when known), or the session just finished starting. The
    /// session's [cwd](RemoteSession::cwd) is updated.
    Ready { status: Option<i32> },
    /// The remote shell exited, or the connection dropped.
    Closed,
}

/// russh event handler. Only server-key verification is needed.
pub(crate) struct Handler;

impl client::Handler for Handler {
    type Error = russh::Error;

    // todo: Verify against `~/.ssh/known_hosts` and refuse a changed key.
    // Until then every host key is accepted, as it was before.
    async fn check_server_key(
        &mut self,
        _server_public_key: &ssh_key::PublicKey,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

/// For the writer task.
enum Input {
    Data(Vec<u8>),
    Resize(u16, u16),
}

/// From the reader task.
enum Msg {
    Output(Vec<u8>),
    Prompt(PromptInfo),
    /// No prompt marker within [BOOTSTRAP_TIMEOUT].
    NoHook,
    Closed,
}

#[derive(Debug, PartialEq)]
struct PromptInfo {
    status: Option<i32>,
    cwd: String,
    home: String,
}

/// What [establish] hands to [Connecting].
struct Parts {
    handle: Handle<Handler>,
    input: mpsc::UnboundedSender<Input>,
    events: mpsc::Receiver<Msg>,
    size: (u16, u16),
}

/// A connection being made in the background. Dropping it cancels the
/// attempt.
pub struct Connecting {
    rt: Option<Runtime>,
    rx: std_mpsc::Receiver<Result<Parts, ConnectError>>,
    host: String,
    port: u16,
    user: String,
}

/// Start connecting to `req.host`, in the background. `waker` is called when
/// the attempt finishes, and later whenever the session has news.
pub fn connect(req: ConnectRequest, waker: Waker) -> Connecting {
    let (tx, rx) = std_mpsc::channel();
    let (host, port, user) = (req.host.clone(), req.port, req.user.clone());
    // One worker thread, so russh's session task, our reader and writer, and
    // keepalives all run whether or not the frontend is calling in.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_name("ssh")
        .enable_all()
        .build();
    let rt = match rt {
        Ok(rt) => {
            rt.spawn(async move {
                let result = establish(req, waker.clone()).await;
                let _ = tx.send(result);
                waker();
            });
            Some(rt)
        }
        Err(e) => {
            let _ = tx.send(Err(ConnectError::Other(format!("ssh: runtime: {e}"))));
            None
        }
    };
    Connecting {
        rt,
        rx,
        host,
        port,
        user,
    }
}

impl Connecting {
    /// `user@host`.
    pub fn label(&self) -> String {
        format!("{}@{}", self.user, self.host)
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn user(&self) -> &str {
        &self.user
    }

    /// The finished attempt, if it's finished. Never blocks.
    pub fn poll(&mut self) -> Option<Result<RemoteSession, ConnectError>> {
        match self.rx.try_recv() {
            Ok(result) => Some(self.finish(result)),
            Err(std_mpsc::TryRecvError::Empty) => None,
            Err(std_mpsc::TryRecvError::Disconnected) => Some(Err(ConnectError::Other(
                "ssh: the connection attempt ended unexpectedly".into(),
            ))),
        }
    }

    /// Like [Self::poll], waiting up to `wait` for the attempt to finish.
    pub fn poll_timeout(&mut self, wait: Duration) -> Option<Result<RemoteSession, ConnectError>> {
        match self.rx.recv_timeout(wait) {
            Ok(result) => Some(self.finish(result)),
            Err(std_mpsc::RecvTimeoutError::Timeout) => None,
            Err(std_mpsc::RecvTimeoutError::Disconnected) => Some(Err(ConnectError::Other(
                "ssh: the connection attempt ended unexpectedly".into(),
            ))),
        }
    }

    fn finish(
        &mut self,
        result: Result<Parts, ConnectError>,
    ) -> Result<RemoteSession, ConnectError> {
        let parts = result?;
        let rt = self
            .rt
            .take()
            .ok_or_else(|| ConnectError::Other("ssh: no runtime".into()))?;
        Ok(RemoteSession {
            rt: Some(rt),
            handle: Arc::new(parts.handle),
            input: parts.input,
            events: parts.events,
            host: self.host.clone(),
            port: self.port,
            user: self.user.clone(),
            phase: Phase::Starting,
            cwd: String::new(),
            home: String::new(),
            last_status: None,
            command: None,
            modes: ModeTracker::default(),
            size: parts.size,
            closed: false,
        })
    }
}

impl Drop for Connecting {
    fn drop(&mut self) {
        if let Some(rt) = self.rt.take() {
            // Cancels the attempt without waiting on the network.
            rt.shutdown_background();
        }
    }
}

/// A live, logged-in connection with the remote shell running in a PTY.
pub struct RemoteSession {
    rt: Option<Runtime>,
    handle: Arc<Handle<Handler>>,
    input: mpsc::UnboundedSender<Input>,
    events: mpsc::Receiver<Msg>,
    host: String,
    port: u16,
    user: String,
    phase: Phase,
    /// The remote's `$PWD` and `$HOME` at the last prompt; empty until known.
    cwd: String,
    home: String,
    last_status: Option<i32>,
    /// The line given to the running command's [Self::run].
    command: Option<String>,
    modes: ModeTracker,
    size: (u16, u16),
    closed: bool,
}

impl RemoteSession {
    /// `user@host`, for prompts and the Remote panel.
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

    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// Whether the shell is at its prompt, ready for [Self::run].
    pub fn at_prompt(&self) -> bool {
        self.phase == Phase::Idle
    }

    /// The remote working directory at the last prompt, absolute; empty
    /// until known.
    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    /// [Self::cwd] with the remote home shown as `~`, for prompts.
    pub fn display_cwd(&self) -> String {
        let (cwd, home) = (self.cwd.as_str(), self.home.trim_end_matches('/'));
        if cwd.is_empty() {
            return "~".to_string();
        }
        if !home.is_empty()
            && let Some(rest) = cwd.strip_prefix(home)
            && (rest.is_empty() || rest.starts_with('/'))
        {
            return format!("~{rest}");
        }
        cwd.to_string()
    }

    /// The exit status of the last command, when known.
    pub fn last_status(&self) -> Option<i32> {
        self.last_status
    }

    /// The line the running command was started with.
    pub fn command(&self) -> Option<&str> {
        self.command.as_deref()
    }

    /// Whether the remote program switched the cursor keys to application
    /// mode, which changes what the arrow keys send (see
    /// [super::keys::encode_key]).
    pub fn app_cursor(&self) -> bool {
        self.modes.app_cursor
    }

    /// Whether the remote program asked for pastes to be bracketed.
    pub fn bracketed_paste(&self) -> bool {
        self.modes.bracketed_paste
    }

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Run `line` in the remote shell, as if typed at its prompt. Its output
    /// arrives through [Self::poll], then [RemoteEvent::Ready] once it's done.
    /// In [Phase::Raw] the line is just typed into the terminal.
    pub fn run(&mut self, line: &str) {
        if self.phase == Phase::Idle {
            self.phase = Phase::Busy;
            self.command = Some(line.to_string());
        }
        let mut bytes = line.as_bytes().to_vec();
        bytes.push(b'\r');
        self.send(&bytes);
    }

    /// Send raw bytes to the remote terminal: keystrokes, or a line for the
    /// running program. Never blocks.
    pub fn send(&self, bytes: &[u8]) {
        if !bytes.is_empty() {
            let _ = self.input.send(Input::Data(bytes.to_vec()));
        }
    }

    /// Paste `text` into the running program (see [encode_paste]).
    pub fn send_paste(&self, text: &str) {
        self.send(&encode_paste(text, self.modes.bracketed_paste));
    }

    /// Ctrl+C, for the running command.
    pub fn interrupt(&self) {
        self.send(&[0x03]);
    }

    /// Tell the remote the terminal's new size, so full-screen programs
    /// redraw to fit. A no-op when unchanged.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        if cols == 0 || rows == 0 || (cols, rows) == self.size {
            return;
        }
        self.size = (cols, rows);
        let _ = self.input.send(Input::Resize(cols, rows));
    }

    /// The terminal size the remote was last told.
    pub fn size(&self) -> (u16, u16) {
        self.size
    }

    /// What's arrived since the last call, without blocking. Stops once the
    /// output taken passes `max_bytes`, so one call can't take arbitrarily
    /// long; the rest waits for the next.
    pub fn poll(&mut self, max_bytes: usize) -> Vec<RemoteEvent> {
        let mut events = Vec::new();
        let mut bytes = 0;
        while bytes < max_bytes {
            let Ok(msg) = self.events.try_recv() else {
                break;
            };
            events.push(match msg {
                Msg::Output(data) => {
                    bytes += data.len();
                    self.modes.scan(&data);
                    RemoteEvent::Output(data)
                }
                Msg::NoHook => {
                    if self.phase == Phase::Starting {
                        self.phase = Phase::Raw;
                        // What arrived so far is mostly the bootstrap line's
                        // echo and the remote's complaint about it, which were
                        // dropped; Enter brings up a fresh prompt.
                        self.send(b"\r");
                    }
                    continue;
                }
                Msg::Prompt(info) => {
                    self.phase = Phase::Idle;
                    self.command = None;
                    self.last_status = info.status;
                    if info.cwd.starts_with('/') {
                        self.cwd = info.cwd;
                    }
                    if info.home.starts_with('/') {
                        self.home = info.home;
                    }
                    RemoteEvent::Ready {
                        status: info.status,
                    }
                }
                Msg::Closed => {
                    self.closed = true;
                    RemoteEvent::Closed
                }
            });
        }
        events
    }

    /// Log out and close the connection. Waits at most briefly on the network.
    pub fn disconnect(self) {
        if let Some(rt) = &self.rt {
            let handle = Arc::clone(&self.handle);
            let _ = rt.block_on(async move {
                timeout(
                    Duration::from_millis(300),
                    handle.disconnect(russh::Disconnect::ByApplication, "", "en"),
                )
                .await
            });
        }
        // Drop shuts the runtime down.
    }
}

impl Drop for RemoteSession {
    fn drop(&mut self) {
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }
}

fn other(e: russh::Error) -> ConnectError {
    ConnectError::Other(format!("ssh: {e}"))
}

/// Connect, log in, and start the shell. Runs on the session's runtime.
async fn establish(req: ConnectRequest, waker: Waker) -> Result<Parts, ConnectError> {
    let config = Arc::new(client::Config {
        // Idle sessions stay up, as with OpenSSH; keepalives notice a dead
        // connection instead.
        inactivity_timeout: None,
        keepalive_interval: Some(KEEPALIVE_INTERVAL),
        keepalive_max: KEEPALIVE_MAX,
        // Keystrokes go out at once rather than waiting to be batched.
        nodelay: true,
        ..Default::default()
    });

    let mut handle = match timeout(
        CONNECT_TIMEOUT,
        client::connect(config, (req.host.as_str(), req.port), Handler),
    )
    .await
    {
        Err(_) => return Err(ConnectError::Timeout),
        Ok(Err(e)) => return Err(ConnectError::Network(format!("ssh: {e}"))),
        Ok(Ok(handle)) => handle,
    };

    match timeout(
        AUTH_TIMEOUT,
        authenticate(&mut handle, &req.user, req.password.as_deref()),
    )
    .await
    {
        Err(_) => return Err(ConnectError::Other("ssh: timed out logging in".into())),
        Ok(result) => result?,
    }

    timeout(
        OPEN_TIMEOUT,
        open_shell(handle, req.cols.max(1), req.rows.max(1), waker),
    )
    .await
    .map_err(|_| ConnectError::Other("ssh: timed out starting the remote shell".into()))?
}

/// Log in: the password when we have one, then any SSH agent's keys, then
/// unencrypted keys in `~/.ssh`.
async fn authenticate(
    handle: &mut Handle<Handler>,
    user: &str,
    password: Option<&str>,
) -> Result<(), ConnectError> {
    if let Some(password) = password
        && try_password(handle, user, password).await?
    {
        return Ok(());
    }
    if try_agent(handle, user).await || try_key_files(handle, user).await? {
        return Ok(());
    }
    Err(ConnectError::Auth {
        password_tried: password.is_some(),
    })
}

async fn try_password(
    handle: &mut Handle<Handler>,
    user: &str,
    password: &str,
) -> Result<bool, ConnectError> {
    let result = handle
        .authenticate_password(user, password)
        .await
        .map_err(other)?;
    match result {
        AuthResult::Success => Ok(true),
        // Servers that check passwords through PAM often offer only this.
        AuthResult::Failure {
            remaining_methods, ..
        } if remaining_methods.contains(&MethodKind::KeyboardInteractive) => {
            try_keyboard_interactive(handle, user, password).await
        }
        AuthResult::Failure { .. } => Ok(false),
    }
}

/// Keyboard-interactive login, answering each hidden prompt with `password`.
async fn try_keyboard_interactive(
    handle: &mut Handle<Handler>,
    user: &str,
    password: &str,
) -> Result<bool, ConnectError> {
    let mut response = handle
        .authenticate_keyboard_interactive_start(user, None::<String>)
        .await
        .map_err(other)?;
    for _ in 0..5 {
        match response {
            KeyboardInteractiveAuthResponse::Success => return Ok(true),
            KeyboardInteractiveAuthResponse::Failure { .. } => return Ok(false),
            KeyboardInteractiveAuthResponse::InfoRequest { prompts, .. } => {
                let answers = prompts
                    .iter()
                    .map(|p| {
                        if p.echo {
                            String::new()
                        } else {
                            password.to_string()
                        }
                    })
                    .collect();
                response = handle
                    .authenticate_keyboard_interactive_respond(answers)
                    .await
                    .map_err(other)?;
            }
        }
    }
    Ok(false)
}

/// The hash for an RSA key's signature; `None` for other key types.
async fn rsa_hash(handle: &Handle<Handler>, alg: ssh_key::Algorithm) -> Option<keys::HashAlg> {
    if alg.is_rsa() {
        handle
            .best_supported_rsa_hash()
            .await
            .ok()
            .flatten()
            .flatten()
    } else {
        None
    }
}

/// Log in with a key held by a running SSH agent: `ssh-agent` through
/// `SSH_AUTH_SOCK`, or on Windows the OpenSSH agent service or Pageant.
async fn try_agent(handle: &mut Handle<Handler>, user: &str) -> bool {
    #[cfg(unix)]
    if let Ok(Ok(agent)) = timeout(AGENT_TIMEOUT, AgentClient::connect_env()).await
        && agent_auth(handle, user, agent).await
    {
        return true;
    }
    #[cfg(windows)]
    {
        if let Ok(Ok(agent)) = timeout(
            AGENT_TIMEOUT,
            AgentClient::connect_named_pipe(r"\\.\pipe\openssh-ssh-agent"),
        )
        .await
            && agent_auth(handle, user, agent).await
        {
            return true;
        }
        if let Ok(Ok(agent)) = timeout(AGENT_TIMEOUT, AgentClient::connect_pageant()).await
            && agent_auth(handle, user, agent).await
        {
            return true;
        }
    }
    false
}

async fn agent_auth<S>(handle: &mut Handle<Handler>, user: &str, mut agent: AgentClient<S>) -> bool
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let Ok(Ok(identities)) = timeout(AGENT_TIMEOUT, agent.request_identities()).await else {
        return false;
    };
    for identity in identities {
        let key = identity.public_key().into_owned();
        let hash = rsa_hash(handle, key.algorithm()).await;
        let result = timeout(
            AGENT_TIMEOUT,
            handle.authenticate_publickey_with(user, key, hash, &mut agent),
        )
        .await;
        if let Ok(Ok(AuthResult::Success)) = result {
            return true;
        }
    }
    false
}

/// Log in with `~/.ssh/id_ed25519`, `id_ecdsa`, or `id_rsa`, as `ssh` would
/// by default. Keys with a passphrase are skipped (an agent covers those).
async fn try_key_files(handle: &mut Handle<Handler>, user: &str) -> Result<bool, ConnectError> {
    let Some(dir) = ssh_dir() else {
        return Ok(false);
    };
    for name in ["id_ed25519", "id_ecdsa", "id_rsa"] {
        let Ok(key) = keys::load_secret_key(dir.join(name), None) else {
            continue;
        };
        let hash = rsa_hash(handle, key.algorithm()).await;
        let key = PrivateKeyWithHashAlg::new(Arc::new(key), hash);
        if handle
            .authenticate_publickey(user, key)
            .await
            .map_err(other)?
            .success()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `~/.ssh`. `HOME` wins over `USERPROFILE` where both are set, as for
/// OpenSSH itself on Unix.
fn ssh_dir() -> Option<PathBuf> {
    let home = if cfg!(windows) {
        std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))
    } else {
        std::env::var_os("HOME")
    };
    Some(PathBuf::from(home?).join(".ssh"))
}

/// Open the PTY and shell, install the prompt hook, and start the reader
/// and writer tasks.
async fn open_shell(
    handle: Handle<Handler>,
    cols: u16,
    rows: u16,
    waker: Waker,
) -> Result<Parts, ConnectError> {
    let channel = handle.channel_open_session().await.map_err(other)?;
    // A typical Linux terminal's settings, except flow control: with IXON,
    // Ctrl+S (save, in many editors) would freeze the output instead.
    let modes = [
        (Pty::VINTR, 3),
        (Pty::VQUIT, 28),
        (Pty::VERASE, 127),
        (Pty::VKILL, 21),
        (Pty::VEOF, 4),
        (Pty::VSUSP, 26),
        (Pty::VWERASE, 23),
        (Pty::VLNEXT, 22),
        (Pty::ICRNL, 1),
        (Pty::IXON, 0),
        (Pty::IUTF8, 1),
        (Pty::ISIG, 1),
        (Pty::ICANON, 1),
        (Pty::IEXTEN, 1),
        (Pty::ECHO, 1),
        (Pty::ECHOE, 1),
        (Pty::ECHOK, 1),
        (Pty::ECHOCTL, 1),
        (Pty::ECHOKE, 1),
        (Pty::OPOST, 1),
        (Pty::ONLCR, 1),
        (Pty::CS8, 1),
    ];
    channel
        .request_pty(false, TERM, cols.into(), rows.into(), 0, 0, &modes)
        .await
        .map_err(other)?;
    channel.request_shell(false).await.map_err(other)?;

    let (read, write) = channel.split();
    // Typed ahead: the terminal holds it until the shell reads it.
    write
        .data_bytes(bootstrap().into_bytes())
        .await
        .map_err(other)?;

    let (input, input_rx) = mpsc::unbounded_channel();
    let (events_tx, events) = mpsc::channel(EVENT_QUEUE);
    tokio::spawn(writer(write, input_rx));
    tokio::spawn(reader(read, events_tx, waker));
    Ok(Parts {
        handle,
        input,
        events,
        size: (cols, rows),
    })
}

/// The line that installs the prompt hook. It has to parse in every shell it
/// may land in, so it's POSIX at the top level, with each shell's own syntax
/// behind an `if` (and zsh's arrays inside `eval`, which `sh` won't parse).
///
/// At each prompt `__pg_e` turns echo off and prints the marker: exit status,
/// `$PWD`, `$HOME`. bash's `PS0` and zsh's `preexec` turn echo back on as a
/// command starts. The shells' line editors are switched off (bash `set +o
/// emacs`, zsh `unsetopt zle`), so lines are read through the terminal, which
/// honours the echo setting, and don't redraw anything. The prompts are made
/// empty: the frontend draws its own.
///
/// Plain `sh` has no hooks, so its `PS1` carries the marker itself, and echo
/// stays off; most programs that read input set their own terminal modes.
///
/// The leading space keeps it out of bash's history under the common
/// `HISTCONTROL=ignorespace`.
fn bootstrap() -> String {
    let emit = r#"__pg_e(){ stty -echo 2>/dev/null; printf '\033]7770;%s\037%s\037%s\007' "$1" "$PWD" "$HOME"; }"#;
    let zsh = r#"__pg_p(){ __pg_e $?; PS1=; PS2=; RPS1=; }; __pg_x(){ stty echo 2>/dev/null; }; eval 'precmd_functions+=(__pg_p); preexec_functions+=(__pg_x)'; unsetopt zle prompt_sp prompt_cr 2>/dev/null; PS1=; PS2=; RPS1="#;
    let bash = r#"__pg_p(){ __pg_e $?; PS1=; PS2=; }; unset PROMPT_COMMAND; PROMPT_COMMAND=__pg_p; PS0='$(stty echo 2>/dev/null)'; set +o emacs +o vi; PS1=; PS2="#;
    let posix = r#"stty -echo 2>/dev/null; PS1="$(printf '\033]7770;')"'$?'"$(printf '\037')"'$PWD'"$(printf '\037')"'$HOME'"$(printf '\007')"; PS2="#;
    format!(
        " {emit}; if [ -n \"$ZSH_VERSION\" ]; then {zsh}; elif [ -n \"$BASH_VERSION\" ]; then {bash}; else {posix}; fi\r"
    )
}

/// Send the frontend's input to the remote. Ends (closing our side of the
/// channel, which ends the remote shell) once the session is dropped.
async fn writer(write: ChannelWriteHalf<client::Msg>, mut input: mpsc::UnboundedReceiver<Input>) {
    while let Some(msg) = input.recv().await {
        let result = match msg {
            Input::Data(bytes) => write.data_bytes(bytes).await,
            Input::Resize(cols, rows) => write.window_change(cols.into(), rows.into(), 0, 0).await,
        };
        if result.is_err() {
            break;
        }
    }
    let _ = write.eof().await;
}

/// Read the remote's output, separating out the prompt markers. Output from
/// before the first marker (the login banner, the bootstrap line's echo) is
/// dropped.
async fn reader(mut read: ChannelReadHalf, tx: mpsc::Sender<Msg>, waker: Waker) {
    let mut filter = MarkerFilter::default();
    let mut started = false;
    let deadline = Instant::now() + BOOTSTRAP_TIMEOUT;
    loop {
        let msg = if started {
            read.wait().await
        } else {
            match timeout_at(deadline, read.wait()).await {
                Ok(msg) => msg,
                Err(_) => {
                    started = true;
                    if tx.send(Msg::NoHook).await.is_err() {
                        return;
                    }
                    waker();
                    continue;
                }
            }
        };
        let data = match msg {
            Some(ChannelMsg::Data { data } | ChannelMsg::ExtendedData { data, .. }) => data,
            Some(ChannelMsg::Eof | ChannelMsg::Close) | None => break,
            Some(_) => continue,
        };
        for piece in filter.feed(&data) {
            let msg = match piece {
                Piece::Bytes(bytes) if started => Msg::Output(bytes),
                Piece::Bytes(_) => continue,
                Piece::Marker(info) => {
                    started = true;
                    Msg::Prompt(info)
                }
            };
            if tx.send(msg).await.is_err() {
                return;
            }
        }
        waker();
    }
    let rest = filter.finish();
    if started && !rest.is_empty() {
        let _ = tx.send(Msg::Output(rest)).await;
    }
    let _ = tx.send(Msg::Closed).await;
    waker();
}

enum Piece {
    Bytes(Vec<u8>),
    Marker(PromptInfo),
}

/// Splits prompt markers out of the output stream. A marker can arrive split
/// across reads, so a possible start of one is held until it's complete.
#[derive(Default)]
struct MarkerFilter {
    pending: Vec<u8>,
}

impl MarkerFilter {
    fn feed(&mut self, data: &[u8]) -> Vec<Piece> {
        let mut buf = std::mem::take(&mut self.pending);
        buf.extend_from_slice(data);
        let mut pieces = Vec::new();
        let mut text_start = 0;
        let mut i = 0;
        let push_text = |pieces: &mut Vec<Piece>, bytes: &[u8]| {
            if !bytes.is_empty() {
                pieces.push(Piece::Bytes(bytes.to_vec()));
            }
        };
        while let Some(off) = buf[i..].iter().position(|&b| b == 0x1b) {
            let at = i + off;
            let rest = &buf[at..];
            if rest.len() < MARKER.len() {
                if MARKER.starts_with(rest) {
                    push_text(&mut pieces, &buf[text_start..at]);
                    self.pending = rest.to_vec();
                    return pieces;
                }
                i = at + 1;
                continue;
            }
            if !rest.starts_with(MARKER) {
                i = at + 1;
                continue;
            }
            let body = &rest[MARKER.len()..];
            match body.iter().position(|&b| b == 0x07) {
                Some(end) => {
                    push_text(&mut pieces, &buf[text_start..at]);
                    pieces.push(Piece::Marker(parse_marker(&body[..end])));
                    i = at + MARKER.len() + end + 1;
                    text_start = i;
                }
                None if rest.len() < MARKER_MAX => {
                    push_text(&mut pieces, &buf[text_start..at]);
                    self.pending = rest.to_vec();
                    return pieces;
                }
                // Too long to be ours: let it through.
                None => i = at + 1,
            }
        }
        push_text(&mut pieces, &buf[text_start..]);
        pieces
    }

    /// Whatever was held back as a possible marker, once the stream ends.
    fn finish(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }
}

fn parse_marker(body: &[u8]) -> PromptInfo {
    let mut fields = body.split(|&b| b == FIELD_SEP);
    let mut next = || String::from_utf8_lossy(fields.next().unwrap_or_default()).into_owned();
    let status = next().trim().parse().ok();
    PromptInfo {
        status,
        cwd: next(),
        home: next(),
    }
}

/// Follows the terminal modes the frontends need for encoding input: cursor
/// key mode (`ESC [ ? 1 h`) and bracketed paste (`ESC [ ? 2004 h`).
#[derive(Default)]
struct ModeTracker {
    state: ModeState,
    params: Vec<u8>,
    app_cursor: bool,
    bracketed_paste: bool,
}

#[derive(Default, Clone, Copy, PartialEq)]
enum ModeState {
    #[default]
    Ground,
    Esc,
    CsiStart,
    Csi {
        private: bool,
    },
}

impl ModeTracker {
    fn scan(&mut self, data: &[u8]) {
        for &b in data {
            self.state = match (self.state, b) {
                (_, 0x1b) => ModeState::Esc,
                (ModeState::Esc, b'[') => {
                    self.params.clear();
                    ModeState::CsiStart
                }
                // RIS, a full reset.
                (ModeState::Esc, b'c') => {
                    self.app_cursor = false;
                    self.bracketed_paste = false;
                    ModeState::Ground
                }
                (ModeState::CsiStart, b'?') => ModeState::Csi { private: true },
                (ModeState::CsiStart | ModeState::Csi { .. }, 0x30..=0x3f) => {
                    if self.params.len() < 64 {
                        self.params.push(b);
                    }
                    match self.state {
                        ModeState::Csi { private } => ModeState::Csi { private },
                        _ => ModeState::Csi { private: false },
                    }
                }
                (ModeState::CsiStart | ModeState::Csi { .. }, 0x20..=0x2f) => self.state,
                (ModeState::Csi { private: true }, b'h' | b'l') => {
                    let on = b == b'h';
                    for param in self.params.split(|&c| c == b';') {
                        match param {
                            b"1" => self.app_cursor = on,
                            b"2004" => self.bracketed_paste = on,
                            _ => {}
                        }
                    }
                    ModeState::Ground
                }
                _ => ModeState::Ground,
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_split(input: &[u8]) -> Vec<Vec<Piece>> {
        (0..=input.len())
            .map(|i| {
                let mut f = MarkerFilter::default();
                let mut pieces = f.feed(&input[..i]);
                pieces.extend(f.feed(&input[i..]));
                let rest = f.finish();
                if !rest.is_empty() {
                    pieces.push(Piece::Bytes(rest));
                }
                pieces
            })
            .collect()
    }

    /// Joined output bytes, and the markers, from `pieces`.
    fn flatten(pieces: Vec<Piece>) -> (Vec<u8>, Vec<PromptInfo>) {
        let mut bytes = Vec::new();
        let mut markers = Vec::new();
        for p in pieces {
            match p {
                Piece::Bytes(b) => bytes.extend(b),
                Piece::Marker(m) => markers.push(m),
            }
        }
        (bytes, markers)
    }

    #[test]
    fn markers_are_found_at_every_split_point() {
        let input =
            b"out\x1b[31mred\x1b]7770;1\x1f/home/me/src\x1f/home/me\x07\x1b]0;title\x07tail\x1b";
        for pieces in feed_split(input) {
            let (bytes, markers) = flatten(pieces);
            assert_eq!(bytes, b"out\x1b[31mred\x1b]0;title\x07tail\x1b");
            assert_eq!(
                markers,
                [PromptInfo {
                    status: Some(1),
                    cwd: "/home/me/src".into(),
                    home: "/home/me".into(),
                }]
            );
        }
    }

    #[test]
    fn an_unexpanded_posix_prompt_still_counts_as_a_marker() {
        let mut f = MarkerFilter::default();
        let (_, markers) = flatten(f.feed(b"\x1b]7770;$?\x1f$PWD\x1f$HOME\x07"));
        assert_eq!(markers.len(), 1);
        assert_eq!(markers[0].status, None);
    }

    #[test]
    fn modes_follow_private_csi_sequences() {
        let mut m = ModeTracker::default();
        m.scan(b"\x1b[?1h\x1b=\x1b[?20");
        m.scan(b"04h");
        assert!(m.app_cursor && m.bracketed_paste);
        m.scan(b"\x1b[?1;2004l");
        assert!(!m.app_cursor && !m.bracketed_paste);
        m.scan(b"\x1b[1h");
        assert!(!m.app_cursor);
    }

    #[test]
    fn the_bootstrap_is_one_line() {
        let b = bootstrap();
        assert!(b.starts_with(' ') && b.ends_with('\r'));
        assert!(!b[..b.len() - 1].contains(['\r', '\n']));
    }
}
