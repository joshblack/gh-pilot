# gh-mission-control

A **terminal session manager for AI coding agents** — a `gh` extension written in Rust.

`gh mission-control` gives you a beautiful TUI to manage all your [GitHub Copilot](https://github.com/features/copilot) agent sessions at once. It works like a multiplexer for AI: see which sessions are active, navigate between them, and inspect their output — all from one terminal.

---

## Features

- **Split-pane TUI** — sessions list on the left, session detail/output on the right
- **Sessions grouped by project folder** and sorted newest-first
- **Active / Inactive / Paused** status indicators (`●` / `○` / `⏸`)
- **Vim-style navigation** — `j`/`k` to move, `Enter`/`Space` to select
- **Create new sessions** — `n` to name a session and link it to a project path
- **Delete sessions** — `d` with a confirmation prompt
- **Toggle status** — `t` to cycle a session between Active ↔ Inactive
- **Scrollable log output** — view the output transcript of each session
- **Reload** — `r` to refresh from disk at any time

---

## Installation

### As a `gh` extension

```sh
gh extension install joshblack/gh-mission-control
```

Then run:

```sh
gh mission-control
```

### Build from source

```sh
git clone https://github.com/joshblack/gh-mission-control
cd gh-mission-control
cargo build --release
./target/release/gh-mission-control
```

---

## Key Bindings

### Sessions panel (left)

| Key | Action |
|-----|--------|
| `j` / `↓` | Move selection down |
| `k` / `↑` | Move selection up |
| `Enter` / `Space` | Open session detail |
| `n` | Create new session |
| `d` | Delete selected session |
| `t` | Toggle Active ↔ Inactive |
| `r` | Reload sessions from disk |
| `q` | Quit |

### Detail panel (right)

| Key | Action |
|-----|--------|
| `j` / `↓` | Scroll log down |
| `k` / `↑` | Scroll log up |
| `Esc` / `h` / `←` | Return to sessions list |
| `t` | Toggle status |
| `d` | Delete session |
| `q` | Quit |

---

## Session storage

Sessions are stored as JSON files in:

- **Linux/WSL:** `~/.local/share/gh-mission-control/sessions/`
- **macOS:** `~/Library/Application Support/gh-mission-control/sessions/`

Each session has a metadata file (`<id>.json`) and an optional output log (`<id>.log`).

### Session JSON format

```json
{
  "id": "uuid-v4",
  "name": "feature/auth-refactor",
  "project_path": "~/projects/webapp",
  "created_at": "2024-01-15T10:30:00Z",
  "updated_at": "2024-01-15T11:45:00Z",
  "status": "active",
  "description": "Refactoring the authentication layer",
  "pid": null
}
```

You can append lines to a session's log from your AI agent scripts:

```sh
echo "[$(date)] Working on feature X" >> ~/.local/share/gh-mission-control/sessions/<id>.log
```

---

## Override sessions directory

```sh
gh mission-control --sessions-dir /path/to/sessions
```
