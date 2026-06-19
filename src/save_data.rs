//! Persistent application state. Currently the user's bookmark list plus
//! the recent-directories list, command history, and saved remote
//! terminals, but the file format is line-based and tagged so we can add
//! more record types later without breaking existing files.
//!
//! Format:
//!   # comments and blank lines are ignored
//!   BOOKMARK <absolute path>
//!   RECENT_DIR <rfc3339 timestamp> <absolute path>
//!   HISTORY <rfc3339 timestamp>\t<absolute path>\t<command text>
//!   REMOTE_TERMINAL <host>\t<port>\t<username>\t<password>
//!   PANEL_VIS <key>=<0|1> <key>=<0|1> ...
//!   WINDOW_SIZE x=<width> y=<height>
//!   OPEN_TAB <absolute path>
//!   ACTIVE_TAB <index>
//!
//! HISTORY and REMOTE_TERMINAL use TAB as a field separator (rather than
//! space like RECENT_DIR) because the trailing fields can contain spaces.
//! Newlines in the command text are flattened to spaces on save so each
//! entry stays on one line.
//!
//! Unknown record types are silently skipped on load so older builds reading
//! a file written by a newer build don't choke.
//!
//! Both the CLI and GUI crates call into this module with plain slices /
//! `Vec`s — neither one re-implements the parsing or formatting. The CLI
//! locks its `Arc<Mutex<_>>`s before calling in; the GUI passes its owned
//! `Vec`s directly.

use std::{
    fs,
    io::{self, BufRead, BufReader, Write},
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};

use crate::state::{HistoryItem, OpenTabs, PanelVis, RecentDir, RemoteTerminal, WindowSize};

pub const FILENAME: &str = "shell_state.ss";

const BOOKMARK_TAG: &str = "BOOKMARK ";
const RECENT_DIR_TAG: &str = "RECENT_DIR ";
const HISTORY_TAG: &str = "HISTORY ";
const REMOTE_TERMINAL_TAG: &str = "REMOTE_TERMINAL ";
const PANEL_VIS_TAG: &str = "PANEL_VIS ";
const WINDOW_SIZE_TAG: &str = "WINDOW_SIZE ";
const OPEN_TAB_TAG: &str = "OPEN_TAB ";
const ACTIVE_TAB_TAG: &str = "ACTIVE_TAB ";

/// Bundle of everything `load_state` returns. Lets callers destructure
/// in one step and lets us grow the format without churning every call
/// site.
pub struct LoadedState {
    pub bookmarks: Vec<PathBuf>,
    pub recent_dirs: Vec<RecentDir>,
    pub history: Vec<HistoryItem>,
    pub remote_terminals: Vec<RemoteTerminal>,
    pub panel_vis: PanelVis,
    /// `None` when the file has no `WINDOW_SIZE` line (e.g. it predates the
    /// feature, or was last written by a CLI-only session that never had a
    /// size to record). The GUI falls back to its default size in that case.
    pub window_size: Option<WindowSize>,
    /// The GUI tabs that were open when the file was last written. `paths`
    /// is empty when the file has no `OPEN_TAB` lines (predates the feature
    /// or a CLI-only file), in which case the GUI opens a single default tab.
    pub open_tabs: OpenTabs,
}

/// Where the state file lives by default: `<home>/shell_state.ss`. Falls back
/// to `None` if neither `USERPROFILE` nor `HOME` is set (rare).
pub fn default_path() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(|h| PathBuf::from(h).join(FILENAME))
}

/// Overwrite the state file with the given bookmark, recent-dir, history,
/// and remote-terminal lists. Creates parent directories as needed.
pub fn save_state(
    bookmarks: &[PathBuf],
    recent_dirs: &[RecentDir],
    history: &[HistoryItem],
    remote_terminals: &[RemoteTerminal],
    panel_vis: &PanelVis,
    window_size: Option<WindowSize>,
    open_tabs: &OpenTabs,
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
        let text = h.text.replace(['\r', '\n'], " ").replace('\t', " ");
        writeln!(
            f,
            "{HISTORY_TAG}{}\t{}\t{}",
            h.dt.to_rfc3339(),
            h.dir.display(),
            text
        )?;
    }

    // PanelVis: one line, space-separated `key=0|1` pairs. Stable order
    // so the file diff is reproducible. Forward-compatible: unknown keys
    // are skipped on load; missing keys keep their default value.
    writeln!(
        f,
        "{PANEL_VIS_TAG}bookmarks={} recent_dirs={} recent_cmds={} recent_cmds_in_dir={} remote_terminals={} file_browser={}",
        bool_to_int(panel_vis.bookmarks),
        bool_to_int(panel_vis.recent_dirs),
        bool_to_int(panel_vis.recent_cmds),
        bool_to_int(panel_vis.recent_cmds_in_dir),
        bool_to_int(panel_vis.remote_terminals),
        bool_to_int(panel_vis.file_browser),
    )?;

    // WindowSize: GUI-only. Written only when present so a CLI-side save
    // (which passes `None`) preserves whatever the GUI last recorded rather
    // than clobbering or zeroing it.
    if let Some(ws) = window_size {
        writeln!(f, "{WINDOW_SIZE_TAG}x={} y={}", ws.x, ws.y)?;
    }

    // OpenTabs: GUI-only. One `OPEN_TAB <path>` line per tab in tab order,
    // followed by a single `ACTIVE_TAB <index>`. Written only when there's at
    // least one tab so a CLI-side save (which round-trips an empty `OpenTabs`)
    // doesn't emit a meaningless `ACTIVE_TAB 0` with no tabs.
    if !open_tabs.paths.is_empty() {
        for path in &open_tabs.paths {
            writeln!(f, "{OPEN_TAB_TAG}{}", path.display())?;
        }
        writeln!(f, "{ACTIVE_TAB_TAG}{}", open_tabs.active)?;
    }

    for rt in remote_terminals {
        // todo: Storing the password in cleartext alongside the rest of
        // todo: the state file is obviously not OK long-term — revisit
        // todo: once we settle on an OS keyring / encryption approach.
        let host = sanitize_field(&rt.host);
        let username = sanitize_field(&rt.username);
        let password = sanitize_field(&rt.password);
        writeln!(
            f,
            "{REMOTE_TERMINAL_TAG}{}\t{}\t{}\t{}",
            host, rt.port, username, password
        )?;
    }

    Ok(())
}

/// Flatten characters that would break the line/tab-based record format.
fn sanitize_field(s: &str) -> String {
    s.replace(['\r', '\n', '\t'], " ")
}

fn bool_to_int(b: bool) -> u8 {
    if b { 1 } else { 0 }
}

/// Read the persistent state. A missing file is not an error — it just
/// means no saved state yet, so we return empty vecs.
pub fn load_state(path: &Path) -> io::Result<LoadedState> {
    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Ok(LoadedState {
                bookmarks: Vec::new(),
                recent_dirs: Vec::new(),
                history: Vec::new(),
                remote_terminals: Vec::new(),
                panel_vis: PanelVis::default(),
                window_size: None,
                open_tabs: OpenTabs::default(),
            });
        }
        Err(e) => return Err(e),
    };

    let mut bookmarks = Vec::new();
    let mut recent_dirs = Vec::new();
    let mut history = Vec::new();
    let mut remote_terminals = Vec::new();
    let mut panel_vis = PanelVis::default();
    let mut window_size = None;
    let mut open_tabs = OpenTabs::default();

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
        if let Some(rest) = trimmed.strip_prefix(PANEL_VIS_TAG) {
            // Parse `key=val key=val ...`. Unknown keys are ignored;
            // missing keys keep their default. A malformed value just
            // leaves that field at its default.
            for pair in rest.trim_end().split_whitespace() {
                let Some((k, v)) = pair.split_once('=') else {
                    continue;
                };
                let on = v == "1" || v.eq_ignore_ascii_case("true");
                match k {
                    "bookmarks" => panel_vis.bookmarks = on,
                    "recent_dirs" => panel_vis.recent_dirs = on,
                    "recent_cmds" => panel_vis.recent_cmds = on,
                    "recent_cmds_in_dir" => panel_vis.recent_cmds_in_dir = on,
                    "remote_terminals" => panel_vis.remote_terminals = on,
                    "file_browser" => panel_vis.file_browser = on,
                    _ => {}
                }
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix(WINDOW_SIZE_TAG) {
            // Parse `x=<width> y=<height>`. Only adopt the line when both
            // dimensions parse as positive, finite numbers — a malformed or
            // degenerate entry leaves `window_size` at `None` so the GUI
            // uses its default rather than opening a zero-size window.
            let (mut x, mut y) = (None, None);
            for pair in rest.trim_end().split_whitespace() {
                let Some((k, v)) = pair.split_once('=') else {
                    continue;
                };
                match k {
                    "x" => x = v.parse::<f32>().ok(),
                    "y" => y = v.parse::<f32>().ok(),
                    _ => {}
                }
            }
            if let (Some(x), Some(y)) = (x, y) {
                if x.is_finite() && y.is_finite() && x > 0.0 && y > 0.0 {
                    window_size = Some(WindowSize { x, y });
                }
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix(OPEN_TAB_TAG) {
            open_tabs.paths.push(PathBuf::from(rest.trim_end()));
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix(ACTIVE_TAB_TAG) {
            // A malformed index just leaves `active` at its default of 0; the
            // GUI clamps it to a valid tab in any case.
            if let Ok(idx) = rest.trim_end().parse::<usize>() {
                open_tabs.active = idx;
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix(REMOTE_TERMINAL_TAG) {
            let rest = rest.trim_end_matches('\r');
            let mut parts = rest.splitn(4, '\t');
            if let (Some(host), Some(port_str), Some(username), Some(password)) =
                (parts.next(), parts.next(), parts.next(), parts.next())
            {
                if let Ok(port) = port_str.parse::<u16>() {
                    remote_terminals.push(RemoteTerminal {
                        host: host.to_string(),
                        port,
                        username: username.to_string(),
                        password: password.to_string(),
                    });
                }
            }
            continue;
        }
        // Unknown tags are ignored on purpose for forward compatibility.
    }
    Ok(LoadedState {
        bookmarks,
        recent_dirs,
        history,
        remote_terminals,
        panel_vis,
        window_size,
        open_tabs,
    })
}
