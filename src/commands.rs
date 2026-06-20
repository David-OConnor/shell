//! Cross-frontend implementations of shell built-ins.
//!
//! `sync` (git add/commit/push) and `logs` (journalctl tail) are built-ins
//! that both the CLI (`shell`) and the GUI (`shell_gui`) need to run. The
//! tricky part is that the two frontends route output differently: the CLI
//! prints to stdout/stderr, the GUI pushes lines into its terminal pane.
//!
//! We bridge that by having each function take an [`OutputSink`] callback.
//! The frontend supplies a closure that knows how to render a chunk of
//! output, and the shared logic stays in one place.
//!
//! `logs` is Linux-only because `journalctl` is. On other platforms we
//! compile a stub that just reports that via the sink.

use std::{
    env,
    fs::File,
    io,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::Command,
};

use chrono::Utc;

use crate::{HistoryItem, path_from_args, state::State};

/// Which stream a chunk of output came from. Frontends use this to colour
/// the line (stderr red, stdout default) and/or pick between stdout/stderr
/// when printing.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OutKind {
    Stdout,
    Stderr,
}

/// Callback used by the shared built-ins to emit output. The first arg is
/// the source stream; the second is the text (may or may not have a
/// trailing newline — sinks should normalise).
pub type OutputSink<'a> = &'a mut dyn FnMut(OutKind, String);

/// Implements `sync <message>`: runs `git add .`, `git commit -am <message>`,
/// `git push` in sequence under `cwd`, stopping on the first non-zero exit.
/// All git output and any wrapper diagnostics go through `sink`.
///
/// Note: this captures each step's output with `.output()` rather than
/// streaming with `.status()`, so the GUI can route lines into its pane.
/// The CLI consequently sees output in step-sized batches rather than
/// streaming live, which is a minor UX change vs. the previous behaviour.
pub fn sync(message: &str, cwd: &Path, sink: OutputSink) {
    let message = message.trim().trim_matches('"');
    if message.is_empty() {
        sink(
            OutKind::Stderr,
            "sync: commit message required, e.g. sync \"my commit message\"".to_string(),
        );
        return;
    }

    let steps: [&[&str]; 3] = [&["add", "."], &["commit", "-am", message], &["push"]];
    for step in steps {
        match Command::new("git").args(step).current_dir(cwd).output() {
            Ok(out) => {
                if !out.stdout.is_empty() {
                    sink(
                        OutKind::Stdout,
                        String::from_utf8_lossy(&out.stdout).into_owned(),
                    );
                }
                if !out.stderr.is_empty() {
                    sink(
                        OutKind::Stderr,
                        String::from_utf8_lossy(&out.stderr).into_owned(),
                    );
                }
                if !out.status.success() {
                    sink(
                        OutKind::Stderr,
                        format!("sync: `git {}` failed", step.join(" ")),
                    );
                    return;
                }
            }
            Err(e) => {
                sink(OutKind::Stderr, format!("sync: failed to run git: {e}"));
                return;
            }
        }
    }
}

/// Implements `logs <service>`: runs `sudo journalctl -u <service>`.
///
/// With `follow = true` the child inherits stdio and tails logs live
/// (`journalctl -f`); the sink is used only for wrapper diagnostics since
/// stdout/stderr go straight to the terminal. The CLI uses this.
///
/// With `follow = false` we capture a bounded snapshot (`-n 200 --no-pager`)
/// and emit it through `sink`. The GUI uses this — a live `-f` tail would
/// block the UI thread indefinitely, and capturing via `.output()` only
/// returns once the child exits.
#[cfg(target_os = "linux")]
pub fn logs(service: &str, follow: bool, sink: OutputSink) {
    let service = service.trim().trim_matches('"');
    if service.is_empty() {
        sink(
            OutKind::Stderr,
            "logs: service required, e.g. logs gunicorn".to_string(),
        );
        return;
    }

    if follow {
        // Stream live via the inherited tty; the user breaks out with Ctrl+C.
        match Command::new("sudo")
            .args(["journalctl", "-u", service, "-f"])
            .status()
        {
            Ok(status) if !status.success() => {
                sink(
                    OutKind::Stderr,
                    "logs: journalctl exited non-zero".to_string(),
                );
            }
            Err(e) => {
                sink(
                    OutKind::Stderr,
                    format!("logs: failed to run journalctl: {e}"),
                );
            }
            _ => {}
        }
    } else {
        // Bounded snapshot for callers (the GUI) that can't host a live tail.
        // `--no-pager` keeps journalctl from trying to invoke `less` when
        // stdout isn't a tty (it would otherwise either hang or error out).
        match Command::new("sudo")
            .args(["journalctl", "-u", service, "-n", "200", "--no-pager"])
            .output()
        {
            Ok(out) => {
                if !out.stdout.is_empty() {
                    sink(
                        OutKind::Stdout,
                        String::from_utf8_lossy(&out.stdout).into_owned(),
                    );
                }
                if !out.stderr.is_empty() {
                    sink(
                        OutKind::Stderr,
                        String::from_utf8_lossy(&out.stderr).into_owned(),
                    );
                }
                if !out.status.success() {
                    sink(
                        OutKind::Stderr,
                        "logs: journalctl exited non-zero".to_string(),
                    );
                }
            }
            Err(e) => {
                sink(
                    OutKind::Stderr,
                    format!("logs: failed to run journalctl: {e}"),
                );
            }
        }
    }
}

/// Stub for non-Linux targets. `journalctl` only exists on systemd-based
/// Linux distros, so rather than silently falling through to the system
/// shell (where the word `journalctl` is a "command not found"), we surface
/// a clear message.
#[cfg(not(target_os = "linux"))]
pub fn logs(_service: &str, _follow: bool, sink: OutputSink) {
    sink(
        OutKind::Stderr,
        "logs: only supported on Linux (uses journalctl)".to_string(),
    );
}

/// Like the Linux cat command; outputs the contents of a file to stdout.
/// On any error (file missing, not readable, mid-stream read failure) it
/// prints a `cat: ...` diagnostic to stderr and returns, matching the
/// shell's other builtins.
pub fn cat(path: &Path) {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("cat: {}: {e}", path.display());
            return;
        }
    };

    for line in BufReader::new(file).lines() {
        match line {
            Ok(l) => println!("{l}"),
            Err(e) => {
                eprintln!("cat: {}: {e}", path.display());
                return;
            }
        }
    }
}

/// Runs one command line. Returns false if the shell should exit.
pub fn run_command(state: &mut State, state_path: &Path, input: &str) -> bool {
    let input = input.trim();
    if input.is_empty() {
        return true;
    }

    // Split into command + remainder for built-in dispatch.
    let (cmd, args) = match input.find(char::is_whitespace) {
        Some(i) => (&input[..i], input[i..].trim()),
        None => (input, ""),
    };

    // `his`/`hist <n>` re-runs a previous history item. Handle it before
    // recording the meta-invocation so the user's history stays focused on
    // the resolved command (which the recursive call below will record).
    if cmd == "his" || cmd == "hist" {
        match args.parse::<usize>() {
            Ok(idx) => {
                let resolved = state
                    .history
                    .lock()
                    .ok()
                    .and_then(|h| h.get(idx).map(|item| item.text.clone()));
                match resolved {
                    Some(text) => {
                        println!("> {text}");
                        return run_command(state, state_path, &text);
                    }
                    None => eprintln!("{cmd}: no history item at index {idx}"),
                }
            }
            Err(_) => eprintln!("{cmd}: usage: {cmd} <number>"),
        }
        return true;
    }

    // `hisd <n>` re-runs a previous history item in its original working
    // directory, without changing the shell's CWD. Bypasses the built-in
    // dispatcher and shells the command out directly, since the point is to
    // run it elsewhere on the filesystem.
    if cmd == "hisd" {
        match args.parse::<usize>() {
            Ok(idx) => {
                let resolved = state
                    .history
                    .lock()
                    .ok()
                    .and_then(|h| h.get(idx).map(|item| (item.text.clone(), item.dir.clone())));

                match resolved {
                    Some((text, dir)) => {
                        println!("> {text}  (in {})", dir.display());
                        let result = if cfg!(windows) {
                            Command::new("pwsh")
                                .args(["-NoProfile", "-NoLogo", "-Command", &text])
                                .current_dir(&dir)
                                .status()
                        } else {
                            Command::new("sh")
                                .args(["-c", text.as_str()])
                                .current_dir(&dir)
                                .status()
                        };
                        if let Err(e) = result {
                            eprintln!("shell: {e}");
                        }
                    }
                    None => eprintln!("hisd: no history item at index {idx}"),
                }
            }
            Err(_) => eprintln!("hisd: usage: hisd <number>"),
        }
        return true;
    }

    if let Ok(mut hist) = state.history.lock() {
        hist.push(HistoryItem {
            text: input.to_string(),
            dir: state.cwd.clone(),
            dt: Utc::now(),
        });
    }

    // Track directories we've run real commands from (everything except `cd`),
    // so Ctrl+R / `cd <number>` can jump back to them. We always save below
    // regardless, to flush the new history entry to disk.
    if cmd != "cd" {
        let cwd = state.cwd.clone();
        shell::record_recent_dir(&state.recent_dirs, &cwd);
    }

    if let Err(e) = state.save(state_path) {
        eprintln!("warning: failed to save state: {e}");
    }

    match cmd {
        "exit" | "quit" => return false,

        "sync" => {
            // Delegate to the shared implementation; route its sink output
            // to stdout/stderr. Trim trailing newlines so we don't double up
            // on the ones println! adds — git output already ends with `\n`.
            let cwd = state.cwd.clone();
            let mut sink = |kind, msg: String| {
                let msg = msg.trim_end_matches('\n');
                match kind {
                    OutKind::Stdout => println!("{msg}"),
                    OutKind::Stderr => eprintln!("{msg}"),
                }
            };
            sync(args, &cwd, &mut sink);
        }

        "logs" => {
            // CLI uses `follow = true` so the user gets a live tail via
            // inherited stdio; Ctrl+C exits journalctl and returns control
            // to the shell. On non-Linux this is a no-op error via the sink.
            let mut sink = |kind, msg: String| {
                let msg = msg.trim_end_matches('\n');
                match kind {
                    OutKind::Stdout => println!("{msg}"),
                    OutKind::Stderr => eprintln!("{msg}"),
                }
            };
            logs(args, true, &mut sink);
        }

        // On linux, this is likely the same as the system `cat` command, but it works on Windows.
        // Another approach may be to only apply this branch on Windows.
        "cat" => {
            let bookmarks = state.dir_bookmarks.lock();
            let slice: &[PathBuf] = bookmarks.as_deref().map(|v| v.as_slice()).unwrap_or(&[]);
            let target = path_from_args(state.home.as_deref(), &state.cwd, slice, args);
            drop(bookmarks);
            cat(&target);
        }

        "del" => {
            // `del bm <number>`: delete a bookmark by its displayed index
            // (the numbers shown by the Alt+B bookmark list).
            let (sub, rest) = match args.find(char::is_whitespace) {
                Some(i) => (&args[..i], args[i..].trim()),
                None => (args, ""),
            };

            match sub {
                "bm" => match rest.parse::<usize>() {
                    Ok(idx) => {
                        let mut removed = None;
                        match state.dir_bookmarks.lock() {
                            Ok(mut list) => {
                                if idx < list.len() {
                                    removed = Some(list.remove(idx));
                                } else {
                                    eprintln!(
                                        "del bm: no bookmark at index {idx} (have {})",
                                        list.len()
                                    );
                                }
                            }
                            Err(_) => eprintln!("del bm: bookmark list lock poisoned"),
                        }
                        if let Some(path) = removed {
                            println!("Deleted bookmark: {}", path.display());
                            if let Err(e) = state.save(state_path) {
                                eprintln!("del bm: failed to save bookmarks: {e}");
                            }
                        }
                    }
                    Err(_) => {
                        eprintln!("del bm: expected a number, e.g. `del bm 4`");
                    }
                },
                "" => eprintln!("del: usage: del bm <number>"),
                other => eprintln!("del: unknown target `{other}` (expected `bm`)"),
            }
        }

        "cd" => {
            // `cd <number>` (with nothing else after) jumps to a recent
            // directory by its Ctrl+R index. Anything else is resolved as a
            // normal path/bookmark argument.
            // When the arg parses as a number we treat it as a recent-dir
            // index; remember the index so we can prune the entry if its
            // path is stale (deleted/moved on disk).
            let (target, recent_idx) = if let Ok(idx) = args.parse::<usize>() {
                let resolved = state
                    .recent_dirs
                    .lock()
                    .ok()
                    .and_then(|list| list.get(idx).map(|r| r.path.clone()));
                match resolved {
                    Some(p) => (Some(p), Some(idx)),
                    None => {
                        eprintln!("cd: no recent directory at index {idx}");
                        (None, None)
                    }
                }
            } else {
                let bookmarks = state.dir_bookmarks.lock();
                let slice: &[PathBuf] = bookmarks.as_deref().map(|v| v.as_slice()).unwrap_or(&[]);
                (
                    Some(path_from_args(
                        state.home.as_deref(),
                        &state.cwd,
                        slice,
                        args,
                    )),
                    None,
                )
            };

            if let Some(target) = target {
                match env::set_current_dir(&target) {
                    Ok(_) => state.cwd = env::current_dir().unwrap_or(target),
                    Err(e) => {
                        eprintln!("cd: {e}");
                        // If the recent-dir entry's path no longer exists on
                        // disk, prune it so the indices shift down and the
                        // user doesn't hit the same stale row forever.
                        if e.kind() == io::ErrorKind::NotFound {
                            if let Some(i) = recent_idx {
                                let mut removed = false;
                                if let Ok(mut list) = state.recent_dirs.lock() {
                                    if list.get(i).map(|r| r.path == target).unwrap_or(false) {
                                        list.remove(i);
                                        removed = true;
                                    }
                                }
                                if removed {
                                    eprintln!("cd: removed stale recent-dir entry {i}");
                                    if let Err(e) = state.save(state_path) {
                                        eprintln!("warning: failed to save state: {e}");
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // `bm <number>`: jump to the bookmark at that Alt+B index. Mirrors
        // `cd <number>` but indexes into the bookmark list instead of
        // recent_dirs.
        "bm" => match args.parse::<usize>() {
            Ok(idx) => {
                let resolved = state
                    .dir_bookmarks
                    .lock()
                    .ok()
                    .and_then(|list| list.get(idx).cloned());
                match resolved {
                    Some(target) => match env::set_current_dir(&target) {
                        Ok(_) => state.cwd = env::current_dir().unwrap_or(target),
                        Err(e) => eprintln!("bm: {e}"),
                    },
                    None => eprintln!("bm: no bookmark at index {idx}"),
                }
            }
            Err(_) => eprintln!("bm: usage: bm <number>"),
        },

        // Everything else: Pass through to the system shell (e.g. the one which we launched this
        // application from)
        _ => {
            let result = if cfg!(windows) {
                // Powershell 7+; we will assume Windows users have this.
                // -NoProfile/-NoLogo skip loading the user's $PROFILE and the
                // startup banner, which together dominate pwsh's cold-start
                // time. Each command spawns a fresh process, so this shaves
                // ~200ms off every passthrough command.
                Command::new("pwsh")
                    .args(["-NoProfile", "-NoLogo", "-Command", input])
                    .status()
            } else {
                Command::new("sh").args(["-c", input]).status()
            };
            if let Err(e) = result {
                eprintln!("shell: {e}");
            }
        }
    }

    true
}
