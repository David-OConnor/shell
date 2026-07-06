//! Functionality related to rendering text on screen; generally string
//! manipulation with color. Note that the GUI version doesn't color text this way;
//! it uses EGUI's color API. The CLI version uses the ANSI codes defined here.

use std::{
    borrow::Cow,
    path::{Path, PathBuf},
};

use rustyline::highlight::Highlighter;
use shell::{DISP_PAGE_LEN, RecentDir, render_page};

use crate::{BRANCH_PREFIX, CD_PREFIX, HIS_PREFIX, ShellHelper, VENV_PREFIX};

// ANSI escape codes; for colors in the terminal. (CLI)
pub const COLOR_RESET: &str = "\x1b[0m";
pub const COLOR_YELLOW: &str = "\x1b[93m";
pub const COLOR_BLUE: &str = "\x1b[94m";
pub const COLOR_CYAN: &str = "\x1b[96m";

// Input syntax-highlighting palette.
pub const COLOR_TEAL: &str = "\x1b[96m"; // program command (e.g. `git`)
pub const COLOR_MAGENTA: &str = "\x1b[95m"; // subcommand (e.g. `commit`)
pub const COLOR_GREEN: &str = "\x1b[92m"; // parameters / flags (e.g. `-am`)
pub const COLOR_ORANGE: &str = "\x1b[38;5;208m"; // quote characters `'` and `"`
pub const COLOR_DIM: &str = "\x1b[90m"; // dimmed grey for ghost-text autosuggestions
pub const COLOR_RED: &str = "\x1b[91m"; // unrecognised command word

/// Render a path as `~/relative` when it lives under the home directory;
/// otherwise use the absolute form. Uses forward slashes after the tilde for
/// consistency with the rest of the shell.
pub fn render_with_tilde(p: &Path, home: Option<&Path>) -> String {
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

/// Render a single word in `color`, but recolor any quote characters (`'`/`"`)
/// orange so they stand out, then return to `color` for the rest of the word.
pub fn render_word(out: &mut String, text: &str, color: &str) {
    out.push_str(color);
    for ch in text.chars() {
        if ch == '"' || ch == '\'' {
            out.push_str(COLOR_RESET);
            out.push_str(COLOR_ORANGE);
            out.push(ch);
            out.push_str(COLOR_RESET);
            out.push_str(color);
        } else {
            out.push(ch);
        }
    }
    out.push_str(COLOR_RESET);
}

pub fn render_recent_dirs(
    recent: &[RecentDir],
    bookmarks: &[PathBuf],
    home: Option<&Path>,
    page: usize,
) -> String {
    render_page(
        "Recent directories",
        "Ctrl+O again: older page",
        "Use `cd <number>` to go; e.g. `cd 4`",
        "(no recent directories)",
        recent,
        page,
        DISP_PAGE_LEN,
        |i, r| {
            let star = if bookmarks.contains(&r.path) { "*" } else { "" };
            format!("{i}:  {star}{}", render_with_tilde(&r.path, home))
        },
    )
}

pub fn render_bookmarks(bookmarks: &[PathBuf], home: Option<&Path>, page: usize) -> String {
    render_page(
        "Bookmarks",
        "Alt+B again: older page",
        "Use `bm <number>` to go, `del bm <number>` to delete; e.g. `bm 4`",
        "(no bookmarks)",
        bookmarks,
        page,
        DISP_PAGE_LEN,
        |i, bm| format!("{i}:  {}", render_with_tilde(bm, home)),
    )
}

impl Highlighter for ShellHelper {
    /// Color the `S` and `$` accents in the prompt yellow, leaving the
    /// directory in its default terminal color. The prompt may carry a
    /// ` branch: NAME` segment (magenta), a ` venv` marker (blue), and at most
    /// one recall indicator (` his N` / ` cd N`, light green) between the cwd
    /// and the `$`. Prompt shape from `State::prompt` is
    /// `"S <cwd>[ branch: NAME][ venv][ his N][ cd N] $ "`.
    fn highlight_prompt<'b, 's: 'b, 'p: 'b>(
        &'s self,
        prompt: &'p str,
        _default: bool,
    ) -> Cow<'b, str> {
        if let Some(rest) = prompt.strip_prefix("S ")
            && let Some(dollar_idx) = rest.rfind(" $ ")
        {
            let body = &rest[..dollar_idx];
            let tail = &rest[dollar_idx + 3..]; // usually empty

            // Peel off the trailing recall indicator first, then the
            // branch slot, leaving the bare cwd. Search from the right
            // on each so a literal "branch:" mid-path can't fool us.
            let his_at = body.rfind(HIS_PREFIX);
            let cd_at = body.rfind(CD_PREFIX);
            let indicator_at = match (his_at, cd_at) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (Some(a), None) => Some(a),
                (None, Some(b)) => Some(b),
                (None, None) => None,
            };

            let (before_indicator, indicator) = match indicator_at {
                Some(i) => (&body[..i], Some(&body[i + 1..])),
                None => (body, None),
            };

            // The venv marker carries no value, so an exact-suffix strip is
            // precise — no need for the right-search the branch slot uses.
            let (before_venv, venv) = match before_indicator.strip_suffix(VENV_PREFIX) {
                Some(rest) => (rest, Some(VENV_PREFIX.trim_start())),
                None => (before_indicator, None),
            };

            let (dir, branch) = match before_venv.rfind(BRANCH_PREFIX) {
                Some(i) => (&before_venv[..i], Some(&before_venv[i + 1..])),
                None => (before_venv, None),
            };

            let branch_part = match branch {
                Some(b) => format!(" {COLOR_MAGENTA}{b}{COLOR_RESET}"),
                None => String::new(),
            };
            let venv_part = match venv {
                Some(v) => format!(" {COLOR_BLUE}{v}{COLOR_RESET}"),
                None => String::new(),
            };
            let indicator_part = match indicator {
                Some(ind) => format!(" {COLOR_GREEN}{ind}{COLOR_RESET}"),
                None => String::new(),
            };

            return Cow::Owned(format!(
                "{COLOR_YELLOW}S{COLOR_RESET} {dir}{branch_part}{venv_part}{indicator_part} {COLOR_YELLOW}${COLOR_RESET} {tail}"
            ));
        }
        Cow::Borrowed(prompt)
    }

    /// Syntax-highlight the user's in-progress input: the command word is teal
    /// when recognised and red when not (fish-style), the subcommand magenta,
    /// flags/parameters light-green, and quote characters orange.
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        if line.is_empty() {
            Cow::Borrowed(line)
        } else {
            Cow::Owned(highlight_input(line, &|cmd| self.command_is_valid(cmd)))
        }
    }

    /// Render the fish-style autosuggestion (ghost text shown after the
    /// cursor) in dimmed grey, so it reads as a proposal distinct from the
    /// text the user has actually typed. Press → / End at end-of-line to
    /// accept it.
    fn highlight_hint<'h>(&self, hint: &'h str) -> Cow<'h, str> {
        Cow::Owned(format!("{COLOR_DIM}{hint}{COLOR_RESET}"))
    }

    /// Tell rustyline to re-run `highlight` on every keystroke so the color
    /// extends to newly-typed characters.
    fn highlight_char(
        &self,
        _line: &str,
        _pos: usize,
        _forced: rustyline::highlight::CmdKind,
    ) -> bool {
        true
    }
}

/// Syntax-highlight a command line:
/// - the first word (the program command) is teal when `cmd_valid` accepts it
///   and red when not (fish-style "unknown command"),
/// - the first following non-flag word (the subcommand) is magenta,
/// - words beginning with `-` (flags/parameters) are light green,
/// - quote characters are orange,
/// - everything else keeps the base input color.
///
/// `cmd_valid` is passed the (unquoted) first word and reports whether it names
/// something the shell can run.
fn highlight_input(line: &str, cmd_valid: &dyn Fn(&str) -> bool) -> String {
    let words = crate::tokenize_words(line);
    let mut out = String::new();
    let mut last = 0;
    let mut subcommand_assigned = false;

    for (i, &(start, end)) in words.iter().enumerate() {
        // Emit any whitespace before this word uncolored.
        out.push_str(&line[last..start]);

        let text = &line[start..end];
        let color = if i == 0 {
            if cmd_valid(unquote(text)) {
                COLOR_TEAL
            } else {
                COLOR_RED
            }
        } else if text.starts_with('-') {
            COLOR_GREEN
        } else if !subcommand_assigned {
            subcommand_assigned = true;
            COLOR_MAGENTA
        } else {
            COLOR_CYAN
        };

        render_word(&mut out, text, color);
        last = end;
    }

    // Trailing whitespace, if any.
    out.push_str(&line[last..]);
    out
}

/// Strip a single pair of matching surrounding quotes (`"`/`'`) from `s`, so a
/// quoted command like `"my program"` is validated by its inner text. Returns
/// `s` unchanged when it isn't wrapped in matching quotes.
fn unquote(s: &str) -> &str {
    let bytes = s.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        if (first == b'"' || first == b'\'') && *bytes.last().unwrap() == first {
            return &s[1..s.len() - 1];
        }
    }
    s
}
