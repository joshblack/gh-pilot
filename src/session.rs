use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum SessionStatus {
    Active,
    Inactive,
    Paused,
}

impl SessionStatus {
    #[allow(dead_code)]
    pub fn symbol(&self) -> &str {
        match self {
            SessionStatus::Active => "●",
            SessionStatus::Inactive => "○",
            SessionStatus::Paused => "⏸",
        }
    }

    pub fn label(&self) -> &str {
        match self {
            SessionStatus::Active => "Active",
            SessionStatus::Inactive => "Inactive",
            SessionStatus::Paused => "Paused",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub name: String,
    /// Absolute path to the project directory this session belongs to
    pub project_path: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub status: SessionStatus,
    pub description: Option<String>,
    /// Optional PID of the running process
    pub pid: Option<u32>,
}

impl Session {
    pub fn new(name: impl Into<String>, project_path: impl Into<String>) -> Self {
        let now = Utc::now();
        Session {
            id: Uuid::new_v4().to_string(),
            name: name.into(),
            project_path: project_path.into(),
            created_at: now,
            updated_at: now,
            status: SessionStatus::Active,
            description: None,
            pid: None,
        }
    }

    /// Display name for the project folder (last two path components)
    #[allow(dead_code)]
    pub fn folder_label(&self) -> String {
        let p = Path::new(&self.project_path);
        let mut parts: Vec<&str> = p
            .components()
            .filter_map(|c| c.as_os_str().to_str())
            .collect();
        // Show at most the last 2 segments of the path
        let n = parts.len();
        if n > 2 {
            parts = parts[n - 2..].to_vec();
            format!("…/{}/{}", parts[0], parts[1])
        } else {
            parts.join("/")
        }
    }

    /// Check if the process is still alive (if pid is set)
    pub fn is_process_alive(&self) -> bool {
        if let Some(pid) = self.pid {
            // Send signal 0 to check process existence
            let result = libc_kill(pid as i32, 0);
            result == 0
        } else {
            false
        }
    }

    pub fn log_path(&self, sessions_dir: &Path) -> PathBuf {
        sessions_dir.join(&self.id).with_extension("log")
    }

    pub fn meta_path(&self, sessions_dir: &Path) -> PathBuf {
        sessions_dir.join(&self.id).with_extension("json")
    }

    pub fn save(&self, sessions_dir: &Path) -> Result<()> {
        fs::create_dir_all(sessions_dir)
            .context("Failed to create sessions directory")?;
        let path = self.meta_path(sessions_dir);
        let json = serde_json::to_string_pretty(self).context("Failed to serialize session")?;
        fs::write(&path, json).context("Failed to write session file")?;
        Ok(())
    }

    pub fn append_log(&self, sessions_dir: &Path, line: &str) -> Result<()> {
        use std::io::Write;
        let path = self.log_path(sessions_dir);
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .context("Failed to open log file")?;
        writeln!(file, "{}", line).context("Failed to write log line")?;
        Ok(())
    }

    pub fn read_log(&self, sessions_dir: &Path) -> String {
        let path = self.log_path(sessions_dir);
        fs::read_to_string(&path).unwrap_or_default()
    }

    pub fn delete(&self, sessions_dir: &Path) -> Result<()> {
        let meta = self.meta_path(sessions_dir);
        let log = self.log_path(sessions_dir);
        if meta.exists() {
            fs::remove_file(&meta).context("Failed to remove session file")?;
        }
        if log.exists() {
            let _ = fs::remove_file(&log);
        }
        Ok(())
    }
}

/// Load all sessions from the sessions directory, sorted newest-first.
pub fn load_sessions(sessions_dir: &Path) -> Vec<Session> {
    let mut sessions = Vec::new();

    let entries = match fs::read_dir(sessions_dir) {
        Ok(e) => e,
        Err(_) => return sessions,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("json") {
            if let Ok(content) = fs::read_to_string(&path) {
                if let Ok(mut session) = serde_json::from_str::<Session>(&content) {
                    // Auto-update status based on process liveness
                    if session.status == SessionStatus::Active && !session.is_process_alive() {
                        // Only auto-inactivate if we had a pid set
                        if session.pid.is_some() {
                            session.status = SessionStatus::Inactive;
                        }
                    }
                    sessions.push(session);
                }
            }
        }
    }

    // Sort newest first
    sessions.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    sessions
}

/// Group sessions by project path, preserving per-group sort order (newest first).
pub fn group_sessions(sessions: &[Session]) -> Vec<(String, Vec<usize>)> {
    // We build an ordered list of groups (preserving first-seen order of project paths)
    // Each group entry: (project_path, [indices into sessions])
    let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
    let mut group_index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

    for (i, session) in sessions.iter().enumerate() {
        let key = session.project_path.clone();
        if let Some(&gi) = group_index.get(&key) {
            groups[gi].1.push(i);
        } else {
            let gi = groups.len();
            group_index.insert(key.clone(), gi);
            groups.push((key, vec![i]));
        }
    }

    groups
}

/// Seed some demo sessions so the tool is immediately useful.
pub fn seed_demo_sessions(sessions_dir: &Path) -> Result<()> {
    use std::time::Duration;

    let demos = vec![
        (
            "feature/auth-refactor",
            "~/projects/webapp",
            SessionStatus::Active,
            Some("Refactoring authentication layer with JWT tokens"),
        ),
        (
            "fix/memory-leak",
            "~/projects/webapp",
            SessionStatus::Inactive,
            Some("Investigating memory leak in websocket handler"),
        ),
        (
            "chore/ci-improvements",
            "~/projects/cli-tool",
            SessionStatus::Paused,
            Some("Updating CI pipeline to use caching"),
        ),
        (
            "feature/new-commands",
            "~/projects/cli-tool",
            SessionStatus::Active,
            Some("Adding new subcommands for batch processing"),
        ),
        (
            "docs/api-reference",
            "~/projects/api-service",
            SessionStatus::Inactive,
            Some("Writing OpenAPI 3.0 documentation"),
        ),
    ];

    // Only seed if there are no existing sessions
    if sessions_dir.exists() {
        let count = fs::read_dir(sessions_dir)
            .map(|entries| entries.filter(|e| e.is_ok()).count())
            .unwrap_or(0);
        if count > 0 {
            return Ok(());
        }
    }

    let base_time = Utc::now();
    for (i, (name, path, status, desc)) in demos.iter().enumerate() {
        let offset = Duration::from_secs(i as u64 * 3600); // 1 hour apart
        let created_at = base_time - chrono::Duration::from_std(offset).unwrap_or_default();
        let mut session = Session::new(*name, *path);
        session.created_at = created_at;
        session.updated_at = created_at;
        session.status = status.clone();
        session.description = desc.map(|s| s.to_string());
        session.save(sessions_dir)?;

        // Write some demo log lines
        let log_lines = vec![
            format!("[{}] Session started", created_at.format("%Y-%m-%d %H:%M:%S")),
            format!("[{}] Working on: {}", created_at.format("%Y-%m-%d %H:%M:%S"), name),
            format!("[{}] {}", created_at.format("%Y-%m-%d %H:%M:%S"), desc.unwrap_or("")),
        ];
        for line in &log_lines {
            session.append_log(sessions_dir, line)?;
        }
    }

    Ok(())
}

/// libc kill(2) for checking if a process is alive
#[cfg(unix)]
fn libc_kill(pid: i32, sig: i32) -> i32 {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    unsafe { kill(pid, sig) }
}

#[cfg(not(unix))]
fn libc_kill(_pid: i32, _sig: i32) -> i32 {
    -1
}
