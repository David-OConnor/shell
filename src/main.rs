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
    EventHandler, ExternalPrinter, Helper, KeyEvent, Modifiers, RepeatCount,
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

struct HistoryItem {
    pub text: String,
    pub dt: DateTime<Utc>,
}

struct State {
    /// Cached.
    pub home: Option<PathBuf>,
    pub history: Vec<HistoryItem>,
    /// This initializes to env::current_dir, but is then managed from within
    /// this program.
    pub cwd: PathBuf,
    /// User-controlled list of directory bookmarks that can be easily
    /// navigated to. Shared with the readline key handler (Ctrl+B), which
    /// is why it lives behind an Arc<Mutex<_>>.
    pub dir_bookmarks: Arc<Mutex<Vec<PathBuf>>>,
}

impl State {
    // todo: Change this to Default::default A/R, as there are no params.
    fn new() -> Self {
        Self {
            home: util::get_home(),
            history: Vec::new(),
            cwd: env::current_dir().unwrap_or_default(),
            dir_bookmarks: Arc::new(Mutex::new(Vec::new())),
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

    /// Persist user-controlled state (currently: the bookmark list) to the
    /// given file. Called after every bookmark mutation.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let list = self
            .dir_bookmarks
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "bookmark lock poisoned"))?;
        save_data::save_bookmarks(&list, path)
    }

    /// Restore state from disk, returning a fresh `State` with that data.
    /// A missing file is treated as "no saved state" and yields the default
    /// `State::new()` values (not an error).
    pub fn load(path: &Path) -> io::Result<Self> {
        let bookmarks = save_data::load_bookmarks(path)?;


        Ok(Self {
            home: util::get_home(),
            history: Vec::new(),
            cwd: env::current_dir().unwrap_or_default(),
            dir_bookmarks: Arc::new(Mutex::new(bookmarks)),
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

    /// Render the user's in-progress input in light blue.
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        if line.is_empty() {
            Cow::Borrowed(line)
        } else {
            Cow::Owned(format!("{COLOR_CYAN}{line}{COLOR_RESET}"))
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
                    if let Err(e) = save_data::save_bookmarks(&list, &self.save_path) {
                        eprintln!("warning: failed to save bookmarks: {e}");
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

/// Rustyline key handler for Ctrl+Shift+B: prints the current bookmark list.
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
            const DIVIDER: &str = "----------";

            let mut msg = String::from("\nBookmarks. Use `del bm <number>` to delete; e.g. `del bm 4`:\n");
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

/// Runs one command line. Returns false if the shell should exit.
fn run_command(state: &mut State, state_path: &Path, input: &str) -> bool {
    let input = input.trim();
    if input.is_empty() {
        return true;
    }

    state.history.push(HistoryItem {
        text: input.to_string(),
        dt: Utc::now(),
    });

    // Split into command + remainder for built-in dispatch.
    let (cmd, args) = match input.find(char::is_whitespace) {
        Some(i) => (&input[..i], input[i..].trim()),
        None => (input, ""),
    };

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
            let target = util::path_from_args(state, args);

            match env::set_current_dir(&target) {
                Ok(_) => state.cwd = env::current_dir().unwrap_or(target),
                Err(e) => eprintln!("cd: {e}"),
            }
        }

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
    let state_path = save_data::default_path()
        .unwrap_or_else(|| PathBuf::from(save_data::FILENAME));
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

    // Ctrl+B:  push the CWD onto state.dir_bookmarks (and persist to disk).
    rl.bind_sequence(
        KeyEvent::new('b', Modifiers::CTRL),
        EventHandler::Conditional(Box::new(BookmarkHandler {
            bookmarks: state.dir_bookmarks.clone(),
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
