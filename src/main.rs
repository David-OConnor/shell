//! An unfortunately high amount of this project's architecture is built around
//! Rustyline conventions. This includes liberal use of Arc<Mutex>>, and Trait-based
//! handling of things that could otherwise be plain functions.

use std::{
    env, io,
    path::{Path, PathBuf},
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
    NavState, OpenTabs, PanelVis, RemoteTerminal, WindowSize, commands,
    commands::{self},
    save_data,
    state::{HistoryItem, RecentDir},
};

mod input_completion;
mod key_handling;
mod render;

// Markers `highlight_prompt` looks for when colouring the recall indicators
// (` his N`, ` cd N`) light green. The leading space is part of the marker
// so a cwd that happens to contain "his" or "cd" mid-path doesn't match.
const HIS_PREFIX: &str = " his ";
const CD_PREFIX: &str = " cd ";
// Re-exported from the shell lib so render.rs and the prompt builder use
// exactly the same string (`" branch: "`).
pub use shell::BRANCH_PREFIX;
use shell::state::State;

use crate::input_completion::complete_cd_path;

/// Shared handle to rustyline's `ExternalPrinter`. Key handlers use this to
/// print messages *above* the in-progress prompt line — going through
/// rustyline so it knows to clear the prompt, write the message, and redraw
/// the prompt below. A raw `println!` from a handler corrupts the display
/// because rustyline's cursor-tracking state never sees the write.
type SharedPrinter = Arc<Mutex<Box<dyn ExternalPrinter + Send>>>;

// Display this many history items at a time.
const DISP_HIST_LEN: usize = 20;

const DIVIDER: &str = "----------";

/// Rustyline `Helper` that provides Tab-completion for the `cd` builtin
/// through the shared bookmark + filesystem completer. Other commands fall
/// back to rustyline's built-in filename completer.
struct ShellHelper {
    bookmarks: Arc<Mutex<Vec<PathBuf>>>,
    home: Option<PathBuf>,
    /// Rustyline's built-in filename completer, used as the fallback when no
    /// bookmark matches (and for non-`cd` commands).
    fs_completer: FilenameCompleter,
}

impl Completer for ShellHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        // If the command word is `cd`, use the shared bookmark + filesystem
        // path completer so nested args like `code/Bi` resolve correctly.
        let bookmarks = self.bookmarks.lock();
        let bookmark_slice: &[PathBuf] = bookmarks.as_deref().map(|v| v.as_slice()).unwrap_or(&[]);
        if let Ok(cwd) = env::current_dir() {
            if let Some(result) =
                complete_cd_path(line, pos, &cwd, self.home.as_deref(), bookmark_slice)
            {
                if !result.candidates.is_empty() {
                    let pairs = result
                        .candidates
                        .into_iter()
                        .map(|candidate| Pair {
                            display: candidate.display,
                            replacement: candidate.replacement,
                        })
                        .collect();
                    return Ok((result.start, pairs));
                }
            }
        }
        drop(bookmarks);

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
    /// Snapshot of `panel_vis` taken at handler construction. The CLI
    /// never mutates this field, so the snapshot is always current and
    /// we just write it back unchanged to preserve GUI settings.
    panel_vis: PanelVis,
    /// Snapshot of the GUI window size, written back unchanged for the same
    /// reason as `panel_vis`.
    window_size: Option<WindowSize>,
    /// Snapshot of the GUI open-tab layout, written back unchanged for the
    /// same reason as `panel_vis`.
    open_tabs: OpenTabs,
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
                                    &self.panel_vis,
                                    self.window_size,
                                    &self.open_tabs,
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
            panel_vis: state.panel_vis,
            window_size: state.window_size,
            open_tabs: state.open_tabs.clone(),
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
                if !commands::run_command(&mut state, &state_path, &line) {
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
