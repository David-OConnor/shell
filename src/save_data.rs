//! Persistent application state. Currently just the user's bookmark list,
//! but the file format is line-based and tagged so we can add more record
//! types later without breaking existing files.
//!
//! Format:
//!   # comments and blank lines are ignored
//!   BOOKMARK <absolute path>
//!
//! Unknown record types are silently skipped on load so older builds reading
//! a file written by a newer build don't choke.

use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

pub const FILENAME: &str = "shell_state.ss";

const BOOKMARK_TAG: &str = "BOOKMARK ";

/// Where the state file lives by default: `<home>/shell_state.ss`. Falls back
/// to `None` if neither `USERPROFILE` nor `HOME` is set (rare).
pub fn default_path() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(|h| PathBuf::from(h).join(FILENAME))
}

/// Overwrite the state file with the given bookmark list.
/// Creates parent directories as needed.
pub fn save_bookmarks(bookmarks: &[PathBuf], path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    let mut f = fs::File::create(path)?;
    writeln!(f, "# Shell state — auto-generated. Do not edit while shell is running.")?;
    for bm in bookmarks {
        writeln!(f, "{BOOKMARK_TAG}{}", bm.display())?;
    }
    Ok(())
}

/// Read bookmarks from the state file. A missing file is not an error — it
/// just means no bookmarks yet, so we return an empty vec.
pub fn load_bookmarks(path: &Path) -> io::Result<Vec<PathBuf>> {
    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix(BOOKMARK_TAG) {
            out.push(PathBuf::from(rest));
        }
        // Unknown tags are ignored on purpose — forward compatibility.
    }
    Ok(out)
}
