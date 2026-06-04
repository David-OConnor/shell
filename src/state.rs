use std::path::{Path, PathBuf};
use chrono::{DateTime, Utc};

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