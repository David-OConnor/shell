//! For automatically executing the python interpreter in a venv. If the current
//! directory holds a python venv, the `python` alias will use the venv's python.
//!
//! "Holds a venv" means a `.venv` or `venv` subdirectory that looks like a real
//! virtual environment: it carries a `pyvenv.cfg` marker file (written by
//! `python -m venv`) *and* an interpreter executable in the platform's usual
//! spot (`Scripts\python.exe` on Windows, `bin/python` on Unix). We check both
//! layouts regardless of host OS, so a venv created under either convention is
//! recognised.
//!
//! Two consumers use this: `State::prompt` appends [`VENV_PREFIX`] to the prompt
//! when a venv is present, and `commands::run_command` reroutes a bare `python`
//! invocation through [`venv_python`] and a bare `pip` invocation through
//! [`venv_pip`].

use std::path::{Path, PathBuf};

/// Prompt marker shown when the cwd holds a usable venv, e.g.
/// `S <cwd> branch: main venv $`. The leading space is part of the marker
/// (mirroring `git::BRANCH_PREFIX`) so the CLI prompt highlighter can find it
/// and a cwd containing "venv" mid-path is less likely to collide. Both
/// `State::prompt` (which appends it) and the highlighter look for this exact
/// string.
pub const VENV_PREFIX: &str = " venv";

/// Candidate virtual-environment subdirectory names, checked in this order so a
/// `.venv` wins over a `venv` when both exist.
const VENV_DIRS: &[&str] = &[".venv", "venv"];

/// Interpreter locations within a venv root, relative to it. Windows venvs put
/// the interpreter under `Scripts\`; POSIX venvs under `bin/`. We check both so
/// either layout is recognised regardless of host OS. (`Path::join` accepts the
/// `/` separators here on Windows too.)
const INTERPRETERS: &[&str] = &[
    "Scripts/python.exe",
    "Scripts/python3.exe",
    "bin/python",
    "bin/python3",
];

/// `pip` executable locations within a venv root, relative to it. Same
/// per-layout split as [`INTERPRETERS`]: `Scripts\` on Windows, `bin/` on POSIX.
const PIPS: &[&str] = &["Scripts/pip.exe", "Scripts/pip3.exe", "bin/pip", "bin/pip3"];

/// Locate a venv interpreter for `dir`: when `dir` contains a `.venv` or `venv`
/// folder holding a valid virtual environment, return the path to its python
/// executable. Returns `None` when no usable venv is present.
pub fn venv_python(dir: &Path) -> Option<PathBuf> {
    venv_executable(dir, INTERPRETERS)
}

/// Locate a venv `pip` for `dir`, mirroring [`venv_python`]. Returns the path to
/// the virtual environment's `pip` executable, or `None` when no usable venv is
/// present (or the venv lacks a `pip`).
pub fn venv_pip(dir: &Path) -> Option<PathBuf> {
    venv_executable(dir, PIPS)
}

/// Shared lookup behind [`venv_python`] and [`venv_pip`]: scan the candidate
/// venv directories of `dir` and return the first `candidates` executable found
/// inside a valid virtual environment.
fn venv_executable(dir: &Path, candidates: &[&str]) -> Option<PathBuf> {
    VENV_DIRS
        .iter()
        .find_map(|name| executable_in_venv(&dir.join(name), candidates))
}

/// Render the venv slot for a prompt: [`VENV_PREFIX`] when `dir` holds a usable
/// venv, otherwise the empty string. Mirrors `git::branch_indicator`.
pub fn venv_indicator(dir: &Path) -> String {
    if venv_python(dir).is_some() {
        VENV_PREFIX.to_string()
    } else {
        String::new()
    }
}

/// Given a candidate venv root, return the first `candidates` executable path
/// when the directory looks like a real virtual environment: it carries a
/// `pyvenv.cfg` marker and an executable in one of the expected per-layout
/// locations.
fn executable_in_venv(venv: &Path, candidates: &[&str]) -> Option<PathBuf> {
    if !venv.join("pyvenv.cfg").is_file() {
        return None;
    }
    candidates
        .iter()
        .map(|rel| venv.join(rel))
        .find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    /// Build a fake venv under `root/<name>` with the given interpreter
    /// relative path, plus a `pyvenv.cfg`. Returns the interpreter's full path.
    fn make_venv(root: &Path, name: &str, interpreter: &str) -> PathBuf {
        let venv = root.join(name);
        let interp = venv.join(interpreter);
        fs::create_dir_all(interp.parent().unwrap()).unwrap();
        fs::write(&interp, b"").unwrap();
        fs::write(venv.join("pyvenv.cfg"), b"home = /usr\n").unwrap();
        interp
    }

    fn tmp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("shell_venv_test_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn detects_dot_venv_with_unix_layout() {
        let dir = tmp_dir("dotvenv_unix");
        let interp = make_venv(&dir, ".venv", "bin/python");
        assert_eq!(venv_python(&dir), Some(interp));
        assert_eq!(venv_indicator(&dir), VENV_PREFIX);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn detects_venv_with_windows_layout() {
        let dir = tmp_dir("venv_win");
        let interp = make_venv(&dir, "venv", "Scripts/python.exe");
        assert_eq!(venv_python(&dir), Some(interp));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn detects_pip_alongside_python() {
        let dir = tmp_dir("pip_unix");
        make_venv(&dir, ".venv", "bin/python");
        let pip = dir.join(".venv/bin/pip");
        fs::write(&pip, b"").unwrap();
        assert_eq!(venv_pip(&dir), Some(pip));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn pip_none_when_absent() {
        // A valid venv with a python but no pip yields no pip path.
        let dir = tmp_dir("nopip");
        make_venv(&dir, ".venv", "bin/python");
        assert_eq!(venv_pip(&dir), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn dot_venv_wins_over_venv() {
        let dir = tmp_dir("both");
        let preferred = make_venv(&dir, ".venv", "bin/python");
        make_venv(&dir, "venv", "bin/python");
        assert_eq!(venv_python(&dir), Some(preferred));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_cfg_is_not_a_venv() {
        let dir = tmp_dir("nocfg");
        // Interpreter present but no pyvenv.cfg — not a valid venv.
        let venv = dir.join(".venv");
        let interp = venv.join("bin/python");
        fs::create_dir_all(interp.parent().unwrap()).unwrap();
        fs::write(&interp, b"").unwrap();
        assert_eq!(venv_python(&dir), None);
        assert_eq!(venv_indicator(&dir), "");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_venv_dir_is_none() {
        let dir = tmp_dir("empty");
        assert_eq!(venv_python(&dir), None);
        let _ = fs::remove_dir_all(&dir);
    }
}
