//! Basic git integration: For `sync`, and displaying info about the current branch
//! if in a folder which contains a repo.

use std::path::Path;

use crate::quiet_command;

/// Maximum number of branch-name characters shown in the prompt before we
/// truncate with `...`. Only used by `branch_indicator` below; the GUI passes
/// its own literal to `truncate_branch`, so this stays private to the module.
const BRANCH_NAME_MAX: usize = 10;

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
    let output = quiet_command("git")
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
/// stays in sync with what the CLI highlighter looks for. Only the lib's own
/// `State::prompt` builds this slot, so it's crate-private.
pub(crate) fn branch_indicator(branch: Option<&str>) -> String {
    match branch {
        Some(b) => format!("{BRANCH_PREFIX}{}", truncate_branch(b, BRANCH_NAME_MAX)),
        None => String::new(),
    }
}
