//! Standard directories each OS ships with or makes for the user, such as
//! `~/Desktop`, `C:\Program Files` or `/etc`. `cd` falls back to these after
//! bookmarks and recent directories, so they're reachable by name with
//! nothing saved: `cd program f` goes to `C:\Program Files`, `cd etc` to
//! `/etc`.

use std::{
    env,
    path::{Path, PathBuf},
};

/// The user's media and document folders, directly under home.
const USER_FOLDERS: [&str; 6] = [
    "Desktop",
    "Documents",
    "Downloads",
    "Pictures",
    "Music",
    if cfg!(target_os = "macos") {
        "Movies"
    } else {
        "Videos"
    },
];

/// Other folders under home, as path segments.
#[cfg(windows)]
const HOME_DIRS: &[&[&str]] = &[
    &["AppData"],
    &["AppData", "Local"],
    &["AppData", "Roaming"],
    &[".ssh"],
];
#[cfg(target_os = "macos")]
const HOME_DIRS: &[&[&str]] = &[
    &["Library"],
    &["Library", "Application Support"],
    &["Library", "LaunchAgents"],
    &["Library", "Logs"],
    &[".config"],
    &[".ssh"],
];
#[cfg(all(unix, not(target_os = "macos")))]
const HOME_DIRS: &[&[&str]] = &[&[".config"], &[".local", "share"], &[".cache"], &[".ssh"]];

/// System-wide folders, outside home. Windows builds its list at runtime; see
/// [system_dirs].
#[cfg(target_os = "macos")]
const SYSTEM_DIRS: &[&str] = &[
    "/Applications",
    "/Library",
    "/System",
    "/Users",
    "/Volumes",
    "/opt",
    "/opt/homebrew",
    "/usr/local",
    "/etc",
    "/var/log",
    "/tmp",
];
#[cfg(all(unix, not(target_os = "macos")))]
const SYSTEM_DIRS: &[&str] = &[
    "/etc",
    "/etc/systemd/system",
    "/opt",
    "/usr",
    "/usr/local",
    "/usr/share",
    "/var",
    "/var/log",
    "/var/www",
    "/srv",
    "/mnt",
    "/media",
    "/home",
    "/boot",
    "/tmp",
];

/// The standard directories that exist on this machine, best first: when two
/// share a name (e.g. `~/Library` and `/Library` on macOS), the earlier one
/// wins. Home folders come before system ones.
pub fn common_dirs(home: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = home {
        dirs.extend(USER_FOLDERS.iter().map(|f| user_folder(home, f)));
        for segments in HOME_DIRS {
            dirs.push(segments.iter().fold(home.to_path_buf(), |p, s| p.join(s)));
        }
    }
    dirs.extend(system_dirs());
    dirs.retain(|p| p.is_dir());
    dirs
}

/// Where Alt + `, 1, 2, 3 and 4 go, as do Peregrine's Files panel buttons:
/// home, Desktop, Downloads, per-user app data (`AppData`, `~/Library` or
/// `~/.local`) and installed programs (`Program Files`, `/Applications` or
/// `/usr`).
pub fn folder_key_dirs(home: Option<&Path>) -> [Option<PathBuf>; 5] {
    let app_data = if cfg!(windows) {
        "AppData"
    } else if cfg!(target_os = "macos") {
        "Library"
    } else {
        ".local"
    };
    let programs = if cfg!(windows) {
        env::var_os("ProgramFiles")
            .map_or_else(|| PathBuf::from(r"C:\Program Files"), PathBuf::from)
    } else if cfg!(target_os = "macos") {
        PathBuf::from("/Applications")
    } else {
        PathBuf::from("/usr")
    };

    [
        home.map(Path::to_path_buf),
        home.map(|h| user_folder(h, "Desktop")),
        home.map(|h| user_folder(h, "Downloads")),
        home.map(|h| h.join(app_data)),
        Some(programs),
    ]
}

/// `~/<folder>`, or on Windows its OneDrive copy when OneDrive backup has
/// moved it there.
fn user_folder(home: &Path, folder: &str) -> PathBuf {
    let local = home.join(folder);
    if cfg!(windows)
        && !local.is_dir()
        && let Some(onedrive) = env::var_os("OneDrive")
    {
        return PathBuf::from(onedrive).join(folder);
    }
    local
}

#[cfg(windows)]
fn system_dirs() -> Vec<PathBuf> {
    // From the environment where Windows sets it, as these can live on
    // another drive; otherwise the usual location.
    let env_or = |var: &str, default: &str| {
        env::var_os(var)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(default))
    };
    let drive = env::var("SystemDrive").unwrap_or_else(|_| "C:".to_string());
    let root = PathBuf::from(format!("{drive}\\"));
    // `SystemRoot` is often upper-case (`C:\WINDOWS`), which would then show
    // in the prompt; prefer the usual spelling when it's there.
    let windows = Some(root.join("Windows"))
        .filter(|p| p.is_dir())
        .unwrap_or_else(|| env_or("SystemRoot", "C:\\Windows"));

    vec![
        env_or("ProgramFiles", "C:\\Program Files"),
        env_or("ProgramFiles(x86)", "C:\\Program Files (x86)"),
        env_or("ProgramData", "C:\\ProgramData"),
        root.join("Users"),
        windows.join("System32"),
        windows,
        // `%TEMP%`, i.e. `~/AppData/Local/Temp`.
        env::temp_dir(),
    ]
}

#[cfg(unix)]
fn system_dirs() -> Vec<PathBuf> {
    SYSTEM_DIRS.iter().map(PathBuf::from).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_existing_home_folders_are_listed() {
        let home = env::temp_dir().join("shell_common_dirs_test_home");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join("Desktop")).unwrap();
        let dirs = common_dirs(Some(&home));
        let _ = std::fs::remove_dir_all(&home);
        assert!(dirs.contains(&home.join("Desktop")));
        assert!(!dirs.contains(&home.join("Downloads")));
    }
}
