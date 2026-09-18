//! For executing commands, e.g. launching applications etc. Generally thin wrappers
//! over `process::Command`. In addition to wrapping commands your terminal has, it provides
//! overrides in some cases, like directory navigation, and custom commands.
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

use crate::{HistoryItem, path_from_args, quiet_command, ssh, state::State};

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
        // `his p<N>` jumps straight to a page of the history list (1-based,
        // matching the header's `Page N/M`) instead of running an item.
        if let Some(page) = args.strip_prefix('p').and_then(|n| n.parse::<usize>().ok()) {
            if let Ok(h) = state.history.lock() {
                print!("{}", crate::render_history(&h, page.saturating_sub(1)));
            }
            return true;
        }
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
            Err(_) => eprintln!("{cmd}: usage: {cmd} <number>, or {cmd} p<page> to show a page"),
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
                ssh::remote_set_mode(state, args);
                return true;
            }
            // Our own help, not the remote's — answer locally.
            "shelp" => {
                print!("{}", shelp_text(Frontend::Cli));
                return true;
            }
            "--version" | "-v" => {
                println!("{}", version_line());
                return true;
            }
            "logs" => match ssh::remote_logs_command(args, &mut sink) {
                Some(cmd) => Some(cmd),
                None => return true,
            },
            "sync" => match ssh::remote_sync_command(args, &mut sink) {
                Some(cmd) => Some(cmd),
                None => return true,
            },
            _ => Some(input.to_string()),
        };

        if let Some(command) = remote_cmd
            && let Some(session) = state.active_remote.as_mut()
            && let Err(e) = session.run(&command, &mut sink)
        {
            eprintln!("ssh: {e}");
        }
        return true;
    }

    match cmd {
        "exit" | "quit" => return false,

        "shelp" => print!("{}", shelp_text(Frontend::Cli)),

        "--version" | "-v" => println!("{}", version_line()),

        "ssh" => ssh::cmd_ssh(state, state_path, args),

        "remote" => ssh::cmd_remote(state, state_path, args),

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
                        if e.kind() == io::ErrorKind::NotFound
                            && let Some(i) = recent_idx
                        {
                            let mut removed = false;
                            if let Ok(mut list) = state.recent_dirs.lock()
                                && list.get(i).map(|r| r.path == target).unwrap_or(false)
                            {
                                list.remove(i);
                                removed = true;
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

/// Keep Ctrl+C aimed at launched child processes, not the shell itself.
///
/// The passthrough commands below run children with inherited stdio, and the
/// terminal delivers the interrupt keystroke to *every* process attached to
/// it: on Windows the console broadcasts CTRL_C_EVENT to all attached
/// processes, and on Unix SIGINT goes to the whole foreground process group.
/// Without a handler of our own, the default action terminates the shell
/// along with the child, dropping the user out to their outer terminal.
///
/// Call once at startup. Rustyline is unaffected: during `readline()` it puts
/// the terminal in raw mode (`ENABLE_PROCESSED_INPUT` off / `ISIG` off), so
/// Ctrl+C there arrives as an ordinary key event and still cancels the input
/// line rather than going through these process-level handlers.
#[cfg(windows)]
pub fn install_ctrl_c_shield() {
    use windows_sys::Win32::System::Console::{
        CTRL_BREAK_EVENT, CTRL_C_EVENT, SetConsoleCtrlHandler,
    };

    // Returning TRUE marks the event handled, so the system's default handler
    // (ExitProcess) never runs for Ctrl+C / Ctrl+Break. Other events (console
    // close, logoff, shutdown) fall through to the default via FALSE, so
    // closing the terminal window still terminates the shell. Handler lists
    // are per-process — children are unaffected and die from Ctrl+C normally.
    unsafe extern "system" fn handler(event: u32) -> windows_sys::core::BOOL {
        matches!(event, CTRL_C_EVENT | CTRL_BREAK_EVENT).into()
    }
    unsafe {
        SetConsoleCtrlHandler(Some(handler), 1);
    }
}

/// See the Windows variant above for the full story. A no-op *handler
/// function* rather than `SIG_IGN`, because an ignored disposition survives
/// `execve` — `SIG_IGN` would leave children ignoring Ctrl+C too, whereas a
/// caught handler resets to the default action in the child automatically.
/// `libc::signal` (as opposed to raw sigaction) gives BSD semantics on every
/// libc we run on, i.e. SA_RESTART, so the shell's blocking `wait()`/reads
/// aren't interrupted with EINTR when the keystroke lands.
#[cfg(unix)]
pub fn install_ctrl_c_shield() {
    unsafe extern "C" fn noop(_sig: libc::c_int) {}
    let handler: unsafe extern "C" fn(libc::c_int) = noop;
    unsafe {
        libc::signal(libc::SIGINT, handler as libc::sighandler_t);
        // Ctrl+\ — would otherwise kill the shell with a core dump; bash
        // ignores it at the interactive prompt for the same reason.
        libc::signal(libc::SIGQUIT, handler as libc::sighandler_t);
    }
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

/// Which frontend is asking for the [shelp_text] listing. The two shells
/// differ in a handful of places — the GUI has always-visible panels instead
/// of the CLI's list keystrokes, and its window can't be closed by typing
/// `exit` — so each gets its own rows for those.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Frontend {
    Cli,
    Gui,
}

/// Append `rows` as `  - <invocation>   <description>` lines, with the
/// invocation column padded so the descriptions line up.
fn push_help_rows(out: &mut String, rows: &[(&str, &str)]) {
    let width = rows.iter().map(|(name, _)| name.len()).max().unwrap_or(0);
    for (name, desc) in rows {
        out.push_str(&format!("  - {name:width$}   {desc}\n"));
    }
}

/// The `Shell version x.y.z` line shown by `--version` / `-v` and at the top
/// of [shelp_text]. The number comes from `Cargo.toml`.
pub fn version_line() -> String {
    format!("Shell version {}", env!("CARGO_PKG_VERSION"))
}

/// Text for the `shelp` built-in: every command and key shortcut, one per
/// line, indented with a dash. Lives here (rather than in either frontend) so
/// the CLI and GUI listings can't drift apart — the CLI prints the string, the
/// GUI pushes it into its output pane.
pub fn shelp_text(frontend: Frontend) -> String {
    let cli = frontend == Frontend::Cli;

    let mut commands: Vec<(&str, &str)> = vec![
        ("shelp", "Show this list of commands and key shortcuts"),
        ("--version, -v", "Show the shell's version"),
        (
            "cd <path>",
            "Change directory. Takes `~`, a real path, or the start of a bookmark's name",
        ),
        ("cd <number>", "Go to a recent directory by its index"),
        ("bm <number>", "Go to a bookmark by its index"),
        ("del bm <number>", "Delete a bookmark by its index"),
        (
            "cat <file>",
            "Print a file's contents; works on Windows too",
        ),
        (
            "his <number>",
            "Re-run a command from history (`hist` also works)",
        ),
    ];
    if cli {
        // The GUI shows history in a panel, so it has no page-jump form.
        commands.push((
            "his p<page>",
            "Show a page of the history list; page 1 is the newest",
        ));
    }
    commands.extend_from_slice(&[
        (
            "hisd <number>",
            "Re-run a history item in the directory it was originally run from",
        ),
        (
            "sync <message>",
            "`git add .`, then `git commit -am <message>`, then `git push`",
        ),
        (
            "logs <service>",
            "Show a systemd service's journalctl logs. Linux only",
        ),
        (
            "ssh [user@]host [port]",
            "Connect to a host; `ssh <number>` connects to a saved remote",
        ),
        ("remote list", "List saved remotes with their indices"),
        (
            "remote add [user@]host[:port]",
            "Save a remote, prompting for a password to keep in the OS keyring",
        ),
        (
            "remote del <number>",
            "Forget a saved remote, and its stored password",
        ),
        (
            "mode exec|pty",
            "While connected: captured per-command output, or an interactive shell",
        ),
    ]);
    commands.push(if cli {
        (
            "exit, quit",
            "Exit the shell. While connected to a remote, disconnect instead",
        )
    } else {
        (
            "exit, quit",
            "While connected to a remote, disconnect. Otherwise, close the window",
        )
    });
    commands.extend_from_slice(&[
        (
            "python, pip",
            "Run the current directory's virtualenv copy, when it has one",
        ),
        (
            "(anything else)",
            "Passed through to the system shell: PowerShell 7 on Windows, `sh` elsewhere",
        ),
    ]);

    let keys: &[(&str, &str)] = if cli {
        &[
            ("Enter", "Run the input"),
            (
                "Tab",
                "Autocomplete: bookmarks and directories after `cd`, filenames otherwise",
            ),
            (
                "Up / Down",
                "Walk command history. With text already typed, only entries starting with it",
            ),
            (
                "Left / Right",
                "Walk recent directories, loading `cd <path>` into the input",
            ),
            (
                "Right / End",
                "Accept the dimmed autosuggestion at the end of the line",
            ),
            ("Ctrl + B", "Bookmark the current directory"),
            ("Alt + B, or Ctrl + 1", "List bookmarks"),
            ("Ctrl + O, or Ctrl + 2", "List recent directories"),
            ("Ctrl + H, or Ctrl + 3", "List command history"),
            ("Ctrl + R, or Ctrl + 4", "List saved SSH remotes"),
            ("Ctrl + ]", "Leave an interactive (PTY) remote shell"),
            ("Ctrl + C", "Cancel the current input"),
            ("Ctrl + D", "Exit"),
        ]
    } else {
        &[
            ("Enter", "Run the input"),
            ("Tab", "Autocomplete the input"),
            ("Up / Down", "Walk command history"),
            (
                "Left / Right",
                "Walk recent directories, while the input is empty",
            ),
        ]
    };

    let mut out = format!("\n{}\n\nCommands:\n", version_line());
    push_help_rows(&mut out, &commands);
    out.push_str("\nKey shortcuts:\n");
    push_help_rows(&mut out, keys);
    if cli {
        out.push_str(
            "\nPress a list's keystroke again to page back through older entries.\n\
             The Ctrl + 1-4 aliases are Windows-only.\n",
        );
    }
    out
}
