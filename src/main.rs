use std::{env, process::Command};

use chrono::{DateTime, Utc};
use rustyline::{DefaultEditor, error::ReadlineError};

struct HistoryItem {
    pub text: String,
    pub dt: DateTime<Utc>,
}

struct State {
    pub history: Vec<HistoryItem>,
    pub cwd: std::path::PathBuf,
}

impl State {
    fn new() -> Self {
        Self {
            history: Vec::new(),
            cwd: env::current_dir().unwrap_or_default(),
        }
    }

    fn prompt(&self) -> String {
        format!("{} $ ", self.cwd.display())
    }
}

/// Runs one command line. Returns false if the shell should exit.
fn run_command(state: &mut State, input: &str) -> bool {
    let input = input.trim();
    if input.is_empty() {
        return true;
    }

    state.history.push(HistoryItem {
        text: input.to_string(),
        dt: Utc::now(),
    });

    // Split into command + remainder for built-in dispatch.
    let (cmd, args) = match input.find(char::is_whitespace) {
        Some(i) => (&input[..i], input[i..].trim()),
        None => (input, ""),
    };

    match cmd {
        "exit" | "quit" => return false,

        "cd" => {
            let target = if args.is_empty() {
                // cd with no args goes home
                env::var("USERPROFILE")
                    .or_else(|_| env::var("HOME"))
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|_| state.cwd.clone())
            } else {
                state.cwd.join(args)
            };
            match env::set_current_dir(&target) {
                Ok(_) => state.cwd = env::current_dir().unwrap_or(target),
                Err(e) => eprintln!("cd: {e}"),
            }
        }

        // Everything else: passthrough to the system shell.
        _ => {
            let result = if cfg!(windows) {
                Command::new("cmd").args(["/C", input]).status()
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

fn main() {
    let mut state = State::new();

    // DefaultEditor gives us: line editing, arrow-key history, Ctrl+A/E/K/W, etc.
    let mut rl = match DefaultEditor::new() {
        Ok(rl) => rl,
        Err(e) => {
            eprintln!("Failed to start editor: {e}");
            return;
        }
    };

    loop {
        match rl.readline(&state.prompt()) {
            Ok(line) => {
                if !line.trim().is_empty() {
                    let _ = rl.add_history_entry(&line);
                }
                if !run_command(&mut state, &line) {
                    break;
                }
            }
            Err(ReadlineError::Interrupted) => println!("^C"), // Ctrl+C clears line
            Err(ReadlineError::Eof) => break,                  // Ctrl+D exits
            Err(e) => {
                eprintln!("Error: {e}");
                break;
            }
        }
    }
}
