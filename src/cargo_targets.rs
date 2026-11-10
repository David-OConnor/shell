//! The `rm_targets` command: find cargo `target` folders under the current
//! directory, list them with their sizes, and delete them once the user
//! confirms. These folders hold build output only — the next `cargo build`
//! recreates them — but easily run to several GB per project, which adds up
//! across a folder like `~/code`.
//!
//! We delete with `fs::remove_dir_all` rather than running `cargo clean` in
//! each project. `cargo clean` removes whichever directory cargo resolves as
//! the target dir, which `CARGO_TARGET_DIR` or a `.cargo/config.toml` can point
//! somewhere else, so it may not remove the folder we listed. It also fails on
//! a project whose manifest no longer parses, and spawns a process per project.
//! Deleting directly removes exactly what the user approved.
//!
//! The GUI's System panel uses [find_targets] and [dir_size] to report the
//! space these folders take.

use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::{input_completion::is_hidden, util::confirm};

/// Directories the scan never descends into: they won't hold a crate, and are
/// large enough to slow the walk noticeably.
const SKIP_DIRS: &[&str] = &["node_modules", "__pycache__", "venv", "site-packages"];

/// A cargo build-output folder found by [find_targets].
pub struct TargetDir {
    pub path: PathBuf,
    /// Total size of the files inside, in bytes.
    pub size: u64,
}

/// Implements `rm_targets`: scan `cwd` for cargo target folders, list them,
/// and delete them if the user answers yes. Failures to delete one folder
/// (e.g. a file held open by a running program) are reported, and the rest
/// still get deleted.
pub fn rm_targets(cwd: &Path) {
    println!("Scanning {} for cargo target folders...", cwd.display());
    let targets = find_targets(cwd);
    if targets.is_empty() {
        println!("rm_targets: no cargo target folders found");
        return;
    }

    let sizes: Vec<String> = targets.iter().map(|t| format_gb(t.size)).collect();
    let width = sizes.iter().map(String::len).max().unwrap_or(0);
    println!();
    for (target, size) in targets.iter().zip(&sizes) {
        println!("  {size:>width$}   {}", target.path.display());
    }
    let total: u64 = targets.iter().map(|t| t.size).sum();
    println!(
        "\n{} {}. Deleting them would free {}.",
        targets.len(),
        plural(targets.len()),
        format_gb(total)
    );

    if !confirm(&format!(
        "Delete {} {}?",
        targets.len(),
        plural(targets.len())
    )) {
        println!("rm_targets: nothing deleted");
        return;
    }

    let mut freed = 0;
    let mut failed = 0;
    for target in &targets {
        println!("Deleting {}", target.path.display());
        match fs::remove_dir_all(&target.path) {
            Ok(()) => freed += target.size,
            Err(e) => {
                failed += 1;
                eprintln!("rm_targets: {}: {e}", target.path.display());
            }
        }
    }

    println!("Freed {}.", format_gb(freed));
    if failed > 0 {
        eprintln!(
            "rm_targets: {failed} {} could not be fully deleted",
            plural(failed)
        );
    }
}

/// Recursively search `root` for cargo target folders: a directory named
/// `target` with a `Cargo.toml` beside it. Sub-crates and workspace members
/// are searched too, but not the inside of a target folder, since nothing in
/// build output is worth listing separately.
///
/// Hidden directories (`.git`, `AppData`, …) are skipped, as are symlinks and
/// Windows junctions: `DirEntry::file_type` doesn't follow them, so they never
/// read as directories. That keeps the walk from looping, and from finding (and
/// deleting) anything outside `root`.
pub fn find_targets(root: &Path) -> Vec<TargetDir> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        let is_crate = dir.join("Cargo.toml").is_file();
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|t| t.is_dir()) || is_hidden(&entry) {
                continue;
            }
            let name = entry.file_name();
            let path = entry.path();
            if is_crate && name == "target" {
                found.push(TargetDir {
                    size: dir_size(&path),
                    path,
                });
            } else if !SKIP_DIRS.contains(&name.to_string_lossy().as_ref()) {
                stack.push(path);
            }
        }
    }

    found.sort_by(|a, b| a.path.cmp(&b.path));
    found
}

/// Total size in bytes of the files under `dir`. Unreadable entries count as
/// zero; the figure is informational only.
pub fn dir_size(dir: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                stack.push(entry.path());
            } else if let Ok(meta) = entry.metadata() {
                total += meta.len();
            }
        }
    }
    total
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "folder" } else { "folders" }
}

/// A byte count in GB to two decimals, e.g. `1.42 GB`. Always GB, even for
/// small folders, so the listed sizes line up and compare at a glance.
/// Binary (1024-based) units, matching what Windows Explorer reports.
fn format_gb(bytes: u64) -> String {
    format!("{:.2} GB", bytes as f64 / (1024. * 1024. * 1024.))
}
