//! Functionality related to rendering text on screen; generally string
//! manipluation with color.

use std::{
    borrow::Cow,
    path::{Path, PathBuf},
};

use rustyline::highlight::Highlighter;
use shell::{HistoryItem, NavState, RecentDir};

use crate::{BRANCH_PREFIX, CD_PREFIX, DISP_HIST_LEN, DIVIDER, HIS_PREFIX, ShellHelper};

// ANSI escape codes. Modern Windows consoles (Windows Terminal, pwsh, post-2019
// conhost) handle these natively; rustyline enables VT processing on startup.
pub const COLOR_RESET: &str = "\x1b[0m";
pub const COLOR_YELLOW: &str = "\x1b[93m";
pub const COLOR_BLUE: &str = "\x1b[94m";
pub const COLOR_CYAN: &str = "\x1b[96m";
// Input syntax-highlighting palette.
pub const COLOR_TEAL: &str = "\x1b[96m"; // program command (e.g. `git`)
pub const COLOR_MAGENTA: &str = "\x1b[95m"; // subcommand (e.g. `commit`)
pub const COLOR_GREEN: &str = "\x1b[92m"; // parameters / flags (e.g. `-am`)
pub const COLOR_ORANGE: &str = "\x1b[38;5;208m"; // quote characters `'` and `"`

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

/// Render one page of a list: header with paging hint + usage hint, a
/// page of rows, and a closing divider. Page 0 = the last `per_page`
/// items (newest at the bottom). Rows are labelled with their absolute
/// index into `items`, so the displayed number lines up with the
/// corresponding `<cmd> <number>` invocation.
fn render_page<T>(
    title: &str,
    usage_hint: &str,
    empty_msg: &str,
    items: &[T],
    page: usize,
    per_page: usize,
    mut format_row: impl FnMut(usize, &T) -> String,
) -> String {
    let total = items.len();
    let pages = crate::page_count(total, per_page);
    let page = page.min(pages - 1);

    let mut msg = format!(
        "\n{title}  (← older page  → newer page).  {usage_hint}.  Page {}/{}:\n",
        page + 1,
        pages
    );
    msg.push_str(DIVIDER);
    msg.push('\n');

    if total == 0 {
        msg.push_str(empty_msg);
        msg.push('\n');
    } else {
        let end = total - page * per_page;
        let start = end.saturating_sub(per_page);
        for i in start..end {
            msg.push_str(&format_row(i, &items[i]));
            msg.push('\n');
        }
    }
    msg.push_str(DIVIDER);
    msg.push_str("\n\n");
    msg
}

pub fn render_history(history: &[HistoryItem], page: usize) -> String {
    render_page(
        "History",
        "Use `his <number>` to run; e.g. `his 4`",
        "(no history)",
        history,
        page,
        DISP_HIST_LEN,
        |i, item| format!("{i}:  {}", item.text),
    )
}

pub fn render_recent_dirs(
    recent: &[RecentDir],
    bookmarks: &[PathBuf],
    home: Option<&Path>,
    page: usize,
) -> String {
    render_page(
        "Recent directories",
        "Use `cd <number>` to go; e.g. `cd 4`",
        "(no recent directories)",
        recent,
        page,
        DISP_HIST_LEN,
        |i, r| {
            let star = if bookmarks.contains(&r.path) { "*" } else { "" };
            format!("{i}:  {star}{}", render_with_tilde(&r.path, home))
        },
    )
}

pub fn render_bookmarks(bookmarks: &[PathBuf], home: Option<&Path>, page: usize) -> String {
    render_page(
        "Bookmarks",
        "Use `del bm <number>` to delete; e.g. `del bm 4`",
        "(no bookmarks)",
        bookmarks,
        page,
        DISP_HIST_LEN,
        |i, bm| format!("{i}:  {}", render_with_tilde(bm, home)),
    )
}

impl Highlighter for ShellHelper {
    /// Color the `S` and `$` accents in the prompt yellow, leaving the
    /// directory in its default terminal color. The prompt may carry a
    /// ` branch: NAME` segment (magenta) and at most one recall indicator
    /// (` his N` / ` cd N`, light green) between the cwd and the `$`.
    /// Prompt shape from `State::prompt` is
    /// `"S <cwd>[ branch: NAME][ his N][ cd N] $ "`.
    fn highlight_prompt<'b, 's: 'b, 'p: 'b>(
        &'s self,
        prompt: &'p str,
        _default: bool,
    ) -> Cow<'b, str> {
        if let Some(rest) = prompt.strip_prefix("S ") {
            if let Some(dollar_idx) = rest.rfind(" $ ") {
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

                let (dir, branch) = match before_indicator.rfind(BRANCH_PREFIX) {
                    Some(i) => (&before_indicator[..i], Some(&before_indicator[i + 1..])),
                    None => (before_indicator, None),
                };

                let branch_part = match branch {
                    Some(b) => format!(" {COLOR_MAGENTA}{b}{COLOR_RESET}"),
                    None => String::new(),
                };
                let indicator_part = match indicator {
                    Some(ind) => format!(" {COLOR_GREEN}{ind}{COLOR_RESET}"),
                    None => String::new(),
                };

                return Cow::Owned(format!(
                    "{COLOR_YELLOW}S{COLOR_RESET} {dir}{branch_part}{indicator_part} {COLOR_YELLOW}${COLOR_RESET} {tail}"
                ));
            }
        }
        Cow::Borrowed(prompt)
    }

    /// Syntax-highlight the user's in-progress input: teal command, magenta
    /// subcommand, light-green flags/parameters, and orange quote characters.
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        if line.is_empty() {
            Cow::Borrowed(line)
        } else {
            Cow::Owned(highlight_input(line))
        }
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
/// - the first word (the program command) is teal,
/// - the first following non-flag word (the subcommand) is magenta,
/// - words beginning with `-` (flags/parameters) are light green,
/// - quote characters are orange,
/// - everything else keeps the base input color.
fn highlight_input(line: &str) -> String {
    let words = crate::tokenize_words(line);
    let mut out = String::new();
    let mut last = 0;
    let mut subcommand_assigned = false;

    for (i, &(start, end)) in words.iter().enumerate() {
        // Emit any whitespace before this word uncolored.
        out.push_str(&line[last..start]);

        let text = &line[start..end];
        let color = if i == 0 {
            COLOR_TEAL
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
