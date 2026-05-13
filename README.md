# gh-pilot

`gh pilot` is a GitHub CLI extension for managing GitHub Copilot CLI
sessions from a tmux-backed terminal UI.

## Installation

Install as a `gh` extension:

```sh
gh extension install joshblack/gh-pilot
```

Run the following command:

```sh
gh pilot
```

## Session cache

`gh-pilot` stores the last known local and remote session list in SQLite so the
tree can be restored between restarts while live tmux and remote data refresh in
the background. The database is stored at
`$XDG_CONFIG_HOME/gh-pilot/sessions.sqlite3`, or
`~/.config/gh-pilot/sessions.sqlite3` when `XDG_CONFIG_HOME` is not set.
