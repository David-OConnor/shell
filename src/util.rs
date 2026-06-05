//! Misc utility functionality.

use std::{
    env,
    path::{Path, PathBuf},
};

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

pub fn get_home() -> Option<PathBuf> {
    // Resolve the home directory once; used for bare `cd` and for
    // expanding a leading `~` / `~/...` in the argument.
    env::var_os("USERPROFILE")
        .or_else(|| env::var_os("HOME"))
        .map(PathBuf::from)
}
