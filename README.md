# gh-pilot

`gh pilot` is a GitHub CLI extension for managing GitHub Copilot CLI
sessions from a terminal UI.

It reads Copilot's local session files, shows recent sessions and remote agent
tasks, and lets you open or start Copilot sessions without leaving the UI.

## What it does

- Lists local Copilot CLI sessions from `~/.copilot/session-state/`
- Shows session title, directory, repository, branch, status, and recent activity
- Reads conversation history from `~/.copilot/session-store.db`
- Opens local sessions in an embedded Copilot terminal backed by `tmux`
- Starts a new Copilot session in the current directory or another path
- Polls active sessions for Running, Waiting, Idle, and Error status changes
- Lists remote agent tasks from `gh agent-task list` when that command is
  available

- **Reads real Copilot CLI sessions** from `~/.copilot/session-state/` — no extra configuration
- **Split-pane TUI** — sessions list on the left, session detail/conversation on the right
- **Sessions grouped by working directory** and sorted newest-first
- **Smart status polling** with Running (`●` green), Waiting (`◐` yellow), Idle (`○` gray), and Error (`✕` red) indicators
- **Conversation history** — view user messages and Copilot responses in the detail pane
- **Vim-style navigation** — `j`/`k` to move, `Enter`/`Space` to view detail
- **Launch new sessions** — `n` to start `copilot -C <dir>` with the current directory pre-filled
- **Resume sessions** — `o` to resume any existing session with `copilot --resume=<id>`
- **Embedded Copilot terminal** — resumed and newly launched sessions run in tmux-backed panes
- **Reload** — `r` to refresh from disk at any time
- **Shortcut help** — `?` shows a scrollable shortcut reference

Remote agent tasks are shown for visibility, but they cannot be opened in the
local embedded terminal.

## Requirements

- [GitHub CLI](https://cli.github.com/)
- GitHub Copilot CLI available as `copilot` (for example, via `gh copilot`)
- `tmux`, used for embedded Copilot terminals
- Rust and Cargo, only if building from source

Remote agent tasks require an authenticated GitHub CLI with `gh agent-task list`
support.

## Install and start

Install as a `gh` extension:

```sh
gh extension install joshblack/gh-pilot
```

`gh extension install` downloads the prebuilt binary from the latest GitHub
Release for your platform. Releases are published automatically when a `v*` tag
is pushed.

Start the UI:

```sh
gh pilot
```

Build and run from source:

```sh
git clone https://github.com/joshblack/gh-pilot
cd gh-pilot
cargo build --release
./target/release/gh-pilot
```

## Usage

| Key | Action |
| --- | --- |
| `j` / `Down` | Move down or scroll down |
| `k` / `Up` | Move up or scroll up |
| `Enter` / `Space` | View the selected session |
| `o` | Open or resume the selected local session in Copilot |
| `n` | Start a new Copilot session |
| `r` | Reload sessions from disk |
| `?` | Show or hide shortcut help |
| `q` | Quit from normal mode |
| `Ctrl+C` | Quit |

When an embedded Copilot terminal is open:

| Key | Action |
| --- | --- |
| `Ctrl+F` | Toggle fullscreen |
| `Ctrl+W` | Detach from the embedded session |
| Mouse input | Forwarded to Copilot while fullscreen |

When starting a new session, the prompt is pre-filled with the current session's
directory when one is selected. Otherwise it uses the directory where
`gh pilot` was started.

## Data sources

`gh-pilot` reads Copilot data from:

- `~/.copilot/session-state/<id>/workspace.yaml` for local session metadata
- `~/.copilot/session-state/<id>/events.jsonl` for live session status
- `~/.copilot/session-store.db` for session summaries and conversation history
- `gh agent-task list --json ...` for remote agent tasks

Local sessions are shown when they have been active within the last seven days.

### Embedded terminal

| Key | Action |
|-----|--------|
| `Ctrl+F` | Toggle fullscreen |
| `Ctrl+W` | Detach from the embedded terminal |
| Other keys | Forward input to Copilot |

Embedded terminals are tmux clients. Detaching closes only the in-app terminal view; the backing Copilot process keeps running in its tmux session so it can be resumed later.
