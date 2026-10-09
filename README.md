# Shell
The terminal application I want to use. CLI or GUI.

## What this is

A terminal application with improvements over the native ones it wraps. Good autocomplete. Knowledge of what directories are commonly used. Directory bookmarks. Syntax highlighting. Integrated SSH support. Convenience functionality for git repos and python virtual environments.

Compatible with Windows, Linux, and Mac. Windows users need to have Powershell 7 or higher installed.

This is not a shell scripting language like bash or powershell: It wraps the existing terminal you 
launch it from. In this sense, it differs from `zsh`, `fish` etc: These are full scripting/execution systems, in addition to improved an improved UI. This program provides the latter only.

For a GUI version which has correspondingly more features, see [shell-gui](https://github.com/David-OConnor/shell-gui).

Highlights:

- Syntax highlighting
- Directory bookmarks
- Intuitive autocomplete (fuzzy / substring matching)
- Fish-style autosuggestions and prefix history search
- Shortcuts for common workflows, e.g. with git.

![Example history](/screenshots/his_example_0.png)


## Quickstart
Download and launch from [the releases page](https://github.com/David-OConnor/shell/releases/). If not using Windows
or Ubuntu/Debian, or on an ARM CPU, compile with `cargo r --release`. You may wish to place the executable
somewhere convenient, and add it to the system path; then you can launch by typing `shell`.

### Try these commands:
- `shelp`: Show all commands and key shortcuts.

- Ctrl + B: Bookmark the current directory.
- Ctrl + 1: Show all directory bookmarks
- `bm <number>` (e.g. `bm 2`): Go to this number in the bookmarks
- `bm <a few letters>` to go to a bookmark that contains these letters

- Ctrl + 2: Show recent directories
- `cd <number>` (e.g. `cd 2`): Go to this number in the recent directories
- `cd <a few letters>` to go to a recent directory that contains these letters

- Ctrl + 3: Show command history. Press again to page through older entries
- `his <number>` (e.g. `his 2`): Go to this number in the history
- `his p<page>` (e.g. `his p2`): Jump to a page of the history list
- `his <a few letters>` to go to a recent command that contains these letters

- Ctrl + 4: Show command history in the current directory. Press again to page through older entries
- `this <number>` (e.g. `this 2`): Go to this number in the history
- `this p<page>` (e.g. `this p2`): Jump to a page of the history list
- `this <a few letters>` to go to a recent command that contains these letters

- `remote add username@host`: Add a remote
- Ctrl + 5 (or `remote list`): Show remotes
- `ssh 2` Go to #2 on the remotes list
- `cd cod` + Tab: Go to a bookmark or recent directory that contains these letters, e.g ~/code

- `pull`, `push`, `branch`, `commit`, `checkout`, `clone`: Shorthand for `git pull`, `git push`, etc.
- `clone <name>`: After one clone by full URL, e.g. `clone https://github.com/david-oconnor/shell`, clone
  other repos from the same place by name: `clone lin_alg`.
- `run`, `build`, `fmt`: Shorthand for `cargo run`, `cargo build`, `cargo +nightly fmt`. `run release` runs `cargo run --release`.
- `rm_targets`: Find cargo `target` folders under the current directory, and delete them after confirming.
- `open`: Open the current directory in the OS file browser.

- Use the arrow keys to navigate to recent items
- Press Tab to autocomplete


## Example use

### Using directory bookmarks

#### Loading bookmarks

![Example bookmarks and ssh](/screenshots/bm_example_0.png)

Type `cd`, then a few characters from the folder name, then press tab to complete the bookmark.

## Autocomplete

![Example bookmarks and ssh](/screenshots/recent_dir_example_0.png)

### Autosuggestions
As you type, Shell shows a dimmed (grey) suggestion after the cursor: the most recent command from your history that starts with what you've typed so far. Press Right Arrow or End to accept it; keep typing to ignore it. Suggestions draw on your full saved history, not just the current session.


### Tab completion
Tab completes the `cd` argument against your bookmarks first, then directories on disk (including nested paths like `code/Bi`), then recent directories, then directories nested anywhere under a bookmark (up to 4 levels deep). For example, with `~/code/Bio` bookmarked, `cd plasc` + Tab completes to `~/code/Bio/plascad` from any directory. After `clone`, Tab completes a repo name to its full URL; see [Cloning from common roots](#cloning-from-common-roots). Other commands fall back to filename completion in the current directory.


## Syntax highlighting
The in-progress input is colored as you type: the command word is teal, the subcommand magenta, flags/parameters green, and quote characters orange. The command word turns **red** when it isn't recognized, i.e. it's not a built-in, not a known shell word, and not an executable found on your PATH.


## Git assistance
Run `sync` followed by a commit message in quote. Quotes are optional. This runs the following:
  - `git add .`
  - `git commit -am <the commit message>`
  - `git push`

```shell
sync "A commit message"

// Or:

sync A commit message
```

Warning: This isn't suitable for all workflows. If you use git in a way where it isn't appropriate to sync all gitignored files, this may have unintended consequences!

`pull`, `push`, `branch`, `commit`, `checkout`, and `clone` are shorthand for the `git` command of the same name. Any
arguments are passed through unchanged:

```shell
pull                     // git pull
push origin main         // git push origin main
branch -a                // git branch -a
commit -m "A message"    // git commit -m "A message"
```

### Cloning from common roots
When you clone a repo by its full URL, Shell saves the URL's root: everything before the repo name. After that,
`clone` (or `git clone`) followed by just a repo name clones it from that root:

```shell
clone https://github.com/david-oconnor/shell    // Clones, and saves https://github.com/david-oconnor
clone lin_alg                                   // git clone https://github.com/david-oconnor/lin_alg
git clone -b main graphics                      // git clone -b main https://github.com/david-oconnor/graphics
```

The full command is printed before it runs, so you can see which URL was used. SSH addresses work the same way:
cloning `git@github.com:david-oconnor/shell.git` saves `git@github.com:david-oconnor`.

- A root is saved only when the clone succeeds, so a mistyped URL isn't remembered.
- Several roots can be saved; a bare name uses the one you cloned from most recently. Cloning from a full URL again
  makes its root the most recent.
- Tab completes the repo name: `clone lin` + Tab gives `clone https://github.com/david-oconnor/lin`. With nothing
  typed after `clone`, or the start of a URL, Tab lists your saved roots instead, so you can pick a different one.
- Only bare names are expanded. URLs, paths like `../repo`, and names that match a directory in the current folder
  (a local repo you're cloning) run unchanged.
- URLs containing a password, e.g. `https://user:token@github.com/...`, are never saved.

Roots are saved in the [application state file](#application-state), alongside your bookmarks.

Likewise, `run`, `build`, and `fmt` are shorthand for `cargo run`, `cargo build`, and `cargo +nightly fmt`. Arguments
are passed through, except that a leading `release` becomes `--release`:

```shell
run                      // cargo run
run release              // cargo run --release
build release            // cargo build --release
run -- --some-flag       // cargo run -- --some-flag
fmt                      // cargo +nightly fmt
```


Shell will display the current git branch in the input terminal, if in a directory which hosts
a git repo.


## Cleaning up cargo build folders
Cargo's `target` folders hold build output only, and are recreated by the next build, but often take up several
GB per project. Run `rm_targets` to find them all under the current directory, e.g. in `~/code`, and delete them:

```shell
rm_targets

// Scanning C:\Users\you\code for cargo target folders...
//
//   2.31 GB   C:\Users\you\code\project_a\target
//   0.79 GB   C:\Users\you\code\project_b\target
//   0.09 GB   C:\Users\you\code\project_b\sub_crate\target
//
// 3 folders. Deleting them would free 3.19 GB.
// Delete 3 folders? [y/N]
```

Each folder's size, and the total that deleting them would free, are shown in GB. Nothing is deleted unless you
answer `y`. A folder counts as a cargo `target` folder when it's named `target`, and
has a `Cargo.toml` beside it. Subfolders are searched too, so this finds the folders of sub-crates and workspace
members. Hidden folders (e.g. `.git`), `node_modules`, and symlinks are skipped.

The folders are deleted directly rather than with `cargo clean`, so exactly the listed folders get removed, even if
a project sets a custom target directory, or its `Cargo.toml` no longer builds. If a folder can't be fully deleted,
e.g. because a program built into it is still running, the error is shown, and the rest are still deleted.


### Linux: JournalCtl logs:
Run `logs <service>`, to view the recent Journalctl logs. Linux only. For example, this runs:
`sudo journalctl -u gunicorn -f` for the gunicorn service.


### Typed commands
- `shelp`: List every command and key shortcut, one per line.
- `exit` or `quit`: Exit the program.
- `sync`: Run `git add .`, `git commit -am <the commit message>`, and `git push`.
- `pull`, `push`, `branch`, `commit`, `checkout`, and `clone`: Aliases for `git pull`, `git push`, etc.
- `clone <name>`: Clone a repo by name, from the root of the last repo you cloned by full URL.
- `run`, `build`, and `fmt`: Aliases for `cargo run`, `cargo build`, and `cargo +nightly fmt`. `run release` runs `cargo run --release`.
- `rm_targets`: Find cargo `target` folders under the current directory, list them with their sizes in GB, show the total that would be freed, and delete them after you confirm.
- `open`: Open the current directory in the OS file browser: Explorer on Windows, or the default file manager (e.g. Nautilus on Gnome) via `xdg-open` on Linux.
- `logs`: Runs journalctl -u -f with the service.
- `del bm <number>`: Delete a bookmark by number. 
- `his <number>`: Execute a command from history.
- `his <letters>`: Execute the newest command containing those letters.
- `his p<page>` (e.g. `his p2`): Show a page of the history list; page 1 is the most recent.
- `this <number>` or `this <letters>`: Execute a matching history command from the current directory.
- `this p<page>` (e.g. `this p2`): Show a page of history from the current directory.
- `hisd <number>`: Execute a command from history, in its original working dir.
- `cat`: Displays the contents of a (generally text) file. Similar to the standard Linux operation, but
also works on Windows.
- `cd <number>`: Go to this recent directory (as listed with Ctrl + 2). For bookmarks, use `bm <number>`.
- `bm <number>`: Go to this bookmark (as listed with Ctrl + 1).
- `bm <letters>`: Go to a bookmark whose path contains those letters.
- `cd <letters>`: Go to a bookmarked or recent directory whose path contains those letters.
- `cd <part-of-path>` + Tab key: Go to this directory history item


### SSH

![Example bookmarks and ssh](/screenshots/ssh_example_0.png)

The shell handles `ssh` itself (in-process, via the `russh` library) instead of launching the OS's `ssh` client. Passwords are stored in the OS keyring (Windows Credential Manager / macOS Keychain / Linux Secret Service), never in the state file — so reconnecting to a saved remote needs no re-typing.

- `ssh [user@]host [port]` or `ssh <number>`: Connect to a host, or to a saved
  remote by its `remote list` index. On first connect you're prompted for a
  password (entered hidden), which is then saved to the keyring.
- `remote list` (or Ctrl + 5): List saved remotes with their indices.
- `remote add [user@]host[:port]`: Save a remote (prompts for a password to store).
- `remote del <number>`: Remove a saved remote (and its keyring password).
- While connected, typed commands run on the remote. Two modes:
  - **exec** (default): each command runs and its output is captured.
  - `mode pty`: an interactive shell (full-screen apps like `vim`/`top` work);
    Ctrl+] detaches back to exec mode. `mode exec` switches back.
- `exit` (while connected) disconnects and returns to the local shell.


## Key commands
- Enter key: Send input
- ↑ / ↓: Walk through previously-entered history items (across all directories)   and load each into the input. Replaces the OS shell's default history   behavior. A green ` his N` indicator next to the prompt shows the current
  item — e.g. `S <cwd> his 29 $ ...`. Fish-style prefix search: if you've already typed something, ↑ walks only the history entries that start with it (e.g. type `git` then ↑ to step through past `git` commands); with an empty
  input it walks everything.
- ← / →: Walk through recent directories. Each step loads `cd <path>` into
  the input and shows a green ` cd N` indicator next to the prompt; Enter
  goes there. Left/Right still move the caret when the input has text and
  no cd recall is active.
- Tab key: while using with cd, autocompletes, including to bookmarks.


### Recent or frequent commands
- Ctrl + B: Bookmark the current directory.
- Ctrl + 2: List the most recent directories a command has been executed from.
- Ctrl + 3: List the most recent commands executed.
- Ctrl + 4: List commands entered in the current directory.
- Ctrl + 5: List saved SSH remotes.

- Ctrl + 1: List all bookmarks.
- Ctrl + D: Exit

Note: Ctrl + 1–5 work in the Windows terminal via the bundled line editor.
Most Unix terminals cannot transmit Ctrl + digit; use the typed commands there.

These lists are paginated (newest items first). Press the same keystroke again
to step to the next (older) page, wrapping back to the first page after the
last. For history, `his p<number>` or `this p<number>` jumps straight to a page.


### General terminal commands
Standard line-editing shortcuts, provided by the underlying line editor:

- Ctrl + A / Home: Move the cursor to the start of the line.
- Ctrl + E / End: Move the cursor to the end of the line.
- Ctrl + ← / Ctrl + →: Move the cursor back / forward one word.
- Ctrl + W: Delete the word before the cursor.
- Alt + D: Delete the word after the cursor.
- Ctrl + K: Delete from the cursor to the end of the line.
- Ctrl + U: Delete from the cursor to the start of the line.
- Ctrl + Y: Paste the last deleted text.
- Ctrl + T: Swap the two characters around the cursor. Alt + T swaps words.
- Alt + C / Alt + U / Alt + L: Capitalize / uppercase / lowercase the word at the cursor.
- Ctrl + _: Undo.
- Ctrl + L: Clear the screen.
- Ctrl + N / Ctrl + P: Next / previous history entry.
- Ctrl + C: Cancel the current input.
- Ctrl + D: Exit (on an empty line); delete the character under the cursor otherwise.



## Application state
Application state, including folder bookmarks and git clone roots, is saved in a file called `shell_state.ss`, in the user's
home directory. It is text-based file format with backwards compatibility support.
