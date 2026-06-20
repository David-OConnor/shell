use std::{
    env, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use chrono::{DateTime, Utc};

use crate::{branch_indicator, current_branch, get_home, read_browser_files, save_data};

// todo: Instead of storing these Arc<Mutex>>s, perhaps we do it some other way; this is due
// todo: due to how Rustyline expects it.
pub struct State {
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
    /// GUI panel-visibility settings. The CLI doesn't read these — they're
    /// here purely so a CLI-side save round-trips the GUI's layout choice
    /// instead of clobbering it back to defaults.
    pub panel_vis: PanelVis,
    /// Last GUI window size. Like `panel_vis`, the CLI never reads or mutates
    /// this — it's held only so a CLI-side save writes the GUI's value back
    /// unchanged instead of dropping the `WINDOW_SIZE` line.
    pub window_size: Option<WindowSize>,
    /// GUI open-tab layout. Like `panel_vis` / `window_size`, the CLI never
    /// reads or mutates this — it's held only so a CLI-side save round-trips
    /// the GUI's tabs back unchanged instead of dropping them.
    pub open_tabs: OpenTabs,
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
            panel_vis: PanelVis::default(),
            window_size: None,
            open_tabs: OpenTabs::default(),
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
    pub(crate) fn prompt(&self, nav: &NavState) -> String {
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
    pub(crate) fn refresh_branch(&mut self) {
        self.branch = current_branch(&self.cwd);
    }

    /// Re-read the directory listing for `cwd` into `browser_files`. The CLI
    /// itself doesn't expose this listing yet, but the GUI uses the same
    /// shared helper, so we keep the field populated for parity (and for
    /// any future CLI-side use). Called after every successful directory
    /// change (cd / bm / hist-recall) and at startup.
    pub(crate) fn refresh_browser_files(&self) {
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
    pub(crate) fn save(&self, path: &Path) -> io::Result<()> {
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

        save_data::save_state(
            &bookmarks,
            &recent,
            &history,
            &remote_terminals,
            &self.panel_vis,
            self.window_size,
            &self.open_tabs,
            path,
        )
    }

    /// Restore state from disk, returning a fresh `State` with that data.
    /// A missing file is treated as "no saved state" and yields the default
    /// `State::new()` values (not an error).
    pub(crate) fn load(path: &Path) -> io::Result<Self> {
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
            panel_vis: loaded.panel_vis,
            window_size: loaded.window_size,
            open_tabs: loaded.open_tabs,
            branch,
        })
    }
}

pub struct HistoryItem {
    pub text: String,
    pub dir: PathBuf,
    pub dt: DateTime<Utc>,
}

/// One entry in the recent-directories list: a path we ran a command from,
/// plus the time we last visited it. Lives in `save_data` so the persistence
/// format owns its row type; both the CLI and GUI binaries re-use it.
pub struct RecentDir {
    pub path: PathBuf,
    pub dt: DateTime<Utc>,
}

/// todo: Determine if you want this, or can use PathBuf directly. This may have
/// todo advantages for cacheing data for the GUI.
pub struct BrowserFile {
    pub path: PathBuf,
    pub disp_name: String,
    pub is_folder: bool,
    pub is_executable: bool,
}

/// E.g. for SSH
pub struct RemoteTerminal {
    pub host: String,
    pub port: u16, // todo: A/R
    pub username: String,
    pub password: String, // todo: Determine how to handle this
}

/// Which optional side panels in the GUI are currently visible. Lives in
/// the shared lib so the CLI can round-trip it through the save file
/// without understanding what each panel does — the user's preferred
/// layout sticks across runs even if CLI and GUI usage are interleaved.
/// Each field is `true` for visible, `false` for hidden.
#[derive(Clone, Copy, Debug)]
pub struct PanelVis {
    pub bookmarks: bool,
    pub recent_dirs: bool,
    pub recent_cmds: bool,
    pub recent_cmds_in_dir: bool,
    pub remote_terminals: bool,
    pub file_browser: bool,
}

impl Default for PanelVis {
    fn default() -> Self {
        Self {
            bookmarks: true,
            recent_dirs: true,
            recent_cmds: true,
            recent_cmds_in_dir: true,
            remote_terminals: false,
            file_browser: true,
        }
    }
}

/// Last GUI window size, in logical points (`x` = width, `y` = height).
/// Persisted so the window reopens at the size the user left it. The CLI
/// has no window and never reads this — like [PanelVis] it lives here only
/// so a CLI-side save round-trips the GUI's value instead of dropping it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WindowSize {
    pub x: f32,
    pub y: f32,
}

/// GUI tab layout persisted across runs: the working directory of each open
/// tab (in tab order) plus the index of the one that was active. Persisted so
/// reopening the GUI restores the same set of tabs the user left open. Like
/// [PanelVis] / [WindowSize] this lives in the shared lib only so a CLI-side
/// save round-trips the GUI's value instead of dropping it; the CLI has no
/// tabs of its own. An empty `paths` means "nothing saved" — the GUI falls
/// back to a single tab at the current directory.
#[derive(Clone, Debug, Default)]
pub struct OpenTabs {
    pub paths: Vec<PathBuf>,
    pub active: usize,
}

/// Arrow-key recall state shared by the CLI and GUI. Tracks two independent
/// axes that both load text into the input box:
///   * `his_cursor` — Up/Down walks `state.history` (all dirs).
///   * `cd_cursor` — Left/Right walks `state.recent_dirs`.
///
/// `cursor = None` on a given axis means the user is at their live input
/// for that axis; `Some(i)` means the input row is currently showing the
/// recall for `[i]`. The two axes are mutually exclusive — stepping one
/// resets the other so only one indicator is shown at a time.
///
/// `draft` is the user's in-progress input, snapshotted the first time any
/// axis becomes active. It's restored when the user walks back past the
/// newest entry on whichever axis is currently active.
pub struct NavState {
    pub his_cursor: Option<usize>,
    pub cd_cursor: Option<usize>,
    pub draft: String,
    pub draft_set: bool,
}

impl NavState {
    pub fn new() -> Self {
        Self {
            his_cursor: None,
            cd_cursor: None,
            draft: String::new(),
            draft_set: false,
        }
    }

    pub fn reset(&mut self) {
        self.his_cursor = None;
        self.cd_cursor = None;
        self.draft.clear();
        self.draft_set = false;
    }

    /// `true` when *any* recall axis is currently active.
    pub fn is_active(&self) -> bool {
        self.his_cursor.is_some() || self.cd_cursor.is_some()
    }

    /// Snapshot the live input as the draft the first time recall starts.
    fn ensure_draft(&mut self, live_input: &str) {
        if !self.draft_set {
            self.draft = live_input.to_string();
            self.draft_set = true;
        }
    }

    /// Pop the draft when the cursor walks past the newest entry.
    fn pop_draft(&mut self) -> String {
        self.draft_set = false;
        std::mem::take(&mut self.draft)
    }

    /// Step the history (his) axis in response to an Up (`up = true`) or
    /// Down arrow. `live_input` is the current input buffer.
    ///
    /// Returns the text the input box should now show, or `None` when the
    /// step is a no-op (Down with nothing recalled, Up at the oldest entry,
    /// empty history).
    pub fn step_his(
        &mut self,
        history: &[HistoryItem],
        up: bool,
        live_input: &str,
    ) -> Option<String> {
        let len = history.len();
        if len == 0 {
            return None;
        }
        let new_cursor: Option<usize> = match (self.his_cursor, up) {
            (None, true) => {
                self.ensure_draft(live_input);
                self.cd_cursor = None;
                Some(len - 1)
            }
            (None, false) => return None,
            (Some(0), true) => return None,
            (Some(c), true) => Some(c - 1),
            (Some(c), false) => {
                if c + 1 < len {
                    Some(c + 1)
                } else {
                    None
                }
            }
        };
        let text = match new_cursor {
            Some(c) => history[c].text.clone(),
            None => self.pop_draft(),
        };
        self.his_cursor = new_cursor;
        Some(text)
    }

    /// Step the recent-dirs (cd) axis in response to a Left (`left = true`)
    /// or Right arrow. `render_buffer` formats the buffer text for a chosen
    /// entry — typically `cd <tilde-rendered path>` so pressing Enter goes
    /// there. `live_input` is the current input buffer.
    ///
    /// Returns the text the input box should now show, or `None` when the
    /// step is a no-op.
    pub fn step_cd<F>(
        &mut self,
        recent: &[RecentDir],
        left: bool,
        live_input: &str,
        render_buffer: F,
    ) -> Option<String>
    where
        F: FnOnce(&Path) -> String,
    {
        let len = recent.len();
        if len == 0 {
            return None;
        }
        let new_cursor: Option<usize> = match (self.cd_cursor, left) {
            (None, true) => {
                self.ensure_draft(live_input);
                self.his_cursor = None;
                Some(len - 1)
            }
            (None, false) => return None,
            (Some(0), true) => return None,
            (Some(c), true) => Some(c - 1),
            (Some(c), false) => {
                if c + 1 < len {
                    Some(c + 1)
                } else {
                    None
                }
            }
        };
        let text = match new_cursor {
            Some(c) => render_buffer(&recent[c].path),
            None => self.pop_draft(),
        };
        self.cd_cursor = new_cursor;
        Some(text)
    }

    /// Render the his-indicator slot for a prompt: empty when no his recall
    /// is active, or ` his N` (with a leading space) so it sits cleanly
    /// between the cwd and `$`.
    pub fn his_indicator(&self) -> String {
        nav_indicator("his", self.his_cursor)
    }

    /// Render the cd-indicator slot for a prompt: empty when no cd recall
    /// is active, or ` cd N` with a leading space.
    pub fn cd_indicator(&self) -> String {
        nav_indicator("cd", self.cd_cursor)
    }
}

impl Default for NavState {
    fn default() -> Self {
        Self::new()
    }
}

/// Build a ` <prefix> N` indicator (with leading space) or empty string.
/// Used by both axes; see [NavState::his_indicator] / [NavState::cd_indicator].
pub fn nav_indicator(prefix: &str, cursor: Option<usize>) -> String {
    match cursor {
        Some(i) => format!(" {prefix} {i}"),
        None => String::new(),
    }
}
