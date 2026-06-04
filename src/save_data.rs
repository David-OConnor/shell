//! Persistent application state. Currently the user's bookmark list plus
//! the recent-directories list and command history, but the file format is
//! line-based and tagged so we can add more record types later without
//! breaking existing files.
//!
//! Format:
//!   # comments and blank lines are ignored
//!   BOOKMARK <absolute path>
//!   RECENT_DIR <rfc3339 timestamp> <absolute path>
//!   HISTORY <rfc3339 timestamp>\t<absolute path>\t<command text>
//!
//! HISTORY uses TAB as a field separator (rather than space like RECENT_DIR)
//! because the command text can contain spaces. Newlines in the command text
//! are flattened to spaces on save so each entry stays on one line.
//!
//! Unknown record types are silently skipped on load so older builds reading
//! a file written by a newer build don't choke.

use std::{
    fs,
    io::{self, BufRead, BufReader, Write},
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use crate::state::{HistoryItem, RecentDir};

pub const FILENAME: &str = "shell_state.ss";

const BOOKMARK_TAG: &str = "BOOKMARK ";
const RECENT_DIR_TAG: &str = "RECENT_DIR ";
const HISTORY_TAG: &str = "HISTORY ";

/// Where the state file lives by default: `<home>/shell_state.ss`. Falls back
/// to `None` if neither `USERPROFILE` nor `HOME` is set (rare).
pub fn default_path() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(|h| PathBuf::from(h).join(FILENAME))
}

/// Overwrite the state file with the given bookmark, recent-dir, and history
/// lists. Creates parent directories as needed.
pub fn save_state(
    bookmarks: &[PathBuf],
    recent_dirs: &[RecentDir],
    history: &[HistoryItem],
    path: &Path,
) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }

    let mut f = fs::File::create(path)?;
    writeln!(
        f,
        "# Shell state — auto-generated. Do not edit while shell is running."
    )?;

    for bm in bookmarks {
        writeln!(f, "{BOOKMARK_TAG}{}", bm.display())?;
    }
    for r in recent_dirs {
        // "<rfc3339> <path>" — rfc3339 has no spaces, so the path can be the
        // (possibly space-containing) tail.
        writeln!(
            f,
            "{RECENT_DIR_TAG}{} {}",
            r.dt.to_rfc3339(),
            r.path.display()
        )?;
    }
    for h in history {
        // Flatten any newlines so each history entry is one line on disk.
        // (We split on '\t' to recover fields; tabs in user input are rare
        // enough that we just drop them rather than escape.)
        let text = h
            .text
            .replace(['\r', '\n'], " ")
            .replace('\t', " ");
        writeln!(
            f,
            "{HISTORY_TAG}{}\t{}\t{}",
            h.dt.to_rfc3339(),
            h.dir.display(),
            text
        )?;
    }

    Ok(())
}

/// Read the persistent state. A missing file is not an error — it just
/// means no saved state yet, so we return empty vecs.
pub fn load_state(
    path: &Path,
) -> io::Result<(Vec<PathBuf>, Vec<RecentDir>, Vec<HistoryItem>)> {
    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Ok((Vec::new(), Vec::new(), Vec::new()));
        }
        Err(e) => return Err(e),
    };

    let mut bookmarks = Vec::new();
    let mut recent_dirs = Vec::new();
    let mut history = Vec::new();

    for line in BufReader::new(file).lines() {
        let line = line?;
        // Only trim the start: history's command text may legitimately end
        // in whitespace we'd rather preserve. Comments / blank-line checks
        // still work because they're a prefix test.
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix(BOOKMARK_TAG) {
            bookmarks.push(PathBuf::from(rest.trim_end()));
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix(RECENT_DIR_TAG) {
            let rest = rest.trim_end();
            // Split on the first space: token 1 is the rfc3339 dt, the rest
            // is the path (which may itself contain spaces).
            if let Some(space) = rest.find(' ') {
                let (dt_str, path_str) = rest.split_at(space);
                let path_str = path_str.trim_start();
                if let Ok(dt) = DateTime::parse_from_rfc3339(dt_str) {
                    recent_dirs.push(RecentDir {
                        path: PathBuf::from(path_str),
                        dt: dt.with_timezone(&Utc),
                    });
                }
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix(HISTORY_TAG) {
            // Trim only \r (Windows line endings) from the tail — leave any
            // trailing space the user typed alone.
            let rest = rest.trim_end_matches('\r');
            let mut parts = rest.splitn(3, '\t');
            if let (Some(dt_str), Some(dir_str), Some(text)) =
                (parts.next(), parts.next(), parts.next())
            {
                if let Ok(dt) = DateTime::parse_from_rfc3339(dt_str) {
                    history.push(HistoryItem {
                        text: text.to_string(),
                        dir: PathBuf::from(dir_str),
                        dt: dt.with_timezone(&Utc),
                    });
                }
            }
            continue;
        }
        // Unknown tags are ignored on purpose for forward compatibility.
    }
    Ok((bookmarks, recent_dirs, history))
}
