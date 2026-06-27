# Shell
Making the terminal application I want to use. CLI or GUI.

[![Docs](https://docs.rs/dynamics/badge.svg)](https://www.athanorlab.com/docs)


## What this is

A terminal application with improvements over the native ones it wraps. Good autocomplete. Knowledge of what folders are commonly used. Less typing. Syntax highlighting. First-class SSH support. Convenience functions for git repos and python virtual environments.

Compatible with Windows, Linux, and Mac. Windows users need to have Powershell 7 or higher installed.

This is not a shell scripting language like bash or powershell: It wraps the existing terminal you 
launch it from. In this sense, it differs from `zsh`, `fish` etc: These are full scripting/execution systems, in addition to improved an improved UI. This program provides the latter only.

For a GUI version which has correspondingly more features, see [shell-gui](https://github.com/David-OConnor/shell-gui).

Highlights:

- Syntax highlighting (unrecognised commands shown in red)
- Directory bookmarks
- Intuitive autocomplete (fuzzy / substring matching)
- Fish-style autosuggestions and prefix history search
- Shortcuts for common workflows, e.g. with git.


## Example use

### Using directory bookmarks

Saving bookmarks
```sh
```

#### Loading bookmarks

Type `cd`, then a few characters from the folder name, then press tab to complete the bookmark.


## Autocomplete

### Autosuggestions (fish-style ghost text)
As you type, Shell shows a dimmed (grey) suggestion after the cursor: the most recent command from your history that starts with what you've typed so far. Press → (Right arrow) or End — at the end of the line — to accept it; keep typing to ignore it. Suggestions draw on your full saved history, not just the current session.


### Tab completion
Tab completes the `cd` argument against your bookmarks first, then directories on disk (including nested paths like `code/Bi`). Other commands fall back to filename completion in the current directory.

Matching is fuzzy, ranked best-first: an exact prefix wins, then a substring match (e.g. `cd ponents` → `components`), then a subsequence/fuzzy match where the typed characters appear in order (e.g. `cd cpt` → `components`). Matching is case-insensitive throughout.


## Syntax highlighting
The in-progress input is colored as you type: the command word is teal, the subcommand magenta, flags/parameters green, and quote characters orange. The command word turns **red** when it isn't recognized — i.e. it's not a built-in, not a known shell word, and not an executable found on your PATH.


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
- `hisd <number>`: Execute a command from history, in its original working dir.
- `cat`: Displays the contents of a (generally text) file. Similar to the standard Linux operation, but
also works on Windows.
- `cd <number>`: Go to this recent directory (As listed with Ctrl + R). Or bookmark.
- `bm <number>`: Go to this bookmark (As listed with Alt + B)
- `cd <part-of-path>` + Tab key: Go to this directory history item


### SSH
The shell handles `ssh` itself (in-process, via the `russh` library) instead of launching the OS's `ssh` client. Passwords are stored in the OS keyring (Windows Credential Manager / macOS Keychain / Linux Secret Service), never in the state file — so reconnecting to a saved remote needs no re-typing.

- `ssh [user@]host [port]` or `ssh <number>`: Connect to a host, or to a saved
  remote by its `remote list` index. On first connect you're prompted for a
  password (entered hidden), which is then saved to the keyring.
- `remote list`: List saved remotes with their indices.
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
- Ctrl + R: List the most recent directories a command has been executed from.
- Ctrl + H: List the most recent items from history.

- Alt + B: List all bookmarks.
- Ctrl + D: Exit



## Application state
Application state, including folder bookmarks, is saved in a file called `shell_state.ss`, in the user's
home directory. It is small, typically a few tens of kb.
