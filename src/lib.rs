//! This .lib entry point exists so we can share code (native to this project) with
//! the GUI variant, which calls this as a library. It is not part of the CLI shell application, and
//! this distinction would not exist if this project was only used for the [CLI] executable.

mod cargo_targets;
pub mod commands;
mod common_dirs;
mod git;
mod input_completion;
mod python;
pub mod save_data;
pub mod ssh;
pub mod state;
mod util;

pub use common_dirs::folder_key_dirs;
pub use git::{BRANCH_PREFIX, clone_root, current_branch, expand_clone, truncate_branch};
pub use input_completion::{
    CompletionCandidate, CompletionResult, CompletionSources, HintCache, InputHint,
    apply_completion, complete_cd_path, complete_clone_url, complete_command_path, complete_input,
    input_hint,
};
pub use python::{VENV_PREFIX, venv_python};
pub use ssh::RemoteSession;
pub use state::{
    BrowserFile, ClipboardContent, ClipboardEntry, HistoryItem, NavState, OpenTabs, RecentDir,
    RemoteTerminal, dedup_history, record_clone_root, record_history, record_recent_dir,
};
pub use util::{
    DISP_PAGE_LEN, DIVIDER, DirList, dir_missing, find_bookmark, find_cd_fallback,
    find_history_index, get_home, history_indices_in_dir, history_latest_indices, page_count,
    path_from_args, project_root, quiet_command, read_browser_files, remove_missing_recent_dirs,
    remove_saved_dir, render_history, render_history_in_dir, render_page, resolve_cd, resolve_root,
    stale_dir_list,
};
