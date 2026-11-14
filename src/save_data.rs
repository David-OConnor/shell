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
//! RUST_TARGET_ROOT is a folder the GUI's System status pane scans for cargo
//! target folders.
//!
//! The GUI's settings and layout (window, panels, font sizes etc.) aren't
//! here; it saves those with eframe. Lines for them in older files are
//! ignored, like any unknown tag.
//!
//! Timestamps are written as second-precision RFC 3339 (e.g.
//! `2026-06-23T13:41:32Z`) to keep rows compact.
//!
//! Both the CLI and GUI crates call into this module — neither one
//! re-implements the parsing or formatting.
//!
//! Several processes (CLI shells and the GUI) share the file, each holding
//! its lists in memory. So [save] merges: it keeps what other processes saved
//! since this one last read or wrote the file, as well as this one's changes.
//! The CLI keeps the GUI-only fields as on disk.

use std::{
    collections::HashSet,
    fs,
    hash::Hash,
    io::{self, BufRead, BufReader, Write},
    path::{Path, PathBuf},
};

use chrono::{DateTime, SecondsFormat, Utc};

use crate::state::{
    ClipboardContent, ClipboardEntry, HistoryItem, OpenTabs, RecentDir, RemoteTerminal,
    dedup_history,
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
const RUST_TARGET_ROOT_TAG: &str = "RUST_TARGET_ROOT ";

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
    /// The folders the GUI's System status pane scans for cargo target
    /// folders. Empty when the file has no `RUST_TARGET_ROOT` lines.
    pub rust_target_roots: Vec<PathBuf>,
}

/// Where the state file lives by default: `<home>/shell_state.ss`. Falls back
/// to `None` if neither `USERPROFILE` nor `HOME` is set (rare).
pub fn default_path() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(|h| PathBuf::from(h).join(FILENAME))
}

impl LoadedState {
    pub fn shared(&self) -> Shared {
        Shared {
            bookmarks: self.bookmarks.clone(),
            recent_dirs: self.recent_dirs.clone(),
            history: self.history.clone(),
            remote_terminals: self.remote_terminals.clone(),
            clone_roots: self.clone_roots.clone(),
        }
    }
}

/// The lists both the CLI and the GUI change.
#[derive(Clone, Default)]
pub struct Shared {
    pub bookmarks: Vec<PathBuf>,
    pub recent_dirs: Vec<RecentDir>,
    pub history: Vec<HistoryItem>,
    pub remote_terminals: Vec<RemoteTerminal>,
    pub clone_roots: Vec<String>,
}

/// A process's own [Shared] lists, which [save] and [sync] update in place.
pub struct Ours<'a> {
    pub bookmarks: &'a mut Vec<PathBuf>,
    pub recent_dirs: &'a mut Vec<RecentDir>,
    pub history: &'a mut Vec<HistoryItem>,
    pub remote_terminals: &'a mut Vec<RemoteTerminal>,
    pub clone_roots: &'a mut Vec<String>,
}

impl Ours<'_> {
    fn to_shared(&self) -> Shared {
        Shared {
            bookmarks: self.bookmarks.clone(),
            recent_dirs: self.recent_dirs.clone(),
            history: self.history.clone(),
            remote_terminals: self.remote_terminals.clone(),
            clone_roots: self.clone_roots.clone(),
        }
    }
}

/// The fields only the GUI changes.
pub struct GuiOnly<'a> {
    pub open_tabs: &'a OpenTabs,
    pub bookmark_order: &'a [PathBuf],
    pub clipboard: &'a [ClipboardEntry],
    pub scratchpad: &'a str,
    pub rust_target_roots: &'a [PathBuf],
}

/// Writes `ours` to the file, merged first with what other processes saved
/// there (see [sync]). `base` is the lists as this process last read or wrote
/// them. `gui` is `None` from the CLI, which keeps the GUI-only fields as on
/// disk. Returns whether `ours` took changes from disk.
pub fn save(
    base: &mut Shared,
    mut ours: Ours,
    gui: Option<GuiOnly>,
    path: &Path,
) -> io::Result<bool> {
    let disk = load_state(path)?;
    let changed = merge(base, &mut ours, disk.shared());

    let gui = gui.unwrap_or(GuiOnly {
        open_tabs: &disk.open_tabs,
        bookmark_order: &disk.bookmark_order,
        clipboard: &disk.clipboard,
        scratchpad: &disk.scratchpad,
        rust_target_roots: &disk.rust_target_roots,
    });
    save_state(&ours, &gui, path)?;
    *base = ours.to_shared();

    Ok(changed)
}

/// Takes into `ours` what other processes saved to the file since `base`,
/// keeping its own changes since then. `base` becomes the file's lists.
/// Returns whether `ours` changed.
pub fn sync(base: &mut Shared, mut ours: Ours, path: &Path) -> io::Result<bool> {
    let disk = load_state(path)?;

    Ok(merge(base, &mut ours, disk.shared()))
}

/// Three-way merges each list: `theirs`, less what `ours` removed since
/// `base`, plus what it added. `base` becomes `theirs`.
fn merge(base: &mut Shared, ours: &mut Ours, theirs: Shared) -> bool {
    let bookmarks = merge_list(&base.bookmarks, ours.bookmarks, &theirs.bookmarks, |p| {
        p.clone()
    });
    // Second precision, as on disk.
    let recent_dirs = merge_list(
        &base.recent_dirs,
        ours.recent_dirs,
        &theirs.recent_dirs,
        |r| (r.path.clone(), r.dt.timestamp()),
    );
    let history = merge_list(&base.history, ours.history, &theirs.history, |h| {
        (h.text.clone(), h.dir.clone(), h.dt.timestamp())
    });
    let remote_terminals = merge_list(
        &base.remote_terminals,
        ours.remote_terminals,
        &theirs.remote_terminals,
        |r| (r.host.clone(), r.port, r.username.clone()),
    );
    let clone_roots = merge_list(
        &base.clone_roots,
        ours.clone_roots,
        &theirs.clone_roots,
        |r| r.clone(),
    );

    // Both sides may have visited a dir or rerun a command: keep the latest.
    if recent_dirs {
        ours.recent_dirs.sort_by_key(|r| r.dt);
        let mut seen = HashSet::new();
        let keep: Vec<bool> = ours
            .recent_dirs
            .iter()
            .rev()
            .map(|r| seen.insert(r.path.clone()))
            .collect();
        let mut keep = keep.into_iter().rev();
        ours.recent_dirs.retain(|_| keep.next().unwrap_or(true));
    }

    if history {
        ours.history.sort_by_key(|h| h.dt);
        dedup_history(ours.history);
    }

    *base = theirs;

    bookmarks || recent_dirs || history || remote_terminals || clone_roots
}

/// [merge] for one list, its items told apart by `key`. Returns whether `ours`
/// changed: not when `theirs` is still `base`.
fn merge_list<T: Clone, K: Hash + Eq>(
    base: &[T],
    ours: &mut Vec<T>,
    theirs: &[T],
    key: impl Fn(&T) -> K,
) -> bool {
    if base.len() == theirs.len() && base.iter().zip(theirs).all(|(b, t)| key(b) == key(t)) {
        return false;
    }

    let base_keys: HashSet<K> = base.iter().map(&key).collect();
    let our_keys: HashSet<K> = ours.iter().map(&key).collect();

    let mut merged: Vec<T> = theirs
        .iter()
        .filter(|t| {
            let k = key(t);
            our_keys.contains(&k) || !base_keys.contains(&k)
        })
        .cloned()
        .collect();
    let mut present: HashSet<K> = merged.iter().map(&key).collect();

    for item in ours.iter() {
        let k = key(item);
        if !base_keys.contains(&k) && present.insert(k) {
            merged.push(item.clone());
        }
    }

    *ours = merged;

    true
}

/// Overwrites the file. Written in full to a temporary file first, then
/// moved into place, so another process never reads half of it: [merge]
/// would take the missing half as removals.
fn save_state(ours: &Ours, gui: &GuiOnly, path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }

    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".{}.tmp", std::process::id()));
    let tmp = PathBuf::from(tmp);

    let written = fs::File::create(&tmp).and_then(|file| {
        // Buffered: the GUI saves on every copy, and clipboard entries can be big.
        let mut f = io::BufWriter::new(file);
        write_state(&mut f, ours, gui)?;
        f.flush()
    });
    // Replacing can fail while something else (e.g. an antivirus scan) has
    // the file open; then write over it.
    let result =
        written.and_then(|()| fs::rename(&tmp, path).or_else(|_| fs::copy(&tmp, path).map(|_| ())));
    let _ = fs::remove_file(&tmp);

    result
}

fn write_state(f: &mut impl Write, ours: &Ours, gui: &GuiOnly) -> io::Result<()> {
    let GuiOnly {
        open_tabs,
        bookmark_order,
        clipboard,
        scratchpad,
        rust_target_roots,
    } = gui;

    writeln!(
        f,
        "# Shell state — auto-generated. Do not edit while shell is running."
    )?;

    for bm in ours.bookmarks.iter() {
        writeln!(f, "{BOOKMARK_TAG}{}", bm.display())?;
    }

    // GUI-only: the custom order, one path per line, like BM.
    for bm in bookmark_order.iter() {
        writeln!(f, "{BOOKMARK_ORDER_TAG}{}", bm.display())?;
    }

    for r in ours.recent_dirs.iter() {
        writeln!(
            f,
            "{RECENT_DIR_TAG}{} {}",
            r.dt.to_rfc3339_opts(SecondsFormat::Secs, true),
            r.path.display()
        )?;
    }

    for h in ours.history.iter() {
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

    for rt in ours.remote_terminals.iter() {
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

    for root in ours.clone_roots.iter() {
        writeln!(f, "{CLONE_ROOT_TAG}{}", sanitize_field(root))?;
    }

    // GUI-only.
    for root in rust_target_roots.iter() {
        writeln!(f, "{RUST_TARGET_ROOT_TAG}{}", root.display())?;
    }

    // Scratchpad: GUI-only, escaped like CLIP so its newlines and tabs come
    // back. Written only when there's text.
    if !scratchpad.is_empty() {
        writeln!(f, "{SCRATCHPAD_TAG}{}", escape_field(scratchpad))?;
    }

    // Clipboard: GUI-only, newest first. Last, since entries can be long.
    // `<dt>\ttext\t<text>` or `<dt>\tcopy|cut\t<path>\t<path>...`.
    for entry in clipboard.iter() {
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

    Ok(())
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
                rust_target_roots: Vec::new(),
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
    let mut rust_target_roots = Vec::new();

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
        if let Some(rest) = trimmed.strip_prefix(RUST_TARGET_ROOT_TAG) {
            let root = rest.trim_end();
            if !root.is_empty() {
                rust_target_roots.push(PathBuf::from(root));
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
        rust_target_roots,
    })
}
