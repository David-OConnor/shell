//! This .lib entry point exists so we can share code (native to this project) with
//! the GUI variant. It is not part of the CLI shell application.

pub mod commands;
pub mod save_data;
pub mod state;
pub mod util;

pub use state::{
    BrowserFile, HistoryItem, NavState, OpenTabs, PanelVis, RecentDir, RemoteTerminal, WindowSize,
    nav_indicator,
};
pub use util::{
    BRANCH_NAME_MAX, BRANCH_PREFIX, CompletionCandidate, CompletionResult, apply_completion,
    branch_indicator, complete_cd_path, current_branch, get_home, path_from_args,
    read_browser_files, render_with_tilde, truncate_branch,
};
