//! Logic related to auto-completing text as the user types, or with the Tab key.

use std::{
    collections::{HashSet, VecDeque},
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use crate::{HistoryItem, RecentDir, common_dirs::common_dirs, git, util};

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

/// What Tab completion and the greyed suggestion draw on besides the line
/// itself. The CLI and GUI frontends fill it from the same state, so both
/// complete and suggest alike.
#[derive(Clone, Copy)]
pub struct CompletionSources<'a> {
    pub cwd: &'a Path,
    pub home: Option<&'a Path>,
    pub bookmarks: &'a [PathBuf],
    pub recent_dirs: &'a [RecentDir],
    pub clone_roots: &'a [String],
    pub history: &'a [HistoryItem],
    /// Set while commands run on an SSH remote. Local files say nothing
    /// about it, so Tab completes nothing and only history is suggested.
    pub remote: bool,
}

/// Tab completion for `line` with the caret at `pos`, trying in order: `cd`
/// paths (see [complete_cd_path]), `clone` repo URLs, explicit paths
/// (`./script.sh`), then files and folders.
pub fn complete_input(line: &str, pos: usize, src: &CompletionSources) -> Option<CompletionResult> {
    complete_ranked(line, pos, src, DESCENDANT_TIME_BUDGET).map(|(result, _)| result)
}

/// [complete_input], with the worst [match_tier] among its candidates.
/// `walk_budget` caps the bookmark-subtree walk (see
/// [complete_bookmark_descendants]).
fn complete_ranked(
    line: &str,
    pos: usize,
    src: &CompletionSources,
    walk_budget: Duration,
) -> Option<(CompletionResult, u8)> {
    if src.remote {
        return None;
    }
    if let Some((start, arg)) = cd_arg(line, pos)
        && let Some((tier, candidates)) = cd_candidates(
            arg,
            src.cwd,
            src.home,
            src.bookmarks,
            src.recent_dirs,
            walk_budget,
        )
    {
        return Some((CompletionResult { start, candidates }, tier));
    }
    // Clone roots and filenames match by prefix only.
    if let Some(result) =
        complete_clone_url(line, pos, src.clone_roots).filter(|r| !r.candidates.is_empty())
    {
        return Some((result, 0));
    }
    if let Some((start, scored)) = command_path_scored(line, pos, src.cwd, src.home)
        && !scored.is_empty()
    {
        let tier = scored
            .iter()
            .map(|(s, _)| match_tier(*s))
            .max()
            .unwrap_or(0);
        let candidates = scored.into_iter().map(|(_, c)| c).collect();
        return Some((CompletionResult { start, candidates }, tier));
    }
    complete_filename(line, pos, src.cwd, src.home).map(|result| (result, 0))
}

/// The greyed suggestion shown after the caret, which →, End and Ctrl+E
/// accept. See [input_hint].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputHint {
    /// What's shown: the rest of the line, or, for a completion that rewrites
    /// the word being typed, `→` and what it becomes.
    pub display: String,
    /// The input line once the suggestion is accepted.
    pub line: String,
}

/// How long the suggestion may spend on the bookmark-subtree walk. It runs
/// on every keystroke, so much less than Tab's [DESCENDANT_TIME_BUDGET].
const HINT_WALK_BUDGET: Duration = Duration::from_millis(25);

/// The suggestion for `line` with the caret at `pos`, as both frontends show
/// it, fish-style: only with the caret at the end of the line. First the rest
/// of the newest past command that starts with the line. Failing that,
/// Tab's completion, when it has just one candidate and that isn't a fuzzy
/// match: `cd ~/strike/White Wedding/ew` suggests `-debrief`, and
/// `cd wedding/ew`, which Tab rewrites, `→ ~/strike/White Wedding/ew-debrief`.
pub fn input_hint(line: &str, pos: usize, src: &CompletionSources) -> Option<InputHint> {
    if line.is_empty() || pos != line.len() {
        return None;
    }

    // Only the newest match counts, as with rustyline's history hints: if
    // that's the line itself, nothing from history is suggested.
    if let Some(entry) = src.history.iter().rev().find(|h| h.text.starts_with(line))
        && entry.text != line
    {
        return Some(InputHint {
            display: entry.text[line.len()..].to_string(),
            line: entry.text.clone(),
        });
    }

    let (result, tier) = complete_ranked(line, pos, src, HINT_WALK_BUDGET)?;
    let [only] = result.candidates.as_slice() else {
        return None;
    };
    if tier == 2 || !line.is_char_boundary(result.start) {
        return None;
    }
    let typed = &line[result.start..pos];
    let mut accepted = line.to_string();
    accepted.replace_range(result.start..pos, &only.replacement);
    // Case-insensitive, as matching is: `white wedding/ew` carries on to
    // `-debrief`, and accepting it fixes the case.
    let carries_on = only
        .replacement
        .get(..typed.len())
        .is_some_and(|head| head.to_lowercase() == typed.to_lowercase());
    let display = if carries_on {
        only.replacement[typed.len()..].to_string()
    } else {
        format!(" → {}", only.replacement)
    };
    (!display.is_empty() && accepted != line).then_some(InputHint {
        display,
        line: accepted,
    })
}

/// The last [input_hint] worked out, for the line, cwd and remote state it
/// was for. Frontends ask on every redraw, and working it out reads the disk.
#[derive(Default)]
pub struct HintCache(Option<(String, PathBuf, bool, Option<InputHint>)>);

impl HintCache {
    /// [input_hint], reusing the last one when nothing it depends on changed.
    pub fn get(&mut self, line: &str, pos: usize, src: &CompletionSources) -> Option<InputHint> {
        if pos != line.len() {
            return None;
        }
        if let Some((l, cwd, remote, hint)) = &self.0
            && l == line
            && cwd == src.cwd
            && *remote == src.remote
        {
            return hint.clone();
        }
        let hint = input_hint(line, pos, src);
        self.0 = Some((
            line.to_string(),
            src.cwd.to_path_buf(),
            src.remote,
            hint.clone(),
        ));
        hint
    }

    /// The last suggestion worked out, if it was for `line`: the one on
    /// screen, for a key handler to accept.
    pub fn last_for(&self, line: &str) -> Option<&InputHint> {
        match &self.0 {
            Some((l, _, _, hint)) if l == line => hint.as_ref(),
            _ => None,
        }
    }
}

/// Shared `cd` autocomplete used by both the CLI and GUI frontends. It
/// completes bookmarked directory names first, then directory entries on disk
/// (including nested relative paths like `code/Bi`), then paths under a
/// bookmark or recent dir named by their first segment (`wedding/ew`), then
/// recent directories, then directories nested anywhere under a bookmark,
/// then the OS's standard directories (see [common_dirs]).
pub fn complete_cd_path(
    line: &str,
    pos: usize,
    cwd: &Path,
    home: Option<&Path>,
    bookmarks: &[PathBuf],
    recent_dirs: &[RecentDir],
) -> Option<CompletionResult> {
    let (start, arg) = cd_arg(line, pos)?;
    let candidates = cd_candidates(
        arg,
        cwd,
        home,
        bookmarks,
        recent_dirs,
        DESCENDANT_TIME_BUDGET,
    )
    .map(|(_, c)| c)
    .unwrap_or_default();
    Some(CompletionResult { start, candidates })
}

/// Where `cd <arg>` goes when `arg` names no directory as written, nor as a
/// bookmark, recent or standard directory: the one directory Tab would
/// complete it to, e.g. `~/strike/White Wedding/ew` →
/// `~/strike/White Wedding/ew-debrief`. Fuzzy matches don't count. `Ok(None)`
/// when there's no such directory; `Err` with the candidates' names when there
/// are several.
pub(crate) fn cd_completion_target(
    arg: &str,
    cwd: &Path,
    home: Option<&Path>,
    bookmarks: &[PathBuf],
    recent_dirs: &[RecentDir],
) -> Result<Option<PathBuf>, Vec<String>> {
    let Some((tier, candidates)) = cd_candidates(
        arg,
        cwd,
        home,
        bookmarks,
        recent_dirs,
        DESCENDANT_TIME_BUDGET,
    ) else {
        return Ok(None);
    };
    match candidates.as_slice() {
        _ if tier == 2 => Ok(None),
        [only] => Ok(Some(util::path_from_args(
            home,
            cwd,
            &[],
            &only.replacement,
        ))),
        many => Err(many.iter().map(|c| c.display.clone()).collect()),
    }
}

/// The argument of a `cd` line, up to `pos`, and its byte offset. `None` when
/// the line isn't a `cd` command.
fn cd_arg(line: &str, pos: usize) -> Option<(usize, &str)> {
    if pos > line.len() || !line.is_char_boundary(pos) {
        return None;
    }

    let before = &line[..pos];
    let trimmed = before.trim_start();
    let leading = before.len() - trimmed.len();

    let i = trimmed.find(char::is_whitespace)?;
    if &trimmed[..i] != "cd" {
        return None;
    }

    let arg = trimmed[i..].trim_start();
    Some((leading + (trimmed.len() - arg.len()), arg))
}

/// [complete_cd_path]'s candidates for `arg`, with their match tier (see
/// [match_tier]), or `None` when nothing matches. `walk_budget` caps the
/// bookmark-subtree walk (see [complete_bookmark_descendants]).
fn cd_candidates(
    arg: &str,
    cwd: &Path,
    home: Option<&Path>,
    bookmarks: &[PathBuf],
    recent_dirs: &[RecentDir],
    walk_budget: Duration,
) -> Option<(u8, Vec<CompletionCandidate>)> {
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
        complete_under_saved_dir(arg, cwd, home, bookmarks, recent_dirs),
        recent,
    ];
    // The bookmark-subtree walk is the one costly source, so skip it when a
    // cheaper source already has a prefix match: it could never win then.
    // Bare names only; paths like `code/Bi` or `~/x` are resolved on disk.
    let bare_name = !arg.is_empty() && !arg.starts_with('~') && !arg.contains(['/', '\\']);
    if bare_name && !sources.iter().flatten().any(|(score, _)| *score == 0) {
        sources.push(complete_bookmark_descendants(
            arg,
            home,
            bookmarks,
            walk_budget,
        ));
    }
    // Last, so anything of the user's own wins a tie with e.g. `/etc`.
    if bare_name {
        sources.push(complete_common_dirs(arg, home));
    }

    (0..=2).find_map(|tier| {
        sources.iter().find_map(|source| {
            let hits: Vec<CompletionCandidate> = source
                .iter()
                .filter(|(score, _)| match_tier(*score) == tier)
                .map(|(_, c)| c.clone())
                .collect();
            (!hits.is_empty()).then_some((tier, hits))
        })
    })
}

/// Complete a relative path whose first segment isn't in the cwd but names a
/// bookmark, recent dir or standard dir, inside that directory: with
/// `~/strike/White Wedding` bookmarked, `wedding/ew` →
/// `~/strike/White Wedding/ew-debrief`, from anywhere.
fn complete_under_saved_dir(
    arg: &str,
    cwd: &Path,
    home: Option<&Path>,
    bookmarks: &[PathBuf],
    recent_dirs: &[RecentDir],
) -> Vec<(u32, CompletionCandidate)> {
    let Some(i) = arg.find(['/', '\\']) else {
        return Vec::new();
    };
    let (head, tail) = (&arg[..i], &arg[i + 1..]);
    // `~/x`, `./x`, `/x` and `C:/x` already say where they are.
    if head.is_empty() || head.starts_with(['~', '.']) || head.ends_with(':') {
        return Vec::new();
    }
    if cwd.join(head).is_dir() {
        return Vec::new();
    }
    let Some(base) = saved_dir_named(head, home, bookmarks, recent_dirs) else {
        return Vec::new();
    };
    let rendered = util::render_with_tilde(&base, home);
    let sep = if rendered.starts_with('~') {
        '/'
    } else {
        std::path::MAIN_SEPARATOR
    };
    scored_paths(&format!("{rendered}{sep}{tail}"), cwd, home, true)
}

/// The bookmark, recent dir or standard dir whose name best matches `name`:
/// prefix before substring, then the shortest name. Within a tier, bookmarks
/// beat recent dirs, which beat standard dirs.
fn saved_dir_named(
    name: &str,
    home: Option<&Path>,
    bookmarks: &[PathBuf],
    recent_dirs: &[RecentDir],
) -> Option<PathBuf> {
    // Newest first, so they win ties.
    let bookmark =
        util::ranked_path_match(bookmarks.iter().rev().map(PathBuf::as_path), name, false);
    let recent = util::ranked_path_match(
        recent_dirs.iter().rev().map(|r| r.path.as_path()),
        name,
        false,
    );
    // `min_by_key` keeps the first of equal minimums.
    if let Some((_, p)) = [bookmark, recent]
        .into_iter()
        .flatten()
        .min_by_key(|((tier, _), _)| *tier)
    {
        return Some(p.to_path_buf());
    }
    let common = common_dirs(home);
    util::ranked_path_match(common.iter().map(PathBuf::as_path), name, false)
        .map(|(_, p)| p.to_path_buf())
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

/// Complete against the OS's standard directories by name, e.g. `cd program f`
/// → `C:\Program Files`. Shows full paths, as names like `Local` or `log`
/// say little on their own.
fn complete_common_dirs(arg: &str, home: Option<&Path>) -> Vec<(u32, CompletionCandidate)> {
    let mut scored: Vec<(u32, CompletionCandidate)> = common_dirs(home)
        .iter()
        .filter_map(|p| {
            let score = match_score(arg, p.file_name()?.to_str()?)?;
            let replacement = util::render_with_tilde(p, home);
            Some((
                score,
                CompletionCandidate {
                    display: replacement.clone(),
                    replacement,
                },
            ))
        })
        .collect();
    sort_scored(&mut scored);
    scored
}

#[cfg(test)]
mod common_completion_tests {
    use super::*;

    /// No bookmarks or recent dirs, and nothing matching in the cwd.
    fn complete_from_nowhere(line: &str) -> Vec<String> {
        let cwd = std::env::temp_dir().join("shell_completion_test_common");
        fs::create_dir_all(&cwd).unwrap();
        let result = complete_cd_path(line, line.len(), &cwd, None, &[], &[]).unwrap();
        result
            .candidates
            .into_iter()
            .map(|c| c.replacement)
            .collect()
    }

    #[cfg(windows)]
    #[test]
    fn tab_completes_program_files() {
        let program_files = std::env::var("ProgramFiles").unwrap();
        let candidates = complete_from_nowhere("cd program f");
        assert_eq!(candidates.first(), Some(&program_files));
    }

    #[cfg(unix)]
    #[test]
    fn tab_completes_etc() {
        assert_eq!(complete_from_nowhere("cd etc"), vec!["/etc".to_string()]);
    }
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
/// completing the obvious one. Gives up after `budget`, with what it found by
/// then.
fn complete_bookmark_descendants(
    arg: &str,
    home: Option<&Path>,
    bookmarks: &[PathBuf],
    budget: Duration,
) -> Vec<(u32, CompletionCandidate)> {
    let deadline = Instant::now() + budget;
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
pub(crate) fn is_hidden(entry: &fs::DirEntry) -> bool {
    entry.file_name().to_string_lossy().starts_with('.') || util::has_hidden_attr(entry)
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
    let (start, scored) = command_path_scored(line, pos, cwd, home)?;
    Some(CompletionResult {
        start,
        candidates: scored.into_iter().map(|(_, c)| c).collect(),
    })
}

/// [complete_command_path]'s word start and candidates, keeping each
/// candidate's [match_score].
fn command_path_scored(
    line: &str,
    pos: usize,
    cwd: &Path,
    home: Option<&Path>,
) -> Option<(usize, Vec<(u32, CompletionCandidate)>)> {
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

    Some((word_start, scored_paths(word, cwd, home, false)))
}

/// Complete the repository argument of `git clone` / `clone` from the saved
/// clone roots (oldest first; see [crate::state::State::clone_roots]). A bare repo
/// name completes to the newest root, e.g. `clone shell` →
/// `clone https://github.com/david-oconnor/shell`. An empty argument, or the
/// start of a root, completes to the matching roots with a trailing `/`,
/// newest first, so a different root can be picked before typing the name.
/// Returns `None` when the word under the cursor isn't the repository.
pub fn complete_clone_url(line: &str, pos: usize, roots: &[String]) -> Option<CompletionResult> {
    if roots.is_empty() || pos > line.len() || !line.is_char_boundary(pos) {
        return None;
    }

    let before = &line[..pos];
    // The word the cursor sits at the end of; empty right after a space.
    let word_start = before.trim_end_matches(|c: char| !c.is_whitespace()).len();
    let word = &before[word_start..];
    let prior: Vec<&str> = before[..word_start].split_whitespace().collect();
    if word.starts_with('-') || git::clone_repo_index(&prior) != Some(prior.len()) {
        return None;
    }

    let typed = word.to_lowercase();
    let mut candidates: Vec<CompletionCandidate> = roots
        .iter()
        .rev()
        .filter(|root| root.to_lowercase().starts_with(&typed))
        .map(|root| format!("{root}/"))
        .map(|url| CompletionCandidate {
            display: url.clone(),
            replacement: url,
        })
        .collect();
    if candidates.is_empty()
        && git::is_bare_repo_name(word)
        && let Some(newest) = roots.last()
    {
        let url = format!("{newest}/{word}");
        candidates.push(CompletionCandidate {
            display: url.clone(),
            replacement: url,
        });
    }

    Some(CompletionResult {
        start: word_start,
        candidates,
    })
}

#[cfg(test)]
mod clone_completion_tests {
    use super::*;

    fn roots() -> Vec<String> {
        vec![
            "git@github.com:rust-lang".to_string(),
            "https://github.com/david-oconnor".to_string(),
        ]
    }

    fn replacements(line: &str) -> Option<Vec<String>> {
        let result = complete_clone_url(line, line.len(), &roots())?;
        Some(
            result
                .candidates
                .into_iter()
                .map(|c| c.replacement)
                .collect(),
        )
    }

    #[test]
    fn bare_name_completes_to_newest_root() {
        let line = "clone shel";
        let result = complete_clone_url(line, line.len(), &roots()).unwrap();
        assert_eq!(result.start, 6);
        assert_eq!(
            apply_completion(line, line.len(), &result).as_deref(),
            Some("clone https://github.com/david-oconnor/shel")
        );
        assert_eq!(
            replacements("git clone -b main shel"),
            Some(vec!["https://github.com/david-oconnor/shel".to_string()])
        );
    }

    #[test]
    fn empty_or_partial_url_lists_roots() {
        assert_eq!(
            replacements("clone "),
            Some(vec![
                "https://github.com/david-oconnor/".to_string(),
                "git@github.com:rust-lang/".to_string(),
            ])
        );
        assert_eq!(
            replacements("git clone git@"),
            Some(vec!["git@github.com:rust-lang/".to_string()])
        );
    }

    #[test]
    fn ignores_other_words() {
        assert_eq!(replacements("clone"), None);
        assert_eq!(replacements("clone -b "), None);
        assert_eq!(replacements("clone --dep"), None);
        assert_eq!(replacements("git status "), None);
        assert_eq!(replacements("clone shell my_di"), None);
        assert_eq!(complete_clone_url("clone shel", 10, &[]), None);
    }
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

/// Fallback Tab completion: the word before the caret completes against
/// files and folders in the directory it points into (the cwd for a bare
/// name), by case-folded prefix. Folders get a trailing `/` so completion
/// continues into them. A name with a space is opened with a `"`, and inside
/// an open quote the word runs back to that quote, so `python "White W`
/// completes too. Files close the quote; folders leave it open for the next
/// Tab.
fn complete_filename(
    line: &str,
    pos: usize,
    cwd: &Path,
    home: Option<&Path>,
) -> Option<CompletionResult> {
    let before = line.get(..pos)?;
    let quote = open_quote(before);
    let start = match quote {
        Some((i, _)) => i + 1,
        None => before.rfind(char::is_whitespace).map_or(0, |i| i + 1),
    };

    let word = &before[start..];
    let (dir_part, leaf) = match word.rfind(['/', '\\']) {
        Some(i) => (&word[..=i], &word[i + 1..]),
        None => ("", word),
    };

    let dir = match dir_part
        .strip_prefix("~/")
        .or_else(|| dir_part.strip_prefix("~\\"))
    {
        Some(rest) => home?.join(rest),
        None => cwd.join(dir_part),
    };

    let leaf = fold_case(leaf);

    let mut candidates: Vec<CompletionCandidate> = fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if !fold_case(&name).starts_with(&leaf) {
                return None;
            }
            let is_dir = fs::metadata(e.path()).ok()?.is_dir();
            let mut replacement = format!("{dir_part}{name}");
            if is_dir {
                replacement.push('/');
            }
            match quote {
                Some((_, q)) if !is_dir => replacement.push(q),
                None if replacement.contains(' ') => {
                    replacement.insert(0, '"');
                    if !is_dir {
                        replacement.push('"');
                    }
                }
                _ => (),
            }
            let display = if is_dir { format!("{name}/") } else { name };
            Some(CompletionCandidate {
                display,
                replacement,
            })
        })
        .collect();
    candidates.sort_by(|a, b| a.display.cmp(&b.display));
    Some(CompletionResult { start, candidates })
}

/// Byte index and char of a quote (`"` or `'`) that `s` leaves unclosed.
fn open_quote(s: &str) -> Option<(usize, char)> {
    let mut open = None;
    for (i, c) in s.char_indices() {
        match open {
            Some((_, q)) if c == q => open = None,
            None if c == '"' || c == '\'' => open = Some((i, c)),
            _ => (),
        }
    }
    open
}

/// Filenames match case-insensitively where the filesystem is.
fn fold_case(s: &str) -> String {
    if cfg!(any(windows, target_os = "macos")) {
        s.to_lowercase()
    } else {
        s.to_string()
    }
}

/// Complete the leaf of `arg` against the directory it points into, keeping
/// each candidate's [match_score], best-first. When `dirs_only` is set only
/// subdirectories are offered (used by `cd`); otherwise files are offered too
/// and directory names get a trailing `/` so completing into a subdirectory
/// continues naturally.
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

#[cfg(test)]
mod saved_dir_completion_tests {
    use super::*;

    /// A home with `~/strike/White Wedding/{ew-debrief, wsv-tool}`, the
    /// latter bookmarked, and a cwd elsewhere. Returns (home, cwd, bookmark).
    fn setup(name: &str) -> (PathBuf, PathBuf, PathBuf) {
        let home = std::env::temp_dir().join(name);
        let _ = fs::remove_dir_all(&home);
        let ww = home.join("strike").join("White Wedding");
        fs::create_dir_all(ww.join("ew-debrief").join("src")).unwrap();
        fs::create_dir_all(ww.join("wsv-tool")).unwrap();
        let cwd = home.join("elsewhere");
        fs::create_dir_all(&cwd).unwrap();
        (home, cwd, ww)
    }

    fn sources<'a>(
        cwd: &'a Path,
        home: &'a Path,
        bookmarks: &'a [PathBuf],
        history: &'a [HistoryItem],
    ) -> CompletionSources<'a> {
        CompletionSources {
            cwd,
            home: Some(home),
            bookmarks,
            recent_dirs: &[],
            clone_roots: &[],
            history,
            remote: false,
        }
    }

    #[test]
    fn hint_suggests_tabs_only_completion() {
        let (home, cwd, ww) = setup("shell_completion_test_hint");
        let bookmarks = std::slice::from_ref(&ww);
        let src = sources(&cwd, &home, bookmarks, &[]);
        let hint = |line: &str| input_hint(line, line.len(), &src);
        let ew = hint("cd ~/strike/White Wedding/ew");
        let ew_case = hint("cd ~/strike/white wedding/EW");
        let under_bookmark = hint("cd wedding/ew");
        let bookmark = hint("cd wedd");
        let ambiguous = hint("cd ~/strike/White Wedding/");
        let done = hint("cd ~/strike/White Wedding/ew-debrief");
        let mid_line = input_hint("cd ~/strike/White Wedding/ew", 5, &src);
        let remote = input_hint(
            "cd wedd",
            7,
            &CompletionSources {
                remote: true,
                ..src
            },
        );
        let _ = fs::remove_dir_all(&home);

        let hint = |display: &str, line: &str| {
            Some(InputHint {
                display: display.to_string(),
                line: line.to_string(),
            })
        };
        assert_eq!(ew, hint("-debrief", "cd ~/strike/White Wedding/ew-debrief"));
        assert_eq!(
            ew_case,
            hint("-debrief", "cd ~/strike/white wedding/ew-debrief")
        );
        // Tab rewrites these, so the suggestion shows what they become.
        assert_eq!(
            under_bookmark,
            hint(
                " → ~/strike/White Wedding/ew-debrief",
                "cd ~/strike/White Wedding/ew-debrief"
            )
        );
        assert_eq!(
            bookmark,
            hint(" → ~/strike/White Wedding", "cd ~/strike/White Wedding")
        );
        assert_eq!(ambiguous, None);
        assert_eq!(done, None);
        assert_eq!(mid_line, None);
        assert_eq!(remote, None);
    }

    #[test]
    fn history_hint_comes_first() {
        let (home, cwd, ww) = setup("shell_completion_test_hint_history");
        let history = vec![HistoryItem {
            text: "cd ~/strike/White Wedding/ew-debrief/src".to_string(),
            dir: cwd.clone(),
            dt: chrono::Utc::now(),
        }];
        let bookmarks = std::slice::from_ref(&ww);
        let src = sources(&cwd, &home, bookmarks, &history);
        let line = "cd ~/strike/White Wedding/ew";
        let from_history = input_hint(line, line.len(), &src);
        // The newest match being the line itself suggests nothing from
        // history, so the completion is suggested.
        let line = "cd ~/strike/White Wedding/ew-debrief/src";
        let exact = input_hint(line, line.len(), &src);
        let _ = fs::remove_dir_all(&home);
        assert_eq!(
            from_history.map(|h| h.display),
            Some("-debrief/src".to_string())
        );
        assert_eq!(exact, None);
    }

    #[test]
    fn tab_completes_under_bookmark_by_name() {
        let (home, cwd, ww) = setup("shell_completion_test_under_bookmark");
        let line = "cd wedding/ew";
        let result = complete_cd_path(
            line,
            line.len(),
            &cwd,
            Some(&home),
            std::slice::from_ref(&ww),
            &[],
        )
        .unwrap();
        let _ = fs::remove_dir_all(&home);
        assert_eq!(result.candidates.len(), 1);
        assert_eq!(
            result.candidates[0].replacement,
            "~/strike/White Wedding/ew-debrief"
        );
    }

    #[test]
    fn tab_completes_under_recent_dir_by_name() {
        let (home, cwd, ww) = setup("shell_completion_test_under_recent");
        let recent = vec![RecentDir {
            path: ww.join("ew-debrief"),
            dt: chrono::Utc::now(),
        }];
        let line = "cd debrief/s";
        let result = complete_cd_path(line, line.len(), &cwd, Some(&home), &[], &recent).unwrap();
        let _ = fs::remove_dir_all(&home);
        assert_eq!(result.candidates.len(), 1);
        assert_eq!(
            result.candidates[0].replacement,
            "~/strike/White Wedding/ew-debrief/src"
        );
    }

    #[test]
    fn enter_goes_where_tab_completes() {
        let (home, cwd, ww) = setup("shell_completion_test_resolve");
        fs::create_dir_all(ww.join("synthetic-a")).unwrap();
        fs::create_dir_all(ww.join("synthetic-b")).unwrap();
        let bookmarks = std::slice::from_ref(&ww);
        let resolve = |args: &str| util::resolve_cd(Some(&home), &cwd, bookmarks, &[], args);
        let tilde = resolve("~/strike/White Wedding/ew");
        let named = resolve("wedding/ew");
        let ambiguous = resolve("~/strike/White Wedding/synth");
        let nothing = resolve("~/strike/White Wedding/zzz");
        let _ = fs::remove_dir_all(&home);
        assert_eq!(tilde, Ok(ww.join("ew-debrief")));
        assert_eq!(named, Ok(ww.join("ew-debrief")));
        assert!(ambiguous.is_err_and(|e| e.contains("synthetic-a") && e.contains("synthetic-b")));
        assert_eq!(nothing, Ok(home.join("strike/White Wedding/zzz")));
    }
}
