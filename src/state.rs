use std::{
    env, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use chrono::{DateTime, Utc};

use crate::{
    current_branch, get_home, git::branch_indicator, read_browser_files, save_data,
    ssh::RemoteSession,
};

// todo: Instead of storing these Arc<Mutex>>s, perhaps we do it some other way; this is due
// todo: due to how Rustyline expects it.
pub struct State {
    /// Cached. Read by `commands` (path resolution) but not by the binary
    /// frontends, so crate-private.
    pub(crate) home: Option<PathBuf>,
    /// Shared with the Ctrl+H / arrow-key handlers, which render pages of
    /// recent commands without holding `State`.
    pub history: Arc<Mutex<Vec<HistoryItem>>>,
    /// This initializes to env::current_dir, but is then managed from within
    /// this program. Mutated by `commands` (cd/bm), so crate-private rather
    /// than fully public.
    pub(crate) cwd: PathBuf,
    /// User-controlled list of directory bookmarks that can be easily
    /// navigated to. Shared with the readline key handler (Ctrl+B), which
    /// is why it lives behind an Arc<Mutex<_>>.
    pub dir_bookmarks: Arc<Mutex<Vec<PathBuf>>>,
    /// Paths we've execute commands from. Works in a similar way to bookmarks.
    pub recent_dirs: Arc<Mutex<Vec<RecentDir>>>,
    pub remote_terminals: Arc<Mutex<Vec<RemoteTerminal>>>,
    /// In the current dir. Note persistent, unlike some of our other lists.
    /// Currently unused in this application; TBD. (The GUI keeps its own copy
    /// on its own `State`.) Only `refresh_browser_files` writes it, so it's
    /// private to this module.
    browser_files: Arc<Mutex<Vec<BrowserFile>>>,
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
    /// whenever the user has had a chance to mutate repo state. Only written
    /// by `refresh_branch` and read by `prompt`, both here, so it's private.
    branch: Option<String>,
    /// The live SSH session, when the user has run `ssh`. While `Some`, typed
    /// commands are routed to the remote instead of the local shell, and the
    /// prompt shows `user@host`. Not persisted (a connection can't outlive the
    /// process) and not behind a lock — only `commands::run_command` touches
    /// it, so it's crate-private.
    pub(crate) active_remote: Option<RemoteSession>,
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
            active_remote: None,
        }
    }
}

impl State {
    /// This defines what the general prompt looks like. Its adorning
    /// characters let the user know they're in this shell. `nav` carries
    /// the active recall cursors (see [NavState]); when either is `Some`,
    /// the prompt grows by ` his N` or ` cd N` before the `$` to indicate
    /// which item is currently loaded into the input.
    pub fn prompt(&self, nav: &NavState) -> String {
        // When connected to a remote, the prompt reflects the SSH session
        // (user@host + the tracked remote cwd) so the user always knows their
        // commands are running elsewhere.
        if let Some(remote) = &self.active_remote {
            let cwd = if remote.cwd().is_empty() {
                "~"
            } else {
                remote.cwd()
            };
            let mode = match remote.mode() {
                crate::SshMode::Exec => "",
                crate::SshMode::Pty => " (pty)",
            };
            return format!(
                "S [{}:{}]{}{}{} $ ",
                remote.label(),
                cwd,
                mode,
                nav.his_indicator(),
                nav.cd_indicator(),
            );
        }

        // Mark the directory with a leading `*` when it's bookmarked.
        let bookmarked = self
            .dir_bookmarks
            .lock()
            .map(|list| list.contains(&self.cwd))
            .unwrap_or(false);
        let star = if bookmarked { "*" } else { "" };
        format!(
            "S {star}{}{}{}{}{} $ ",
            self.cwd.display(),
            branch_indicator(self.branch.as_deref()),
            crate::python::venv_indicator(&self.cwd),
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
    /// avoid lock-order deadlocks. Only `commands::run_command` calls this
    /// (the binary persists via `save_data::save_state` directly), so it's
    /// crate-private.
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
            panel_vis: loaded.panel_vis,
            window_size: loaded.window_size,
            open_tabs: loaded.open_tabs,
            branch,
            active_remote: None,
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

/// A saved SSH remote. The password is *not* stored here — it lives in the OS
/// keyring (see `crate::secrets`), keyed by `username@host:port`. This struct
/// (and the on-disk state file) only ever holds the non-secret connection
/// details, so the plaintext password problem is gone.
pub struct RemoteTerminal {
    pub host: String,
    pub port: u16,
    pub username: String,
}

/// Which optional side panels in the GUI are currently visible. This lives in
/// the shared lib so the CLI can round-trip it through the save file
/// without understanding what each panel does.
///
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
    /// In-progress input snapshot, managed through `step_his`/`step_cd` and
    /// `reset`. Public because `shell_gui` constructs `NavState` with a struct
    /// literal (`..NavState::new()`), which requires every field be visible.
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

    /// Step the history axis in response to an Up or Down arrow key press.
    /// `live_input` is the current input buffer.
    ///
    /// Fish-style prefix search: whatever the user had typed when recall
    /// started (the snapshotted `draft`) is treated as a prefix, and only
    /// history entries beginning with it are walked. An empty draft matches
    /// every entry, preserving the original "walk all history" behaviour.
    /// `his_cursor` still stores the absolute index into `history` so the
    /// ` his N` prompt indicator lines up with `his N` / `hisd N`.
    ///
    /// Returns the text the input box should now show, or `None` when the
    /// step is a no-op (Down with nothing recalled, Up at the oldest match,
    /// empty history, or no entry matches the prefix).
    pub fn step_his(
        &mut self,
        history: &[HistoryItem],
        up: bool,
        live_input: &str,
    ) -> Option<String> {
        if history.is_empty() {
            return None;
        }

        // The search prefix: the stored draft once recall is active, else the
        // current live input (captured as the draft on the first step below).
        let prefix = if self.draft_set {
            self.draft.clone()
        } else {
            live_input.to_string()
        };

        // Absolute indices of matching entries, oldest → newest.
        let matches: Vec<usize> = history
            .iter()
            .enumerate()
            .filter(|(_, item)| prefix.is_empty() || item.text.starts_with(&prefix))
            .map(|(i, _)| i)
            .collect();
        if matches.is_empty() {
            return None;
        }

        // Where the current cursor sits within `matches` (None ⇒ at the draft).
        let cur_pos = self
            .his_cursor
            .and_then(|abs| matches.iter().position(|&i| i == abs));

        let new_pos: Option<usize> = match (cur_pos, up) {
            (None, true) => {
                self.ensure_draft(live_input);
                self.cd_cursor = None;
                Some(matches.len() - 1)
            }
            (None, false) => return None,
            (Some(0), true) => return None,
            (Some(p), true) => Some(p - 1),
            (Some(p), false) => {
                if p + 1 < matches.len() {
                    Some(p + 1)
                } else {
                    None
                }
            }
        };

        let text = match new_pos {
            Some(p) => {
                let abs = matches[p];
                self.his_cursor = Some(abs);
                history[abs].text.clone()
            }
            None => {
                self.his_cursor = None;
                self.pop_draft()
            }
        };

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

/// Record `cwd` in the recent-dirs list. If the path is already present we
/// remove the old entry and push a fresh one to the end, so the list stays
/// deduped and the newest entry sits at the bottom of the display. Shared by
/// the CLI (`commands::run_command`) and the readline key handlers, hence the
/// `Arc<Mutex<_>>` handle rather than a plain `&mut Vec`.
pub fn record_recent_dir(recent: &Arc<Mutex<Vec<RecentDir>>>, cwd: &Path) {
    if let Ok(mut list) = recent.lock() {
        list.retain(|r| r.path != cwd);
        list.push(RecentDir {
            path: cwd.to_path_buf(),
            dt: Utc::now(),
        });
    }
}

/// Build a ` <prefix> N` indicator (with leading space) or empty string.
/// Used by both axes; see [NavState::his_indicator] / [NavState::cd_indicator].
/// Internal helper for those two methods, so it's module-private.
fn nav_indicator(prefix: &str, cursor: Option<usize>) -> String {
    match cursor {
        Some(i) => format!(" {prefix} {i}"),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hist(texts: &[&str]) -> Vec<HistoryItem> {
        texts
            .iter()
            .map(|t| HistoryItem {
                text: (*t).to_string(),
                dir: PathBuf::new(),
                dt: Utc::now(),
            })
            .collect()
    }

    // An empty draft walks the entire history, newest first — the original
    // pre-prefix-search behaviour.
    #[test]
    fn step_his_empty_prefix_walks_all() {
        let h = hist(&["one", "two", "three"]);
        let mut nav = NavState::new();
        assert_eq!(nav.step_his(&h, true, "").as_deref(), Some("three"));
        assert_eq!(nav.step_his(&h, true, "").as_deref(), Some("two"));
        assert_eq!(nav.step_his(&h, true, "").as_deref(), Some("one"));
        // At the oldest entry, Up is a no-op.
        assert_eq!(nav.step_his(&h, true, ""), None);
        assert_eq!(nav.his_cursor, Some(0));
    }

    // With a typed prefix, only matching entries are walked, and the absolute
    // `his_cursor` index points at the matched entry (for the ` his N` prompt).
    #[test]
    fn step_his_prefix_filters() {
        let h = hist(&["git status", "cargo build", "git commit", "ls"]);
        let mut nav = NavState::new();
        // First Up snapshots "git" as the prefix and jumps to the newest match.
        assert_eq!(nav.step_his(&h, true, "git").as_deref(), Some("git commit"));
        assert_eq!(nav.his_cursor, Some(2));
        assert_eq!(nav.step_his(&h, true, "git").as_deref(), Some("git status"));
        assert_eq!(nav.his_cursor, Some(0));
        // No older "git" match — no-op.
        assert_eq!(nav.step_his(&h, true, "git"), None);
    }

    // Walking back down past the newest match restores the user's draft.
    #[test]
    fn step_his_down_restores_draft() {
        let h = hist(&["git status", "git commit"]);
        let mut nav = NavState::new();
        assert_eq!(
            nav.step_his(&h, true, "git ").as_deref(),
            Some("git commit")
        );
        // Down past the newest match yields the original in-progress draft.
        assert_eq!(
            nav.step_his(&h, false, "git commit").as_deref(),
            Some("git ")
        );
        assert_eq!(nav.his_cursor, None);
    }

    // A prefix matching nothing is a no-op and doesn't start recall.
    #[test]
    fn step_his_no_match_is_noop() {
        let h = hist(&["git status"]);
        let mut nav = NavState::new();
        assert_eq!(nav.step_his(&h, true, "zzz"), None);
        assert_eq!(nav.his_cursor, None);
    }
}
