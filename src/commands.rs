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

use crate::{
    HistoryItem, RemoteTerminal, path_from_args, quiet_command, secrets, ssh,
    ssh::{RemoteSession, SshMode},
    state::State,
};

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
        match quiet_command("git").args(step).current_dir(cwd).output() {
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

/// Build the remote shell command that `sync <message>` maps to over SSH.
/// Mirrors the local [`sync`] steps (add → commit → push) as a single
/// `&&`-chained line so it stops on the first failure, run from the session's
/// tracked remote cwd. Returns `None` (after emitting a diagnostic via `sink`)
/// when the commit message is empty, matching the local guard.
fn remote_sync_command(message: &str, sink: OutputSink) -> Option<String> {
    let message = message.trim().trim_matches('"');
    if message.is_empty() {
        sink(
            OutKind::Stderr,
            "sync: commit message required, e.g. sync \"my commit message\"".to_string(),
        );
        return None;
    }
    // Single-quote the message so spaces/specials survive the remote shell;
    // escape any embedded single quotes the usual `'\''` way.
    let escaped = message.replace('\'', "'\\''");
    Some(format!(
        "git add . && git commit -am '{escaped}' && git push"
    ))
}

/// Used for our `log` command: wrapper around `sudo journalctl -u <service> -f
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

/// `journalctl` only exists on systemd-based
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

/// Build the remote shell command that `logs <service>` maps to over SSH.
/// Mirrors the non-follow local path (`logs` with `follow = false`): a bounded,
/// non-paged snapshot, since exec-mode SSH has no tty to host a live `-f` tail.
/// `journalctl` runs under `sudo` to match the local built-in — over exec mode
/// (no tty) that needs passwordless sudo on the remote, otherwise sudo's prompt
/// surfaces as a stderr diagnostic. Returns `None` (after emitting a diagnostic
/// via `sink`) when the service name is empty, matching the local guard.
fn remote_logs_command(service: &str, sink: OutputSink) -> Option<String> {
    let service = service.trim().trim_matches('"');
    if service.is_empty() {
        sink(
            OutKind::Stderr,
            "logs: service required, e.g. logs gunicorn".to_string(),
        );
        return None;
    }
    // Single-quote the unit name so it survives the remote shell intact.
    let escaped = service.replace('\'', "'\\''");
    Some(format!("sudo journalctl -u '{escaped}' -n 200 --no-pager"))
}

/// Like the Linux cat command; outputs the contents of a file to stdout.
/// On any error (file missing, not readable, mid-stream read failure) it
/// prints a `cat: ...` diagnostic to stderr and returns, matching the
/// shell's other builtins. Only `run_command` (below) dispatches to this, so
/// it stays private to the module.
fn cat(path: &Path) {
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
        crate::record_recent_dir(&state.recent_dirs, &cwd);
    }

    if let Err(e) = state.save(state_path) {
        eprintln!("warning: failed to save state: {e}");
    }

    // While an SSH session is live, typed commands run on the remote rather
    // than locally. Only the session-management keywords are intercepted here;
    // everything else (including `cd`, `ls`, …) is sent to the remote shell.
    if state.active_remote.is_some() {
        let mut sink = |kind, msg: String| {
            let msg = msg.trim_end_matches('\n');
            match kind {
                OutKind::Stdout => println!("{msg}"),
                OutKind::Stderr => eprintln!("{msg}"),
            }
        };

        // Resolve what to actually run on the remote. Session-management
        // keywords are handled here and short-circuit. The shell's Rust
        // built-ins (`logs`, `sync`) are translated to their equivalent remote
        // command — the remote shell has never heard of them and would just
        // answer "command not found". Everything else is sent verbatim.
        let remote_cmd: Option<String> = match cmd {
            "exit" | "quit" | "logout" | "disconnect" => {
                if let Some(session) = state.active_remote.take() {
                    session.disconnect();
                }
                println!("ssh: disconnected");
                return true;
            }
            "mode" => {
                remote_set_mode(state, args);
                return true;
            }
            "logs" => match remote_logs_command(args, &mut sink) {
                Some(cmd) => Some(cmd),
                None => return true,
            },
            "sync" => match remote_sync_command(args, &mut sink) {
                Some(cmd) => Some(cmd),
                None => return true,
            },
            _ => Some(input.to_string()),
        };

        if let Some(command) = remote_cmd {
            if let Some(session) = state.active_remote.as_mut() {
                if let Err(e) = session.run(&command, &mut sink) {
                    eprintln!("ssh: {e}");
                }
            }
        }
        return true;
    }

    match cmd {
        "exit" | "quit" => return false,

        "ssh" => cmd_ssh(state, state_path, args),

        "remote" => cmd_remote(state, state_path, args),

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
            // Delete a bookmark by its displayed index
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
            // When the arg parses as a number, we treat it as a recent-dir
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

        // `python` (or `python3`): when the current directory holds a virtual
        // environment, run that venv's interpreter instead of whatever `python`
        // resolves to on PATH. Falls through to the normal passthrough when
        // there's no venv, so the system python still works as before.
        "python" | "python3" => match crate::python::venv_python(&state.cwd) {
            Some(interpreter) => run_passthrough(&rewrite_venv_command(&interpreter, args)),
            None => run_passthrough(input),
        },

        // `pip` (or `pip3`): same idea as `python` above — when the cwd holds a
        // venv, run that venv's `pip` so installs land in the environment rather
        // than the system site-packages. Falls through otherwise.
        "pip" | "pip3" => match crate::python::venv_pip(&state.cwd) {
            Some(pip) => run_passthrough(&rewrite_venv_command(&pip, args)),
            None => run_passthrough(input),
        },

        // Everything else: Pass through to the system shell (e.g. the one which we launched this
        // application from)
        _ => run_passthrough(input),
    }

    true
}

/// Run a command line through the system shell (PowerShell 7+ on Windows, `sh`
/// elsewhere), inheriting stdio so interactive programs work. Used for the
/// catch-all passthrough and the venv-rewritten `python` invocation.
///
/// `-NoProfile`/`-NoLogo` skip loading the user's `$PROFILE` and the startup
/// banner, which together dominate pwsh's cold-start time. Each command spawns a
/// fresh process, so this shaves ~200ms off every passthrough command.
fn run_passthrough(line: &str) {
    let result = if cfg!(windows) {
        Command::new("pwsh")
            .args(["-NoProfile", "-NoLogo", "-Command", line])
            .status()
    } else {
        Command::new("sh").args(["-c", line]).status()
    };
    if let Err(e) = result {
        eprintln!("shell: {e}");
    }
}

/// Rewrite a `python`/`pip` invocation to run a specific venv `exe` executable,
/// keeping the user's original arguments. The path is single-quoted so spaces
/// in it survive the system shell; on Windows the pwsh call operator `&` is
/// required to launch a quoted executable path.
fn rewrite_venv_command(exe: &Path, args: &str) -> String {
    let exe = exe.display();
    let args = args.trim();
    let lead = if cfg!(windows) { "& " } else { "" };
    if args.is_empty() {
        format!("{lead}'{exe}'")
    } else {
        format!("{lead}'{exe}' {args}")
    }
}

// ---------------------------------------------------------------------------
// SSH built-ins (CLI side). The actual transport lives in `crate::ssh`; these
// functions just parse arguments, fetch/store passwords in the OS keyring
// (`crate::secrets`), and drive the connection from the CLI.
// ---------------------------------------------------------------------------

/// The OS account name, used as the default SSH username when the user writes
/// just `ssh host` (no `user@`).
fn default_user() -> String {
    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "root".to_string())
}

/// Split `args` into its first whitespace-delimited word and the trimmed rest.
fn split_first_word(args: &str) -> (&str, &str) {
    match args.find(char::is_whitespace) {
        Some(i) => (&args[..i], args[i..].trim()),
        None => (args, ""),
    }
}

/// Parse a `[user@]host[:port] [port]` spec into `(host, port, user)`. The
/// optional trailing port argument wins over a `:port` suffix. Defaults: user =
/// the OS account, port = 22.
fn parse_remote_spec(spec: &str) -> (String, u16, String) {
    let (target, rest) = split_first_word(spec);
    let port_arg = rest.split_whitespace().next().and_then(|p| p.parse().ok());

    let (user, hostport) = match target.split_once('@') {
        Some((u, hp)) => (u.to_string(), hp),
        None => (default_user(), target),
    };
    let (host, port) = match hostport.split_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().unwrap_or(22)),
        None => (hostport.to_string(), 22),
    };
    (host, port_arg.unwrap_or(port), user)
}

/// Resolve the argument to `ssh` into a concrete `(host, port, user)`: either a
/// saved-remote index, or a freeform `[user@]host[:port]` spec. Prints a usage
/// / error message and returns `None` on failure.
fn resolve_ssh_target(state: &State, args: &str) -> Option<(String, u16, String)> {
    let args = args.trim();
    if args.is_empty() {
        eprintln!("ssh: usage: ssh [user@]host [port]  |  ssh <saved-index>  (see `remote list`)");
        return None;
    }
    // A bare number selects a saved remote by its `remote list` index.
    if let Ok(idx) = args.parse::<usize>() {
        let resolved = state.remote_terminals.lock().ok().and_then(|l| {
            l.get(idx)
                .map(|r| (r.host.clone(), r.port, r.username.clone()))
        });
        if resolved.is_none() {
            eprintln!("ssh: no saved remote at index {idx} (see `remote list`)");
        }
        return resolved;
    }
    Some(parse_remote_spec(args))
}

/// Dedupe-record a remote into the saved list (newest last), mirroring
/// `record_recent_dir`.
fn record_remote(state: &State, host: &str, port: u16, user: &str) {
    if let Ok(mut list) = state.remote_terminals.lock() {
        list.retain(|r| !(r.host == host && r.port == port && r.username == user));
        list.push(RemoteTerminal {
            host: host.to_string(),
            port,
            username: user.to_string(),
        });
    }
}

/// `ssh [user@]host [port]` / `ssh <index>` — connect and enter a remote
/// session. The password comes from the keyring if saved; otherwise we prompt
/// (hidden) and save it for next time.
fn cmd_ssh(state: &mut State, state_path: &Path, args: &str) {
    let Some((host, port, user)) = resolve_ssh_target(state, args) else {
        return;
    };

    let password = match secrets::get_password(&host, port, &user) {
        Some(p) => p,
        None => match read_password(&format!("Password for {user}@{host}: ")) {
            Ok(p) => {
                // Save for next time so future connects need no prompt.
                if let Err(e) = secrets::set_password(&host, port, &user, &p) {
                    eprintln!("ssh: warning: couldn't save password to keyring: {e}");
                }
                p
            }
            Err(e) => {
                eprintln!("ssh: {e}");
                return;
            }
        },
    };

    println!("Connecting to {user}@{host}:{port} …");
    match ssh::connect(&host, port, &user, &password) {
        Ok(session) => {
            println!(
                "Connected to {user}@{host}. Commands run remotely; `mode pty` opens an interactive shell (python, vim, …), `exit` disconnects."
            );
            record_remote(state, &host, port, &user);
            if let Err(e) = state.save(state_path) {
                eprintln!("warning: failed to save state: {e}");
            }
            state.active_remote = Some(session);

            // Stay in the shell's own command interface — prompt, history,
            // highlighting, completion — and route each typed command to the
            // remote via exec mode. (The session connects in PTY mode by
            // default for the GUI's sake; the CLI switches to exec so it keeps
            // its rich line editing instead of handing the terminal to a raw
            // remote shell.) The user opts into a raw interactive shell with
            // `mode pty` when one is actually needed — e.g. python, vim, top.
            if let Some(session) = state.active_remote.as_mut() {
                if let Err(e) = session.set_mode(SshMode::Exec) {
                    eprintln!("ssh: {e}");
                }
            }
        }
        Err(e) => eprintln!("ssh: {e}"),
    }
}

/// `remote list | add <[user@]host[:port]> | del <index>` — manage saved
/// remotes and their keyring passwords.
fn cmd_remote(state: &mut State, state_path: &Path, args: &str) {
    let (sub, rest) = split_first_word(args);
    match sub {
        "" | "list" | "ls" => {
            let Ok(list) = state.remote_terminals.lock() else {
                eprintln!("remote: list lock poisoned");
                return;
            };
            if list.is_empty() {
                println!("remote: no saved remotes (add one with `remote add user@host`)");
                return;
            }
            for (i, r) in list.iter().enumerate() {
                println!("{i}: {}@{}:{}", r.username, r.host, r.port);
            }
        }
        "add" => {
            if rest.is_empty() {
                eprintln!("remote: usage: remote add [user@]host[:port] [port]");
                return;
            }
            let (host, port, user) = parse_remote_spec(rest);
            match read_password(&format!("Password for {user}@{host} (blank to skip): ")) {
                Ok(pw) if !pw.is_empty() => {
                    if let Err(e) = secrets::set_password(&host, port, &user, &pw) {
                        eprintln!("remote: warning: couldn't save password to keyring: {e}");
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    eprintln!("remote: {e}");
                    return;
                }
            }
            record_remote(state, &host, port, &user);
            if let Err(e) = state.save(state_path) {
                eprintln!("remote: failed to save state: {e}");
            }
            println!("Saved remote {user}@{host}:{port}");
        }
        "del" | "rm" | "remove" => match rest.parse::<usize>() {
            Ok(idx) => {
                let removed = match state.remote_terminals.lock() {
                    Ok(mut list) if idx < list.len() => Some(list.remove(idx)),
                    Ok(_) => {
                        eprintln!("remote: no saved remote at index {idx}");
                        None
                    }
                    Err(_) => {
                        eprintln!("remote: list lock poisoned");
                        None
                    }
                };
                if let Some(r) = removed {
                    let _ = secrets::delete_password(&r.host, r.port, &r.username);
                    if let Err(e) = state.save(state_path) {
                        eprintln!("remote: failed to save state: {e}");
                    }
                    println!("Removed remote {}@{}:{}", r.username, r.host, r.port);
                }
            }
            Err(_) => eprintln!("remote: usage: remote del <index>"),
        },
        other => {
            eprintln!("remote: unknown subcommand `{other}` (expected list/add/del)");
        }
    }
}

/// `mode exec|pty` while connected: switch the live session's interaction mode.
/// Entering PTY mode runs the raw-terminal interactive loop until the remote
/// shell exits (then we disconnect) or the user detaches with Ctrl+] (then we
/// return to exec mode, keeping the connection).
fn remote_set_mode(state: &mut State, args: &str) {
    let target = match args.trim() {
        "pty" | "interactive" | "shell" => SshMode::Pty,
        "exec" | "cmd" | "" => SshMode::Exec,
        other => {
            eprintln!("mode: expected `exec` or `pty`, got `{other}`");
            return;
        }
    };

    if target == SshMode::Exec {
        if let Some(session) = state.active_remote.as_mut() {
            if let Err(e) = session.set_mode(SshMode::Exec) {
                eprintln!("mode: {e}");
            }
        }
        return;
    }

    // Entering PTY mode: open the shell channel (a no-op if we're already in
    // PTY mode, e.g. straight after connect), then drive the interactive loop.
    if let Some(session) = state.active_remote.as_mut() {
        if let Err(e) = session.set_mode(SshMode::Pty) {
            eprintln!("mode: {e}");
            return;
        }
    }
    enter_pty_loop(state);
}

/// Drive the raw-terminal interactive loop for the active session's PTY,
/// returning to the caller when the remote shell exits (we disconnect) or the
/// user detaches with Ctrl+] (we drop back to exec mode, keeping the
/// connection). The session must already be in [`SshMode::Pty`]. Shared by the
/// initial connect (which lands in PTY mode by default) and `mode pty`.
fn enter_pty_loop(state: &mut State) {
    if let Some(session) = state.active_remote.as_mut() {
        println!("-- interactive shell (Ctrl+] to detach to command mode) --");
        if let Err(e) = cli_pty_loop(session) {
            eprintln!("mode: pty: {e}");
        }
    }

    // The loop ended: either the remote shell exited (disconnect entirely) or
    // the user detached (drop back to exec mode, keeping the connection).
    let closed = state
        .active_remote
        .as_ref()
        .map(|s| s.pty_closed())
        .unwrap_or(false);
    if closed {
        if let Some(session) = state.active_remote.take() {
            session.disconnect();
        }
        println!("ssh: remote shell exited; disconnected");
    } else if let Some(session) = state.active_remote.as_mut() {
        let _ = session.set_mode(SshMode::Exec);
        println!(
            "ssh: detached to command mode (`mode pty` resumes the interactive shell, `exit` disconnects)"
        );
    }
}

/// Translate a crossterm key event into the bytes to feed the remote PTY.
/// Covers printable chars, Ctrl-letters, and the common navigation/edit keys.
fn key_to_bytes(key: crossterm::event::KeyEvent) -> Option<Vec<u8>> {
    use crossterm::event::{KeyCode, KeyModifiers};

    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                let upper = c.to_ascii_uppercase();
                if upper.is_ascii_alphabetic() {
                    return Some(vec![upper as u8 - b'A' + 1]);
                }
                match c {
                    ' ' => return Some(vec![0]),
                    '\\' => return Some(vec![0x1c]),
                    _ => {}
                }
            }
            let mut buf = [0u8; 4];
            Some(c.encode_utf8(&mut buf).as_bytes().to_vec())
        }
        KeyCode::Enter => Some(vec![b'\r']),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Tab => Some(vec![b'\t']),
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Up => Some(b"\x1b[A".to_vec()),
        KeyCode::Down => Some(b"\x1b[B".to_vec()),
        KeyCode::Right => Some(b"\x1b[C".to_vec()),
        KeyCode::Left => Some(b"\x1b[D".to_vec()),
        KeyCode::Home => Some(b"\x1b[H".to_vec()),
        KeyCode::End => Some(b"\x1b[F".to_vec()),
        KeyCode::Delete => Some(b"\x1b[3~".to_vec()),
        KeyCode::PageUp => Some(b"\x1b[5~".to_vec()),
        KeyCode::PageDown => Some(b"\x1b[6~".to_vec()),
        _ => None,
    }
}

/// Raw-terminal interactive loop for PTY mode. Puts the terminal into raw mode,
/// pumps keystrokes to the remote and remote output to stdout, and returns when
/// the remote shell closes or the user presses Ctrl+] to detach. Always
/// restores cooked mode on the way out.
fn cli_pty_loop(session: &mut RemoteSession) -> io::Result<()> {
    use std::io::Write;

    use crossterm::{
        event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
        terminal,
    };

    terminal::enable_raw_mode()?;
    if let Ok((cols, rows)) = terminal::size() {
        let _ = session.window_change(cols as u32, rows as u32);
    }

    // Inner closure so we can always disable raw mode afterwards, even on error.
    let pump = || -> io::Result<()> {
        let mut stdout = io::stdout();
        loop {
            if session.pty_closed() {
                break;
            }
            let out = session.drain_output();
            if !out.is_empty() {
                stdout.write_all(&out)?;
                stdout.flush()?;
            }
            // Short poll so we keep draining output even while idle.
            if event::poll(std::time::Duration::from_millis(10))? {
                match event::read()? {
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        // Ctrl+] detaches back to exec mode without closing the shell.
                        if key.modifiers.contains(KeyModifiers::CONTROL)
                            && key.code == KeyCode::Char(']')
                        {
                            break;
                        }
                        if let Some(bytes) = key_to_bytes(key) {
                            session.send(&bytes)?;
                        }
                    }
                    Event::Resize(cols, rows) => {
                        let _ = session.window_change(cols as u32, rows as u32);
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    };

    let result = pump();
    terminal::disable_raw_mode()?;

    // Flush any trailing output produced after the loop broke.
    let out = session.drain_output();
    if !out.is_empty() {
        let mut stdout = io::stdout();
        let _ = stdout.write_all(&out);
        let _ = stdout.flush();
    }
    println!();
    result
}

/// Prompt for a password on the CLI without echoing it. Uses crossterm raw mode
/// to read characters silently; Enter finishes, Esc / Ctrl+C aborts.
fn read_password(prompt: &str) -> io::Result<String> {
    use std::io::Write;

    use crossterm::{
        event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
        terminal,
    };

    print!("{prompt}");
    io::stdout().flush()?;

    terminal::enable_raw_mode()?;
    let mut pw = String::new();
    let mut read = || -> io::Result<bool> {
        loop {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Release {
                    continue;
                }
                if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
                    return Ok(false); // aborted
                }
                match key.code {
                    KeyCode::Enter => return Ok(true),
                    KeyCode::Esc => return Ok(false),
                    KeyCode::Backspace => {
                        pw.pop();
                    }
                    KeyCode::Char(c) => pw.push(c),
                    _ => {}
                }
            }
        }
    };
    let finished = read();
    terminal::disable_raw_mode()?;
    println!();

    match finished {
        Ok(true) => Ok(pw),
        Ok(false) => Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "password entry cancelled",
        )),
        Err(e) => Err(e),
    }
}
