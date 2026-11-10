//! For automatically executing the python interpreter in a virtual environment. If the current
//! directory holds a python venv, the `python` and `pip` commands will use that venv's python
//! instead of the one the system PATH environment var normally points to.

use std::path::{Path, PathBuf};

/// Prompt marker shown when the cwd holds a venv.
pub const VENV_PREFIX: &str = " venv";

/// Candidate virtual-environment subdirectory names, checked in this order so a
/// `.venv` wins over a `venv` when both exist. Both are common conventions. `uv` uses `.venv`.
const VENV_DIRS: &[&str] = &[".venv", "venv"];

/// Interpreter locations within a venv root, relative to it. Windows venvs put
/// the interpreter under `Scripts\`; Linux venvs under `bin/`. We check both so
/// either layout is recognised regardless of host OS.
const INTERPRETERS: &[&str] = &[
    "Scripts/python.exe",
    "Scripts/python3.exe",
    "bin/python",
    "bin/python3",
];

/// `pip` executable locations within a venv root, relative to it. Same
/// per-layout split as `INTERPRETERS`: `Scripts` on Windows, `bin` on POSIX.
const PIPS: &[&str] = &["Scripts/pip.exe", "Scripts/pip3.exe", "bin/pip", "bin/pip3"];

/// Locate a venv interpreter for `dir`: when `dir` contains a
/// folder holding a venv, return the path to its python
/// executable.
pub fn venv_python(dir: &Path) -> Option<PathBuf> {
    venv_executable(dir, INTERPRETERS)
}

/// Locate a venv `pip` for `dir`, mirroring [`venv_python`]. Returns the path to
/// the virtual environment's `pip` executable.
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
/// when the directory looks like a real virtual environment.
fn executable_in_venv(venv: &Path, candidates: &[&str]) -> Option<PathBuf> {
    if !venv.join("pyvenv.cfg").is_file() {
        return None;
    }
    candidates
        .iter()
        .map(|rel| venv.join(rel))
        .find(|p| p.is_file())
}
