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
//! CLIP (the GUI's clipboard history) also uses TAB separators, but its
//! fields are escaped rather than flattened (see [escape_field]), so copied
//! text comes back with its newlines and tabs intact. SCRATCHPAD (the GUI's
//! scratchpad text) is one such field on a line of its own.
//!
//! The GUI's settings and layout (window, panels, font sizes etc.) aren't
//! here; it saves those with eframe. Lines for them in older files are
//! ignored, like any unknown tag.
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
    ClipboardContent, ClipboardEntry, HistoryItem, OpenTabs, RecentDir, RemoteTerminal,
};

pub const FILENAME: &str = "shell_state.ss";

const BOOKMARK_TAG: &str = "BM ";
const BOOKMARK_ORDER_TAG: &str = "BM_ORDER ";
const RECENT_DIR_TAG: &str = "REC_DIR ";
const HISTORY_TAG: &str = "HIS ";
const REMOTE_TERMINAL_TAG: &str = "REMOTE_TERMINAL ";
const OPEN_TAB_TAG: &str = "OPEN_TAB ";
const ACTIVE_TAB_TAG: &str = "ACTIVE_TAB ";
const CLONE_ROOT_TAG: &str = "CLONE_ROOT ";
const CLIPBOARD_TAG: &str = "CLIP ";
const SCRATCHPAD_TAG: &str = "SCRATCHPAD ";

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
    /// The GUI tabs that were open when the file was last written. `paths`
    /// is empty when the file has no `OPEN_TAB` lines (predates the feature
    /// or a CLI-only file), in which case the GUI opens a single default tab.
    pub open_tabs: OpenTabs,
    /// The order the GUI's Bookmarks panel was dragged into. Empty when the
    /// file has no `BM_ORDER` lines.
    pub bookmark_order: Vec<PathBuf>,
    /// The GUI's clipboard history, newest first. Empty when the file has
    /// no `CLIP` lines.
    pub clipboard: Vec<ClipboardEntry>,
    /// The GUI's scratchpad text. Empty when the file has no `SCRATCHPAD`
    /// line.
    pub scratchpad: String,
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
    open_tabs: &OpenTabs,
    bookmark_order: &[PathBuf],
    clipboard: &[ClipboardEntry],
    scratchpad: &str,
    path: &Path,
) -> io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }

    // Buffered: the GUI saves on every copy, and clipboard entries can be big.
    let mut f = io::BufWriter::new(fs::File::create(path)?);
    writeln!(
        f,
        "# Shell state — auto-generated. Do not edit while shell is running."
    )?;

    for bm in bookmarks {
        writeln!(f, "{BOOKMARK_TAG}{}", bm.display())?;
    }

    // GUI-only: the custom order, one path per line, like BM.
    for bm in bookmark_order {
        writeln!(f, "{BOOKMARK_ORDER_TAG}{}", bm.display())?;
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

    // GUI-only.
    if !open_tabs.paths.is_empty() {
        for path in &open_tabs.paths {
            writeln!(f, "{OPEN_TAB_TAG}{}", path.display())?;
        }
        writeln!(f, "{ACTIVE_TAB_TAG}{}", open_tabs.active)?;
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

    // Scratchpad: GUI-only, escaped like CLIP so its newlines and tabs come
    // back. Written only when there's text.
    if !scratchpad.is_empty() {
        writeln!(f, "{SCRATCHPAD_TAG}{}", escape_field(scratchpad))?;
    }

    // Clipboard: GUI-only, newest first. Last, since entries can be long.
    // `<dt>\ttext\t<text>` or `<dt>\tcopy|cut\t<path>\t<path>...`.
    for entry in clipboard {
        let dt = entry.added.to_rfc3339_opts(SecondsFormat::Secs, true);
        match &entry.content {
            ClipboardContent::Text(text) => {
                writeln!(f, "{CLIPBOARD_TAG}{dt}\ttext\t{}", escape_field(text))?;
            }
            ClipboardContent::Files { paths, cut } => {
                let kind = if *cut { "cut" } else { "copy" };
                let paths: Vec<String> = paths
                    .iter()
                    .map(|p| escape_field(&p.to_string_lossy()))
                    .collect();
                writeln!(f, "{CLIPBOARD_TAG}{dt}\t{kind}\t{}", paths.join("\t"))?;
            }
        }
    }

    f.flush()
}

/// Flatten characters that would break the line/tab-based record format.
fn sanitize_field(s: &str) -> String {
    s.replace(['\r', '\n', '\t'], " ")
}

/// Escape the characters that would break the line/tab-based record format,
/// for fields that must come back exactly: `\` `\t` `\n` `\r` become `\\`
/// `\t` `\n` `\r`. Undone by [unescape_field].
fn escape_field(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out
}

fn unescape_field(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some(c) => out.push(c),
            None => out.push('\\'),
        }
    }
    out
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
                open_tabs: OpenTabs::default(),
                bookmark_order: Vec::new(),
                clipboard: Vec::new(),
                scratchpad: String::new(),
            });
        }
        Err(e) => return Err(e),
    };

    let mut bookmarks = Vec::new();
    let mut recent_dirs = Vec::new();
    let mut history = Vec::new();
    let mut remote_terminals = Vec::new();
    let mut clone_roots = Vec::new();
    let mut open_tabs = OpenTabs::default();
    let mut bookmark_order = Vec::new();
    let mut clipboard = Vec::new();
    let mut scratchpad = String::new();

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

        if let Some(rest) = trimmed.strip_prefix(BOOKMARK_ORDER_TAG) {
            bookmark_order.push(PathBuf::from(rest.trim_end()));
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
        if let Some(rest) = trimmed.strip_prefix(SCRATCHPAD_TAG) {
            scratchpad = unescape_field(rest.trim_end_matches('\r'));
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix(CLIPBOARD_TAG) {
            // `<dt>\ttext\t<text>` or `<dt>\tcopy|cut\t<path>...`, each field
            // escaped. A malformed line, or a file entry without paths, is
            // skipped.
            let mut parts = rest.trim_end_matches('\r').split('\t');
            if let (Some(dt_str), Some(kind)) = (parts.next(), parts.next())
                && let Ok(dt) = DateTime::parse_from_rfc3339(dt_str)
            {
                let content = match kind {
                    "text" => parts
                        .next()
                        .map(|t| ClipboardContent::Text(unescape_field(t))),
                    "copy" | "cut" => {
                        let paths: Vec<PathBuf> =
                            parts.map(|p| PathBuf::from(unescape_field(p))).collect();
                        (!paths.is_empty()).then_some(ClipboardContent::Files {
                            paths,
                            cut: kind == "cut",
                        })
                    }
                    _ => None,
                };
                if let Some(content) = content {
                    clipboard.push(ClipboardEntry {
                        content,
                        added: dt.with_timezone(&Utc),
                    });
                }
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
        open_tabs,
        bookmark_order,
        clipboard,
        scratchpad,
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
            &OpenTabs::default(),
            &[],
            &[],
            "",
            &path,
        )
        .unwrap();
        let loaded = load_state(&path).unwrap();
        let _ = fs::remove_file(&path);
        assert_eq!(loaded.clone_roots, roots);
    }

    #[test]
    fn clipboard_round_trip() {
        let path = std::env::temp_dir().join("shell_save_data_test_clipboard.ss");
        let at = |s| DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc);
        let entries = vec![
            ClipboardEntry {
                content: ClipboardContent::Text(
                    "line 1\r\nline\t2\\n not a newline\n  trailing \\".into(),
                ),
                added: at("2026-10-15T09:30:00Z"),
            },
            ClipboardEntry {
                content: ClipboardContent::Files {
                    paths: vec![
                        PathBuf::from(r"C:\Users\a b\x.txt"),
                        PathBuf::from("/tmp/y"),
                    ],
                    cut: true,
                },
                added: at("2026-10-14T23:59:59Z"),
            },
            ClipboardEntry {
                content: ClipboardContent::Files {
                    paths: vec![PathBuf::from("z")],
                    cut: false,
                },
                added: at("2026-10-01T00:00:00Z"),
            },
        ];
        save_state(
            &[],
            &[],
            &[],
            &[],
            &[],
            &OpenTabs::default(),
            &[],
            &entries,
            "",
            &path,
        )
        .unwrap();
        assert_eq!(load_state(&path).unwrap().clipboard, entries);

        // Malformed lines are skipped rather than failing the load.
        fs::write(
            &path,
            "CLIP junk\ttext\tx\nCLIP 2026-10-15T09:30:00Z\tcopy\nCLIP 2026-10-15T09:30:00Z\tother\tx\n",
        )
        .unwrap();
        assert!(load_state(&path).unwrap().clipboard.is_empty());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn scratchpad_round_trip() {
        let path = std::env::temp_dir().join("shell_save_data_test_scratchpad.ss");
        for text in ["", "one line", "  lead\tand tab\nsecond \\n line\r\n\n"] {
            save_state(
                &[],
                &[],
                &[],
                &[],
                &[],
                &OpenTabs::default(),
                &[],
                &[],
                text,
                &path,
            )
            .unwrap();
            assert_eq!(load_state(&path).unwrap().scratchpad, text);
        }
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn bookmark_order_round_trip() {
        let path = std::env::temp_dir().join("shell_save_data_test_bookmark_order.ss");
        let bookmarks = vec![PathBuf::from("/a/b"), PathBuf::from("/a/c d")];
        for order in [
            Vec::new(),
            vec![PathBuf::from("/a/c d"), PathBuf::from("/a/b")],
        ] {
            save_state(
                &bookmarks,
                &[],
                &[],
                &[],
                &[],
                &OpenTabs::default(),
                &order,
                &[],
                "",
                &path,
            )
            .unwrap();
            let loaded = load_state(&path).unwrap();
            assert_eq!(loaded.bookmark_order, order);
            // The custom order's lines aren't taken for bookmarks.
            assert_eq!(loaded.bookmarks, bookmarks);
        }
        let _ = fs::remove_file(&path);
    }
}
