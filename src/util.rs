//! Misc utility functionality.

use std::{
    env, fs,
    path::{Path, PathBuf},
};

use crate::state::BrowserFile;

/// Resolve a `cd`/`cat`-style path argument against the shell's state:
/// expands `~`/`~/...` to the home dir, treats real paths literally, and
/// falls back to a case-insensitive prefix match against bookmarked
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
        // back to a prefix-match against bookmarked directories
        // (matched against the bookmark's final path component,
        // case-insensitive).
        let literal = cwd.join(args);
        if literal.is_dir() {
            literal
        } else {
            let needle = args.to_lowercase();
            bookmarks
                .iter()
                .find(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .map(|n| n.to_lowercase().starts_with(&needle))
                        .unwrap_or(false)
                })
                .cloned()
                .unwrap_or(literal)
        }
    }
}

/// Render a path as `~/relative` when it lives under the home directory;
/// otherwise use the absolute form. Uses forward slashes after the tilde for
/// consistency with the rest of the shell.
pub fn render_with_tilde(p: &Path, home: Option<&Path>) -> String {
    if let Some(home) = home {
        if let Ok(rest) = p.strip_prefix(home) {
            let rest_str = rest.to_string_lossy().replace('\\', "/");
            if rest_str.is_empty() {
                return "~".to_string();
            }
            return format!("~/{}", rest_str);
        }
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
    {
        match path
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_ascii_lowercase())
            .as_deref()
        {
            Some("exe") | Some("msi") => true,
            _ => false,
        }
    }
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
