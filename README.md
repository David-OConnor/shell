# Shell
Making the terminal application I want to use.

[![Docs](https://docs.rs/dynamics/badge.svg)](https://www.athanorlab.com/docs)

A simple shell; automates the problems I have trouble with in normal shells. Good autocomplete. Good
knowledge of what folders are commonly used. Less typing for my workflows. Automate the boring
parts of terminals. Compatible with Windows, Linux, and Mac. Windows users probably need to have
Powershell 7 or higher installed.

## Example use

### Using directory bookmarks

Saving bookmarks
```sh
```

#### Loading bookmarks

Type `cd`, then a few characters from the folder name, then press tab to complete the bookmark.


### Autocomplete 

### Git assistance
Run `sync` followed by a commit message in quote. This runs the following:
  - `git add .`
  - `git commit -am <the commit message>`
  - `git push`

```shell
sync "A commit message"
```


### Typed commands
- "exit" or "quit": Exit the program.
- "sync": Run `git add .`, `git commit -am <the commit message>`, and `git push`.
- "del bm <number>": Delete a bookmark by number. 


# Key commands
- Enter key: Send input
- Arrow keys:
- Tab key: while using with cd, autocompletes, including to bookmarks.
- Ctrl + B: Bookmark the current directory.
- Alt + B: List all bookmarks.
- Ctrl + D: Exit
- Ctrl + C: Exit



## Application state
Application state, including folder bookmarks, is saved in a file called `shell_state.ss`, in the user's
home directory. It is small, typically a few kb or less, depending on the number of bookmarks stored.