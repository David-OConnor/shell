mod save_data;
mod tasks;
mod util;

use std::{
    borrow::Cow,
    env, io,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
};

use chrono::{DateTime, Utc};
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

/// Shared handle to rustyline's `ExternalPrinter`. Key handlers use this to
/// print messages *above* the in-progress prompt line — going through
/// rustyline so it knows to clear the prompt, write the message, and redraw
/// the prompt below. A raw `println!` from a handler corrupts the display
/// because rustyline's cursor-tracking state never sees the write.
type SharedPrinter = Arc<Mutex<Box<dyn ExternalPrinter + Send>>>;

// ANSI escape codes. Modern Windows consoles (Windows Terminal, pwsh, post-2019
// conhost) handle these natively; rustyline enables VT processing on startup.
const COLOR_RESET: &str = "\x1b[0m";
const COLOR_YELLOW: &str = "\x1b[93m";
const COLOR_BLUE: &str = "\x1b[94m";
const COLOR_CYAN: &str = "\x1b[96m";
// Input syntax-highlighting palette.
const COLOR_TEAL: &str = "\x1b[96m"; // program command (e.g. `git`)
const COLOR_MAGENTA: &str = "\x1b[95m"; // subcommand (e.g. `commit`)
const COLOR_GREEN: &str = "\x1b[92m"; // parameters / flags (e.g. `-am`)
const COLOR_ORANGE: &str = "\x1b[38;5;208m"; // quote characters `'` and `"`

// Display this many history items at a time.
const DISP_HIST_LEN: usize = 20;

const DIVIDER: &str = "----------";

struct HistoryItem {
    pub text: String,
    pub dt: DateTime<Utc>,
}

struct RecentDir {
    pub path: PathBuf,
    pub dt: DateTime<Utc>,
}

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
}

impl State {
    // todo: Change this to Default::default A/R, as there are no params.
    fn new() -> Self {
        Self {
            home: util::get_home(),
            history: Arc::new(Mutex::new(Vec::new())),
            cwd: env::current_dir().unwrap_or_default(),
            dir_bookmarks: Arc::new(Mutex::new(Vec::new())),
            recent_dirs: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// This defines what the general prompt looks like. Its adorning characters let the user know they're in this shell.
    fn prompt(&self) -> String {
        // Mark the directory with a leading `*` when it's bookmarked.
        let bookmarked = self
            .dir_bookmarks
            .lock()
            .map(|list| list.contains(&self.cwd))
            .unwrap_or(false);
        let star = if bookmarked { "*" } else { "" };
        format!("S {star}{} $ ", self.cwd.display())
    }

    /// Persist user-controlled state (bookmarks + recent dirs) to the given
    /// file. Called after every mutation of either list. Locks bookmarks
    /// before recent_dirs — keep this order consistent across all callers
    /// to avoid lock-order deadlocks.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let bookmarks = self
            .dir_bookmarks
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "bookmark lock poisoned"))?;
        let recent = self
            .recent_dirs
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "recent-dirs lock poisoned"))?;
        save_data::save_state(&bookmarks, &recent, path)
    }

    /// Restore state from disk, returning a fresh `State` with that data.
    /// A missing file is treated as "no saved state" and yields the default
    /// `State::new()` values (not an error).
    pub fn load(path: &Path) -> io::Result<Self> {
        let (bookmarks, recent_dirs) = save_data::load_state(path)?;

        Ok(Self {
            home: util::get_home(),
            history: Arc::new(Mutex::new(Vec::new())),
            cwd: env::current_dir().unwrap_or_default(),
            dir_bookmarks: Arc::new(Mutex::new(bookmarks)),
            recent_dirs: Arc::new(Mutex::new(recent_dirs)),
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

/// Render a path as `~/relative` when it lives under the home directory;
/// otherwise use the absolute form. Uses forward slashes after the tilde for
/// consistency with the rest of the shell.
fn render_with_tilde(p: &Path, home: Option<&Path>) -> String {
    if let Some(home) = home {
        if let Ok(rest) = p.strip_prefix(home) {
            let rest_str = rest.to_string_lossy().replace('\\', "/");
            if rest_str.is_empty() {
                return "~".to_string();
            }
            return format!("~/{}", rest_str);
        }
    }
    p.display().to_string()
}

impl ShellHelper {
    fn render(&self, p: &Path) -> String {
        render_with_tilde(p, self.home.as_deref())
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

/// Render a single word in `color`, but recolor any quote characters (`'`/`"`)
/// orange so they stand out, then return to `color` for the rest of the word.
fn render_word(out: &mut String, text: &str, color: &str) {
    out.push_str(color);
    for ch in text.chars() {
        if ch == '"' || ch == '\'' {
            out.push_str(COLOR_RESET);
            out.push_str(COLOR_ORANGE);
            out.push(ch);
            out.push_str(COLOR_RESET);
            out.push_str(color);
        } else {
            out.push(ch);
        }
    }
    out.push_str(COLOR_RESET);
}

/// Syntax-highlight a command line:
/// - the first word (the program command) is teal,
/// - the first following non-flag word (the subcommand) is magenta,
/// - words beginning with `-` (flags/parameters) are light green,
/// - quote characters are orange,
/// - everything else keeps the base input color.
fn highlight_input(line: &str) -> String {
    let words = tokenize_words(line);
    let mut out = String::new();
    let mut last = 0;
    let mut subcommand_assigned = false;

    for (i, &(start, end)) in words.iter().enumerate() {
        // Emit any whitespace before this word uncolored.
        out.push_str(&line[last..start]);

        let text = &line[start..end];
        let color = if i == 0 {
            COLOR_TEAL
        } else if text.starts_with('-') {
            COLOR_GREEN
        } else if !subcommand_assigned {
            subcommand_assigned = true;
            COLOR_MAGENTA
        } else {
            COLOR_CYAN
        };

        render_word(&mut out, text, color);
        last = end;
    }
    // Trailing whitespace, if any.
    out.push_str(&line[last..]);
    out
}

impl Highlighter for ShellHelper {
    /// Color the `S` and `$` accents in the prompt yellow, leaving the
    /// directory in its default terminal color. Prompt shape from
    /// `State::prompt` is `"S <cwd> $ "`.
    fn highlight_prompt<'b, 's: 'b, 'p: 'b>(
        &'s self,
        prompt: &'p str,
        _default: bool,
    ) -> Cow<'b, str> {
        if let Some(rest) = prompt.strip_prefix("S ") {
            if let Some(dollar_idx) = rest.rfind(" $ ") {
                let dir = &rest[..dollar_idx];
                let tail = &rest[dollar_idx + 3..]; // usually empty
                return Cow::Owned(format!(
                    "{COLOR_YELLOW}S{COLOR_RESET} {dir} {COLOR_YELLOW}${COLOR_RESET} {tail}"
                ));
            }
        }
        Cow::Borrowed(prompt)
    }

    /// Syntax-highlight the user's in-progress input: teal command, magenta
    /// subcommand, light-green flags/parameters, and orange quote characters.
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        if line.is_empty() {
            Cow::Borrowed(line)
        } else {
            Cow::Owned(highlight_input(line))
        }
    }

    /// Tell rustyline to re-run `highlight` on every keystroke so the color
    /// extends to newly-typed characters.
    fn highlight_char(&self, _line: &str, _pos: usize, _forced: bool) -> bool {
        true
    }
}

impl Validator for ShellHelper {}
impl Helper for ShellHelper {}

/// Rustyline key handler: snapshots the current working directory
/// into the shared bookmark list. Runs inline within readline, so we use a
/// shared Arc<Mutex<_>> rather than touching `State` directly. Also persists
/// the list to disk on every successful add.
struct BookmarkHandler {
    bookmarks: Arc<Mutex<Vec<PathBuf>>>,
    /// Held so we can write the full state file (bookmarks + recent dirs)
    /// in a single pass when a bookmark is added.
    recent_dirs: Arc<Mutex<Vec<RecentDir>>>,
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
                    // Lock recent_dirs after bookmarks — same order as
                    // State::save, so no lock-order conflicts.
                    if let Ok(recent) = self.recent_dirs.lock() {
                        if let Err(e) = save_data::save_state(&list, &recent, &self.save_path) {
                            eprintln!("warning: failed to save state: {e}");
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

/// Rustyline key handler: prints the current bookmark list.
struct ListBookmarksHandler {
    bookmarks: Arc<Mutex<Vec<PathBuf>>>,
    home: Option<PathBuf>,
    printer: SharedPrinter,
}

impl ConditionalEventHandler for ListBookmarksHandler {
    fn handle(
        &self,
        _evt: &Event,
        _n: RepeatCount,
        _positive: bool,
        _ctx: &EventContext<'_>,
    ) -> Option<Cmd> {
        if let Ok(list) = self.bookmarks.lock() {
            let mut msg =
                String::from("\nBookmarks. Use `del bm <number>` to delete; e.g. `del bm 4`:\n");
            msg.push_str(DIVIDER);
            msg.push('\n');

            if list.is_empty() {
                msg.push_str("(no bookmarks)\n");
            } else {
                // We may use these displayed indexes so users can delete bookmarks etc.
                for (i, bm) in list.iter().enumerate() {
                    msg.push_str(&format!(
                        "{i}:  {}\n",
                        render_with_tilde(bm, self.home.as_deref())
                    ));
                }
            }
            msg.push_str(DIVIDER);
            msg.push_str("\n\n");
            if let Ok(mut p) = self.printer.lock() {
                let _ = p.print(msg);
            }
        }
        Some(Cmd::Noop)
    }
}

/// Rustyline key handler: prints the recent-directories list (Ctrl+R).
struct ListRecentDirsHandler {
    recent_dirs: Arc<Mutex<Vec<RecentDir>>>,
    /// Consulted so bookmarked recent dirs get a leading `*`, matching the
    /// prompt convention.
    bookmarks: Arc<Mutex<Vec<PathBuf>>>,
    home: Option<PathBuf>,
    printer: SharedPrinter,
}

impl ConditionalEventHandler for ListRecentDirsHandler {
    fn handle(
        &self,
        _evt: &Event,
        _n: RepeatCount,
        _positive: bool,
        _ctx: &EventContext<'_>,
    ) -> Option<Cmd> {
        if let Ok(list) = self.recent_dirs.lock() {
            // Snapshot the bookmark set so we can mark each row without
            // re-locking inside the render loop.
            let bookmarks: Vec<PathBuf> =
                self.bookmarks.lock().map(|b| b.clone()).unwrap_or_default();

            let mut msg =
                String::from("\nRecent directories. Use `cd <number>` to go; e.g. `cd 4`:\n");
            msg.push_str(DIVIDER);
            msg.push('\n');

            if list.is_empty() {
                msg.push_str("(no recent directories)\n");
            } else {
                // Vec is maintained oldest-first; newest sits at the bottom.
                for (i, r) in list.iter().enumerate() {
                    let star = if bookmarks.contains(&r.path) { "*" } else { "" };
                    msg.push_str(&format!(
                        "{i}:  {star}{}\n",
                        render_with_tilde(&r.path, self.home.as_deref())
                    ));
                }
            }
            msg.push_str(DIVIDER);
            msg.push_str("\n\n");
            if let Ok(mut p) = self.printer.lock() {
                let _ = p.print(msg);
            }
        }
        Some(Cmd::Noop)
    }
}

/// Tracks the user's position when paging through history with the arrow
/// keys. Set by Ctrl+H; while `active`, Left/Right paginate when the input
/// line is empty.
struct HistoryNavState {
    active: bool,
    /// 0 = most recent page (newest items, shown at the bottom).
    page: usize,
}

impl HistoryNavState {
    fn new() -> Self {
        Self {
            active: false,
            page: 0,
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

/// Render one page of history. Page 0 = most recent items, with the newest
/// at the bottom. Items are labelled with their absolute index into the
/// history vec so the user can type `his <index>` to re-run them.
fn render_history_page(history: &[HistoryItem], page: usize) -> String {
    let total = history.len();
    let pages = page_count(total, DISP_HIST_LEN);
    let page = page.min(pages - 1);

    let mut msg = String::from(
        "\nHistory  (← older page  → newer page).  Use `his <number>` to run; e.g. `his 4`",
    );
    msg.push_str(&format!(".  Page {}/{}:\n", page + 1, pages));
    msg.push_str(DIVIDER);
    msg.push('\n');

    if total == 0 {
        msg.push_str("(no history)\n");
    } else {
        // page 0 covers the last DISP_HIST_LEN items; page 1 the previous
        // DISP_HIST_LEN before that; etc. Newest sits at the bottom.
        let end = total - page * DISP_HIST_LEN;
        let start = end.saturating_sub(DISP_HIST_LEN);
        for i in start..end {
            msg.push_str(&format!("{i}:  {}\n", history[i].text));
        }
    }
    msg.push_str(DIVIDER);
    msg.push_str("\n\n");
    msg
}

/// Rustyline key handler: prints a page of the command history. Ctrl+H
/// always resets to the most recent page and re-enables arrow-key paging.
struct ListHistoryHandler {
    history: Arc<Mutex<Vec<HistoryItem>>>,
    nav: Arc<Mutex<HistoryNavState>>,
    printer: SharedPrinter,
}

impl ConditionalEventHandler for ListHistoryHandler {
    fn handle(
        &self,
        _evt: &Event,
        _n: RepeatCount,
        _positive: bool,
        _ctx: &EventContext<'_>,
    ) -> Option<Cmd> {
        let msg = if let Ok(mut nav) = self.nav.lock() {
            nav.active = true;
            nav.page = 0;
            self.history
                .lock()
                .ok()
                .map(|h| render_history_page(&h, nav.page))
        } else {
            None
        };
        if let Some(msg) = msg {
            if let Ok(mut p) = self.printer.lock() {
                let _ = p.print(msg);
            }
        }
        Some(Cmd::Noop)
    }
}

/// Rustyline key handler bound to Left/Right when history paging is active.
/// `delta == +1` moves to an older page; `delta == -1` moves to a newer one.
/// Only steals the keystroke when paging is active *and* the input line is
/// empty — otherwise it returns `None` so rustyline does its usual cursor
/// movement.
struct HistoryPageHandler {
    history: Arc<Mutex<Vec<HistoryItem>>>,
    nav: Arc<Mutex<HistoryNavState>>,
    printer: SharedPrinter,
    delta: i32,
}

impl ConditionalEventHandler for HistoryPageHandler {
    fn handle(
        &self,
        _evt: &Event,
        _n: RepeatCount,
        _positive: bool,
        ctx: &EventContext<'_>,
    ) -> Option<Cmd> {
        if !ctx.line().is_empty() {
            return None;
        }

        let (active, _) = match self.nav.lock() {
            Ok(n) => (n.active, n.page),
            Err(_) => return None,
        };
        if !active {
            return None;
        }

        let msg = {
            let hist = self.history.lock().ok()?;
            let mut nav = self.nav.lock().ok()?;
            let pages = page_count(hist.len(), DISP_HIST_LEN);
            let new_page = if self.delta > 0 {
                (nav.page + 1).min(pages - 1)
            } else {
                nav.page.saturating_sub(1)
            };
            if new_page == nav.page {
                // Already at the edge — consume the key but skip the redraw.
                return Some(Cmd::Noop);
            }
            nav.page = new_page;
            render_history_page(&hist, nav.page)
        };
        if let Ok(mut p) = self.printer.lock() {
            let _ = p.print(msg);
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

    if let Ok(mut hist) = state.history.lock() {
        hist.push(HistoryItem {
            text: input.to_string(),
            dt: Utc::now(),
        });
    }

    // Track directories we've run real commands from (everything except `cd`),
    // so Ctrl+R / `cd <number>` can jump back to them.
    if cmd != "cd" {
        let cwd = state.cwd.clone();
        record_recent_dir(&state.recent_dirs, &cwd);
        if let Err(e) = state.save(state_path) {
            eprintln!("warning: failed to save recent dirs: {e}");
        }
    }

    match cmd {
        "exit" | "quit" => return false,

        "sync" => {
            let message = args.trim().trim_matches('"');

            if message.is_empty() {
                eprintln!("sync: commit message required, e.g. sync \"my commit message\"");
            } else {
                let steps: [&[&str]; 3] = [&["add", "."], &["commit", "-am", message], &["push"]];

                for step in steps {
                    match Command::new("git").args(step).status() {
                        Ok(status) if !status.success() => {
                            eprintln!("sync: `git {}` failed", step.join(" "));
                            break;
                        }
                        Err(e) => {
                            eprintln!("sync: failed to run git: {e}");
                            break;
                        }
                        _ => {}
                    }
                }
            }
        }

        // On linux, this is likely the same as the system `cat` command, but it works on Windows.
        // Another approach may be to only apply this branch on Windows.
        "cat" => {
            let target = util::path_from_args(state, args);
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
            let target = if let Ok(idx) = args.parse::<usize>() {
                let resolved = state
                    .recent_dirs
                    .lock()
                    .ok()
                    .and_then(|list| list.get(idx).map(|r| r.path.clone()));
                match resolved {
                    Some(p) => Some(p),
                    None => {
                        eprintln!("cd: no recent directory at index {idx}");
                        None
                    }
                }
            } else {
                Some(util::path_from_args(state, args))
            };

            if let Some(target) = target {
                match env::set_current_dir(&target) {
                    Ok(_) => state.cwd = env::current_dir().unwrap_or(target),
                    Err(e) => eprintln!("cd: {e}"),
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
        State::new()
    });

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
            save_path: state_path.clone(),
            printer: printer.clone(),
        })),
    );

    // Alt + B: Display the current bookmark list.
    rl.bind_sequence(
        KeyEvent::new('b', Modifiers::ALT),
        EventHandler::Conditional(Box::new(ListBookmarksHandler {
            bookmarks: state.dir_bookmarks.clone(),
            home: home.clone(),
            printer: printer.clone(),
        })),
    );

    // Ctrl + R: Display the recent-directories list. Overrides rustyline's
    // default reverse-i-search binding, which this shell doesn't use.
    rl.bind_sequence(
        KeyEvent::new('r', Modifiers::CTRL),
        EventHandler::Conditional(Box::new(ListRecentDirsHandler {
            recent_dirs: state.recent_dirs.clone(),
            bookmarks: state.dir_bookmarks.clone(),
            home: home.clone(),
            printer: printer.clone(),
        })),
    );

    // Shared paging state for the history viewer. Ctrl+H sets it active and
    // resets to the most recent page; Left/Right adjust the page while it
    // remains active and the input line is empty.
    let history_nav = Arc::new(Mutex::new(HistoryNavState::new()));

    // Ctrl + H: Display recent history
    rl.bind_sequence(
        KeyEvent::new('h', Modifiers::CTRL),
        EventHandler::Conditional(Box::new(ListHistoryHandler {
            history: state.history.clone(),
            nav: history_nav.clone(),
            printer: printer.clone(),
        })),
    );

    // ← / → : page through history once Ctrl+H has been pressed. The handlers
    // return `None` when the buffer is non-empty or paging isn't active, so
    // normal cursor movement still works the rest of the time.
    rl.bind_sequence(
        KeyEvent(KeyCode::Left, Modifiers::NONE),
        EventHandler::Conditional(Box::new(HistoryPageHandler {
            history: state.history.clone(),
            nav: history_nav.clone(),
            printer: printer.clone(),
            delta: 1,
        })),
    );
    rl.bind_sequence(
        KeyEvent(KeyCode::Right, Modifiers::NONE),
        EventHandler::Conditional(Box::new(HistoryPageHandler {
            history: state.history.clone(),
            nav: history_nav.clone(),
            printer: printer.clone(),
            delta: -1,
        })),
    );

    loop {
        match rl.readline(&state.prompt()) {
            Ok(line) => {
                if !line.trim().is_empty() {
                    let _ = rl.add_history_entry(&line);
                }
                if !run_command(&mut state, &state_path, &line) {
                    break;
                }
            }
            Err(ReadlineError::Interrupted) => println!("^C"), // Ctrl+C clears line
            Err(ReadlineError::Eof) => break,                  // Ctrl+D exits
            Err(e) => {
                eprintln!("Error: {e}");
                break;
            }
        }
    }
}
