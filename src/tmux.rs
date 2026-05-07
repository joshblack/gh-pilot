use anyhow::{bail, Context, Result};
use std::process::Command;

/// Run a `tmux` subcommand and return its stdout as a string.
fn tmux_output(args: &[&str]) -> Result<String> {
    let out = Command::new("tmux")
        .args(args)
        .output()
        .context("Failed to execute tmux")?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run a `tmux` subcommand and return whether it succeeded.
fn tmux_ok(args: &[&str]) -> bool {
    Command::new("tmux")
        .args(args)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Check that `tmux` is installed and reachable on PATH.
///
/// Returns a helpful error message if not found.
pub fn check_available() -> Result<()> {
    let ok = Command::new("tmux")
        .arg("-V")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if ok {
        Ok(())
    } else {
        bail!(
            "tmux is not installed or not found in PATH.\n\
             gh-mission-control requires tmux to manage agent sessions.\n\
             \n\
             Install tmux:\n\
             • macOS:          brew install tmux\n\
             • Ubuntu/Debian:  sudo apt-get install tmux\n\
             • Fedora/RHEL:    sudo dnf install tmux\n\
             • Arch:           sudo pacman -S tmux"
        )
    }
}

/// Return `true` if a tmux session with the given name exists.
pub fn session_exists(name: &str) -> bool {
    tmux_ok(&["has-session", "-t", name])
}

/// Create a new detached tmux session.
///
/// The session runs `command` (via the user's shell) in the given `cwd`.
pub fn new_session(name: &str, cwd: &str, command: &str) -> Result<()> {
    let status = Command::new("tmux")
        .args(["new-session", "-d", "-s", name, "-c", cwd])
        .arg(command)
        .status()
        .context("Failed to execute tmux new-session")?;

    if !status.success() {
        bail!("tmux new-session failed for session '{}'", name);
    }
    Ok(())
}

/// Attach the current terminal to an existing tmux session.
pub fn attach(name: &str) -> Result<()> {
    let status = Command::new("tmux")
        .args(["attach-session", "-t", name])
        .status()
        .context("Failed to execute tmux attach-session")?;

    if !status.success() {
        bail!("Failed to attach to tmux session '{}'", name);
    }
    Ok(())
}

/// Kill a tmux session by name.
pub fn kill_session(name: &str) -> Result<()> {
    let status = Command::new("tmux")
        .args(["kill-session", "-t", name])
        .status()
        .context("Failed to execute tmux kill-session")?;

    if !status.success() {
        bail!("Failed to kill tmux session '{}'", name);
    }
    Ok(())
}

/// Capture the visible content of the first pane of a tmux session.
pub fn capture_pane(name: &str) -> Result<String> {
    tmux_output(&["capture-pane", "-p", "-t", name])
}

/// Send keys (text) to the first pane of a tmux session, followed by Enter.
pub fn send_keys(name: &str, keys: &str) -> Result<()> {
    let status = Command::new("tmux")
        .args(["send-keys", "-t", name, keys, "Enter"])
        .status()
        .context("Failed to execute tmux send-keys")?;

    if !status.success() {
        bail!("Failed to send keys to tmux session '{}'", name);
    }
    Ok(())
}

/// Return the names of all active tmux sessions.
pub fn list_sessions() -> Vec<String> {
    tmux_output(&["list-sessions", "-F", "#{session_name}"])
        .map(|out| {
            out.lines()
                .filter(|l| !l.is_empty())
                .map(|l| l.to_string())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// session_exists should return false for a name that is almost certainly not running.
    #[test]
    fn test_session_exists_unknown_returns_false() {
        // This name is extremely unlikely to collide with a real tmux session.
        assert!(!session_exists("ghmc_test_nonexistent_xyzzy_99"));
    }

    /// list_sessions returns a Vec (may be empty if no tmux server is running).
    #[test]
    fn test_list_sessions_returns_vec() {
        let sessions = list_sessions();
        // Just check that it is a Vec<String> without panicking.
        for name in &sessions {
            assert!(!name.is_empty(), "session name should be non-empty");
        }
    }
}
