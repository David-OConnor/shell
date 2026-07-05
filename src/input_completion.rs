//! Logic related to auto-completing text as the user types, or with the Tab key.

use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::util;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionResult {
    pub start: usize,
    pub candidates: Vec<CompletionCandidate>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionCandidate {
    pub display: String,
    pub replacement: String,
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

fn complete_bookmarks(
    arg: &str,
    home: Option<&Path>,
    bookmarks: &[PathBuf],
) -> Vec<CompletionCandidate> {
    let mut scored: Vec<(u32, CompletionCandidate)> = bookmarks
        .iter()
        .filter_map(|p| {
            let name = p.file_name()?.to_str()?;
            let score = match_score(arg, name)?;
            Some((
                score,
                CompletionCandidate {
                    display: name.to_string(),
                    replacement: util::render_with_tilde(p, home),
                },
            ))
        })
        .collect();
    sort_scored(&mut scored);
    scored.into_iter().map(|(_, c)| c).collect()
}

/// Complete a command (or argument) written as an explicit path — one starting
/// with `./`, `../`, `~/`, a path separator, or a Windows drive (e.g.
/// `./install_` → `./install_program.sh`). Unlike [complete_cd_path] this
/// matches files as well as directories, so scripts and executables complete.
/// Returns `None` when the word under the cursor isn't such a path.
pub fn complete_command_path(
    line: &str,
    pos: usize,
    cwd: &Path,
    home: Option<&Path>,
) -> Option<CompletionResult> {
    if pos > line.len() || !line.is_char_boundary(pos) {
        return None;
    }

    let before = &line[..pos];
    let trimmed = before.trim_start();
    let leading = before.len() - trimmed.len();

    // Complete only the whitespace-delimited word the cursor sits at the end of.
    let word_offset = trimmed
        .rfind(char::is_whitespace)
        .map(|i| i + 1)
        .unwrap_or(0);
    let word = &trimmed[word_offset..];
    let word_start = leading + word_offset;

    if !is_explicit_path(word) {
        return None;
    }

    Some(CompletionResult {
        start: word_start,
        candidates: complete_paths(word, cwd, home, false),
    })
}

/// True when `word` is written as an explicit filesystem path rather than a
/// bare command name resolved against `PATH`.
fn is_explicit_path(word: &str) -> bool {
    const PREFIXES: [&str; 8] = ["./", ".\\", "../", "..\\", "~/", "~\\", "/", "\\"];
    if PREFIXES.iter().any(|p| word.starts_with(p)) {
        return true;
    }
    // Windows drive-qualified path, e.g. `C:\` or `C:/`.
    let bytes = word.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'/' || bytes[2] == b'\\')
}

fn complete_dirs(arg: &str, cwd: &Path, home: Option<&Path>) -> Vec<CompletionCandidate> {
    complete_paths(arg, cwd, home, true)
}

/// Complete the leaf of `arg` against the directory it points into. When
/// `dirs_only` is set only subdirectories are offered (used by `cd`); otherwise
/// files are offered too and directory names get a trailing `/` so completing
/// into a subdirectory continues naturally.
fn complete_paths(
    arg: &str,
    cwd: &Path,
    home: Option<&Path>,
    dirs_only: bool,
) -> Vec<CompletionCandidate> {
    let (dir_prefix, base_dir, leaf_prefix) = completion_base(arg, cwd, home);
    let entries = match fs::read_dir(base_dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };

    let mut scored: Vec<(u32, CompletionCandidate)> = entries
        .flatten()
        .filter_map(|entry| {
            let is_dir = entry.file_type().ok()?.is_dir();
            if dirs_only && !is_dir {
                return None;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let score = match_score(leaf_prefix, &name)?;
            // `cd` keeps its bare-name display; file completion marks dirs with
            // a trailing slash in both the shown name and the replacement.
            let suffix = if is_dir && !dirs_only { "/" } else { "" };
            Some((
                score,
                CompletionCandidate {
                    display: format!("{name}{suffix}"),
                    replacement: format!("{dir_prefix}{name}{suffix}"),
                },
            ))
        })
        .collect();
    sort_scored(&mut scored);
    scored.into_iter().map(|(_, c)| c).collect()
}

/// Score how well candidate `name` matches the typed `needle`, compared
/// case-insensitively. Lower is better; `None` means no match. Best → worst:
/// prefix match, then substring (earlier position preferred), then subsequence
/// (fuzzy — `needle`'s chars appear in order). An empty needle matches
/// everything with the best score.
fn match_score(needle: &str, name: &str) -> Option<u32> {
    if needle.is_empty() {
        return Some(0);
    }
    let n = needle.to_lowercase();
    let h = name.to_lowercase();
    if h.starts_with(&n) {
        Some(0)
    } else if let Some(idx) = h.find(&n) {
        // +1 keeps every substring match worse than any prefix match; the
        // position term prefers earlier matches and is capped so a substring
        // always outranks a pure subsequence (fuzzy) match.
        Some(1 + (idx as u32).min(SUBSEQ_SCORE - 2))
    } else if is_subsequence(&n, &h) {
        Some(SUBSEQ_SCORE)
    } else {
        None
    }
}

/// Score assigned to a fuzzy (subsequence) match — worse than any prefix or
/// substring match (see [match_score]).
const SUBSEQ_SCORE: u32 = 1_000;

/// True when every char of `needle` appears in `haystack` in order — a fuzzy
/// subsequence match. Both arguments are expected pre-lowercased.
fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut hay = haystack.chars();
    'next: for nc in needle.chars() {
        for hc in hay.by_ref() {
            if hc == nc {
                continue 'next;
            }
        }
        return false;
    }
    true
}

/// Sort scored candidates best-first, breaking ties alphabetically by display
/// name so ordering within a score band is stable and predictable.
fn sort_scored(scored: &mut [(u32, CompletionCandidate)]) {
    scored.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.display.to_lowercase().cmp(&b.1.display.to_lowercase()))
    });
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
