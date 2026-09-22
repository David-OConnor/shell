//! Misc utility functionality.

use std::{
    collections::HashSet,
    env,
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use crate::state::{BrowserFile, HistoryItem, RecentDir};

/// Horizontal rule framing the paginated lists (history, recent dirs,
/// bookmarks, remotes) in the CLI.
pub const DIVIDER: &str = "----------";

/// How many items each paginated list shows per page.
pub const DISP_PAGE_LEN: usize = 20;

/// Total pages needed to show `total` items at `per_page` items per page.
/// Returns 1 when empty so the renderer can still show a "Page 1/1" frame.
pub fn page_count(total: usize, per_page: usize) -> usize {
    if total == 0 {
        1
    } else {
        total.div_ceil(per_page)
    }
}

/// Render one page of a list: header with paging hint + usage hint, a
/// page of rows, and a closing divider. Page 0 = the last `per_page`
/// items (newest at the bottom). Rows are labelled with their absolute
/// index into `items`, so the displayed number lines up with the
/// corresponding `<cmd> <number>` invocation. Shared by the history,
/// recent-directories, bookmarks, and remotes lists so they all present
/// the same frame.
///
/// `paging_hint` tells the user how to reach the other pages (each list
/// has a different trigger key); it's omitted when everything fits on
/// one page.
#[allow(clippy::too_many_arguments)]
pub fn render_page<T>(
    title: &str,
    paging_hint: &str,
    usage_hint: &str,
    empty_msg: &str,
    items: &[T],
    page: usize,
    per_page: usize,
    mut format_row: impl FnMut(usize, &T) -> String,
) -> String {
    let total = items.len();
    let pages = page_count(total, per_page);
    let page = page.min(pages - 1);

    let paging = if pages > 1 {
        format!("  ({paging_hint})")
    } else {
        String::new()
    };
    let mut msg = format!(
        "\n{title}{paging}.  {usage_hint}.  Page {}/{}:\n",
        page + 1,
        pages
    );
    msg.push_str(DIVIDER);
    msg.push('\n');

    if total == 0 {
        msg.push_str(empty_msg);
        msg.push('\n');
    } else {
        let end = total - page * per_page;
        let start = end.saturating_sub(per_page);

        for (i, item) in items.iter().enumerate().take(end).skip(start) {
            msg.push_str(&format_row(i, item));
            msg.push('\n');
        }
    }

    msg.push_str(DIVIDER);
    msg.push_str("\n\n");
    msg
}

/// Absolute history indices of the newest entry for each distinct command
/// text, oldest first. History keeps one entry per command-in-directory (so
/// Ctrl+4 works per directory); the all-directories view uses this to show a
/// command run in several directories only once.
pub fn history_latest_indices(history: &[HistoryItem]) -> Vec<usize> {
    let mut seen = HashSet::new();
    let mut indices: Vec<usize> = history
        .iter()
        .enumerate()
        .rev()
        .filter_map(|(i, item)| seen.insert(item.text.as_str()).then_some(i))
        .collect();
    indices.reverse();
    indices
}

/// Render one page of command history. Lives in the shared lib (rather than
/// the CLI's render module) so the Ctrl+3 key handler and the `his p<N>`
/// builtin print the identical frame.
pub fn render_history(history: &[HistoryItem], page: usize) -> String {
    let indices = history_latest_indices(history);
    render_page(
        "Command History",
        "Ctrl+3 again: older page",
        "Use `his <number>` to run, `his p<N>` to jump to a page; e.g. `his 4`",
        "(no history)",
        &indices,
        page,
        DISP_PAGE_LEN,
        |_, &i| format!("{i}:  {}", history[i].text),
    )
}

/// Absolute history indices for commands run in `cwd`, oldest first. Keeping
/// the original indices makes `this N` agree with the GUI's history panel.
pub fn history_indices_in_dir(history: &[HistoryItem], cwd: &Path) -> Vec<usize> {
    history
        .iter()
        .enumerate()
        .filter_map(|(i, item)| (item.dir == cwd).then_some(i))
        .collect()
}

pub fn render_history_in_dir(history: &[HistoryItem], cwd: &Path, page: usize) -> String {
    let indices = history_indices_in_dir(history, cwd);
    render_page(
        "Command History in this directory",
        "Ctrl+4 again: older page",
        "Use `this <number>` to run, `this p<N>` to jump to a page; e.g. `this 4`",
        "(no history in this directory)",
        &indices,
        page,
        DISP_PAGE_LEN,
        |_, &i| format!("{i}:  {}", history[i].text),
    )
}

/// Resolve an absolute index or the newest case-insensitive substring match.
/// `cwd` limits both forms to commands entered in that directory.
pub fn find_history_index(
    history: &[HistoryItem],
    cwd: Option<&Path>,
    query: &str,
) -> Option<usize> {
    if let Ok(i) = query.parse::<usize>() {
        return history
            .get(i)
            .filter(|item| cwd.is_none_or(|dir| item.dir == dir))
            .map(|_| i);
    }
    let needle = query.to_lowercase();
    history
        .iter()
        .enumerate()
        .rev()
        .find(|(_, item)| {
            cwd.is_none_or(|dir| item.dir == dir) && item.text.to_lowercase().contains(&needle)
        })
        .map(|(i, _)| i)
}

pub fn find_bookmark(bookmarks: &[PathBuf], query: &str) -> Option<PathBuf> {
    let needle = query.to_lowercase();
    bookmarks
        .iter()
        .rev()
        .find(|p| p.to_string_lossy().to_lowercase().contains(&needle))
        .cloned()
}

pub fn find_recent_dir(recent: &[RecentDir], query: &str) -> Option<PathBuf> {
    let needle = query.to_lowercase();
    recent
        .iter()
        .rev()
        .find(|r| r.path.to_string_lossy().to_lowercase().contains(&needle))
        .map(|r| r.path.clone())
}

#[cfg(test)]
mod history_tests {
    use chrono::Utc;

    use super::*;

    #[test]
    fn cwd_history_keeps_global_indices_and_pages_only_matches() {
        let here = PathBuf::from("/work/here");
        let elsewhere = PathBuf::from("/work/elsewhere");
        let history: Vec<_> = (0..25)
            .map(|i| HistoryItem {
                text: format!("task {i}"),
                dir: if i == 2 {
                    elsewhere.clone()
                } else {
                    here.clone()
                },
                dt: Utc::now(),
            })
            .collect();

        let first = render_history_in_dir(&history, &here, 0);
        let second = render_history_in_dir(&history, &here, 1);
        assert!(first.contains("24:  task 24"));
        assert!(!first.lines().any(|line| line.starts_with("2:  ")));
        assert!(second.contains("0:  task 0"));
        assert!(!second.lines().any(|line| line.starts_with("2:  ")));
        assert_eq!(find_history_index(&history, Some(&here), "2"), None);
        assert_eq!(
            find_history_index(&history, Some(&here), "TASK 2"),
            Some(24)
        );
        assert_eq!(find_history_index(&history, None, "task 2"), Some(24));
    }

    #[test]
    fn history_dedups_per_dir_and_hides_cross_dir_repeats_globally() {
        use crate::state::{dedup_history, record_history};

        let a = PathBuf::from("/a");
        let b = PathBuf::from("/b");
        let mut history = Vec::new();
        record_history(&mut history, "build", &a);
        record_history(&mut history, "git pull", &a);
        record_history(&mut history, "build", &a);
        record_history(&mut history, "build", &b);

        // Re-running in the same dir moves the command to the end; the same
        // text in another dir is kept as its own entry.
        let texts: Vec<_> = history
            .iter()
            .map(|h| (h.text.as_str(), h.dir.clone()))
            .collect();
        assert_eq!(
            texts,
            [
                ("git pull", a.clone()),
                ("build", a.clone()),
                ("build", b.clone())
            ]
        );

        // Ctrl+4 in /a lists each of its commands once.
        assert_eq!(history_indices_in_dir(&history, &a), [0, 1]);
        // Ctrl+3 shows `build` only once, at its newest position.
        assert_eq!(history_latest_indices(&history), [0, 2]);
        let global = render_history(&history, 0);
        assert!(global.contains("2:  build"));
        assert!(!global.lines().any(|line| line.starts_with("1:  ")));

        // Loading an older, un-deduped file keeps the newest of each.
        let mut loaded: Vec<_> = ["x", "y", "x", "x"]
            .iter()
            .map(|t| HistoryItem {
                text: t.to_string(),
                dir: a.clone(),
                dt: Utc::now(),
            })
            .collect();
        dedup_history(&mut loaded);
        let texts: Vec<_> = loaded.iter().map(|h| h.text.as_str()).collect();
        assert_eq!(texts, ["y", "x"]);
    }
}

/// Build a [`Command`] that won't make Windows allocate a console window for
/// the child. Use this for any process whose output we capture (`.output()`)
/// or that runs detached — **not** for an interactive child that needs to
/// share the terminal, since `CREATE_NO_WINDOW` denies it a console.
///
/// Why it matters: the `shell_gui` frontend is built `windows_subsystem =
/// "windows"`, so it has no console of its own. When such a process spawns a
/// console program (e.g. `git`, `pwsh`), Windows spins up a fresh `conhost.exe`
/// for each spawn — a multi-hundred-ms stall that shows up as a per-command
/// lag (and a flashing console window). `CREATE_NO_WINDOW` skips that entirely.
/// On non-Windows this is just `Command::new`.
pub fn quiet_command<S: AsRef<OsStr>>(program: S) -> Command {
    #[allow(unused_mut)]
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// Resolve a `cd`/`cat`-style path argument against the shell's state:
/// expands `~`/`~/...` to the home dir, treats real paths literally, and
/// falls back to a case-insensitive substring match against bookmarked
/// directories. Infallible — unresolvable cases degrade to the literal
/// `cwd`-joined path rather than erroring.
///
/// Takes primitives instead of a `State` struct so it can be shared between
/// the CLI shell (which stores bookmarks behind an `Arc<Mutex<_>>`) and the
/// GUI shell (which uses a plain `Vec`). Callers are responsible for any
/// locking before they hand us the slice.
pub fn path_from_args(
    home: Option<&Path>,
    cwd: &Path,
    bookmarks: &[PathBuf],
    args: &str,
) -> PathBuf {
    if args.is_empty() || args == "~" {
        // cd with no args (or a bare `~`) goes home; fall back to the
        // current directory if we couldn't resolve a home dir.
        home.map(|h| h.to_path_buf())
            .unwrap_or_else(|| cwd.to_path_buf())
    } else if let Some(rest) = args.strip_prefix("~/").or_else(|| args.strip_prefix("~\\")) {
        home.map(|h| h.join(rest)).unwrap_or_else(|| cwd.join(args))
    } else {
        // Try the literal path first so real subdirs / absolute paths
        // keep their normal meaning. If it isn't a directory, fall
        // back to a substring match against bookmarked directories.
        let literal = cwd.join(args);
        if literal.is_dir() {
            literal
        } else {
            find_bookmark(bookmarks, args).unwrap_or(literal)
        }
    }
}

/// Render a path as `~/relative` when it lives under the home directory;
/// otherwise use the absolute form. Uses forward slashes after the tilde for
/// consistency with the rest of the shell. Only `input_completion` uses this
/// within the lib (the CLI and GUI each have their own copy), so it's
/// crate-private rather than part of the public API.
pub(crate) fn render_with_tilde(p: &Path, home: Option<&Path>) -> String {
    if let Some(home) = home
        && let Ok(rest) = p.strip_prefix(home)
    {
        let rest_str = rest.to_string_lossy().replace('\\', "/");
        if rest_str.is_empty() {
            return "~".to_string();
        }
        return format!("~/{}", rest_str);
    }
    p.display().to_string()
}

/// True when a non-folder entry is "executable" by the shell.
///  - Linux etc: any of the user/group/other +x bits is set.
///  - Windows: the file's extension (case-insensitive) is `.exe` or `.msi`.
///
/// Called from [read_browser_files] for each entry. Kept as a private
/// helper so the cfg-split lives in one spot.
fn is_executable_file(path: &Path, _metadata: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        _metadata.permissions().mode() & 0o111 != 0
    }

    #[cfg(not(unix))]
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_ascii_lowercase())
            .as_deref(),
        Some("exe") | Some("msi")
    )
}

/// Snapshot the contents of `dir` as a `Vec<BrowserFile>`. Folders are
/// sorted first (case-insensitive alphabetical), then files (same order).
/// Errors reading the directory or any individual entry are swallowed —
/// callers get an empty list (or just the entries we could read).
///
/// Used by both the CLI and the GUI to refresh their file-browser view
/// after `cd`-style navigation, so the directory listing stays in lockstep
/// across both binaries.
pub fn read_browser_files(dir: &Path) -> Vec<BrowserFile> {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    let mut files: Vec<BrowserFile> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let disp_name = entry.file_name().to_string_lossy().into_owned();

        // `file_type()` doesn't follow symlinks and is cheap on Unix; fall
        // back to `metadata()` if it fails (rare).
        let file_type = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        let is_folder = file_type.is_dir();

        let is_executable = if is_folder {
            false
        } else {
            match entry.metadata() {
                Ok(md) => is_executable_file(&path, &md),
                Err(_) => false,
            }
        };

        files.push(BrowserFile {
            path,
            disp_name,
            is_folder,
            is_executable,
        });
    }

    // Folders first, then files; both case-insensitive alphabetical.
    files.sort_by(|a, b| {
        b.is_folder
            .cmp(&a.is_folder)
            .then_with(|| a.disp_name.to_lowercase().cmp(&b.disp_name.to_lowercase()))
    });
    files
}

pub fn get_home() -> Option<PathBuf> {
    // Resolve the home directory once; used for bare `cd` and for
    // expanding a leading `~` / `~/...` in the argument.
    env::var_os("USERPROFILE")
        .or_else(|| env::var_os("HOME"))
        .map(PathBuf::from)
}
