//! Misc utility functionality.

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

use crate::state::BrowserFile;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionCandidate {
    pub display: String,
    pub replacement: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionResult {
    pub start: usize,
    pub candidates: Vec<CompletionCandidate>,
}

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

/// Shared `cd` autocomplete used by both the CLI and GUI frontends. It
/// completes bookmarked directory names first, then falls back to directory
/// entries on disk, including nested relative paths like `code/Bi`.
pub fn complete_cd_path(
    line: &str,
    pos: usize,
    cwd: &Path,
    home: Option<&Path>,
    bookmarks: &[PathBuf],
) -> Option<CompletionResult> {
    if pos > line.len() || !line.is_char_boundary(pos) {
        return None;
    }

    let before = &line[..pos];
    let trimmed = before.trim_start();
    let leading = before.len() - trimmed.len();

    let i = trimmed.find(char::is_whitespace)?;
    let cmd_part = &trimmed[..i];
    if cmd_part != "cd" {
        return None;
    }

    let rest = &trimmed[i..];
    let arg = rest.trim_start();
    let arg_start = leading + (trimmed.len() - arg.len());

    let mut candidates = complete_bookmarks(arg, home, bookmarks);
    if candidates.is_empty() {
        candidates = complete_dirs(arg, cwd, home);
    }

    Some(CompletionResult {
        start: arg_start,
        candidates,
    })
}

/// Apply a completion to an input line, using the sole candidate when there
/// is one or the shared prefix of multiple candidates when possible.
pub fn apply_completion(line: &str, pos: usize, completion: &CompletionResult) -> Option<String> {
    if completion.candidates.is_empty()
        || completion.start > pos
        || pos > line.len()
        || !line.is_char_boundary(pos)
        || !line.is_char_boundary(completion.start)
    {
        return None;
    }

    let mut replacement = completion.candidates[0].replacement.clone();
    for candidate in completion.candidates.iter().skip(1) {
        truncate_to_common_prefix(&mut replacement, &candidate.replacement);
    }

    if replacement == line[completion.start..pos] {
        return None;
    }

    let mut out = line.to_string();
    out.replace_range(completion.start..pos, &replacement);
    Some(out)
}

fn complete_bookmarks(
    arg: &str,
    home: Option<&Path>,
    bookmarks: &[PathBuf],
) -> Vec<CompletionCandidate> {
    let needle = arg.to_lowercase();
    bookmarks
        .iter()
        .filter_map(|p| {
            let name = p.file_name()?.to_str()?;
            if name.to_lowercase().starts_with(&needle) {
                Some(CompletionCandidate {
                    display: name.to_string(),
                    replacement: render_with_tilde(p, home),
                })
            } else {
                None
            }
        })
        .collect()
}

fn complete_dirs(arg: &str, cwd: &Path, home: Option<&Path>) -> Vec<CompletionCandidate> {
    let (dir_prefix, base_dir, leaf_prefix) = completion_base(arg, cwd, home);
    let entries = match fs::read_dir(base_dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    let leaf_prefix = leaf_prefix.to_lowercase();

    let mut candidates: Vec<CompletionCandidate> = entries
        .flatten()
        .filter_map(|entry| {
            if !entry.file_type().ok()?.is_dir() {
                return None;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.to_lowercase().starts_with(&leaf_prefix) {
                Some(CompletionCandidate {
                    display: name.clone(),
                    replacement: format!("{dir_prefix}{name}"),
                })
            } else {
                None
            }
        })
        .collect();

    candidates.sort_by(|a, b| a.display.to_lowercase().cmp(&b.display.to_lowercase()));
    candidates
}

fn completion_base<'a>(
    arg: &'a str,
    cwd: &Path,
    home: Option<&Path>,
) -> (String, PathBuf, &'a str) {
    let Some(sep_idx) = arg.rfind(['/', '\\']) else {
        return (String::new(), cwd.to_path_buf(), arg);
    };

    let dir_prefix = &arg[..=sep_idx];
    let base_arg = if sep_idx == 0 || arg.as_bytes().get(sep_idx.wrapping_sub(1)) == Some(&b':') {
        dir_prefix
    } else {
        &arg[..sep_idx]
    };
    let leaf_prefix = &arg[sep_idx + 1..];

    let base_dir = if base_arg == "~/" || base_arg == "~\\" {
        home.map(Path::to_path_buf)
            .unwrap_or_else(|| cwd.join(base_arg))
    } else if let Some(rest) = base_arg
        .strip_prefix("~/")
        .or_else(|| base_arg.strip_prefix("~\\"))
    {
        home.map(|h| h.join(rest))
            .unwrap_or_else(|| cwd.join(base_arg))
    } else {
        let p = Path::new(base_arg);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            cwd.join(p)
        }
    };

    (dir_prefix.to_string(), base_dir, leaf_prefix)
}

fn truncate_to_common_prefix(a: &mut String, b: &str) {
    let mut end = 0;
    for ((a_idx, a_ch), (_, b_ch)) in a.char_indices().zip(b.char_indices()) {
        if a_ch != b_ch {
            break;
        }
        end = a_idx + a_ch.len_utf8();
    }
    a.truncate(end);
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn test_root(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        env::temp_dir().join(format!("shell_{name}_{}_{}", std::process::id(), stamp))
    }

    #[test]
    fn cd_completion_jumps_into_nested_relative_dir() {
        let root = test_root("nested_relative");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("code").join("Bio")).unwrap();

        let line = "cd code/Bi";
        let result = complete_cd_path(line, line.len(), &root, None, &[]).unwrap();

        assert_eq!(result.start, 3);
        assert_eq!(
            result.candidates,
            vec![CompletionCandidate {
                display: "Bio".to_string(),
                replacement: "code/Bio".to_string(),
            }]
        );
        assert_eq!(
            apply_completion(line, line.len(), &result),
            Some("cd code/Bio".to_string())
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn cd_completion_jumps_into_nested_home_relative_dir() {
        let root = test_root("nested_home");
        let home = root.join("home");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(home.join("code").join("Bio")).unwrap();

        let line = "cd ~/code/Bi";
        let result = complete_cd_path(line, line.len(), &root, Some(&home), &[]).unwrap();

        assert_eq!(
            result.candidates,
            vec![CompletionCandidate {
                display: "Bio".to_string(),
                replacement: "~/code/Bio".to_string(),
            }]
        );
        assert_eq!(
            apply_completion(line, line.len(), &result),
            Some("cd ~/code/Bio".to_string())
        );

        let _ = fs::remove_dir_all(&root);
    }
}

/// Maximum number of branch-name characters shown in the prompt before we
/// truncate with `...`. Kept in the lib so the CLI and GUI agree on the
/// exact form of the indicator (which the CLI's highlighter has to parse).
pub const BRANCH_NAME_MAX: usize = 10;

/// Prefix used in the assembled prompt for the git-branch indicator, e.g.
/// `S <cwd> branch: main $`. The leading space is part of the marker so a
/// cwd that happens to contain "branch:" mid-path doesn't collide.
pub const BRANCH_PREFIX: &str = " branch: ";

/// Detect the current git branch by shelling out to `git rev-parse
/// --abbrev-ref HEAD` from `cwd`. Returns `None` when:
///   * `cwd` isn't inside a git repo (git exits non-zero),
///   * git isn't on PATH (spawn fails),
///   * the trimmed output is empty.
///
/// For a detached HEAD this returns `Some("HEAD")` — the caller decides
/// whether to display that as-is or replace it with a short SHA.
pub fn current_branch(cwd: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

/// Clip `name` to `max` characters, appending `...` when truncation
/// happened. Counts Unicode scalars rather than bytes so a long branch
/// containing multi-byte characters doesn't get cut mid-codepoint.
pub fn truncate_branch(name: &str, max: usize) -> String {
    let chars: Vec<char> = name.chars().collect();
    if chars.len() <= max {
        name.to_string()
    } else {
        let prefix: String = chars.into_iter().take(max).collect();
        format!("{prefix}...")
    }
}

/// Render the branch slot for a prompt: empty string when there's no
/// branch, or ` branch: NAME` (with a leading space) using
/// [BRANCH_NAME_MAX]-char truncation. Used by both shells so the form
/// stays in sync with what the CLI highlighter looks for.
pub fn branch_indicator(branch: Option<&str>) -> String {
    match branch {
        Some(b) => format!("{BRANCH_PREFIX}{}", truncate_branch(b, BRANCH_NAME_MAX)),
        None => String::new(),
    }
}

/// True when a non-folder entry is "executable" by the shell.
///   * Unix: any of the user/group/other +x bits is set.
///   * Windows: the file's extension (case-insensitive) is `.exe` or `.msi`.
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
