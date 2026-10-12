//! Persistent application state. Includes the bookmark list,
//! the recent-directories list, command history, saved remote
//! terminals, and git clone roots. A simple text format; easy to add new
//! functionality without breaking existing files.
//!
//! HIS and REMOTE_TERMINAL use TAB as a field separator (rather than
//! space like REC_DIR) because the trailing fields can contain spaces.
//! Newlines in the command text are flattened to spaces on save so each
//! entry stays on one line.
//!
//! Timestamps are written as second-precision RFC 3339 (e.g.
//! `2026-06-23T13:41:32Z`) to keep rows compact.
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

use chrono::{DateTime, SecondsFormat, Utc};

use crate::state::{
    DirLabelLens, FontSizes, HistoryItem, OpenTabs, PanelVis, RecentDir, RemoteTerminal, WindowSize,
};

pub const FILENAME: &str = "shell_state.ss";

const BOOKMARK_TAG: &str = "BM ";
const RECENT_DIR_TAG: &str = "REC_DIR ";
const HISTORY_TAG: &str = "HIS ";
const REMOTE_TERMINAL_TAG: &str = "REMOTE_TERMINAL ";
const PANEL_VIS_TAG: &str = "PANEL_VIS ";
const WINDOW_SIZE_TAG: &str = "WINDOW_SIZE ";
const OPEN_TAB_TAG: &str = "OPEN_TAB ";
const ACTIVE_TAB_TAG: &str = "ACTIVE_TAB ";
const FONT_SIZE_TAG: &str = "FONT_SIZE ";
const DIR_LABEL_LEN_TAG: &str = "DIR_LABEL_LEN ";
const CLONE_ROOT_TAG: &str = "CLONE_ROOT ";

/// Bundle of everything `load_state` returns. Lets callers destructure
/// in one step and lets us grow the format without churning every call
/// site.
pub struct LoadedState {
    pub bookmarks: Vec<PathBuf>,
    pub recent_dirs: Vec<RecentDir>,
    pub history: Vec<HistoryItem>,
    pub remote_terminals: Vec<RemoteTerminal>,
    /// Roots of repos cloned by full URL, oldest first; see [State::clone_roots].
    ///
    /// [State::clone_roots]: crate::state::State::clone_roots
    pub clone_roots: Vec<String>,
    pub panel_vis: PanelVis,
    /// `None` when the file has no `WINDOW_SIZE` line (e.g. it predates the
    /// feature, or was last written by a CLI-only session that never had a
    /// size to record). The GUI falls back to its default size in that case.
    pub window_size: Option<WindowSize>,
    /// The GUI tabs that were open when the file was last written. `paths`
    /// is empty when the file has no `OPEN_TAB` lines (predates the feature
    /// or a CLI-only file), in which case the GUI opens a single default tab.
    pub open_tabs: OpenTabs,
    /// GUI font sizes, in points. Each is `None` when the `FONT_SIZE` line
    /// lacks it (or there's no such line: it predates the feature, or a
    /// CLI-only save), in which case the GUI falls back to its default size.
    pub font_size: FontSizes,
    /// GUI Bookmarks / Recent dirs path lengths, in characters. Each is `None`
    /// when the `DIR_LABEL_LEN` line lacks it, as with `font_size`.
    pub dir_label_lens: DirLabelLens,
}

/// Where the state file lives by default: `<home>/shell_state.ss`. Falls back
/// to `None` if neither `USERPROFILE` nor `HOME` is set (rare).
pub fn default_path() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(|h| PathBuf::from(h).join(FILENAME))
}

/// Overwrite the state file with the given bookmark, recent-dir, history,
/// remote-terminal, and clone-root lists. Creates parent directories as needed.
pub fn save_state(
    bookmarks: &[PathBuf],
    recent_dirs: &[RecentDir],
    history: &[HistoryItem],
    remote_terminals: &[RemoteTerminal],
    clone_roots: &[String],
    panel_vis: &PanelVis,
    window_size: Option<WindowSize>,
    open_tabs: &OpenTabs,
    font_size: FontSizes,
    dir_label_lens: DirLabelLens,
    path: &Path,
) -> io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
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
        writeln!(
            f,
            "{RECENT_DIR_TAG}{} {}",
            r.dt.to_rfc3339_opts(SecondsFormat::Secs, true),
            r.path.display()
        )?;
    }

    for h in history {
        // Flatten any newlines so each history entry is one line on disk.
        let text = h.text.replace(['\r', '\n'], " ").replace('\t', " ");
        writeln!(
            f,
            "{HISTORY_TAG}{}\t{}\t{}",
            h.dt.to_rfc3339_opts(SecondsFormat::Secs, true),
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

    // GUI-only. Written only when present so a CLI-side save
    // (which passes `None`) preserves whatever the GUI last recorded.
    if let Some(ws) = window_size {
        writeln!(f, "{WINDOW_SIZE_TAG}x={} y={}", ws.x, ws.y)?;
    }

    // GUI-only.
    if !open_tabs.paths.is_empty() {
        for path in &open_tabs.paths {
            writeln!(f, "{OPEN_TAB_TAG}{}", path.display())?;
        }
        writeln!(f, "{ACTIVE_TAB_TAG}{}", open_tabs.active)?;
    }

    // FontSize: GUI-only. `size` is the terminal panes' (CLI) size and `gui`
    // the side panels'. Like WINDOW_SIZE, written only when present, so the
    // CLI's saves (which pass back what they loaded) preserve the GUI's values.
    if font_size.cli.is_some() || font_size.gui.is_some() {
        write!(f, "{FONT_SIZE_TAG}")?;
        let mut pairs = Vec::new();
        if let Some(size) = font_size.cli {
            pairs.push(format!("size={size}"));
        }
        if let Some(size) = font_size.gui {
            pairs.push(format!("gui={size}"));
        }
        writeln!(f, "{}", pairs.join(" "))?;
    }

    // DirLabelLen: GUI-only, written only when present, like FONT_SIZE.
    if dir_label_lens.bookmarks.is_some() || dir_label_lens.recent_dirs.is_some() {
        write!(f, "{DIR_LABEL_LEN_TAG}")?;
        let mut pairs = Vec::new();
        if let Some(len) = dir_label_lens.bookmarks {
            pairs.push(format!("bookmarks={len}"));
        }
        if let Some(len) = dir_label_lens.recent_dirs {
            pairs.push(format!("recent_dirs={len}"));
        }
        writeln!(f, "{}", pairs.join(" "))?;
    }

    for rt in remote_terminals {
        // Only non-secret connection details go on disk; the password lives in
        // the OS keyring (see `crate::secrets`). Format is host\tport\tusername.
        let host = sanitize_field(&rt.host);
        let username = sanitize_field(&rt.username);
        writeln!(
            f,
            "{REMOTE_TERMINAL_TAG}{}\t{}\t{}",
            host, rt.port, username
        )?;
    }

    for root in clone_roots {
        writeln!(f, "{CLONE_ROOT_TAG}{}", sanitize_field(root))?;
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
                clone_roots: Vec::new(),
                panel_vis: PanelVis::default(),
                window_size: None,
                open_tabs: OpenTabs::default(),
                font_size: FontSizes::default(),
                dir_label_lens: DirLabelLens::default(),
            });
        }
        Err(e) => return Err(e),
    };

    let mut bookmarks = Vec::new();
    let mut recent_dirs = Vec::new();
    let mut history = Vec::new();
    let mut remote_terminals = Vec::new();
    let mut clone_roots = Vec::new();
    let mut panel_vis = PanelVis::default();
    let mut window_size = None;
    let mut open_tabs = OpenTabs::default();
    let mut font_size = FontSizes::default();
    let mut dir_label_lens = DirLabelLens::default();

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
                && let Ok(dt) = DateTime::parse_from_rfc3339(dt_str)
            {
                history.push(HistoryItem {
                    text: text.to_string(),
                    dir: PathBuf::from(dir_str),
                    dt: dt.with_timezone(&Utc),
                });
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix(PANEL_VIS_TAG) {
            // Parse `key=val key=val ...`. Unknown keys are ignored;
            // missing keys keep their default. A malformed value just
            // leaves that field at its default.
            for pair in rest.split_whitespace() {
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
            for pair in rest.split_whitespace() {
                let Some((k, v)) = pair.split_once('=') else {
                    continue;
                };
                match k {
                    "x" => x = v.parse::<f32>().ok(),
                    "y" => y = v.parse::<f32>().ok(),
                    _ => {}
                }
            }
            if let (Some(x), Some(y)) = (x, y)
                && x.is_finite()
                && y.is_finite()
                && x > 0.0
                && y > 0.0
            {
                window_size = Some(WindowSize { x, y });
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
        if let Some(rest) = trimmed.strip_prefix(FONT_SIZE_TAG) {
            // Parse `size=<points> gui=<points>`. Only adopt positive, finite
            // values — a malformed or degenerate entry leaves that size at
            // `None` so the GUI uses its default rather than a zero-height font.
            for pair in rest.split_whitespace() {
                let Some((k, v)) = pair.split_once('=') else {
                    continue;
                };
                let Ok(s) = v.parse::<f32>() else {
                    continue;
                };
                if !s.is_finite() || s <= 0.0 {
                    continue;
                }
                match k {
                    "size" => font_size.cli = Some(s),
                    "gui" => font_size.gui = Some(s),
                    _ => {}
                }
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix(DIR_LABEL_LEN_TAG) {
            // Parse `bookmarks=<chars> recent_dirs=<chars>`. A malformed or
            // zero value leaves that length at `None` (the GUI's default).
            for pair in rest.split_whitespace() {
                let Some((k, v)) = pair.split_once('=') else {
                    continue;
                };
                let Ok(len) = v.parse::<usize>() else {
                    continue;
                };
                if len == 0 {
                    continue;
                }
                match k {
                    "bookmarks" => dir_label_lens.bookmarks = Some(len),
                    "recent_dirs" => dir_label_lens.recent_dirs = Some(len),
                    _ => {}
                }
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix(REMOTE_TERMINAL_TAG) {
            let rest = rest.trim_end_matches('\r');

            let mut parts = rest.splitn(4, '\t');
            if let (Some(host), Some(port_str), Some(username)) =
                (parts.next(), parts.next(), parts.next())
                && let Ok(port) = port_str.parse::<u16>()
            {
                remote_terminals.push(RemoteTerminal {
                    host: host.to_string(),
                    port,
                    username: username.to_string(),
                });
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix(CLONE_ROOT_TAG) {
            let root = rest.trim_end();
            if !root.is_empty() {
                clone_roots.push(root.to_string());
            }
            continue;
        }
        // Unknown tags are ignored on purpose for forward compatibility.
    }
    crate::state::dedup_history(&mut history);

    Ok(LoadedState {
        bookmarks,
        recent_dirs,
        history,
        remote_terminals,
        clone_roots,
        panel_vis,
        window_size,
        open_tabs,
        font_size,
        dir_label_lens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clone_roots_round_trip() {
        let path = std::env::temp_dir().join("shell_save_data_test_clone_roots.ss");
        let roots = vec![
            "https://github.com/david-oconnor".to_string(),
            "git@github.com:rust-lang".to_string(),
        ];
        save_state(
            &[],
            &[],
            &[],
            &[],
            &roots,
            &PanelVis::default(),
            None,
            &OpenTabs::default(),
            FontSizes::default(),
            DirLabelLens::default(),
            &path,
        )
        .unwrap();
        let loaded = load_state(&path).unwrap();
        let _ = fs::remove_file(&path);
        assert_eq!(loaded.clone_roots, roots);
    }

    #[test]
    fn font_sizes_round_trip() {
        let path = std::env::temp_dir().join("shell_save_data_test_font_sizes.ss");
        for sizes in [
            FontSizes::default(),
            FontSizes {
                cli: Some(14.0),
                gui: None,
            },
            FontSizes {
                cli: Some(14.0),
                gui: Some(9.5),
            },
        ] {
            save_state(
                &[],
                &[],
                &[],
                &[],
                &[],
                &PanelVis::default(),
                None,
                &OpenTabs::default(),
                sizes,
                DirLabelLens::default(),
                &path,
            )
            .unwrap();
            assert_eq!(load_state(&path).unwrap().font_size, sizes);
        }
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn dir_label_lens_round_trip() {
        let path = std::env::temp_dir().join("shell_save_data_test_dir_label_lens.ss");
        for lens in [
            DirLabelLens::default(),
            DirLabelLens {
                bookmarks: Some(30),
                recent_dirs: None,
            },
            DirLabelLens {
                bookmarks: Some(30),
                recent_dirs: Some(45),
            },
        ] {
            save_state(
                &[],
                &[],
                &[],
                &[],
                &[],
                &PanelVis::default(),
                None,
                &OpenTabs::default(),
                FontSizes::default(),
                lens,
                &path,
            )
            .unwrap();
            assert_eq!(load_state(&path).unwrap().dir_label_lens, lens);
        }
        let _ = fs::remove_file(&path);
    }
}
