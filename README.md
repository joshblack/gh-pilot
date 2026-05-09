# gh-pilot

`gh pilot` is a GitHub CLI extension for managing GitHub Copilot CLI
sessions from a terminal UI.

It reads Copilot's local session files, shows recent sessions and remote agent
tasks, and lets you open or start Copilot sessions without leaving the UI.

## Installation

Install as a `gh` extension:

```sh
gh extension install joshblack/gh-pilot
```

Run the following command:

```sh
gh pilot
```

## Usage

Interact with sessions using arrow keys or j/k momvement. You can also view all keybindings with <kbd>?</kbd>.

When launching a copilot session, it will open the CLI in a new tmux session. To leave the session, use <kbd>Ctrl+b</kbd> (your tmux leader key) and then <kbd>d</kbd> to detach. You can also `/quit` and it will return you to the main session list.

## Requirements

- [GitHub CLI](https://cli.github.com/)
- GitHub Copilot CLI available as `copilot` (for example, via `gh copilot`)
- `tmux`, used for embedded Copilot terminals
- Rust and Cargo, only if building from source

Remote agent tasks require an authenticated GitHub CLI with `gh agent-task list`
support.

## Data sources

`gh-pilot` reads Copilot data from:

- `~/.copilot/session-state/<id>/workspace.yaml` for local session metadata
- `~/.copilot/session-state/<id>/events.jsonl` for live session status
- `~/.copilot/session-store.db` for session summaries and conversation history
- `gh agent-task list --json ...` for remote agent tasks

Local sessions are shown when they have been active within the last seven days.
