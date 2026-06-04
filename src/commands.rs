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

use std::path::Path;
use std::process::Command;

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
