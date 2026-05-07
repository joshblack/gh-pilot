mod registry;
mod session;
mod status;
mod tmux;

use std::path::Path;

use anyhow::Result;
use chrono::Utc;
use clap::{Parser, Subcommand};

use registry::Registry;
use session::{Session, Status};

// ---------------------------------------------------------------------------
// CLI definition
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(
    name = "gh-mission-control",
    about = "Mission control for terminal-based AI agent sessions",
    long_about = "A GitHub CLI extension for managing multiple terminal-based AI coding agent\n\
                  sessions backed by tmux. Register, start, monitor, and attach to agent\n\
                  sessions from a single command.",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Register a new session in the mission-control registry.
    Add {
        /// Project directory (defaults to current working directory).
        path: Option<String>,

        /// Command to run inside the tmux session (e.g. `claude`, `gemini`).
        #[arg(long, short)]
        cmd: String,

        /// Human-readable title for the session (defaults to the directory name).
        #[arg(long, short)]
        title: Option<String>,
    },

    /// Start a registered session inside a new tmux session.
    Start {
        /// Session ID (or unambiguous prefix / title).
        session: String,
    },

    /// Attach your terminal to a running tmux session.
    Attach {
        /// Session ID (or unambiguous prefix / title).
        session: String,
    },

    /// Stop a running tmux session (the registry entry is preserved).
    Stop {
        /// Session ID (or unambiguous prefix / title).
        session: String,
    },

    /// List all registered sessions with their current status.
    List,

    /// Remove a session from the registry (stops it first if running).
    Remove {
        /// Session ID (or unambiguous prefix / title).
        session: String,
    },

    /// Send text to a running session's tmux pane (Enter is appended).
    Send {
        /// Session ID (or unambiguous prefix / title).
        session: String,

        /// Text / keys to send to the session.
        message: String,
    },

    /// Show the current status and a preview of a session's terminal output.
    Status {
        /// Session ID (or unambiguous prefix / title).
        session: String,
    },
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Add { path, cmd, title } => cmd_add(path, cmd, title),
        Commands::Start { session } => cmd_start(&session),
        Commands::Attach { session } => cmd_attach(&session),
        Commands::Stop { session } => cmd_stop(&session),
        Commands::List => cmd_list(),
        Commands::Remove { session } => cmd_remove(&session),
        Commands::Send { session, message } => cmd_send(&session, &message),
        Commands::Status { session } => cmd_status(&session),
    }
}

// ---------------------------------------------------------------------------
// Command implementations
// ---------------------------------------------------------------------------

fn cmd_add(path: Option<String>, cmd: String, title: Option<String>) -> Result<()> {
    // Resolve the project path.
    let project_path = match path {
        Some(p) => {
            let expanded = expand_tilde(&p);
            // Canonicalize when the path already exists; fall back to the raw
            // value when it does not (the user may create it later).
            if Path::new(&expanded).exists() {
                std::fs::canonicalize(&expanded)
                    .unwrap_or_else(|_| expanded.clone().into())
                    .to_string_lossy()
                    .into_owned()
            } else {
                expanded
            }
        }
        None => std::env::current_dir()
            .unwrap_or_else(|_| ".".into())
            .to_string_lossy()
            .into_owned(),
    };

    // Default title to the last component of the path.
    let title = title.unwrap_or_else(|| {
        Path::new(&project_path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "session".to_string())
    });

    let session = Session::new(title.clone(), project_path.clone(), cmd.clone());
    let id = session.id.clone();
    let short_id = &id[..8];

    let mut registry = Registry::load()?;
    registry.add(session);
    registry.save()?;

    println!("✓ Session registered: '{}'", title);
    println!("  ID:      {}", id);
    println!("  Path:    {}", project_path);
    println!("  Command: {}", cmd);
    println!();
    println!("Start it with:  gh mission-control start {}", short_id);

    Ok(())
}

fn cmd_start(query: &str) -> Result<()> {
    tmux::check_available()?;

    let mut registry = Registry::load()?;

    // Collect the data we need while the immutable borrow is in scope.
    let (id, tmux_name, cwd, cmd, title) = {
        let s = registry
            .find(query)
            .ok_or_else(|| anyhow::anyhow!("No session found matching '{}'", query))?;
        (
            s.id.clone(),
            s.tmux_session.clone(),
            s.project_path.clone(),
            s.command.clone(),
            s.title.clone(),
        )
    };

    if tmux::session_exists(&tmux_name) {
        println!("Session '{}' is already running.", title);
        println!("Attach with:  gh mission-control attach {}", &id[..8]);
        return Ok(());
    }

    if !Path::new(&cwd).exists() {
        anyhow::bail!(
            "Project directory does not exist: {}\n\
             Update the session or create the directory first.",
            cwd
        );
    }

    tmux::new_session(&tmux_name, &cwd, &cmd)?;

    // Mark the session as running.
    if let Some(s) = registry.find_mut(query) {
        s.status = Status::Running;
        s.updated_at = Utc::now();
    }
    registry.save()?;

    println!("✓ Started session '{}'", title);
    println!("  tmux session: {}", tmux_name);
    println!();
    println!("Attach with:  gh mission-control attach {}", &id[..8]);

    Ok(())
}

fn cmd_attach(query: &str) -> Result<()> {
    tmux::check_available()?;

    let registry = Registry::load()?;
    let (id, tmux_name, title) = {
        let s = registry
            .find(query)
            .ok_or_else(|| anyhow::anyhow!("No session found matching '{}'", query))?;
        (s.id.clone(), s.tmux_session.clone(), s.title.clone())
    };

    if !tmux::session_exists(&tmux_name) {
        println!("Session '{}' is not currently running.", title);
        println!("Start it with:  gh mission-control start {}", &id[..8]);
        return Ok(());
    }

    println!("Attaching to '{}' (tmux: {}) …", title, tmux_name);
    tmux::attach(&tmux_name)?;

    Ok(())
}

fn cmd_stop(query: &str) -> Result<()> {
    tmux::check_available()?;

    let mut registry = Registry::load()?;
    let (tmux_name, title) = {
        let s = registry
            .find(query)
            .ok_or_else(|| anyhow::anyhow!("No session found matching '{}'", query))?;
        (s.tmux_session.clone(), s.title.clone())
    };

    if tmux::session_exists(&tmux_name) {
        tmux::kill_session(&tmux_name)?;
        println!("✓ Stopped session '{}'", title);
    } else {
        println!("Session '{}' is not running.", title);
    }

    // Always sync stored status to stopped.
    if let Some(s) = registry.find_mut(query) {
        s.status = Status::Stopped;
        s.updated_at = Utc::now();
    }
    registry.save()?;

    Ok(())
}

fn cmd_list() -> Result<()> {
    let mut registry = Registry::load()?;
    let tmux_available = tmux::check_available().is_ok();

    // Collect session IDs and tmux names before any mutable access.
    let ids_and_tmux: Vec<(String, String)> = registry
        .sessions()
        .iter()
        .map(|s| (s.id.clone(), s.tmux_session.clone()))
        .collect();

    if ids_and_tmux.is_empty() {
        println!("No sessions registered.");
        println!();
        println!("Register a session with:");
        println!("  gh mission-control add --cmd <command> --title <title>");
        return Ok(());
    }

    // Refresh statuses from tmux if it is available.
    if tmux_available {
        // Fetch all active tmux sessions in one call for efficiency.
        let active: std::collections::HashSet<String> =
            tmux::list_sessions().into_iter().collect();

        for (id, tmux_name) in &ids_and_tmux {
            let current = if active.contains(tmux_name) {
                status::detect_status(tmux_name)
            } else {
                Status::Stopped
            };
            if let Some(s) = registry.get_mut(id) {
                s.status = current;
                s.updated_at = Utc::now();
            }
        }
        registry.save()?;
    }

    // Print a simple table.
    println!("{:<10}  {:<22}  {:<10}  {:<32}  COMMAND", "ID", "TITLE", "STATUS", "PATH");
    println!("{}", "─".repeat(92));

    for s in registry.sessions() {
        let id_short = &s.id[..8];
        let title = truncate_str(&s.title, 22);
        let status_str = status_label(&s.status);
        let path = truncate_path(&s.project_path, 32);
        let cmd = truncate_str(&s.command, 24);

        println!(
            "{:<10}  {:<22}  {:<10}  {:<32}  {}",
            id_short, title, status_str, path, cmd
        );
    }

    Ok(())
}

fn cmd_remove(query: &str) -> Result<()> {
    let mut registry = Registry::load()?;

    let (session_id, tmux_name, title) = {
        let s = registry
            .find(query)
            .ok_or_else(|| anyhow::anyhow!("No session found matching '{}'", query))?;
        (s.id.clone(), s.tmux_session.clone(), s.title.clone())
    };

    // Stop the tmux session if it is currently running.
    if tmux::check_available().is_ok() && tmux::session_exists(&tmux_name) {
        tmux::kill_session(&tmux_name)?;
        println!("  ↳ Stopped tmux session '{}'", tmux_name);
    }

    registry.remove(&session_id);
    registry.save()?;

    println!("✓ Removed session '{}'", title);

    Ok(())
}

fn cmd_send(query: &str, message: &str) -> Result<()> {
    tmux::check_available()?;

    let registry = Registry::load()?;
    let (tmux_name, title, id) = {
        let s = registry
            .find(query)
            .ok_or_else(|| anyhow::anyhow!("No session found matching '{}'", query))?;
        (s.tmux_session.clone(), s.title.clone(), s.id.clone())
    };

    if !tmux::session_exists(&tmux_name) {
        anyhow::bail!(
            "Session '{}' is not running.\n\
             Start it with:  gh mission-control start {}",
            title,
            &id[..8]
        );
    }

    tmux::send_keys(&tmux_name, message)?;
    println!("✓ Sent to '{}': {:?}", title, message);

    Ok(())
}

fn cmd_status(query: &str) -> Result<()> {
    let tmux_available = tmux::check_available().is_ok();

    let mut registry = Registry::load()?;

    let (id, tmux_name, title, project_path, command) = {
        let s = registry
            .find(query)
            .ok_or_else(|| anyhow::anyhow!("No session found matching '{}'", query))?;
        (
            s.id.clone(),
            s.tmux_session.clone(),
            s.title.clone(),
            s.project_path.clone(),
            s.command.clone(),
        )
    };

    // Refresh status if tmux is available.
    if tmux_available {
        let current = status::detect_status(&tmux_name);
        if let Some(s) = registry.find_mut(query) {
            s.status = current;
            s.updated_at = Utc::now();
        }
        registry.save()?;
    }

    let current_status = registry.get(&id).map(|s| s.status.clone()).unwrap_or(Status::Stopped);

    println!("Session: {}", title);
    println!("  ID:      {}", id);
    println!("  Status:  {}", status_label(&current_status));
    println!("  Path:    {}", project_path);
    println!("  Command: {}", command);
    println!("  tmux:    {}", tmux_name);

    // Show a pane preview when the session is active.
    if tmux_available && tmux::session_exists(&tmux_name) {
        if let Ok(pane) = tmux::capture_pane(&tmux_name) {
            let preview: String = pane
                .lines()
                .rev()
                .filter(|l| !l.trim().is_empty())
                .take(10)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .map(|l| format!("  │ {}", l))
                .collect::<Vec<_>>()
                .join("\n");
            if !preview.is_empty() {
                println!();
                println!("  Terminal preview (last 10 lines):");
                println!("{}", preview);
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Display helpers
// ---------------------------------------------------------------------------

fn status_label(s: &Status) -> String {
    match s {
        Status::Running => "● running".to_string(),
        Status::Waiting => "◎ waiting".to_string(),
        Status::Stopped => "○ stopped".to_string(),
        Status::Idle => "◌ idle".to_string(),
        Status::Error => "✗ error".to_string(),
    }
}

/// Truncate a string to at most `max_chars` display columns, appending `…`
/// if truncation occurs.
fn truncate_str(s: &str, max_chars: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_chars {
        s.to_string()
    } else {
        let truncated: String = chars[..max_chars.saturating_sub(1)].iter().collect();
        format!("{}…", truncated)
    }
}

/// Like [`truncate_str`] but trims from the *left* of a path so the most
/// specific (rightmost) part is visible.
fn truncate_path(path: &str, max_chars: usize) -> String {
    let chars: Vec<char> = path.chars().collect();
    if chars.len() <= max_chars {
        path.to_string()
    } else {
        let suffix: String = chars[chars.len() - (max_chars.saturating_sub(1))..].iter().collect();
        format!("…{}", suffix)
    }
}

/// Expand a leading `~` to the user's home directory.
fn expand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return format!("{}/{}", home.display(), rest);
        }
    } else if path == "~" {
        if let Some(home) = dirs::home_dir() {
            return home.to_string_lossy().into_owned();
        }
    }
    path.to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_str_no_truncation() {
        assert_eq!(truncate_str("hello", 10), "hello");
    }

    #[test]
    fn test_truncate_str_exact_length() {
        assert_eq!(truncate_str("hello", 5), "hello");
    }

    #[test]
    fn test_truncate_str_truncates() {
        let result = truncate_str("hello world", 8);
        assert!(result.ends_with('…'));
        assert!(result.chars().count() <= 8);
    }

    #[test]
    fn test_truncate_path_no_truncation() {
        assert_eq!(truncate_path("/a/b/c", 20), "/a/b/c");
    }

    #[test]
    fn test_truncate_path_truncates_from_left() {
        let result = truncate_path("/very/long/path/to/project", 12);
        assert!(result.starts_with('…'));
        assert!(result.chars().count() <= 12);
    }

    #[test]
    fn test_expand_tilde_replaces_prefix() {
        let result = expand_tilde("~/foo/bar");
        assert!(!result.starts_with('~'), "tilde should have been expanded");
        assert!(result.ends_with("/foo/bar"));
    }

    #[test]
    fn test_expand_tilde_alone() {
        let result = expand_tilde("~");
        assert!(!result.starts_with('~'));
    }

    #[test]
    fn test_expand_tilde_no_tilde() {
        assert_eq!(expand_tilde("/absolute/path"), "/absolute/path");
    }

    #[test]
    fn test_status_label_covers_all_variants() {
        for s in [
            Status::Running,
            Status::Waiting,
            Status::Stopped,
            Status::Idle,
            Status::Error,
        ] {
            let label = status_label(&s);
            assert!(!label.is_empty());
        }
    }
}
