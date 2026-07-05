//! Basic git integration: For `sync`, and displaying info about the current branch
//! if in a folder which contains a repo.

use std::path::Path;

use crate::quiet_command;

/// Maximum number of branch-name characters shown in the prompt before we
/// truncate.
const BRANCH_NAME_MAX: usize = 10;

pub const BRANCH_PREFIX: &str = " branch: ";

/// Detect the current git branch; used for displaying prior to the input prompt.
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

/// Truncates the branch name for display, e.g. prior to the prompt. Keeps this line's size
/// under control if the branch name is long.
pub fn truncate_branch(name: &str, max: usize) -> String {
    let chars: Vec<char> = name.chars().collect();

    if chars.len() <= max {
        name.to_string()
    } else {
        let prefix: String = chars.into_iter().take(max).collect();
        format!("{prefix}...")
    }
}

/// Render the branch slot for a prompt, so the user can see the current directory
/// is a git repo, and what branch is active.
pub(crate) fn branch_indicator(branch: Option<&str>) -> String {
    match branch {
        Some(b) => format!("{BRANCH_PREFIX}{}", truncate_branch(b, BRANCH_NAME_MAX)),
        None => String::new(),
    }
}
