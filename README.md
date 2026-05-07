# gh-mission-control

> Mission control for terminal-based AI coding agent sessions.

`gh-mission-control` is a [GitHub CLI](https://cli.github.com/) extension that
lets you register, start, monitor, and manage multiple
[tmux](https://github.com/tmux/tmux)-backed AI agent sessions (Claude, Gemini,
Codex, OpenCode, or any custom command) from a single terminal.

---

## Prerequisites

| Dependency | Install |
|---|---|
| [GitHub CLI](https://cli.github.com/) `gh` | See [cli.github.com](https://cli.github.com/) |
| [tmux](https://github.com/tmux/tmux) | `brew install tmux` / `apt install tmux` |
| [Rust toolchain](https://rustup.rs/) (build only) | `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \| sh` |

---

## Installation

### As a `gh` extension (recommended)

```sh
# Install directly from GitHub
gh extension install joshblack/gh-mission-control
```

The extension shell script auto-builds the Rust binary on first run.

### Local development

```sh
# Clone and install from the local directory
git clone https://github.com/joshblack/gh-mission-control.git
cd gh-mission-control
gh extension install .

# Now run it
gh mission-control --help
```

### Build the binary manually

```sh
cargo build --release
./target/release/gh-mission-control --help
```

---

## Quick start

```sh
# 1. Register a session for your project
gh mission-control add ~/projects/my-app --cmd "claude" --title "my-app"

# 2. Start the session (launches tmux in the background)
gh mission-control start my-app

# 3. Attach to the running session
gh mission-control attach my-app

# 4. Detach from tmux as usual:  Ctrl-b d

# 5. Check what's running
gh mission-control list

# 6. Stop the session when done
gh mission-control stop my-app
```

---

## Commands

### `add`

Register a new session in the mission-control registry.

```
gh mission-control add [PATH] --cmd <COMMAND> [--title <TITLE>]
```

| Argument | Required | Description |
|---|---|---|
| `PATH` | No | Project directory (defaults to current directory) |
| `--cmd, -c` | **Yes** | Command to run (e.g. `claude`, `gemini`, `opencode`) |
| `--title, -t` | No | Human-readable name (defaults to the directory name) |

**Examples:**

```sh
# Register from inside the project directory
cd ~/projects/my-app
gh mission-control add --cmd "claude" --title "My App"

# Register with an explicit path
gh mission-control add ~/projects/other --cmd "gemini" --title "Other"

# Use a command with flags
gh mission-control add --cmd "claude --dangerously-skip-permissions"
```

---

### `start`

Start a registered session in a detached tmux session.

```
gh mission-control start <SESSION>
```

`<SESSION>` can be the session ID, an unambiguous ID prefix (≥ 4 chars), or the
exact title (case-insensitive).

```sh
gh mission-control start my-app
gh mission-control start a1b2c3d4      # ID prefix
```

---

### `attach`

Attach your terminal to a running tmux session.

```
gh mission-control attach <SESSION>
```

Detach with the normal tmux shortcut: **Ctrl-b d**.

---

### `stop`

Stop a running tmux session (the registry entry is kept).

```
gh mission-control stop <SESSION>
```

---

### `list`

List all registered sessions with their current status.

```
gh mission-control list
```

Example output:

```
ID          TITLE                   STATUS      PATH                              COMMAND
────────────────────────────────────────────────────────────────────────────────────────────
a1b2c3d4    My App                  ● running   /home/user/projects/my-app        claude
e5f6g7h8    Other Project           ○ stopped   /home/user/projects/other         gemini
```

#### Status indicators

| Icon | Status | Meaning |
|---|---|---|
| `●` | `running` | tmux session is active |
| `◎` | `waiting` | Session is waiting for user input / confirmation |
| `◌` | `idle` | Session is at an idle shell prompt |
| `○` | `stopped` | tmux session does not exist |
| `✗` | `error` | Session is in an error state |

---

### `remove`

Remove a session from the registry. If it is currently running the tmux session
is stopped first.

```
gh mission-control remove <SESSION>
```

---

### `send`

Send text to a running session's tmux pane (Enter is appended automatically).

```
gh mission-control send <SESSION> "<TEXT>"
```

Useful for answering yes/no prompts without attaching:

```sh
gh mission-control send my-app "y"
gh mission-control send my-app "continue"
```

---

### `status`

Show detailed status and a terminal preview for a single session.

```
gh mission-control status <SESSION>
```

---

## Session registry

Sessions are persisted to `~/.gh-mission-control/sessions.json` and survive
process restarts. The registry is a plain JSON file that can be inspected or
edited manually if needed.

Each session record contains:

| Field | Description |
|---|---|
| `id` | Stable UUID |
| `title` | Human-readable name |
| `project_path` | Absolute working directory |
| `command` | Command run inside tmux |
| `tmux_session` | tmux session name (prefixed `ghmc_`) |
| `status` | Last-known status |
| `created_at` | ISO-8601 creation timestamp |
| `updated_at` | ISO-8601 last-modified timestamp |

---

## tmux integration

All tmux sessions created by gh-mission-control use the prefix `ghmc_` so they
are easy to identify:

```sh
tmux list-sessions | grep ghmc_
```

The extension will only stop/kill sessions that match the `tmux_session` name
stored in its own registry, so unrelated tmux sessions are never touched.

---

## Limitations & roadmap

This is an MVP release. The following features are planned for future
iterations:

- [ ] TUI dashboard with live pane preview (Ratatui/Crossterm)
- [ ] More provider-specific status heuristics (Claude hooks, Gemini SDK)
- [ ] Session groups and tree navigation
- [ ] Git worktree integration
- [ ] Configuration file (`~/.gh-mission-control/config.toml`)
- [ ] Pre-built binary releases (no Rust toolchain required after install)
- [ ] `conductor` sessions that monitor and coordinate child agents
- [ ] Event watchers (webhook, GitHub, ntfy)

---

## Development

```sh
# Build
cargo build

# Run tests
cargo test

# Run in debug mode
cargo run -- list
cargo run -- add --cmd "echo hello" --title "test"
```

---

## License

MIT
