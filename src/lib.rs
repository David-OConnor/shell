//! This .lib entry point exists so we can share code (native to this project) with
//! the GUI variant. It is not part of the CLI shell application.

// `commands`, `save_data`, and `state` are accessed by path (e.g.
// `shell::state::State`) from the CLI binary and/or `shell_gui`, so they stay
// `pub mod`. `git`, `input_completion`, and `util` are only ever reached
// through the flat re-exports below, so the modules themselves are private.
pub mod commands;
mod git;
mod input_completion;
mod python;
pub mod save_data;
pub mod secrets;
pub mod ssh;
pub mod state;
mod util;

pub use git::{BRANCH_PREFIX, current_branch, truncate_branch};
pub use input_completion::{
    CompletionCandidate, CompletionResult, apply_completion, complete_cd_path,
};
pub use python::{VENV_PREFIX, venv_python};
pub use ssh::{RemoteSession, SshMode};
pub use state::{
    BrowserFile, HistoryItem, NavState, OpenTabs, PanelVis, RecentDir, RemoteTerminal, WindowSize,
    record_recent_dir,
};
pub use util::{get_home, path_from_args, read_browser_files};
