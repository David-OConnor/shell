//! This .lib entry point exists so we can share code (native to this project) with
//! the GUI variant, which calls this as a library. It is not part of the CLI shell application, and
//! this distinction would not exist if this project was only used for the [CLI] executable.

pub mod commands;
mod git;
mod input_completion;
mod python;
pub mod save_data;
pub mod ssh;
pub mod state;
mod util;

pub use git::{BRANCH_PREFIX, current_branch, truncate_branch};
pub use input_completion::{
    CompletionCandidate, CompletionResult, apply_completion, complete_cd_path,
    complete_command_path,
};
pub use python::{VENV_PREFIX, venv_python};
pub use ssh::{RemoteSession, SshMode};
pub use state::{
    BrowserFile, HistoryItem, NavState, OpenTabs, PanelVis, RecentDir, RemoteTerminal, WindowSize,
    record_recent_dir,
};
pub use util::{
    DISP_PAGE_LEN, DIVIDER, get_home, page_count, path_from_args, quiet_command,
    read_browser_files, render_history, render_page,
};
