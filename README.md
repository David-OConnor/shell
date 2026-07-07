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
- Ctrl + B: Bookmark the current directory.
- Alt + B: Show all directory bookmarks
- `bm <number>` (e.g. `bm 2`): Go to this number in the bookmarks
- `bm <a few letters>` to go to a bookmark that contains these letters

- Ctrl + O: Show recent directories
- `cd <number>` (e.g. `cd 2`): Go to this number in the recent directories
- `cd <a few letters>` to go to a recent directory that contains these letters

- Ctrl + H: Show command history. Press again to page through older entries
- `his <number>` (e.g. `his 2`): Go to this number in the history
- `his p<page>` (e.g. `his p2`): Jump to a page of the history list
- `his <a few letters>` to go to a recent command that contains these letters

- `remote add username@host`: Add a remote
- Ctrl + R (or `remote list`): Show remotes
- `ssh 2` Go to #2 on the remotes list
- `cd cod` + Tab: Go to a bookmark or recent directory that contains these letters, e.g ~/code

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
Tab completes the `cd` argument against your bookmarks first, then directories on disk (including nested paths like `code/Bi`). Other commands fall back to filename completion in the current directory.


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


Shell will display the current git branch in the input terminal, if in a directory which hosts
a git repo.


### Linux: JournalCtl logs:
Run `logs <service>`, to view the recent Journalctl logs. Linux only. For example, this runs:
`sudo journalctl -u gunicorn -f` for the gunicorn service.


### Typed commands
- `exit` or `quit`: Exit the program.
- `sync`: Run `git add .`, `git commit -am <the commit message>`, and `git push`.
- `logs`: Runs journalctl -u -f with the service.
- `del bm <number>`: Delete a bookmark by number. 
- `his <number>`: Execute a command from history.
- `his p<page>` (e.g. `his p2`): Show a page of the history list; page 1 is the most recent.
- `hisd <number>`: Execute a command from history, in its original working dir.
- `cat`: Displays the contents of a (generally text) file. Similar to the standard Linux operation, but
also works on Windows.
- `cd <number>`: Go to this recent directory (As listed with Ctrl + O). For bookmarks, use `bm <number>`.
- `bm <number>`: Go to this bookmark (As listed with Alt + B)
- `cd <part-of-path>` + Tab key: Go to this directory history item


### SSH

![Example bookmarks and ssh](/screenshots/ssh_example_0.png)

The shell handles `ssh` itself (in-process, via the `russh` library) instead of launching the OS's `ssh` client. Passwords are stored in the OS keyring (Windows Credential Manager / macOS Keychain / Linux Secret Service), never in the state file — so reconnecting to a saved remote needs no re-typing.

- `ssh [user@]host [port]` or `ssh <number>`: Connect to a host, or to a saved
  remote by its `remote list` index. On first connect you're prompted for a
  password (entered hidden), which is then saved to the keyring.
- `remote list` (or Ctrl + R): List saved remotes with their indices.
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
- Ctrl + O: List the most recent directories a command has been executed from.
- Ctrl + H: List the most recent items from history.
- Ctrl + R: List saved SSH remotes.

- Alt + B: List all bookmarks.
- Ctrl + D: Exit

These lists are paginated (newest items first). Press the same keystroke again
to step to the next (older) page, wrapping back to the first page after the
last. For history, `his p<number>` jumps straight to a page.


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
Application state, including folder bookmarks, is saved in a file called `shell_state.ss`, in the user's
home directory. It is text-based file format with backwards compatibility support.
