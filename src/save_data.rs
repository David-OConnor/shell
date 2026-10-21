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
    Appearance, BookmarkSort, BookmarkView, ClipboardContent, ClipboardEntry, CmdHistoryPrefs,
    DirLabelLens, FilesPanelPrefs, FontSizes, HistoryItem, OpenTabs, PaneSplits, PanelVis,
    PanelWidths, RecentDir, RemoteTerminal, ScratchpadPrefs, ScratchpadTab, WindowSize,
};

pub const FILENAME: &str = "shell_state.ss";

const BOOKMARK_TAG: &str = "BM ";
const BOOKMARK_SORT_TAG: &str = "BM_SORT ";
const BOOKMARK_ORDER_TAG: &str = "BM_ORDER ";
const RECENT_DIR_TAG: &str = "REC_DIR ";
const HISTORY_TAG: &str = "HIS ";
const REMOTE_TERMINAL_TAG: &str = "REMOTE_TERMINAL ";
const PANEL_VIS_TAG: &str = "PANEL_VIS ";
const WINDOW_SIZE_TAG: &str = "WINDOW_SIZE ";
const OPEN_TAB_TAG: &str = "OPEN_TAB ";
const ACTIVE_TAB_TAG: &str = "ACTIVE_TAB ";
const FONT_SIZE_TAG: &str = "FONT_SIZE ";
const DIR_LABEL_LEN_TAG: &str = "DIR_LABEL_LEN ";
const PANEL_WIDTH_TAG: &str = "PANEL_WIDTH ";
const FILES_PANEL_TAG: &str = "FILES_PANEL ";
const PANE_SPLIT_TAG: &str = "PANE_SPLIT ";
const CMD_HISTORY_TAG: &str = "CMD_HISTORY ";
const APPEARANCE_TAG: &str = "APPEARANCE ";
const CLONE_ROOT_TAG: &str = "CLONE_ROOT ";
const CLIPBOARD_TAG: &str = "CLIP ";
const SCRATCHPAD_TAG: &str = "SCRATCHPAD ";
const SCRATCHPAD_PREFS_TAG: &str = "SCRATCHPAD_PREFS ";
const PANE_ORDER_TAG: &str = "PANE_ORDER ";

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
    /// GUI Bookmarks panel ordering. `sort` is `None` when the file has no
    /// `BM_SORT` line, as with `font_size`, and `order` is empty when it has
    /// no `BM_ORDER` lines.
    pub bookmark_view: BookmarkView,
    /// GUI side-panel widths, in points. Each is `None` when the `PANEL_WIDTH`
    /// line lacks it, as with `font_size`.
    pub panel_widths: PanelWidths,
    /// GUI Files-panel toggles. Each is `None` when the `FILES_PANEL` line
    /// lacks it, as with `font_size`.
    pub files_panel: FilesPanelPrefs,
    /// GUI list / details dividers. Each is `None` when the `PANE_SPLIT`
    /// line lacks it, as with `font_size`.
    pub pane_splits: PaneSplits,
    /// GUI Cmd history toggles. Each is `None` when the `CMD_HISTORY` line
    /// lacks it, as with `font_size`.
    pub cmd_history: CmdHistoryPrefs,
    /// GUI appearance. `dark_mode` is `None` when the `APPEARANCE` line lacks
    /// it, as with `font_size`; the GUI then follows the OS theme.
    pub appearance: Appearance,
    /// The GUI's clipboard history, newest first. Empty when the file has
    /// no `CLIP` lines.
    pub clipboard: Vec<ClipboardEntry>,
    /// The GUI's scratchpad text. Empty when the file has no `SCRATCHPAD`
    /// line.
    pub scratchpad: String,
    /// GUI scratchpad settings. Each is `None` when the `SCRATCHPAD_PREFS`
    /// line lacks it, as with `font_size`.
    pub scratchpad_prefs: ScratchpadPrefs,
    /// The GUI's panes, left to right, by their PANEL_VIS keys. Empty when
    /// the file has no `PANE_ORDER` line, in which case the GUI uses its
    /// default order.
    pub pane_order: Vec<String>,
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
    bookmark_view: &BookmarkView,
    panel_widths: PanelWidths,
    files_panel: FilesPanelPrefs,
    pane_splits: PaneSplits,
    cmd_history: CmdHistoryPrefs,
    appearance: Appearance,
    clipboard: &[ClipboardEntry],
    scratchpad: &str,
    scratchpad_prefs: ScratchpadPrefs,
    pane_order: &[String],
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

    // BookmarkView: GUI-only. The sort mode is written only when present,
    // like FONT_SIZE; the custom order is one path per line, like BM.
    if let Some(sort) = bookmark_view.sort {
        writeln!(f, "{BOOKMARK_SORT_TAG}{}", sort.key())?;
    }
    for bm in &bookmark_view.order {
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

    // PanelVis: one line, space-separated `key=0|1` pairs. Stable order
    // so the file diff is reproducible. Forward-compatible: unknown keys
    // are skipped on load; missing keys keep their default value.
    writeln!(
        f,
        "{PANEL_VIS_TAG}bookmarks={} recent_dirs={} recent_cmds={} recent_cmds_in_dir={} remote_terminals={} file_browser={} path_editor={} clipboard={} scratchpad={} terminal={}",
        bool_to_int(panel_vis.bookmarks),
        bool_to_int(panel_vis.recent_dirs),
        bool_to_int(panel_vis.recent_cmds),
        bool_to_int(panel_vis.recent_cmds_in_dir),
        bool_to_int(panel_vis.remote_terminals),
        bool_to_int(panel_vis.file_browser),
        bool_to_int(panel_vis.path_editor),
        bool_to_int(panel_vis.clipboard),
        bool_to_int(panel_vis.scratchpad),
        bool_to_int(panel_vis.terminal),
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

    // DirLabelLen: GUI-only, written only when present, like FONT_SIZE. The
    // names-only toggles are `key=0|1` pairs, like PANEL_VIS.
    let mut pairs = Vec::new();
    if let Some(len) = dir_label_lens.bookmarks {
        pairs.push(format!("bookmarks={len}"));
    }
    if let Some(len) = dir_label_lens.recent_dirs {
        pairs.push(format!("recent_dirs={len}"));
    }
    if let Some(on) = dir_label_lens.bookmark_names {
        pairs.push(format!("bookmark_names={}", bool_to_int(on)));
    }
    if let Some(on) = dir_label_lens.recent_dir_names {
        pairs.push(format!("recent_dir_names={}", bool_to_int(on)));
    }
    if !pairs.is_empty() {
        writeln!(f, "{DIR_LABEL_LEN_TAG}{}", pairs.join(" "))?;
    }

    // PanelWidth: GUI-only, written only when present, like FONT_SIZE. Keys
    // match PANEL_VIS.
    let widths = [
        ("bookmarks", panel_widths.bookmarks),
        ("recent_dirs", panel_widths.recent_dirs),
        ("recent_cmds", panel_widths.recent_cmds),
        ("recent_cmds_in_dir", panel_widths.recent_cmds_in_dir),
        ("remote_terminals", panel_widths.remote_terminals),
        ("file_browser", panel_widths.file_browser),
        ("path_editor", panel_widths.path_editor),
        ("scratchpad", panel_widths.scratchpad),
    ];
    let pairs: Vec<String> = widths
        .iter()
        .filter_map(|(k, w)| w.map(|w| format!("{k}={w}")))
        .collect();
    if !pairs.is_empty() {
        writeln!(f, "{PANEL_WIDTH_TAG}{}", pairs.join(" "))?;
    }

    // PaneOrder: GUI-only, written only when present, like FONT_SIZE.
    // Space-separated keys, left to right, matching PANEL_VIS.
    if !pane_order.is_empty() {
        writeln!(f, "{PANE_ORDER_TAG}{}", pane_order.join(" "))?;
    }

    // FilesPanel: GUI-only, written only when present, like FONT_SIZE.
    // `key=0|1` pairs, like PANEL_VIS.
    let toggles = [
        ("hidden", files_panel.show_hidden),
        ("dotfiles", files_panel.show_dotfiles),
        ("deep_search_names", files_panel.deep_search_names),
    ];
    let pairs: Vec<String> = toggles
        .iter()
        .filter_map(|(k, on)| on.map(|on| format!("{k}={}", bool_to_int(on))))
        .collect();
    if !pairs.is_empty() {
        writeln!(f, "{FILES_PANEL_TAG}{}", pairs.join(" "))?;
    }

    // PaneSplit: GUI-only, written only when present, like FONT_SIZE.
    // `key=<fraction>` pairs, like PANEL_WIDTH.
    let splits = [("files", pane_splits.files), ("path", pane_splits.path)];
    let pairs: Vec<String> = splits
        .iter()
        .filter_map(|(k, s)| s.map(|s| format!("{k}={s}")))
        .collect();
    if !pairs.is_empty() {
        writeln!(f, "{PANE_SPLIT_TAG}{}", pairs.join(" "))?;
    }

    // CmdHistory: GUI-only, written only when present, like FONT_SIZE.
    // `key=0|1` pairs, like PANEL_VIS.
    if let Some(on) = cmd_history.show_dir {
        writeln!(f, "{CMD_HISTORY_TAG}show_dir={}", bool_to_int(on))?;
    }

    // Appearance: GUI-only, written only when present, like FONT_SIZE.
    // `key=value` pairs, like PANEL_VIS.
    let mut pairs = Vec::new();
    if let Some(on) = appearance.dark_mode {
        pairs.push(format!("dark_mode={}", bool_to_int(on)));
    }
    if let Some(n) = appearance.dir_color_depth {
        pairs.push(format!("dir_color_depth={n}"));
    }
    if !pairs.is_empty() {
        writeln!(f, "{APPEARANCE_TAG}{}", pairs.join(" "))?;
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

    // ScratchpadPrefs: GUI-only, written only when present, like FONT_SIZE.
    // `key=value` pairs, like PANEL_VIS.
    if let Some(tab) = scratchpad_prefs.tab {
        writeln!(f, "{SCRATCHPAD_PREFS_TAG}tab={}", tab.key())?;
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
                bookmark_view: BookmarkView::default(),
                panel_widths: PanelWidths::default(),
                files_panel: FilesPanelPrefs::default(),
                pane_splits: PaneSplits::default(),
                cmd_history: CmdHistoryPrefs::default(),
                appearance: Appearance::default(),
                clipboard: Vec::new(),
                scratchpad: String::new(),
                scratchpad_prefs: ScratchpadPrefs::default(),
                pane_order: Vec::new(),
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
    let mut bookmark_view = BookmarkView::default();
    let mut panel_widths = PanelWidths::default();
    let mut files_panel = FilesPanelPrefs::default();
    let mut pane_splits = PaneSplits::default();
    let mut cmd_history = CmdHistoryPrefs::default();
    let mut appearance = Appearance::default();
    let mut clipboard = Vec::new();
    let mut scratchpad = String::new();
    let mut scratchpad_prefs = ScratchpadPrefs::default();
    let mut pane_order = Vec::new();

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

        if let Some(rest) = trimmed.strip_prefix(BOOKMARK_SORT_TAG) {
            // An unknown mode leaves it at `None` (the GUI's default).
            bookmark_view.sort = BookmarkSort::from_key(rest.trim_end());
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix(BOOKMARK_ORDER_TAG) {
            bookmark_view.order.push(PathBuf::from(rest.trim_end()));
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
                    "path_editor" => panel_vis.path_editor = on,
                    "clipboard" => panel_vis.clipboard = on,
                    "scratchpad" => panel_vis.scratchpad = on,
                    "terminal" => panel_vis.terminal = on,
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
            // Parse `bookmarks=<chars> recent_dirs=<chars>
            // bookmark_names=0|1 recent_dir_names=0|1`. A malformed or zero
            // length leaves it at `None` (the GUI's default).
            for pair in rest.split_whitespace() {
                let Some((k, v)) = pair.split_once('=') else {
                    continue;
                };
                let on = Some(v == "1" || v.eq_ignore_ascii_case("true"));
                let len = v.parse::<usize>().ok().filter(|&len| len > 0);
                match k {
                    "bookmarks" if len.is_some() => dir_label_lens.bookmarks = len,
                    "recent_dirs" if len.is_some() => dir_label_lens.recent_dirs = len,
                    "bookmark_names" => dir_label_lens.bookmark_names = on,
                    "recent_dir_names" => dir_label_lens.recent_dir_names = on,
                    _ => {}
                }
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix(PANEL_WIDTH_TAG) {
            // Parse `bookmarks=<points> recent_dirs=<points> ...`. Only adopt
            // positive, finite values, as with FONT_SIZE; anything else leaves
            // that panel at `None` (the GUI's default width).
            for pair in rest.split_whitespace() {
                let Some((k, v)) = pair.split_once('=') else {
                    continue;
                };
                let Ok(w) = v.parse::<f32>() else {
                    continue;
                };
                if !w.is_finite() || w <= 0.0 {
                    continue;
                }
                let w = Some(w);
                match k {
                    "bookmarks" => panel_widths.bookmarks = w,
                    "recent_dirs" => panel_widths.recent_dirs = w,
                    "recent_cmds" => panel_widths.recent_cmds = w,
                    "recent_cmds_in_dir" => panel_widths.recent_cmds_in_dir = w,
                    "remote_terminals" => panel_widths.remote_terminals = w,
                    "file_browser" => panel_widths.file_browser = w,
                    "path_editor" => panel_widths.path_editor = w,
                    "scratchpad" => panel_widths.scratchpad = w,
                    _ => {}
                }
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix(FILES_PANEL_TAG) {
            // Parse `hidden=0|1 dotfiles=0|1 deep_search_names=0|1`, like
            // PANEL_VIS. Unknown keys are ignored; a missing one stays `None`
            // (the GUI's default).
            for pair in rest.split_whitespace() {
                let Some((k, v)) = pair.split_once('=') else {
                    continue;
                };
                let on = Some(v == "1" || v.eq_ignore_ascii_case("true"));
                match k {
                    "hidden" => files_panel.show_hidden = on,
                    "dotfiles" => files_panel.show_dotfiles = on,
                    "deep_search_names" => files_panel.deep_search_names = on,
                    _ => {}
                }
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix(PANE_SPLIT_TAG) {
            // Parse `files=<fraction> path=<fraction>`. Only adopt values
            // strictly between 0 and 1; anything else leaves that divider at
            // `None` (the GUI's default).
            for pair in rest.split_whitespace() {
                let Some((k, v)) = pair.split_once('=') else {
                    continue;
                };
                let Ok(s) = v.parse::<f32>() else {
                    continue;
                };
                if !(s > 0.0 && s < 1.0) {
                    continue;
                }
                match k {
                    "files" => pane_splits.files = Some(s),
                    "path" => pane_splits.path = Some(s),
                    _ => {}
                }
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix(CMD_HISTORY_TAG) {
            // Parse `show_dir=0|1`, like FILES_PANEL.
            for pair in rest.split_whitespace() {
                let Some((k, v)) = pair.split_once('=') else {
                    continue;
                };
                let on = Some(v == "1" || v.eq_ignore_ascii_case("true"));
                if k == "show_dir" {
                    cmd_history.show_dir = on;
                }
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix(APPEARANCE_TAG) {
            // Parse `dark_mode=0|1 dir_color_depth=N`, like CMD_HISTORY.
            for pair in rest.split_whitespace() {
                let Some((k, v)) = pair.split_once('=') else {
                    continue;
                };
                match k {
                    "dark_mode" => {
                        appearance.dark_mode = Some(v == "1" || v.eq_ignore_ascii_case("true"));
                    }
                    "dir_color_depth" => appearance.dir_color_depth = v.parse().ok(),
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
        if let Some(rest) = trimmed.strip_prefix(SCRATCHPAD_TAG) {
            scratchpad = unescape_field(rest.trim_end_matches('\r'));
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix(SCRATCHPAD_PREFS_TAG) {
            // Parse `tab=<mode>`, like CMD_HISTORY. An unknown mode leaves it
            // at `None` (the GUI's default).
            for pair in rest.split_whitespace() {
                let Some((k, v)) = pair.split_once('=') else {
                    continue;
                };
                if k == "tab" {
                    scratchpad_prefs.tab = ScratchpadTab::from_key(v);
                }
            }
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix(PANE_ORDER_TAG) {
            // The GUI checks the keys; unknown ones are its to skip.
            pane_order = rest.split_whitespace().map(str::to_string).collect();
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
        panel_vis,
        window_size,
        open_tabs,
        font_size,
        dir_label_lens,
        bookmark_view,
        panel_widths,
        files_panel,
        pane_splits,
        cmd_history,
        appearance,
        clipboard,
        scratchpad,
        scratchpad_prefs,
        pane_order,
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
            &BookmarkView::default(),
            PanelWidths::default(),
            FilesPanelPrefs::default(),
            PaneSplits::default(),
            CmdHistoryPrefs::default(),
            Appearance::default(),
            &[],
            "",
            ScratchpadPrefs::default(),
            &[],
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
                &BookmarkView::default(),
                PanelWidths::default(),
                FilesPanelPrefs::default(),
                PaneSplits::default(),
                CmdHistoryPrefs::default(),
                Appearance::default(),
                &[],
                "",
                ScratchpadPrefs::default(),
                &[],
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
                ..Default::default()
            },
            DirLabelLens {
                recent_dir_names: Some(false),
                ..Default::default()
            },
            DirLabelLens {
                bookmarks: Some(30),
                recent_dirs: Some(45),
                bookmark_names: Some(true),
                recent_dir_names: Some(false),
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
                &BookmarkView::default(),
                PanelWidths::default(),
                FilesPanelPrefs::default(),
                PaneSplits::default(),
                CmdHistoryPrefs::default(),
                Appearance::default(),
                &[],
                "",
                ScratchpadPrefs::default(),
                &[],
                &path,
            )
            .unwrap();
            assert_eq!(load_state(&path).unwrap().dir_label_lens, lens);
        }
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn panel_widths_round_trip() {
        let path = std::env::temp_dir().join("shell_save_data_test_panel_widths.ss");
        for widths in [
            PanelWidths::default(),
            PanelWidths {
                bookmarks: Some(212.5),
                ..Default::default()
            },
            PanelWidths {
                bookmarks: Some(150.0),
                recent_dirs: Some(175.25),
                recent_cmds: Some(180.0),
                recent_cmds_in_dir: Some(300.0),
                remote_terminals: Some(90.0),
                file_browser: Some(410.75),
                path_editor: Some(260.0),
                scratchpad: Some(320.0),
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
                DirLabelLens::default(),
                &BookmarkView::default(),
                widths,
                FilesPanelPrefs::default(),
                PaneSplits::default(),
                CmdHistoryPrefs::default(),
                Appearance::default(),
                &[],
                "",
                ScratchpadPrefs::default(),
                &[],
                &path,
            )
            .unwrap();
            assert_eq!(load_state(&path).unwrap().panel_widths, widths);
        }
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn files_panel_round_trip() {
        let path = std::env::temp_dir().join("shell_save_data_test_files_panel.ss");
        for prefs in [
            FilesPanelPrefs::default(),
            FilesPanelPrefs {
                show_dotfiles: Some(false),
                ..Default::default()
            },
            FilesPanelPrefs {
                show_hidden: Some(false),
                show_dotfiles: Some(true),
                deep_search_names: Some(true),
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
                DirLabelLens::default(),
                &BookmarkView::default(),
                PanelWidths::default(),
                prefs,
                PaneSplits::default(),
                CmdHistoryPrefs::default(),
                Appearance::default(),
                &[],
                "",
                ScratchpadPrefs::default(),
                &[],
                &path,
            )
            .unwrap();
            assert_eq!(load_state(&path).unwrap().files_panel, prefs);
        }
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn pane_splits_round_trip() {
        let path = std::env::temp_dir().join("shell_save_data_test_pane_splits.ss");
        for splits in [
            PaneSplits::default(),
            PaneSplits {
                files: Some(0.25),
                ..Default::default()
            },
            PaneSplits {
                files: Some(0.5),
                path: Some(0.125),
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
                DirLabelLens::default(),
                &BookmarkView::default(),
                PanelWidths::default(),
                FilesPanelPrefs::default(),
                splits,
                CmdHistoryPrefs::default(),
                Appearance::default(),
                &[],
                "",
                ScratchpadPrefs::default(),
                &[],
                &path,
            )
            .unwrap();
            assert_eq!(load_state(&path).unwrap().pane_splits, splits);
        }

        // Out-of-range fractions are dropped rather than adopted.
        fs::write(&path, "PANE_SPLIT files=1.5 path=0 junk\n").unwrap();
        assert_eq!(
            load_state(&path).unwrap().pane_splits,
            PaneSplits::default()
        );
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn cmd_history_round_trip() {
        let path = std::env::temp_dir().join("shell_save_data_test_cmd_history.ss");
        for prefs in [
            CmdHistoryPrefs::default(),
            CmdHistoryPrefs {
                show_dir: Some(false),
            },
            CmdHistoryPrefs {
                show_dir: Some(true),
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
                DirLabelLens::default(),
                &BookmarkView::default(),
                PanelWidths::default(),
                FilesPanelPrefs::default(),
                PaneSplits::default(),
                prefs,
                Appearance::default(),
                &[],
                "",
                ScratchpadPrefs::default(),
                &[],
                &path,
            )
            .unwrap();
            assert_eq!(load_state(&path).unwrap().cmd_history, prefs);
        }
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn appearance_round_trip() {
        let path = std::env::temp_dir().join("shell_save_data_test_appearance.ss");
        for appearance in [
            Appearance::default(),
            Appearance {
                dark_mode: Some(false),
                dir_color_depth: None,
            },
            Appearance {
                dark_mode: Some(true),
                dir_color_depth: Some(8),
            },
            Appearance {
                dark_mode: None,
                dir_color_depth: Some(12),
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
                DirLabelLens::default(),
                &BookmarkView::default(),
                PanelWidths::default(),
                FilesPanelPrefs::default(),
                PaneSplits::default(),
                CmdHistoryPrefs::default(),
                appearance,
                &[],
                "",
                ScratchpadPrefs::default(),
                &[],
                &path,
            )
            .unwrap();
            assert_eq!(load_state(&path).unwrap().appearance, appearance);
        }
        let _ = fs::remove_file(&path);
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
            &PanelVis::default(),
            None,
            &OpenTabs::default(),
            FontSizes::default(),
            DirLabelLens::default(),
            &BookmarkView::default(),
            PanelWidths::default(),
            FilesPanelPrefs::default(),
            PaneSplits::default(),
            CmdHistoryPrefs::default(),
            Appearance::default(),
            &entries,
            "",
            ScratchpadPrefs::default(),
            &[],
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
                &PanelVis::default(),
                None,
                &OpenTabs::default(),
                FontSizes::default(),
                DirLabelLens::default(),
                &BookmarkView::default(),
                PanelWidths::default(),
                FilesPanelPrefs::default(),
                PaneSplits::default(),
                CmdHistoryPrefs::default(),
                Appearance::default(),
                &[],
                text,
                ScratchpadPrefs::default(),
                &[],
                &path,
            )
            .unwrap();
            assert_eq!(load_state(&path).unwrap().scratchpad, text);
        }
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn scratchpad_prefs_round_trip() {
        let path = std::env::temp_dir().join("shell_save_data_test_scratchpad_prefs.ss");
        let tabs = [
            ScratchpadTab::Tab,
            ScratchpadTab::Spaces,
            ScratchpadTab::FocusTerm,
        ];
        for prefs in [ScratchpadPrefs::default()]
            .into_iter()
            .chain(tabs.map(|tab| ScratchpadPrefs { tab: Some(tab) }))
        {
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
                DirLabelLens::default(),
                &BookmarkView::default(),
                PanelWidths::default(),
                FilesPanelPrefs::default(),
                PaneSplits::default(),
                CmdHistoryPrefs::default(),
                Appearance::default(),
                &[],
                "",
                prefs,
                &[],
                &path,
            )
            .unwrap();
            assert_eq!(load_state(&path).unwrap().scratchpad_prefs, prefs);
        }

        // An unknown mode is dropped rather than failing the load.
        fs::write(&path, "SCRATCHPAD_PREFS tab=sideways\n").unwrap();
        assert_eq!(load_state(&path).unwrap().scratchpad_prefs.tab, None);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn pane_order_and_terminal_vis_round_trip() {
        let path = std::env::temp_dir().join("shell_save_data_test_pane_order.ss");
        let orders: [Vec<String>; 2] = [
            Vec::new(),
            ["terminal", "clipboard", "bookmarks"]
                .map(String::from)
                .to_vec(),
        ];
        for (order, terminal) in orders.iter().zip([true, false]) {
            let vis = PanelVis {
                terminal,
                ..Default::default()
            };
            save_state(
                &[],
                &[],
                &[],
                &[],
                &[],
                &vis,
                None,
                &OpenTabs::default(),
                FontSizes::default(),
                DirLabelLens::default(),
                &BookmarkView::default(),
                PanelWidths::default(),
                FilesPanelPrefs::default(),
                PaneSplits::default(),
                CmdHistoryPrefs::default(),
                Appearance::default(),
                &[],
                "",
                ScratchpadPrefs::default(),
                order,
                &path,
            )
            .unwrap();
            let loaded = load_state(&path).unwrap();
            assert_eq!(&loaded.pane_order, order);
            assert_eq!(loaded.panel_vis.terminal, terminal);
        }
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn bookmark_view_round_trip() {
        let path = std::env::temp_dir().join("shell_save_data_test_bookmark_view.ss");
        let bookmarks = vec![PathBuf::from("/a/b"), PathBuf::from("/a/c d")];
        for view in [
            BookmarkView::default(),
            BookmarkView {
                sort: Some(BookmarkSort::Proximity),
                order: Vec::new(),
            },
            BookmarkView {
                sort: Some(BookmarkSort::Custom),
                order: vec![PathBuf::from("/a/c d"), PathBuf::from("/a/b")],
            },
        ] {
            save_state(
                &bookmarks,
                &[],
                &[],
                &[],
                &[],
                &PanelVis::default(),
                None,
                &OpenTabs::default(),
                FontSizes::default(),
                DirLabelLens::default(),
                &view,
                PanelWidths::default(),
                FilesPanelPrefs::default(),
                PaneSplits::default(),
                CmdHistoryPrefs::default(),
                Appearance::default(),
                &[],
                "",
                ScratchpadPrefs::default(),
                &[],
                &path,
            )
            .unwrap();
            let loaded = load_state(&path).unwrap();
            assert_eq!(loaded.bookmark_view, view);
            // The custom order's lines aren't taken for bookmarks.
            assert_eq!(loaded.bookmarks, bookmarks);
        }

        // An unknown mode is dropped rather than failing the load.
        fs::write(
            &path,
            "BM_SORT sideways
",
        )
        .unwrap();
        assert_eq!(load_state(&path).unwrap().bookmark_view.sort, None);
        let _ = fs::remove_file(&path);
    }
}
