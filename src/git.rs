//! Basic git integration: For `sync`, displaying info about the current branch
//! if in a folder which contains a repo, and cloning by bare repo name from a
//! saved root (`clone shell` → `git clone https://github.com/david-oconnor/shell`).

use std::path::Path;

use crate::quiet_command;

/// Maximum number of branch-name characters shown in the prompt before we
/// truncate.
const BRANCH_NAME_MAX: usize = 10;

/// `git clone` options whose value is a separate argument, e.g. `-b main`.
/// Skipped when looking for the repository argument, so `clone -b main foo`
/// doesn't take `main` for the repository.
const CLONE_VALUE_OPTS: &[&str] = &[
    "-b",
    "--branch",
    "-o",
    "--origin",
    "-u",
    "--upload-pack",
    "-c",
    "--config",
    "-j",
    "--jobs",
    "--depth",
    "--reference",
    "--reference-if-able",
    "--separate-git-dir",
    "--shallow-since",
    "--shallow-exclude",
    "--template",
    "--filter",
    "--server-option",
    "--bundle-uri",
    "--revision",
    "--ref-format",
];

pub const BRANCH_PREFIX: &str = " br: ";

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

/// For the words of a `git clone` (or `clone`) command line, the index of the
/// repository argument: the first word after `clone` that isn't an option or
/// an option's value. This is past the last word when the repository hasn't
/// been typed yet. `None` when the line isn't a clone command.
pub(crate) fn clone_repo_index(words: &[&str]) -> Option<usize> {
    let mut i = match words {
        ["git", "clone", ..] => 2,
        ["clone", ..] => 1,
        _ => return None,
    };
    while let Some(&w) = words.get(i) {
        if w == "--" {
            return Some(i + 1);
        }
        if !w.starts_with('-') {
            break;
        }
        if CLONE_VALUE_OPTS.contains(&w) {
            i += 1;
        }
        i += 1;
    }
    Some(i)
}

/// The repository argument of a `git clone` (or `clone`) command line, with
/// its byte offset in `line`.
fn clone_repo_arg(line: &str) -> Option<(usize, &str)> {
    let words: Vec<&str> = line.split_whitespace().collect();
    let word = *words.get(clone_repo_index(&words)?)?;
    // `word` is a slice of `line`, so the pointer difference is its offset.
    Some((word.as_ptr() as usize - line.as_ptr() as usize, word))
}

/// True for a bare repository name like `shell` or `my-project.git`: one that
/// can be appended to a clone root, as opposed to a URL or a path.
pub(crate) fn is_bare_repo_name(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && s.chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// The root of a full repository URL: everything before its last path
/// segment, e.g. `https://github.com/david-oconnor` for
/// `https://github.com/david-oconnor/shell.git`. Handles `scheme://` URLs and
/// scp-style SSH addresses (`git@github.com:david-oconnor/shell.git`).
///
/// `None` for anything else: bare names, local paths, an address with no path
/// before the repo name (`git@github.com:shell.git`), or a URL carrying a
/// password (`https://user:token@…`), which mustn't be written to the state
/// file.
fn url_root(url: &str) -> Option<&str> {
    let url = url.trim_matches(['"', '\'']).trim_end_matches('/');

    // Byte index where the URL's path starts; the root must end at or after it.
    let path_start = if let Some(i) = url.find("://") {
        let authority_start = i + 3;
        let authority_len = url[authority_start..].find('/')?;
        let authority = &url[authority_start..authority_start + authority_len];
        if authority
            .rsplit_once('@')
            .is_some_and(|(userinfo, _)| userinfo.contains(':'))
        {
            return None;
        }
        authority_start + authority_len
    } else {
        // scp-style `[user@]host:path`. As in git, a slash before the first
        // colon makes it a local path, as does a one-letter host (a Windows
        // drive, e.g. `C:`).
        let colon = url.find(':')?;
        if colon < 2 || url[..colon].contains(['/', '\\']) {
            return None;
        }
        colon + 1
    };

    let slash = url.rfind('/')?;
    (slash >= path_start).then(|| &url[..slash])
}

/// The root of the full URL a `git clone` command line clones from (see
/// [url_root]), to save so later clones from the same place need only the
/// repo name. `None` when the line isn't a clone of a full URL.
pub fn clone_root(line: &str) -> Option<String> {
    let (_, url) = clone_repo_arg(line)?;
    url_root(url).map(str::to_owned)
}

/// Expand a `git clone <name>` (or `clone <name>`) command line, where `<name>`
/// is a bare repository name, to clone `<root>/<name>` instead. Other
/// arguments are kept as typed. When `cwd` holds a directory named `<name>`,
/// that's a local repository to clone, so the line is left alone. `None` when
/// nothing changes.
pub fn expand_clone(line: &str, root: &str, cwd: Option<&Path>) -> Option<String> {
    let (start, name) = clone_repo_arg(line)?;
    if !is_bare_repo_name(name) || cwd.is_some_and(|d| d.join(name).exists()) {
        return None;
    }
    let mut out = line.to_string();
    out.replace_range(start..start + name.len(), &format!("{root}/{name}"));
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: &str = "https://github.com/david-oconnor";

    #[test]
    fn finds_root_of_full_urls() {
        for url in [
            "https://github.com/david-oconnor/shell",
            "https://github.com/david-oconnor/shell.git",
            "https://github.com/david-oconnor/shell/",
            "\"https://github.com/david-oconnor/shell\"",
        ] {
            assert_eq!(url_root(url), Some(ROOT), "{url}");
        }
        assert_eq!(
            url_root("git@github.com:david-oconnor/shell.git"),
            Some("git@github.com:david-oconnor")
        );
        assert_eq!(
            url_root("ssh://git@host:22/team/shell"),
            Some("ssh://git@host:22/team")
        );
        assert_eq!(url_root("https://host/shell"), Some("https://host"));
    }

    #[test]
    fn rejects_non_urls_and_passwords() {
        for arg in [
            "shell",
            "https://github.com",
            "git@github.com:shell.git",
            "../repos/shell",
            "/srv/repos/shell",
            "C:/repos/shell",
            "C:\\repos\\shell",
            "https://user:token@github.com/david-oconnor/shell",
        ] {
            assert_eq!(url_root(arg), None, "{arg}");
        }
    }

    #[test]
    fn finds_clone_root_past_options() {
        let url = "https://github.com/david-oconnor/shell";
        for line in [
            format!("git clone {url}"),
            format!("git clone -b main {url} dir"),
            format!("git clone --depth=1 --recursive {url}"),
            format!("git clone -- {url}"),
        ] {
            assert_eq!(clone_root(&line).as_deref(), Some(ROOT), "{line}");
        }
        assert_eq!(clone_root(&format!("git pull {url}")), None);
        assert_eq!(clone_root("git clone shell"), None);
    }

    #[test]
    fn expands_bare_repo_names() {
        assert_eq!(
            expand_clone("git clone shell", ROOT, None).as_deref(),
            Some("git clone https://github.com/david-oconnor/shell")
        );
        assert_eq!(
            expand_clone("clone shell", ROOT, None).as_deref(),
            Some("clone https://github.com/david-oconnor/shell")
        );
        assert_eq!(
            expand_clone("git clone -b main shell my_dir", ROOT, None).as_deref(),
            Some("git clone -b main https://github.com/david-oconnor/shell my_dir")
        );
        for line in [
            "git clone https://github.com/other/shell",
            "git clone ../shell",
            "git clone -b main",
            "git clone",
            "git status shell",
        ] {
            assert_eq!(expand_clone(line, ROOT, None), None, "{line}");
        }
    }

    #[test]
    fn leaves_local_repo_clones_alone() {
        let cwd = std::env::temp_dir().join("shell_git_test_local_clone");
        std::fs::create_dir_all(cwd.join("shell")).unwrap();
        let expanded = expand_clone("git clone shell", ROOT, Some(&cwd));
        let _ = std::fs::remove_dir_all(&cwd);
        assert_eq!(expanded, None);
    }
}
