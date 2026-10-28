//! An unfortunately high amount of this project's architecture is built around
//! Rustyline conventions. This includes liberal use of Arc<Mutex>>, and Trait-based
//! handling of things that could otherwise be plain functions.

use std::{
    cell::RefCell,
    collections::HashMap,
    env, io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use rustyline::{
    Cmd, CompletionType, ConditionalEventHandler, Config, Context, Editor, Event, EventContext,
    EventHandler, ExternalPrinter, Helper, KeyCode, KeyEvent, Modifiers, RepeatCount,
    completion::{Completer, Pair},
    error::ReadlineError,
    hint::{Hint, Hinter},
    history::FileHistory,
    validate::Validator,
};
use shell::{
    ClipboardEntry, CompletionSources, DISP_PAGE_LEN, HintCache, NavState, OpenTabs,
    RemoteTerminal, commands, complete_input, page_count, save_data,
    state::{HistoryItem, RecentDir},
};

mod key_handling;
mod render;

// Markers `highlight_prompt` looks for when colouring the recall indicators
// (` his N`, ` cd N`) light green. The leading space is part of the marker
// so a cwd that happens to contain "his" or "cd" mid-path doesn't match.
const HIS_PREFIX: &str = " his ";
const CD_PREFIX: &str = " cd ";
// Re-exported from the shell lib so render.rs and the prompt builder use
// exactly the same string (`" branch: "`).
use shell::state::State;
pub use shell::{BRANCH_PREFIX, VENV_PREFIX};

use crate::key_handling::{ArrowHandler, HintHandler};

/// Shared handle to rustyline's `ExternalPrinter`. Key handlers use this to
/// print messages *above* the in-progress prompt line — going through
/// rustyline so it knows to clear the prompt, write the message, and redraw
/// the prompt below. A raw `println!` from a handler corrupts the display
/// because rustyline's cursor-tracking state never sees the write.
type SharedPrinter = Arc<Mutex<Box<dyn ExternalPrinter + Send>>>;

/// Rustyline `Helper` that provides Tab completion and the greyed
/// suggestion through the shell lib's shared completer (see
/// [complete_input], [shell::input_hint]), as Peregrine does.
struct ShellHelper {
    bookmarks: Arc<Mutex<Vec<PathBuf>>>,
    recent_dirs: Arc<Mutex<Vec<RecentDir>>>,
    clone_roots: Arc<Mutex<Vec<String>>>,
    /// The persisted command history, which suggestions are drawn from.
    history: Arc<Mutex<Vec<HistoryItem>>>,
    home: Option<PathBuf>,
    /// Set while an SSH session is live. Commands then run on the remote, so
    /// local files and PATH say nothing about them: no completion, and no
    /// red "unknown command".
    remote: Arc<AtomicBool>,
    /// The suggestion last shown, shared with [HintHandler] so →, End and
    /// Ctrl+E can accept it.
    hints: Arc<Mutex<HintCache>>,
    /// Memoised command-name → "is this runnable?" results, used by the
    /// syntax highlighter to colour an unrecognised command red (fish-style).
    /// Cached because `highlight` re-runs on every keystroke and a PATH probe
    /// would otherwise repeat for each character of the same word. `RefCell`
    /// because `Highlighter` only hands us `&self`; the helper lives on the
    /// single readline thread, so interior mutability is sound here.
    cmd_cache: RefCell<HashMap<String, bool>>,
}

/// Built-in commands handled directly by `commands::run_command`, so they're
/// "valid" even though no executable of that name exists on disk. Keep in sync
/// with the dispatch there.
const BUILTINS: &[&str] = &[
    "exit",
    "quit",
    "shelp",
    "--version",
    "-v",
    "cd",
    "root",
    "bm",
    "cat",
    "del",
    "his",
    "hist",
    "hisd",
    "this",
    "sync",
    "pull",
    "push",
    "branch",
    "commit",
    "checkout",
    "switch",
    "clone",
    "stash",
    "run",
    "build",
    "fmt",
    "rm_targets",
    "open",
    "logs",
    "ssh",
    "remote",
    "logout",
    "disconnect",
];

/// Common words the pass-through shell understands but which aren't files on
/// PATH (shell builtins / aliases). Without these, everyday commands would be
/// wrongly flagged. Heuristic, not exhaustive — we deliberately bias toward
/// *not* reddening, since a false "invalid" is more annoying than a missed one.
/// Platform-specific because the pass-through shell differs: pwsh on Windows,
/// `sh` elsewhere (see `commands::run_command`).
#[cfg(windows)]
const SHELL_WORDS: &[&str] = &[
    "ls", "dir", "gci", "cd", "sl", "chdir", "cls", "clear", "echo", "write", "cat", "gc", "type",
    "cp", "copy", "cpi", "mv", "move", "mi", "rm", "del", "erase", "rd", "ri", "rmdir", "mkdir",
    "md", "ni", "pwd", "gl", "ps", "gps", "kill", "spps", "where", "gcm", "man", "help", "select",
    "sort", "measure", "group", "ft", "fl", "gm", "iex", "icm", "sleep", "start", "saps", "tee",
    "history", "ghy", "popd", "pushd", "exit",
];
#[cfg(not(windows))]
const SHELL_WORDS: &[&str] = &[
    "cd", "echo", "pwd", "export", "alias", "unalias", "set", "unset", "source", "eval", "exec",
    "exit", "read", "test", "true", "false", "type", "command", "hash", "help", "history", "jobs",
    "fg", "bg", "kill", "wait", "trap", "umask", "ulimit", "shift", "getopts", "local", "return",
    "declare", "let", "printf", "time", "dirs", "pushd", "popd", "builtin", "enable", "logout",
];

impl ShellHelper {
    /// Whether `cmd` (the first word of the input) names something the shell
    /// can run: one of our built-ins, a known pass-through-shell word, a
    /// PowerShell `Verb-Noun` cmdlet (Windows), or an executable found on PATH
    /// / at an explicit path. Drives the red "unknown command" highlight.
    ///
    /// Biased toward returning `true` when unsure — see [SHELL_WORDS].
    fn command_is_valid(&self, cmd: &str) -> bool {
        if cmd.is_empty() {
            return false;
        }
        if self.remote.load(Ordering::Relaxed) {
            return true;
        }
        let lower = cmd.to_ascii_lowercase();
        if BUILTINS.contains(&lower.as_str())
            || SHELL_WORDS.contains(&lower.as_str())
            || looks_like_cmdlet(cmd)
        {
            return true;
        }
        if let Some(&hit) = self.cmd_cache.borrow().get(cmd) {
            return hit;
        }
        let found = command_exists(cmd);
        self.cmd_cache.borrow_mut().insert(cmd.to_string(), found);
        found
    }
}

/// True for a PowerShell-style `Verb-Noun` token (e.g. `Get-ChildItem`). On
/// Windows we treat these as valid rather than risk reddening a real cmdlet we
/// don't have an alias for; elsewhere there are no cmdlets, so always false.
#[cfg(windows)]
fn looks_like_cmdlet(cmd: &str) -> bool {
    match cmd.split_once('-') {
        Some((verb, noun)) => {
            !verb.is_empty()
                && !noun.is_empty()
                && verb.chars().all(|c| c.is_ascii_alphabetic())
                && noun.chars().all(|c| c.is_ascii_alphanumeric())
        }
        None => false,
    }
}
#[cfg(not(windows))]
fn looks_like_cmdlet(_cmd: &str) -> bool {
    false
}

/// True if `cmd` resolves to a runnable executable: an explicit path that
/// points at one, or a bare name found in a PATH directory.
fn command_exists(cmd: &str) -> bool {
    if cmd.contains('/') || cmd.contains('\\') {
        return path_is_executable(Path::new(cmd));
    }
    if let Some(paths) = env::var_os("PATH") {
        for dir in env::split_paths(&paths) {
            if dir.as_os_str().is_empty() {
                continue;
            }
            if path_is_executable(&dir.join(cmd)) {
                return true;
            }
        }
    }
    false
}

/// Windows: a path is "executable" if it's a file as-given, or becomes one
/// once a PATHEXT extension (`.EXE`, `.CMD`, …) is appended — matching how the
/// OS resolves bare command names.
#[cfg(windows)]
fn path_is_executable(base: &Path) -> bool {
    use std::ffi::OsString;
    if base.is_file() {
        return true;
    }
    let exts = env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
    for ext in exts.split(';') {
        let ext = ext.trim();
        if ext.is_empty() {
            continue;
        }
        let mut candidate: OsString = base.as_os_str().to_owned();
        if !ext.starts_with('.') {
            candidate.push(".");
        }
        candidate.push(ext);
        if Path::new(&candidate).is_file() {
            return true;
        }
    }
    false
}

/// Unix: a path is "executable" if it's a regular file with any execute bit set.
#[cfg(unix)]
fn path_is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(p) {
        Ok(m) => m.is_file() && m.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

/// Fallback for platforms that are neither Windows nor Unix: best-effort
/// file-existence check.
#[cfg(not(any(windows, unix)))]
fn path_is_executable(p: &Path) -> bool {
    p.is_file()
}

impl ShellHelper {
    /// Run `f` with what completion and suggestions draw on, filled as
    /// Peregrine fills it.
    fn with_sources<R>(&self, f: impl FnOnce(&CompletionSources) -> R) -> Option<R> {
        let bookmarks = self.bookmarks.lock().ok()?;
        let recent_dirs = self.recent_dirs.lock().ok()?;
        let clone_roots = self.clone_roots.lock().ok()?;
        let history = self.history.lock().ok()?;
        let cwd = env::current_dir().ok()?;
        Some(f(&CompletionSources {
            cwd: &cwd,
            home: self.home.as_deref(),
            bookmarks: &bookmarks,
            recent_dirs: &recent_dirs,
            clone_roots: &clone_roots,
            history: &history,
            remote: self.remote.load(Ordering::Relaxed),
        }))
    }
}

impl Completer for ShellHelper {
    type Candidate = Pair;

    /// The shared completion (see [complete_input]), so Tab completes as in
    /// Peregrine.
    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let result = self
            .with_sources(|src| complete_input(line, pos, src))
            .flatten();
        Ok(match result {
            Some(result) => {
                let pairs = result
                    .candidates
                    .into_iter()
                    .map(|candidate| Pair {
                        display: candidate.display,
                        replacement: candidate.replacement,
                    })
                    .collect();
                (result.start, pairs)
            }
            None => (pos, Vec::new()),
        })
    }
}

/// A [shell::InputHint] as rustyline shows it. Only a suggestion that carries
/// on the line has a completion for rustyline to insert; [HintHandler]
/// accepts one that rewrites it.
struct CliHint {
    display: String,
    completion: Option<String>,
}

impl Hint for CliHint {
    fn display(&self) -> &str {
        &self.display
    }

    fn completion(&self) -> Option<&str> {
        self.completion.as_deref()
    }
}

impl Hinter for ShellHelper {
    type Hint = CliHint;

    /// The shared fish-style suggestion (see [shell::input_hint]): the rest
    /// of a past command, or Tab's one completion. Shown dimmed after the
    /// cursor (see `highlight_hint`); [HintHandler] accepts it.
    fn hint(&self, line: &str, pos: usize, _ctx: &Context<'_>) -> Option<CliHint> {
        let hint = self
            .with_sources(|src| self.hints.lock().ok()?.get(line, pos, src))
            .flatten()?;
        let completion = hint.line.strip_prefix(line).map(str::to_string);
        Some(CliHint {
            display: hint.display,
            completion,
        })
    }
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
    /// history + remote terminals + clone roots) in a single pass when a
    /// bookmark is added.
    recent_dirs: Arc<Mutex<Vec<RecentDir>>>,
    history: Arc<Mutex<Vec<HistoryItem>>>,
    remote_terminals: Arc<Mutex<Vec<RemoteTerminal>>>,
    clone_roots: Arc<Mutex<Vec<String>>>,
    /// Snapshot of the GUI open-tab layout taken at handler construction.
    /// The CLI never mutates this field, so the snapshot is always current
    /// and we just write it back unchanged to preserve the GUI's tabs.
    open_tabs: OpenTabs,
    /// Snapshot of the GUI's custom bookmark order, written back unchanged
    /// for the same reason as `open_tabs`.
    bookmark_order: Vec<PathBuf>,
    /// Snapshot of the GUI's clipboard history, written back unchanged for
    /// the same reason as `open_tabs`.
    clipboard: Vec<ClipboardEntry>,
    /// Snapshot of the GUI's scratchpad text, written back unchanged for
    /// the same reason as `open_tabs`.
    scratchpad: String,
    save_path: PathBuf,
    printer: SharedPrinter,
    /// Shared recall state. After adding a bookmark we stash the current input
    /// here and bail via `Cmd::Interrupt` so the main loop rebuilds the prompt
    /// with the `*` marker immediately (rustyline can't change a prompt
    /// mid-line — same trick the arrow handlers use).
    nav: Arc<Mutex<CliNav>>,
}

impl ConditionalEventHandler for BookmarkHandler {
    fn handle(
        &self,
        _evt: &Event,
        _n: RepeatCount,
        _positive: bool,
        ctx: &EventContext<'_>,
    ) -> Option<Cmd> {
        let mut added = false;
        if let Ok(cwd) = env::current_dir()
            && let Ok(mut list) = self.bookmarks.lock()
        {
            let msg = if list.contains(&cwd) {
                "This bookmark already exists\n".to_string()
            } else {
                let msg = format!("Added a bookmark: {}\n", cwd.display());
                list.push(cwd);
                added = true;
                // Lock recent_dirs, history, remote_terminals, clone_roots
                // after bookmarks — same order as State::save, so no
                // lock-order conflicts.
                if let Ok(recent) = self.recent_dirs.lock()
                    && let Ok(history) = self.history.lock()
                    && let Ok(remote_terminals) = self.remote_terminals.lock()
                    && let Ok(clone_roots) = self.clone_roots.lock()
                    && let Err(e) = save_data::save_state(
                        &list,
                        &recent,
                        &history,
                        &remote_terminals,
                        &clone_roots,
                        &self.open_tabs,
                        &self.bookmark_order,
                        &self.clipboard,
                        &self.scratchpad,
                        &self.save_path,
                    )
                {
                    eprintln!("warning: failed to save state: {e}");
                }
                msg
            };
            if let Ok(mut p) = self.printer.lock() {
                let _ = p.print(msg);
            }
        }

        // A newly added bookmark changes the prompt's `*` marker. Stash the
        // current input and interrupt so the main loop redraws the prompt with
        // the star straight away, restoring what the user had typed.
        if added && let Ok(mut nav) = self.nav.lock() {
            nav.pending_restart = Some(ctx.line().to_string());
            return Some(Cmd::Interrupt);
        }

        // Consume the keystroke so rustyline doesn't also run its default
        // Ctrl+B binding (backward-char).
        Some(Cmd::Noop)
    }
}

/// Rustyline key handler for Alt + `, 1, 2, 3 and 4: goes to one of
/// [shell::folder_key_dirs], keeping what's typed. Like the bookmark handler,
/// it interrupts so the main loop changes dir and redraws the prompt.
struct FolderKeyHandler {
    dir: Option<PathBuf>,
    remote: Arc<AtomicBool>,
    printer: SharedPrinter,
    nav: Arc<Mutex<CliNav>>,
}

impl ConditionalEventHandler for FolderKeyHandler {
    fn handle(
        &self,
        _evt: &Event,
        _n: RepeatCount,
        _positive: bool,
        ctx: &EventContext<'_>,
    ) -> Option<Cmd> {
        let msg = if self.remote.load(Ordering::Relaxed) {
            "cd: only runs locally; disconnect first\n".to_string()
        } else {
            match &self.dir {
                Some(dir) if dir.is_dir() => {
                    let mut nav = self.nav.lock().ok()?;
                    nav.pending_cd = Some(dir.clone());
                    nav.pending_restart = Some(ctx.line().to_string());
                    return Some(Cmd::Interrupt);
                }
                Some(dir) => format!("cd: {} doesn't exist\n", dir.display()),
                None => return Some(Cmd::Noop),
            }
        };
        if let Ok(mut p) = self.printer.lock() {
            let _ = p.print(msg);
        }

        // Consumed either way: rustyline's own Alt + digit starts a repeat
        // count.
        Some(Cmd::Noop)
    }
}

/// Which paginated list a Ctrl+1 through Ctrl+5 keystroke opens.
#[derive(Clone, Copy, PartialEq, Eq)]
enum NavKind {
    History,
    HistoryInDir,
    RecentDirs,
    Bookmarks,
    Remotes,
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
    /// Where a [FolderKeyHandler] asked to go, before the next prompt.
    pending_cd: Option<PathBuf>,
}

impl CliNav {
    fn new() -> Self {
        Self {
            nav: NavState::new(),
            pending_restart: None,
            pending_cd: None,
        }
    }

    fn reset(&mut self) {
        self.nav.reset();
        self.pending_restart = None;
        self.pending_cd = None;
    }
}

/// Tracks which paginated list was shown last and on which page, so a
/// repeated keystroke (e.g. Ctrl+3, Ctrl+3) advances to the next-older page
/// instead of re-printing page 1. Shared by all five [ShowListHandler]s;
/// cleared when a line is submitted so the next keystroke starts fresh.
type ListPageState = Arc<Mutex<Option<(NavKind, usize)>>>;

/// Rustyline key handler: prints one of the five paginated lists
/// above the prompt. Pressing the same keystroke again cycles to the next
/// (older) page, wrapping back to page 1 after the last; pressing a
/// different list's keystroke starts that list at page 1.
struct ShowListHandler {
    kind: NavKind,
    history: Arc<Mutex<Vec<HistoryItem>>>,
    recent_dirs: Arc<Mutex<Vec<RecentDir>>>,
    bookmarks: Arc<Mutex<Vec<PathBuf>>>,
    remote_terminals: Arc<Mutex<Vec<RemoteTerminal>>>,
    home: Option<PathBuf>,
    printer: SharedPrinter,
    page_state: ListPageState,
}

impl ShowListHandler {
    /// Render this handler's list at `requested`, wrapping past the last
    /// page. Returns the rendered text and the page actually shown, which
    /// `handle` records in `page_state` for the next keystroke.
    fn render(&self, requested: usize) -> Option<(String, usize)> {
        match self.kind {
            NavKind::History => {
                let h = self.history.lock().ok()?;
                let count = shell::history_latest_indices(&h).len();
                let page = requested % page_count(count, DISP_PAGE_LEN);
                Some((shell::render_history(&h, page), page))
            }
            NavKind::HistoryInDir => {
                let h = self.history.lock().ok()?;
                let cwd = std::env::current_dir().ok()?;
                let count = shell::history_indices_in_dir(&h, &cwd).len();
                let page = requested % page_count(count, DISP_PAGE_LEN);
                Some((shell::render_history_in_dir(&h, &cwd, page), page))
            }
            NavKind::RecentDirs => {
                let r = self.recent_dirs.lock().ok()?;
                let bm = self.bookmarks.lock().ok()?;
                let page = requested % page_count(r.len(), DISP_PAGE_LEN);
                Some((
                    render::render_recent_dirs(&r, &bm, self.home.as_deref(), page),
                    page,
                ))
            }
            NavKind::Bookmarks => {
                let bm = self.bookmarks.lock().ok()?;
                let page = requested % page_count(bm.len(), DISP_PAGE_LEN);
                Some((
                    render::render_bookmarks(&bm, self.home.as_deref(), page),
                    page,
                ))
            }
            NavKind::Remotes => {
                let r = self.remote_terminals.lock().ok()?;
                let page = requested % page_count(r.len(), DISP_PAGE_LEN);
                Some((shell::ssh::render_remotes(&r, page), page))
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
        // Same list as last time → advance a page; anything else → page 1.
        let requested = match self.page_state.lock().ok().and_then(|st| *st) {
            Some((kind, page)) if kind == self.kind => page + 1,
            _ => 0,
        };
        if let Some((msg, page)) = self.render(requested) {
            if let Ok(mut p) = self.printer.lock() {
                let _ = p.print(msg);
            }
            if let Ok(mut st) = self.page_state.lock() {
                *st = Some((self.kind, page));
            }
        }
        Some(Cmd::Noop)
    }
}

fn main() {
    // `shell --version` / `shell -v` from the outer terminal: print and exit
    // without starting the interactive shell.
    if let Some(arg) = env::args().nth(1)
        && (arg == "--version" || arg == "-v")
    {
        println!("{}", commands::version_line());
        return;
    }

    // Keep Ctrl+C pointed at launched child processes: without this, a Ctrl+C
    // meant for e.g. a running python script also terminates this shell,
    // dropping the user out to their outer terminal. Must be installed before
    // the first passthrough command runs; startup is the natural spot.
    commands::install_ctrl_c_shield();

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
    let remote = Arc::new(AtomicBool::new(false));
    let hints = Arc::new(Mutex::new(HintCache::default()));
    rl.set_helper(Some(ShellHelper {
        bookmarks: state.dir_bookmarks.clone(),
        recent_dirs: state.recent_dirs.clone(),
        clone_roots: state.clone_roots.clone(),
        history: state.history.clone(),
        home: home.clone(),
        remote: remote.clone(),
        hints: hints.clone(),
        cmd_cache: RefCell::new(HashMap::new()),
    }));

    // Seed rustyline's in-memory history from our persisted history (oldest
    // first, so the newest ends up most-recent). Rustyline's own search
    // (Ctrl+R) reads this list, so without seeding it would only find
    // commands typed in the current session. Our own ↑/↓ recall and the
    // greyed suggestion read `state.history` directly and are unaffected.
    if let Ok(history) = state.history.lock() {
        for i in shell::history_latest_indices(&history) {
            let _ = rl.add_history_entry(&history[i].text);
        }
    }

    // Shared printer so key handlers can write messages above the in-progress
    // prompt without corrupting rustyline's display state.
    let printer: SharedPrinter = match rl.create_external_printer() {
        Ok(p) => Arc::new(Mutex::new(Box::new(p))),
        Err(e) => {
            eprintln!("Failed to create external printer: {e}");
            return;
        }
    };

    // Shared recall state for the arrow-key handlers and the bookmark handler;
    // declared here so the Ctrl+B binding below can clone it.
    let hist_nav = Arc::new(Mutex::new(CliNav::new()));

    // Ctrl + B:  push the CWD onto state.dir_bookmarks (and persist to disk).
    rl.bind_sequence(
        KeyEvent::new('b', Modifiers::CTRL),
        EventHandler::Conditional(Box::new(BookmarkHandler {
            bookmarks: state.dir_bookmarks.clone(),
            recent_dirs: state.recent_dirs.clone(),
            history: state.history.clone(),
            remote_terminals: state.remote_terminals.clone(),
            clone_roots: state.clone_roots.clone(),
            nav: hist_nav.clone(),
            open_tabs: state.open_tabs.clone(),
            bookmark_order: state.bookmark_order.clone(),
            clipboard: state.clipboard.clone(),
            scratchpad: state.scratchpad.clone(),
            save_path: state_path.clone(),
            printer: printer.clone(),
        })),
    );

    // All five list keystrokes share one handler type; this builds the
    // handler for a given list kind. `list_page` is shared across them so
    // repeating a list's keystroke advances its page (see ListPageState).
    // Ctrl+digit works on Windows through the vendored rustyline patch.
    let list_page: ListPageState = Arc::new(Mutex::new(None));
    let show_list = |kind: NavKind| {
        EventHandler::Conditional(Box::new(ShowListHandler {
            kind,
            history: state.history.clone(),
            recent_dirs: state.recent_dirs.clone(),
            bookmarks: state.dir_bookmarks.clone(),
            remote_terminals: state.remote_terminals.clone(),
            home: home.clone(),
            printer: printer.clone(),
            page_state: list_page.clone(),
        }))
    };

    // Display the current bookmark list.
    rl.bind_sequence(
        KeyEvent::new('1', Modifiers::CTRL),
        show_list(NavKind::Bookmarks),
    );

    // Display the saved-remotes list.
    rl.bind_sequence(
        KeyEvent::new('5', Modifiers::CTRL),
        show_list(NavKind::Remotes),
    );

    // Display the recent-directories list.
    rl.bind_sequence(
        KeyEvent::new('2', Modifiers::CTRL),
        show_list(NavKind::RecentDirs),
    );

    // Display recent command history.
    rl.bind_sequence(
        KeyEvent::new('3', Modifiers::CTRL),
        show_list(NavKind::History),
    );
    rl.bind_sequence(
        KeyEvent::new('4', Modifiers::CTRL),
        show_list(NavKind::HistoryInDir),
    );

    // Alt + `, 1, 2, 3, 4: go home, or to Desktop, Downloads, app data or
    // programs, as Peregrine's Files panel buttons do.
    let folder_dirs = shell::folder_key_dirs(home.as_deref());
    for (key, dir) in ['`', '1', '2', '3', '4'].into_iter().zip(folder_dirs) {
        rl.bind_sequence(
            KeyEvent::new(key, Modifiers::ALT),
            EventHandler::Conditional(Box::new(FolderKeyHandler {
                dir,
                remote: remote.clone(),
                printer: printer.clone(),
                nav: hist_nav.clone(),
            })),
        );
    }

    // Arrow-key recall: ↑/↓ walk `state.history`; ←/→ walk `state.recent_dirs`.
    // All four bail out via Cmd::Interrupt so the main loop can rebuild the
    // prompt with a `his N` / `cd N` indicator (rustyline can't change a
    // prompt mid-line).
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
    // → walks recent dirs as above; otherwise it, End and Ctrl+E accept the
    // greyed suggestion at the end of the line, as in Peregrine.
    rl.bind_sequence(
        KeyEvent(KeyCode::Right, Modifiers::NONE),
        EventHandler::Conditional(Box::new(HintHandler {
            hints: hints.clone(),
            arrow: Some(bind_arrow(NavAxis::Cd, false)),
        })),
    );
    for key in [
        KeyEvent(KeyCode::End, Modifiers::NONE),
        KeyEvent::new('e', Modifiers::CTRL),
    ] {
        rl.bind_sequence(
            key,
            EventHandler::Conditional(Box::new(HintHandler {
                hints: hints.clone(),
                arrow: None,
            })),
        );
    }

    loop {
        // A folder key asked to go somewhere; go before building the prompt.
        let pending_cd = hist_nav.lock().ok().and_then(|mut n| n.pending_cd.take());
        if let Some(dir) = pending_cd
            && let Err(e) = state.change_dir(dir)
        {
            eprintln!("cd: {e}");
        }

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
                let line = rl
                    .helper()
                    .and_then(|h| {
                        h.with_sources(|src| hints.lock().ok()?.line_for_enter(&line, src))
                    })
                    .flatten()
                    .unwrap_or(line);
                if let Ok(mut n) = hist_nav.lock() {
                    n.reset();
                }
                // Submitting a line ends any list-page cycling; the next
                // Ctrl+1 through Ctrl+5 starts back at page 1.
                if let Ok(mut lp) = list_page.lock() {
                    *lp = None;
                }
                if !line.trim().is_empty() {
                    let _ = rl.add_history_entry(&line);
                }
                if !commands::run_command(&mut state, &state_path, &line) {
                    break;
                }
                remote.store(state.is_remote(), Ordering::Relaxed);
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
                    if let Ok(mut lp) = list_page.lock() {
                        *lp = None;
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
