use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Prefix applied to all tmux session names managed by gh-mission-control.
pub const TMUX_PREFIX: &str = "ghmc_";

/// The runtime/display status of a managed session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// The tmux session is running and appears active.
    Running,
    /// The tmux session is paused waiting for user input.
    Waiting,
    /// The tmux session has been stopped or never started.
    Stopped,
    /// The tmux session exists but appears idle at a shell prompt.
    Idle,
    /// The session is in an error state.
    Error,
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Status::Running => write!(f, "running"),
            Status::Waiting => write!(f, "waiting"),
            Status::Stopped => write!(f, "stopped"),
            Status::Idle => write!(f, "idle"),
            Status::Error => write!(f, "error"),
        }
    }
}

/// A managed AI agent session backed by a tmux session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// Stable UUID for the session.
    pub id: String,
    /// Human-readable title.
    pub title: String,
    /// Absolute path to the project/working directory.
    pub project_path: String,
    /// Command to run inside the tmux session (e.g., `claude`, `gemini`).
    pub command: String,
    /// Name of the managed tmux session (always prefixed with `TMUX_PREFIX`).
    pub tmux_session: String,
    /// Current status of the session.
    pub status: Status,
    /// When this session was registered.
    pub created_at: DateTime<Utc>,
    /// When this session was last modified.
    pub updated_at: DateTime<Utc>,
}

impl Session {
    /// Create a new session with a generated ID and tmux session name.
    pub fn new(title: String, project_path: String, command: String) -> Self {
        let id = uuid::Uuid::new_v4().to_string();
        // Use the first 8 hex chars of the UUID for a short tmux name.
        let short = id.replace('-', "");
        let tmux_session = format!("{}{}", TMUX_PREFIX, &short[..8]);
        let now = Utc::now();
        Session {
            id,
            title,
            project_path,
            command,
            tmux_session,
            status: Status::Stopped,
            created_at: now,
            updated_at: now,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_new_sets_defaults() {
        let s = Session::new(
            "Test".to_string(),
            "/tmp/proj".to_string(),
            "claude".to_string(),
        );
        assert_eq!(s.title, "Test");
        assert_eq!(s.project_path, "/tmp/proj");
        assert_eq!(s.command, "claude");
        assert_eq!(s.status, Status::Stopped);
        // UUID must be 36 characters (8-4-4-4-12 with hyphens).
        assert_eq!(s.id.len(), 36);
    }

    #[test]
    fn test_session_tmux_prefix() {
        let s = Session::new("T".to_string(), "/".to_string(), "cmd".to_string());
        assert!(
            s.tmux_session.starts_with(TMUX_PREFIX),
            "tmux_session '{}' must start with '{}'",
            s.tmux_session,
            TMUX_PREFIX
        );
    }

    #[test]
    fn test_session_unique_ids() {
        let a = Session::new("A".to_string(), "/".to_string(), "cmd".to_string());
        let b = Session::new("B".to_string(), "/".to_string(), "cmd".to_string());
        assert_ne!(a.id, b.id);
        assert_ne!(a.tmux_session, b.tmux_session);
    }

    #[test]
    fn test_status_display() {
        assert_eq!(Status::Running.to_string(), "running");
        assert_eq!(Status::Waiting.to_string(), "waiting");
        assert_eq!(Status::Stopped.to_string(), "stopped");
        assert_eq!(Status::Idle.to_string(), "idle");
        assert_eq!(Status::Error.to_string(), "error");
    }

    #[test]
    fn test_session_serialization_roundtrip() {
        let original = Session::new(
            "My Agent".to_string(),
            "/home/user/project".to_string(),
            "claude --dangerously-skip-permissions".to_string(),
        );
        let json = serde_json::to_string(&original).expect("serialize");
        let restored: Session = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(original.id, restored.id);
        assert_eq!(original.title, restored.title);
        assert_eq!(original.status, restored.status);
    }
}
