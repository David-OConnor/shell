//! An unfortunately high amount of this project's architecture is built around
//! Rustyline conventions. This includes liberal use of Arc<Mutex>>, and Trait-based
//! handling of things that could otherwise be plain functions.

use std::{
    env, io,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
};

use chrono::Utc;
use rustyline::{
    Cmd, CompletionType, ConditionalEventHandler, Config, Context, Editor, Event, EventContext,
    EventHandler, ExternalPrinter, Helper, KeyCode, KeyEvent, Modifiers, RepeatCount,
    completion::{Completer, FilenameCompleter, Pair},
    error::ReadlineError,
    highlight::Highlighter,
    hint::Hinter,
    history::FileHistory,
    validate::Validator,
};
use shell::{
    BrowserFile, NavState, RemoteTerminal, branch_indicator,
    commands::{self, OutKind},
    current_branch, get_home, path_from_args, read_browser_files, save_data,
    state::{HistoryItem, RecentDir},
};

mod render;
mod tasks;

// Markers `highlight_prompt` looks for when colouring the recall indicators
// (` his N`, ` cd N`) light green. The leading space is part of the marker
// so a cwd that happens to contain "his" or "cd" mid-path doesn't match.
const HIS_PREFIX: &str = " his ";
const CD_PREFIX: &str = " cd ";
// Re-exported from the shell lib so render.rs and the prompt builder use
// exactly the same string (`" branch: "`).
pub use shell::BRANCH_PREFIX;

/// Shared handle to rustyline's `ExternalPrinter`. Key handlers use this to
/// print messages *above* the in-progress prompt line — going through
/// rustyline so it knows to clear the prompt, write the message, and redraw
/// the prompt below. A raw `println!` from a handler corrupts the display
/// because rustyline's cursor-tracking state never sees the write.
type SharedPrinter = Arc<Mutex<Box<dyn ExternalPrinter + Send>>>;

// Display this many history items at a time.
const DISP_HIST_LEN: usize = 20;

const DIVIDER: &str = "----------";

// todo: Instead of storing these Arc<Mutex>>s, perhaps we do it some other way; this is due
// todo: due to how Rustyline expects it.
struct State {
    /// Cached.
    pub home: Option<PathBuf>,
    /// Shared with the Ctrl+H / arrow-key handlers, which render pages of
    /// recent commands without holding `State`.
    pub history: Arc<Mutex<Vec<HistoryItem>>>,
    /// This initializes to env::current_dir, but is then managed from within
    /// this program.
    pub cwd: PathBuf,
    /// User-controlled list of directory bookmarks that can be easily
    /// navigated to. Shared with the readline key handler (Ctrl+B), which
    /// is why it lives behind an Arc<Mutex<_>>.
    pub dir_bookmarks: Arc<Mutex<Vec<PathBuf>>>,
    /// Paths we've execute commands from. Works in a similar way to bookmarks.
    pub recent_dirs: Arc<Mutex<Vec<RecentDir>>>,
    pub remote_terminals: Arc<Mutex<Vec<RemoteTerminal>>>,
    /// In the current dir. Note persistent, unlike some of our other lists.
    /// Currently unused in this application; TBD. Used in the GUI
    /// version.
    pub browser_files: Arc<Mutex<Vec<BrowserFile>>>,
    /// Cached git branch for `cwd`. `None` when cwd isn't inside a repo.
    /// Refreshed by `refresh_branch` after every command and after `cd` —
    /// branch can change behind our back via `git checkout`, so we re-check
    /// whenever the user has had a chance to mutate repo state.
    pub branch: Option<String>,
}

impl Default for State {
    fn default() -> Self {
        let cwd = env::current_dir().unwrap_or_default();
        let branch = current_branch(&cwd);

        Self {
            home: get_home(),
            history: Arc::new(Mutex::new(Vec::new())),
            cwd,
            dir_bookmarks: Arc::new(Mutex::new(Vec::new())),
            recent_dirs: Arc::new(Mutex::new(Vec::new())),
            remote_terminals: Arc::new(Mutex::new(Vec::new())),
            browser_files: Arc::new(Mutex::new(Vec::new())),
            branch,
        }
    }
}

impl State {
    /// This defines what the general prompt looks like. Its adorning
    /// characters let the user know they're in this shell. `nav` carries
    /// the active recall cursors (see [NavState]); when either is `Some`,
    /// the prompt grows by ` his N` or ` cd N` before the `$` to indicate
    /// which item is currently loaded into the input.
    fn prompt(&self, nav: &NavState) -> String {
        // Mark the directory with a leading `*` when it's bookmarked.
        let bookmarked = self
            .dir_bookmarks
            .lock()
            .map(|list| list.contains(&self.cwd))
            .unwrap_or(false);
        let star = if bookmarked { "*" } else { "" };
        format!(
            "S {star}{}{}{}{} $ ",
            self.cwd.display(),
            branch_indicator(self.branch.as_deref()),
            nav.his_indicator(),
            nav.cd_indicator(),
        )
    }

    /// Re-detect the git branch for `cwd`. Called after `cd` (cwd may have
    /// moved in/out of a repo) and after every command (a `git checkout`
    /// might have switched branches behind our back).
    pub fn refresh_branch(&mut self) {
        self.branch = current_branch(&self.cwd);
    }

    /// Re-read the directory listing for `cwd` into `browser_files`. The CLI
    /// itself doesn't expose this listing yet, but the GUI uses the same
    /// shared helper, so we keep the field populated for parity (and for
    /// any future CLI-side use). Called after every successful directory
    /// change (cd / bm / hist-recall) and at startup.
    pub fn refresh_browser_files(&self) {
        let files = read_browser_files(&self.cwd);
        if let Ok(mut list) = self.browser_files.lock() {
            *list = files;
        }
    }

    /// Persist user-controlled state (bookmarks + recent dirs + history +
    /// remote terminals) to the given file. Called after every mutation of
    /// any of them. Locks in the order bookmarks → recent_dirs → history →
    /// remote_terminals — keep this order consistent across all callers to
    /// avoid lock-order deadlocks.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let bookmarks = self
            .dir_bookmarks
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "bookmark lock poisoned"))?;

        let recent = self
            .recent_dirs
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "recent-dirs lock poisoned"))?;

        let history = self
            .history
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "history lock poisoned"))?;

        let remote_terminals = self
            .remote_terminals
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "remote-terminals lock poisoned"))?;

        save_data::save_state(&bookmarks, &recent, &history, &remote_terminals, path)
    }

    /// Restore state from disk, returning a fresh `State` with that data.
    /// A missing file is treated as "no saved state" and yields the default
    /// `State::new()` values (not an error).
    pub fn load(path: &Path) -> io::Result<Self> {
        let loaded = save_data::load_state(path)?;
        let cwd = env::current_dir().unwrap_or_default();
        let branch = current_branch(&cwd);

        Ok(Self {
            home: get_home(),
            history: Arc::new(Mutex::new(loaded.history)),
            cwd,
            dir_bookmarks: Arc::new(Mutex::new(loaded.bookmarks)),
            recent_dirs: Arc::new(Mutex::new(loaded.recent_dirs)),
            remote_terminals: Arc::new(Mutex::new(loaded.remote_terminals)),
            browser_files: Arc::new(Mutex::new(Vec::new())),
            branch,
        })
    }
}

/// Rustyline `Helper` that provides Tab-completion for the `cd` builtin
/// against the user's bookmark list. Matches case-insensitively against the
/// last path component of each bookmark, and replaces the partial argument
/// with the full path (formatted as `~/...` when under the home dir).
struct ShellHelper {
    bookmarks: Arc<Mutex<Vec<PathBuf>>>,
    home: Option<PathBuf>,
    /// Rustyline's built-in filename completer, used as the fallback when no
    /// bookmark matches (and for non-`cd` commands).
    fs_completer: FilenameCompleter,
}

impl ShellHelper {
    fn render(&self, p: &Path) -> String {
        render::render_with_tilde(p, self.home.as_deref())
    }
}

impl Completer for ShellHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let before = &line[..pos];
        let trimmed = before.trim_start();
        let leading = before.len() - trimmed.len();

        // If the command word is `cd`, try bookmark completion first.
        if let Some(i) = trimmed.find(char::is_whitespace) {
            let cmd_part = &trimmed[..i];
            let rest = &trimmed[i..];
            if cmd_part == "cd" {
                let arg = rest.trim_start();
                let arg_start = leading + (trimmed.len() - arg.len());
                let needle = arg.to_lowercase();

                let bookmark_pairs: Vec<Pair> = match self.bookmarks.lock() {
                    Ok(list) => list
                        .iter()
                        .filter_map(|p| {
                            let name = p.file_name()?.to_str()?;
                            if name.to_lowercase().starts_with(&needle) {
                                Some(Pair {
                                    display: name.to_string(),
                                    replacement: self.render(p),
                                })
                            } else {
                                None
                            }
                        })
                        .collect(),
                    Err(_) => Vec::new(),
                };

                if !bookmark_pairs.is_empty() {
                    return Ok((arg_start, bookmark_pairs));
                }
                // No bookmark match — fall through to filesystem completion.
            }
        }

        // Default: complete files & directories in the CWD (bash-style).
        self.fs_completer.complete(line, pos, ctx)
    }
}

impl Hinter for ShellHelper {
    type Hint = String;
}

/// Split a line into word ranges (byte start, byte end), treating quoted
/// regions as part of the surrounding word so that spaces inside `"..."` or
/// `'...'` don't break a token apart.
fn tokenize_words(line: &str) -> Vec<(usize, usize)> {
    let mut words = Vec::new();
    let mut quote: Option<char> = None;
    let mut word_start: Option<usize> = None;

    for (idx, ch) in line.char_indices() {
        match quote {
            Some(q) => {
                if ch == q {
                    quote = None;
                }
                if word_start.is_none() {
                    word_start = Some(idx);
                }
            }
            None => {
                if ch == '"' || ch == '\'' {
                    quote = Some(ch);
                    if word_start.is_none() {
                        word_start = Some(idx);
                    }
                } else if ch.is_whitespace() {
                    if let Some(s) = word_start.take() {
                        words.push((s, idx));
                    }
                } else if word_start.is_none() {
                    word_start = Some(idx);
                }
            }
        }
    }
    if let Some(s) = word_start {
        words.push((s, line.len()));
    }
    words
}

impl Validator for ShellHelper {}
impl Helper for ShellHelper {}

/// Rustyline key handler: snapshots the current working directory
/// into the shared bookmark list. Runs inline within readline, so we use a
/// shared Arc<Mutex<_>> rather than touching `State` directly. Also persists
/// the list to disk on every successful add.
struct BookmarkHandler {
    bookmarks: Arc<Mutex<Vec<PathBuf>>>,
    /// Held so we can write the full state file (bookmarks + recent dirs +
    /// history + remote terminals) in a single pass when a bookmark is
    /// added.
    recent_dirs: Arc<Mutex<Vec<RecentDir>>>,
    history: Arc<Mutex<Vec<HistoryItem>>>,
    remote_terminals: Arc<Mutex<Vec<RemoteTerminal>>>,
    save_path: PathBuf,
    printer: SharedPrinter,
}

impl ConditionalEventHandler for BookmarkHandler {
    fn handle(
        &self,
        _evt: &Event,
        _n: RepeatCount,
        _positive: bool,
        _ctx: &EventContext<'_>,
    ) -> Option<Cmd> {
        if let Ok(cwd) = env::current_dir() {
            if let Ok(mut list) = self.bookmarks.lock() {
                let msg = if list.contains(&cwd) {
                    "This bookmark already exists\n".to_string()
                } else {
                    let msg = format!("Added a bookmark: {}\n", cwd.display());
                    list.push(cwd);
                    // Lock recent_dirs, history, remote_terminals after
                    // bookmarks — same order as State::save, so no
                    // lock-order conflicts.
                    if let Ok(recent) = self.recent_dirs.lock() {
                        if let Ok(history) = self.history.lock() {
                            if let Ok(remote_terminals) = self.remote_terminals.lock() {
                                if let Err(e) = save_data::save_state(
                                    &list,
                                    &recent,
                                    &history,
                                    &remote_terminals,
                                    &self.save_path,
                                ) {
                                    eprintln!("warning: failed to save state: {e}");
                                }
                            }
                        }
                    }
                    msg
                };
                if let Ok(mut p) = self.printer.lock() {
                    let _ = p.print(msg);
                }
            }
        }

        // Consume the keystroke so rustyline doesn't also run its default
        // Ctrl+B binding (backward-char).
        Some(Cmd::Noop)
    }
}

/// Which paginated list a Ctrl+H / Ctrl+R / Alt+B keystroke opens.
#[derive(Clone, Copy, PartialEq, Eq)]
enum NavKind {
    History,
    RecentDirs,
    Bookmarks,
}

/// Which recall axis a key handler steps. Up/Down → His (`state.history`);
/// Left/Right → Cd (`state.recent_dirs`).
#[derive(Clone, Copy)]
enum NavAxis {
    His,
    Cd,
}

/// CLI-side wrapper around the shared [NavState]. Adds the `pending_restart`
/// channel: when an arrow handler successfully walks one of the recall
/// lists, it stores the buffer text it wants the next `readline()` to start
/// with here and bails out via [Cmd::Interrupt] so the main loop can
/// re-render the prompt with the matching indicator baked in (rustyline
/// can't change a prompt mid-line — see [State::prompt]).
struct CliNav {
    nav: NavState,
    pending_restart: Option<String>,
}

impl CliNav {
    fn new() -> Self {
        Self {
            nav: NavState::new(),
            pending_restart: None,
        }
    }

    fn reset(&mut self) {
        self.nav.reset();
        self.pending_restart = None;
    }
}

/// Rustyline key handler bound to one of the four arrow keys. On a
/// successful step it stores the new buffer text in `pending_restart` and
/// returns `Cmd::Interrupt` so the main loop can tear the prompt down and
/// re-call `readline_with_initial` with an updated prompt that includes
/// the matching ` his N` or ` cd N` indicator.
///
/// Left/Right only steal the keystroke when the input buffer is empty
/// *or* a cd recall is already active — otherwise they fall through to
/// rustyline's default cursor movement so the user can still edit the line.
/// Up/Down don't have that conflict (there's nowhere for them to move in a
/// single-line buffer) so they always step.
struct ArrowHandler {
    history: Arc<Mutex<Vec<HistoryItem>>>,
    recent_dirs: Arc<Mutex<Vec<RecentDir>>>,
    home: Option<PathBuf>,
    nav: Arc<Mutex<CliNav>>,
    axis: NavAxis,
    /// Direction: Up / Left ⇒ `true` (older); Down / Right ⇒ `false`.
    backward: bool,
}

impl ConditionalEventHandler for ArrowHandler {
    fn handle(
        &self,
        _evt: &Event,
        _n: RepeatCount,
        _positive: bool,
        ctx: &EventContext<'_>,
    ) -> Option<Cmd> {
        let mut nav = self.nav.lock().ok()?;
        match self.axis {
            NavAxis::His => {
                let history = self.history.lock().ok()?;
                match nav.nav.step_his(&history, self.backward, ctx.line()) {
                    Some(text) => {
                        nav.pending_restart = Some(text);
                        Some(Cmd::Interrupt)
                    }
                    None => Some(Cmd::Noop),
                }
            }
            NavAxis::Cd => {
                // Preserve normal cursor movement when the user is editing.
                if !ctx.line().is_empty() && nav.nav.cd_cursor.is_none() {
                    return None;
                }
                let recent = self.recent_dirs.lock().ok()?;
                let home = self.home.clone();
                let result = nav.nav.step_cd(&recent, self.backward, ctx.line(), |path| {
                    format!("cd {}", render::render_with_tilde(path, home.as_deref()))
                });
                match result {
                    Some(text) => {
                        nav.pending_restart = Some(text);
                        Some(Cmd::Interrupt)
                    }
                    None => Some(Cmd::Noop),
                }
            }
        }
    }
}

/// Total pages needed to show `total` items at `per_page` items per page.
/// Returns 1 when empty so the renderer can still show a "Page 1/1" frame.
fn page_count(total: usize, per_page: usize) -> usize {
    if total == 0 {
        1
    } else {
        total.div_ceil(per_page)
    }
}

/// Rustyline key handler: prints one of the paginated lists (Ctrl+H for
/// history, Ctrl+R for recent dirs, Alt+B for bookmarks) above the prompt.
/// Only ever shows page 0 — Left/Right are now bound to recent-dir recall,
/// so multi-page browsing isn't available from the prompt.
struct ShowListHandler {
    kind: NavKind,
    history: Arc<Mutex<Vec<HistoryItem>>>,
    recent_dirs: Arc<Mutex<Vec<RecentDir>>>,
    bookmarks: Arc<Mutex<Vec<PathBuf>>>,
    home: Option<PathBuf>,
    printer: SharedPrinter,
}

impl ShowListHandler {
    fn render(&self) -> Option<String> {
        match self.kind {
            NavKind::History => {
                let h = self.history.lock().ok()?;
                Some(render::render_history(&h, 0))
            }
            NavKind::RecentDirs => {
                let r = self.recent_dirs.lock().ok()?;
                let bm = self.bookmarks.lock().ok()?;
                Some(render::render_recent_dirs(&r, &bm, self.home.as_deref(), 0))
            }
            NavKind::Bookmarks => {
                let bm = self.bookmarks.lock().ok()?;
                Some(render::render_bookmarks(&bm, self.home.as_deref(), 0))
            }
        }
    }
}

impl ConditionalEventHandler for ShowListHandler {
    fn handle(
        &self,
        _evt: &Event,
        _n: RepeatCount,
        _positive: bool,
        _ctx: &EventContext<'_>,
    ) -> Option<Cmd> {
        if let Some(msg) = self.render() {
            if let Ok(mut p) = self.printer.lock() {
                let _ = p.print(msg);
            }
        }
        Some(Cmd::Noop)
    }
}

/// Record `cwd` in the recent-dirs list. If the path is already present we
/// remove the old entry and push a fresh one to the end, so the list stays
/// deduped and the newest entry sits at the bottom of the display.
fn record_recent_dir(recent: &Arc<Mutex<Vec<RecentDir>>>, cwd: &Path) {
    if let Ok(mut list) = recent.lock() {
        list.retain(|r| r.path != cwd);
        list.push(RecentDir {
            path: cwd.to_path_buf(),
            dt: Utc::now(),
        });
    }
}

/// Runs one command line. Returns false if the shell should exit.
fn run_command(state: &mut State, state_path: &Path, input: &str) -> bool {
    let input = input.trim();
    if input.is_empty() {
        return true;
    }

    // Split into command + remainder for built-in dispatch.
    let (cmd, args) = match input.find(char::is_whitespace) {
        Some(i) => (&input[..i], input[i..].trim()),
        None => (input, ""),
    };

    // `his`/`hist <n>` re-runs a previous history item. Handle it before
    // recording the meta-invocation so the user's history stays focused on
    // the resolved command (which the recursive call below will record).
    if cmd == "his" || cmd == "hist" {
        match args.parse::<usize>() {
            Ok(idx) => {
                let resolved = state
                    .history
                    .lock()
                    .ok()
                    .and_then(|h| h.get(idx).map(|item| item.text.clone()));
                match resolved {
                    Some(text) => {
                        println!("> {text}");
                        return run_command(state, state_path, &text);
                    }
                    None => eprintln!("{cmd}: no history item at index {idx}"),
                }
            }
            Err(_) => eprintln!("{cmd}: usage: {cmd} <number>"),
        }
        return true;
    }

    // `hisd <n>` re-runs a previous history item in its original working
    // directory, without changing the shell's CWD. Bypasses the built-in
    // dispatcher and shells the command out directly, since the point is to
    // run it elsewhere on the filesystem.
    if cmd == "hisd" {
        match args.parse::<usize>() {
            Ok(idx) => {
                let resolved = state
                    .history
                    .lock()
                    .ok()
                    .and_then(|h| h.get(idx).map(|item| (item.text.clone(), item.dir.clone())));

                match resolved {
                    Some((text, dir)) => {
                        println!("> {text}  (in {})", dir.display());
                        let result = if cfg!(windows) {
                            Command::new("pwsh")
                                .args(["-NoProfile", "-NoLogo", "-Command", &text])
                                .current_dir(&dir)
                                .status()
                        } else {
                            Command::new("sh")
                                .args(["-c", text.as_str()])
                                .current_dir(&dir)
                                .status()
                        };
                        if let Err(e) = result {
                            eprintln!("shell: {e}");
                        }
                    }
                    None => eprintln!("hisd: no history item at index {idx}"),
                }
            }
            Err(_) => eprintln!("hisd: usage: hisd <number>"),
        }
        return true;
    }

    if let Ok(mut hist) = state.history.lock() {
        hist.push(HistoryItem {
            text: input.to_string(),
            dir: state.cwd.clone(),
            dt: Utc::now(),
        });
    }

    // Track directories we've run real commands from (everything except `cd`),
    // so Ctrl+R / `cd <number>` can jump back to them. We always save below
    // regardless, to flush the new history entry to disk.
    if cmd != "cd" {
        let cwd = state.cwd.clone();
        record_recent_dir(&state.recent_dirs, &cwd);
    }

    if let Err(e) = state.save(state_path) {
        eprintln!("warning: failed to save state: {e}");
    }

    match cmd {
        "exit" | "quit" => return false,

        "sync" => {
            // Delegate to the shared implementation; route its sink output
            // to stdout/stderr. Trim trailing newlines so we don't double up
            // on the ones println! adds — git output already ends with `\n`.
            let cwd = state.cwd.clone();
            let mut sink = |kind, msg: String| {
                let msg = msg.trim_end_matches('\n');
                match kind {
                    OutKind::Stdout => println!("{msg}"),
                    OutKind::Stderr => eprintln!("{msg}"),
                }
            };
            commands::sync(args, &cwd, &mut sink);
        }

        "logs" => {
            // CLI uses `follow = true` so the user gets a live tail via
            // inherited stdio; Ctrl+C exits journalctl and returns control
            // to the shell. On non-Linux this is a no-op error via the sink.
            let mut sink = |kind, msg: String| {
                let msg = msg.trim_end_matches('\n');
                match kind {
                    OutKind::Stdout => println!("{msg}"),
                    OutKind::Stderr => eprintln!("{msg}"),
                }
            };
            commands::logs(args, true, &mut sink);
        }

        // On linux, this is likely the same as the system `cat` command, but it works on Windows.
        // Another approach may be to only apply this branch on Windows.
        "cat" => {
            let bookmarks = state.dir_bookmarks.lock();
            let slice: &[PathBuf] = bookmarks.as_deref().map(|v| v.as_slice()).unwrap_or(&[]);
            let target = path_from_args(state.home.as_deref(), &state.cwd, slice, args);
            drop(bookmarks);
            tasks::cat(&target);
        }

        "del" => {
            // `del bm <number>`: delete a bookmark by its displayed index
            // (the numbers shown by the Alt+B bookmark list).
            let (sub, rest) = match args.find(char::is_whitespace) {
                Some(i) => (&args[..i], args[i..].trim()),
                None => (args, ""),
            };

            match sub {
                "bm" => match rest.parse::<usize>() {
                    Ok(idx) => {
                        let mut removed = None;
                        match state.dir_bookmarks.lock() {
                            Ok(mut list) => {
                                if idx < list.len() {
                                    removed = Some(list.remove(idx));
                                } else {
                                    eprintln!(
                                        "del bm: no bookmark at index {idx} (have {})",
                                        list.len()
                                    );
                                }
                            }
                            Err(_) => eprintln!("del bm: bookmark list lock poisoned"),
                        }
                        if let Some(path) = removed {
                            println!("Deleted bookmark: {}", path.display());
                            if let Err(e) = state.save(state_path) {
                                eprintln!("del bm: failed to save bookmarks: {e}");
                            }
                        }
                    }
                    Err(_) => {
                        eprintln!("del bm: expected a number, e.g. `del bm 4`");
                    }
                },
                "" => eprintln!("del: usage: del bm <number>"),
                other => eprintln!("del: unknown target `{other}` (expected `bm`)"),
            }
        }

        "cd" => {
            // `cd <number>` (with nothing else after) jumps to a recent
            // directory by its Ctrl+R index. Anything else is resolved as a
            // normal path/bookmark argument.
            // When the arg parses as a number we treat it as a recent-dir
            // index; remember the index so we can prune the entry if its
            // path is stale (deleted/moved on disk).
            let (target, recent_idx) = if let Ok(idx) = args.parse::<usize>() {
                let resolved = state
                    .recent_dirs
                    .lock()
                    .ok()
                    .and_then(|list| list.get(idx).map(|r| r.path.clone()));
                match resolved {
                    Some(p) => (Some(p), Some(idx)),
                    None => {
                        eprintln!("cd: no recent directory at index {idx}");
                        (None, None)
                    }
                }
            } else {
                let bookmarks = state.dir_bookmarks.lock();
                let slice: &[PathBuf] = bookmarks.as_deref().map(|v| v.as_slice()).unwrap_or(&[]);
                (
                    Some(path_from_args(
                        state.home.as_deref(),
                        &state.cwd,
                        slice,
                        args,
                    )),
                    None,
                )
            };

            if let Some(target) = target {
                match env::set_current_dir(&target) {
                    Ok(_) => state.cwd = env::current_dir().unwrap_or(target),
                    Err(e) => {
                        eprintln!("cd: {e}");
                        // If the recent-dir entry's path no longer exists on
                        // disk, prune it so the indices shift down and the
                        // user doesn't hit the same stale row forever.
                        if e.kind() == io::ErrorKind::NotFound {
                            if let Some(i) = recent_idx {
                                let mut removed = false;
                                if let Ok(mut list) = state.recent_dirs.lock() {
                                    if list.get(i).map(|r| r.path == target).unwrap_or(false) {
                                        list.remove(i);
                                        removed = true;
                                    }
                                }
                                if removed {
                                    eprintln!("cd: removed stale recent-dir entry {i}");
                                    if let Err(e) = state.save(state_path) {
                                        eprintln!("warning: failed to save state: {e}");
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // `bm <number>`: jump to the bookmark at that Alt+B index. Mirrors
        // `cd <number>` but indexes into the bookmark list instead of
        // recent_dirs.
        "bm" => match args.parse::<usize>() {
            Ok(idx) => {
                let resolved = state
                    .dir_bookmarks
                    .lock()
                    .ok()
                    .and_then(|list| list.get(idx).cloned());
                match resolved {
                    Some(target) => match env::set_current_dir(&target) {
                        Ok(_) => state.cwd = env::current_dir().unwrap_or(target),
                        Err(e) => eprintln!("bm: {e}"),
                    },
                    None => eprintln!("bm: no bookmark at index {idx}"),
                }
            }
            Err(_) => eprintln!("bm: usage: bm <number>"),
        },

        // Everything else: Pass through to the system shell (e.g. the one which we launched this
        // application from)
        _ => {
            let result = if cfg!(windows) {
                // Powershell 7+; we will assume Windows users have this.
                // -NoProfile/-NoLogo skip loading the user's $PROFILE and the
                // startup banner, which together dominate pwsh's cold-start
                // time. Each command spawns a fresh process, so this shaves
                // ~200ms off every passthrough command.
                Command::new("pwsh")
                    .args(["-NoProfile", "-NoLogo", "-Command", input])
                    .status()
            } else {
                Command::new("sh").args(["-c", input]).status()
            };
            if let Err(e) = result {
                eprintln!("shell: {e}");
            }
        }
    }

    true
}

fn main() {
    // Resolve the persistent-state file path, then try to load. A missing
    // file is fine (first run); other I/O errors are reported but non-fatal.
    let state_path =
        save_data::default_path().unwrap_or_else(|| PathBuf::from(save_data::FILENAME));

    let mut state = State::load(&state_path).unwrap_or_else(|e| {
        eprintln!("warning: failed to load saved state ({e}); starting fresh");
        State::default()
    });
    state.refresh_browser_files();

    // Editor gives us: line editing, arrow-key history, Ctrl+A/E/K/W, etc.
    // We pair it with a custom Helper so Tab completes bookmark paths after
    // `cd ` and falls back to filesystem paths otherwise. `CompletionType::List`
    // gives bash-style behavior: partial-complete to the common prefix when
    // multiple candidates match, then list them.
    let config = Config::builder()
        .completion_type(CompletionType::List)
        .build();
    let mut rl: Editor<ShellHelper, FileHistory> = match Editor::with_config(config) {
        Ok(rl) => rl,
        Err(e) => {
            eprintln!("Failed to start editor: {e}");
            return;
        }
    };
    let home: Option<PathBuf> = env::var_os("USERPROFILE")
        .or_else(|| env::var_os("HOME"))
        .map(PathBuf::from);
    rl.set_helper(Some(ShellHelper {
        bookmarks: state.dir_bookmarks.clone(),
        home: home.clone(),
        fs_completer: FilenameCompleter::new(),
    }));

    // Shared printer so key handlers can write messages above the in-progress
    // prompt without corrupting rustyline's display state.
    let printer: SharedPrinter = match rl.create_external_printer() {
        Ok(p) => Arc::new(Mutex::new(Box::new(p))),
        Err(e) => {
            eprintln!("Failed to create external printer: {e}");
            return;
        }
    };

    // Ctrl + B:  push the CWD onto state.dir_bookmarks (and persist to disk).
    rl.bind_sequence(
        KeyEvent::new('b', Modifiers::CTRL),
        EventHandler::Conditional(Box::new(BookmarkHandler {
            bookmarks: state.dir_bookmarks.clone(),
            recent_dirs: state.recent_dirs.clone(),
            history: state.history.clone(),
            remote_terminals: state.remote_terminals.clone(),
            save_path: state_path.clone(),
            printer: printer.clone(),
        })),
    );

    // Alt + B: Display the current bookmark list.
    rl.bind_sequence(
        KeyEvent::new('b', Modifiers::ALT),
        EventHandler::Conditional(Box::new(ShowListHandler {
            kind: NavKind::Bookmarks,
            history: state.history.clone(),
            recent_dirs: state.recent_dirs.clone(),
            bookmarks: state.dir_bookmarks.clone(),
            home: home.clone(),
            printer: printer.clone(),
        })),
    );

    // Ctrl + R: Display the recent-directories list. Overrides rustyline's
    // default reverse-i-search binding, which this shell doesn't use.
    rl.bind_sequence(
        KeyEvent::new('r', Modifiers::CTRL),
        EventHandler::Conditional(Box::new(ShowListHandler {
            kind: NavKind::RecentDirs,
            history: state.history.clone(),
            recent_dirs: state.recent_dirs.clone(),
            bookmarks: state.dir_bookmarks.clone(),
            home: home.clone(),
            printer: printer.clone(),
        })),
    );

    // Ctrl + H: Display recent history
    rl.bind_sequence(
        KeyEvent::new('h', Modifiers::CTRL),
        EventHandler::Conditional(Box::new(ShowListHandler {
            kind: NavKind::History,
            history: state.history.clone(),
            recent_dirs: state.recent_dirs.clone(),
            bookmarks: state.dir_bookmarks.clone(),
            home: home.clone(),
            printer: printer.clone(),
        })),
    );

    // Arrow-key recall: ↑/↓ walk `state.history`; ←/→ walk `state.recent_dirs`.
    // All four bail out via Cmd::Interrupt so the main loop can rebuild the
    // prompt with a `his N` / `cd N` indicator (rustyline can't change a
    // prompt mid-line).
    let hist_nav = Arc::new(Mutex::new(CliNav::new()));
    let bind_arrow = |axis: NavAxis, backward: bool| ArrowHandler {
        history: state.history.clone(),
        recent_dirs: state.recent_dirs.clone(),
        home: home.clone(),
        nav: hist_nav.clone(),
        axis,
        backward,
    };
    rl.bind_sequence(
        KeyEvent(KeyCode::Up, Modifiers::NONE),
        EventHandler::Conditional(Box::new(bind_arrow(NavAxis::His, true))),
    );
    rl.bind_sequence(
        KeyEvent(KeyCode::Down, Modifiers::NONE),
        EventHandler::Conditional(Box::new(bind_arrow(NavAxis::His, false))),
    );
    rl.bind_sequence(
        KeyEvent(KeyCode::Left, Modifiers::NONE),
        EventHandler::Conditional(Box::new(bind_arrow(NavAxis::Cd, true))),
    );
    rl.bind_sequence(
        KeyEvent(KeyCode::Right, Modifiers::NONE),
        EventHandler::Conditional(Box::new(bind_arrow(NavAxis::Cd, false))),
    );

    loop {
        // Snapshot the nav state for this iteration. If a previous arrow
        // press left us with `pending_restart`, we use it as `readline`'s
        // initial buffer and bake the matching `his N` / `cd N` into the
        // prompt. Otherwise this is a fresh prompt.
        let (initial, prompt) = match hist_nav.lock() {
            Ok(mut n) => (n.pending_restart.take(), state.prompt(&n.nav)),
            Err(_) => (None, state.prompt(&NavState::new())),
        };

        let result = match initial.as_deref() {
            Some(text) => {
                // Wipe the previous prompt line (which still shows the old
                // `his N` / cwd / input) so the redraw doesn't stack lines
                // as the user walks through history.
                print!("\x1b[1A\r\x1b[2K");
                let _ = io::Write::flush(&mut io::stdout());
                rl.readline_with_initial(&prompt, (text, ""))
            }
            None => rl.readline(&prompt),
        };

        match result {
            Ok(line) => {
                if let Ok(mut n) = hist_nav.lock() {
                    n.reset();
                }
                if !line.trim().is_empty() {
                    let _ = rl.add_history_entry(&line);
                }
                if !run_command(&mut state, &state_path, &line) {
                    break;
                }
                // The command may have changed our cwd (cd) or the current
                // branch (`git checkout`); re-detect so the next prompt is
                // accurate.
                state.refresh_branch();
                state.refresh_browser_files();
            }
            Err(ReadlineError::Interrupted) => {
                // Two cases: (a) Up/Down handler asked us to restart with a
                // new prompt — pending_restart is set, just loop. (b) Real
                // Ctrl+C — reset nav and print ^C.
                let restart = hist_nav
                    .lock()
                    .ok()
                    .map(|n| n.pending_restart.is_some())
                    .unwrap_or(false);
                if !restart {
                    if let Ok(mut n) = hist_nav.lock() {
                        n.reset();
                    }
                    println!("^C");
                }
            }
            Err(ReadlineError::Eof) => break, // Ctrl+D exits
            Err(e) => {
                eprintln!("Error: {e}");
                break;
            }
        }
    }
}
