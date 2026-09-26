//! Logic related to auto-completing text as the user types, or with the Tab key.

use std::{
    collections::{HashSet, VecDeque},
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use crate::{RecentDir, util};

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
/// completes bookmarked directory names first, then directory entries on disk
/// (including nested relative paths like `code/Bi`), then recent directories,
/// then directories nested anywhere under a bookmark.
pub fn complete_cd_path(
    line: &str,
    pos: usize,
    cwd: &Path,
    home: Option<&Path>,
    bookmarks: &[PathBuf],
    recent_dirs: &[RecentDir],
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

    // Sources in priority order. Match quality ranks first, so a prefix match
    // in the cwd beats a fuzzy match against a bookmark or recent dir; within
    // the same quality, the earlier source wins.
    let recent = if arg.is_empty() {
        Vec::new()
    } else {
        complete_recent_dirs(arg, home, recent_dirs)
    };
    let mut sources = vec![
        complete_bookmarks(arg, home, bookmarks),
        scored_paths(arg, cwd, home, true),
        recent,
    ];
    // The bookmark-subtree walk is the one costly source, so skip it when a
    // cheaper source already has a prefix match: it could never win then.
    // Bare names only; paths like `code/Bi` or `~/x` are resolved on disk.
    let bare_name = !arg.is_empty() && !arg.starts_with('~') && !arg.contains(['/', '\\']);
    if bare_name && !sources.iter().flatten().any(|(score, _)| *score == 0) {
        sources.push(complete_bookmark_descendants(arg, home, bookmarks));
    }

    let candidates = (0..=2)
        .find_map(|tier| {
            sources.iter().find_map(|source| {
                let hits: Vec<CompletionCandidate> = source
                    .iter()
                    .filter(|(score, _)| match_tier(*score) == tier)
                    .map(|(_, c)| c.clone())
                    .collect();
                (!hits.is_empty()).then_some(hits)
            })
        })
        .unwrap_or_default();

    Some(CompletionResult {
        start: arg_start,
        candidates,
    })
}

/// Coarse match quality from a [match_score]: 0 = prefix, 1 = substring,
/// 2 = subsequence (fuzzy).
fn match_tier(score: u32) -> u8 {
    match score {
        0 => 0,
        s if s < SUBSEQ_SCORE => 1,
        _ => 2,
    }
}

fn complete_recent_dirs(
    arg: &str,
    home: Option<&Path>,
    recent_dirs: &[RecentDir],
) -> Vec<(u32, CompletionCandidate)> {
    let mut scored: Vec<(u32, CompletionCandidate)> = recent_dirs
        .iter()
        .filter_map(|r| {
            let name = r.path.file_name()?.to_str()?;
            let score =
                match_score(arg, name).or_else(|| match_score(arg, &r.path.to_string_lossy()))?;
            Some((
                score,
                CompletionCandidate {
                    display: name.to_string(),
                    replacement: util::render_with_tilde(&r.path, home),
                },
            ))
        })
        .collect();
    sort_scored(&mut scored);
    scored
}

#[cfg(test)]
mod recent_completion_tests {
    use chrono::Utc;

    use super::*;

    #[test]
    fn tab_completes_recent_directory() {
        let recent = vec![RecentDir {
            path: PathBuf::from("/work/code"),
            dt: Utc::now(),
        }];
        let result = complete_cd_path("cd cod", 6, Path::new("/work"), None, &[], &recent).unwrap();
        assert_eq!(result.candidates.len(), 1);
        assert!(result.candidates[0].replacement.ends_with("code"));
    }

    #[test]
    fn local_prefix_beats_fuzzy_recent_directory() {
        let cwd = std::env::temp_dir().join("shell_completion_test_local_prefix");
        fs::create_dir_all(cwd.join("openmm")).unwrap();
        let recent = vec![RecentDir {
            path: PathBuf::from("/code/position_mesh/position_mesh_x"),
            dt: Utc::now(),
        }];
        let result = complete_cd_path("cd openm", 8, &cwd, None, &[], &recent).unwrap();
        let _ = fs::remove_dir_all(&cwd);
        assert_eq!(result.candidates.len(), 1);
        assert_eq!(result.candidates[0].replacement, "openmm");
    }
}

fn complete_bookmarks(
    arg: &str,
    home: Option<&Path>,
    bookmarks: &[PathBuf],
) -> Vec<(u32, CompletionCandidate)> {
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
    scored
}

/// How many levels below each bookmark [complete_bookmark_descendants] looks.
const DESCENDANT_MAX_DEPTH: usize = 4;

/// Wall-clock cap on the bookmark-subtree walk, so a huge tree (e.g. a
/// bookmarked home dir) or a slow network drive can't stall the Tab key.
const DESCENDANT_TIME_BUDGET: Duration = Duration::from_millis(300);

/// Directories the bookmark-subtree walk never enters: build output and
/// dependency trees, which are large and never a useful `cd` suggestion.
const DESCENDANT_SKIP: [&str; 5] = [
    "node_modules",
    "target",
    "__pycache__",
    "venv",
    "site-packages",
];

/// Complete against directories nested under any bookmark, so `cd plasc`
/// finds `~/code/Bio/plascad` when `~/code/Bio` is bookmarked. Walks all
/// bookmarks breadth-first together, so a directory under nested bookmarks is
/// visited once, at its shallowest depth. Only prefix and substring matches
/// count: fuzzy matching across whole trees hits almost anything.
///
/// Returns only the shallowest matches of the best quality: a deeper namesake
/// (e.g. an asset folder named after its project) would otherwise stop Tab
/// completing the obvious one.
fn complete_bookmark_descendants(
    arg: &str,
    home: Option<&Path>,
    bookmarks: &[PathBuf],
) -> Vec<(u32, CompletionCandidate)> {
    let deadline = Instant::now() + DESCENDANT_TIME_BUDGET;
    // Bookmarks themselves are covered by [complete_bookmarks]; marking them
    // visited up front also stops one bookmark's walk re-entering another's.
    let mut visited: HashSet<String> = bookmarks.iter().map(|p| visit_key(p)).collect();
    let mut queue: VecDeque<(PathBuf, usize)> = bookmarks.iter().map(|p| (p.clone(), 0)).collect();
    // (score, depth, candidate)
    let mut found: Vec<(u32, usize, CompletionCandidate)> = Vec::new();
    // Depth of the first prefix match. Once that depth is fully read, nothing
    // deeper can be returned, so the walk stops.
    let mut prefix_depth: Option<usize> = None;

    while let Some((dir, depth)) = queue.pop_front() {
        if prefix_depth.is_some_and(|d| depth >= d) || Instant::now() > deadline {
            break;
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|t| t.is_dir()) || is_hidden(&entry) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if DESCENDANT_SKIP.contains(&name.as_str()) {
                continue;
            }
            let path = entry.path();
            if !visited.insert(visit_key(&path)) {
                continue;
            }
            let child_depth = depth + 1;
            if let Some(score) = match_score(arg, &name).filter(|&s| s < SUBSEQ_SCORE) {
                if score == 0 && prefix_depth.is_none() {
                    prefix_depth = Some(child_depth);
                }
                // Show the full path: names like `src` repeat across projects.
                let replacement = util::render_with_tilde(&path, home);
                found.push((
                    score,
                    child_depth,
                    CompletionCandidate {
                        display: replacement.clone(),
                        replacement,
                    },
                ));
            }
            if child_depth < DESCENDANT_MAX_DEPTH {
                queue.push_back((path, child_depth));
            }
        }
    }

    let Some(best) = found.iter().map(|(s, d, _)| (match_tier(*s), *d)).min() else {
        return Vec::new();
    };
    let mut scored: Vec<(u32, CompletionCandidate)> = found
        .into_iter()
        .filter(|(s, d, _)| (match_tier(*s), *d) == best)
        .map(|(s, _, c)| (s, c))
        .collect();
    sort_scored(&mut scored);
    scored
}

/// Key for the subtree walk's visited set. Windows paths are
/// case-insensitive, so `code\bio` and `code\Bio` must collide there.
fn visit_key(p: &Path) -> String {
    let s = p.to_string_lossy();
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s.into_owned()
    }
}

/// Dot-directories everywhere, plus directories with the Hidden attribute on
/// Windows (e.g. `AppData`).
fn is_hidden(entry: &fs::DirEntry) -> bool {
    if entry.file_name().to_string_lossy().starts_with('.') {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
        if entry
            .metadata()
            .is_ok_and(|m| m.file_attributes() & FILE_ATTRIBUTE_HIDDEN != 0)
        {
            return true;
        }
    }
    false
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
    scored_paths(arg, cwd, home, dirs_only)
        .into_iter()
        .map(|(_, c)| c)
        .collect()
}

/// [complete_paths], keeping each candidate's [match_score], best-first.
fn scored_paths(
    arg: &str,
    cwd: &Path,
    home: Option<&Path>,
    dirs_only: bool,
) -> Vec<(u32, CompletionCandidate)> {
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
    scored
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

#[cfg(test)]
mod descendant_completion_tests {
    use super::*;

    #[test]
    fn tab_completes_directory_under_bookmark() {
        let root = std::env::temp_dir().join("shell_completion_test_descendant");
        let bookmark = root.join("code").join("Bio");
        let cwd = root.join("elsewhere");
        fs::create_dir_all(bookmark.join("plascad").join("src")).unwrap();
        // A deeper namesake mustn't turn this into an ambiguous completion.
        fs::create_dir_all(bookmark.join("site").join("images").join("plascad")).unwrap();
        fs::create_dir_all(&cwd).unwrap();
        let result = complete_cd_path(
            "cd plasc",
            8,
            &cwd,
            None,
            std::slice::from_ref(&bookmark),
            &[],
        )
        .unwrap();
        let _ = fs::remove_dir_all(&root);
        assert_eq!(result.candidates.len(), 1);
        assert_eq!(
            result.candidates[0].replacement,
            bookmark.join("plascad").display().to_string()
        );
    }

    #[test]
    fn local_prefix_beats_directory_under_bookmark() {
        let root = std::env::temp_dir().join("shell_completion_test_descendant_local");
        let bookmark = root.join("bm");
        fs::create_dir_all(bookmark.join("plascad")).unwrap();
        fs::create_dir_all(root.join("cwd").join("plascad_local")).unwrap();
        let result =
            complete_cd_path("cd plasc", 8, &root.join("cwd"), None, &[bookmark], &[]).unwrap();
        let _ = fs::remove_dir_all(&root);
        assert_eq!(result.candidates.len(), 1);
        assert_eq!(result.candidates[0].replacement, "plascad_local");
    }
}
