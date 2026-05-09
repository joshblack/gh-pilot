use crate::terminal::{tmux_session_name, TMUX_SESSION_PREFIX};
use chrono::{DateTime, Duration, Utc};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

// ── Status ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum SessionStatus {
    Running,
    Waiting,
    Idle,
    Error,
}

impl SessionStatus {
    pub fn label(&self) -> &str {
        match self {
            SessionStatus::Running => "Running",
            SessionStatus::Waiting => "Waiting",
            SessionStatus::Idle => "Idle",
            SessionStatus::Error => "Error",
        }
    }
}

// ── Session model ────────────────────────────────────────────────────────────

/// A Copilot CLI session read from `~/.copilot/session-state/<id>/workspace.yaml`.
#[derive(Debug, Clone)]
pub struct CopilotSession {
    pub id: String,
    /// Working directory where the session was started
    pub cwd: PathBuf,
    /// Git root (may differ from cwd)
    #[allow(dead_code)]
    pub git_root: Option<PathBuf>,
    /// GitHub repository (e.g., "owner/repo")
    #[allow(dead_code)]
    pub repository: Option<String>,
    /// Current git branch
    pub branch: Option<String>,
    /// Auto-generated or user-provided summary / name
    pub summary: Option<String>,
    /// Last assistant response, used as a one-line session description
    pub last_agent_message: Option<String>,
    /// Whether the user explicitly named this session
    #[allow(dead_code)]
    pub user_named: bool,
    #[allow(dead_code)]
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub status: SessionStatus,
}

impl CopilotSession {
    /// Display name for this session.
    /// Priority: workspace title/summary → branch → last cwd component → id prefix.
    pub fn display_name(&self) -> String {
        if let Some(ref s) = self.summary {
            let first = s.lines().next().unwrap_or("").trim();
            if !first.is_empty() {
                return first.to_string();
            }
        }
        if let Some(ref b) = self.branch {
            if !b.is_empty() {
                return b.clone();
            }
        }
        self.cwd
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&self.id[..8])
            .to_string()
    }
}

// ── Conversation turns ───────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Turn {
    pub turn_index: i64,
    pub user_message: Option<String>,
    pub assistant_response: Option<String>,
    #[allow(dead_code)]
    pub timestamp: String,
}

// ── Loading ──────────────────────────────────────────────────────────────────

/// Default path to the copilot configuration directory.
pub fn copilot_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".copilot")
}

/// Path to the session-state directory inside the copilot config dir.
pub fn session_state_dir(copilot_dir: &Path) -> PathBuf {
    copilot_dir.join("session-state")
}

/// Path to the SQLite session store.
pub fn session_db_path(copilot_dir: &Path) -> PathBuf {
    copilot_dir.join("session-store.db")
}

/// Load all copilot sessions from `~/.copilot/session-state/`, sorted newest-first.
pub fn load_sessions(copilot_dir: &Path) -> Vec<CopilotSession> {
    let state_dir = session_state_dir(copilot_dir);
    let db_path = session_db_path(copilot_dir);
    // Keep the session list focused on recently active workspaces.
    let oldest_active = Utc::now() - Duration::days(7);
    let active_tmux_sessions = active_tmux_session_names();

    let mut sessions = Vec::new();

    let entries = match fs::read_dir(&state_dir) {
        Ok(e) => e,
        Err(_) => return sessions,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let workspace = path.join("workspace.yaml");
        if !workspace.exists() {
            continue;
        }
        if let Ok(content) = fs::read_to_string(&workspace) {
            if let Some(mut session) = parse_workspace_yaml(&content) {
                if session.updated_at < oldest_active {
                    continue;
                }
                // Try to enrich with summary from SQLite when workspace.yaml has no title/summary.
                if session.summary.is_none() {
                    session.summary = load_summary_from_db(&db_path, &session.id);
                }
                session.last_agent_message = load_last_agent_message_from_db(&db_path, &session.id);
                session.status = detect_session_status(
                    copilot_dir,
                    &db_path,
                    &session.id,
                    &active_tmux_sessions,
                );
                sessions.push(session);
            }
        }
    }

    // Sort newest-first by updated_at
    sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    sessions
}

/// Refresh status for already-loaded sessions using active gh-mission-control tmux sessions.
pub fn refresh_session_statuses(copilot_dir: &Path, sessions: &mut [CopilotSession]) {
    let db_path = session_db_path(copilot_dir);
    let active_tmux_sessions = active_tmux_session_names();

    for session in sessions {
        session.status =
            detect_session_status(copilot_dir, &db_path, &session.id, &active_tmux_sessions);
    }
}

/// Load conversation turns for a session from the SQLite database.
pub fn load_turns(db_path: &Path, session_id: &str) -> Vec<Turn> {
    let conn = match rusqlite::Connection::open(db_path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let mut stmt = match conn.prepare(
        "SELECT turn_index, user_message, assistant_response, timestamp \
         FROM turns WHERE session_id = ? ORDER BY turn_index ASC",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    stmt.query_map([session_id], |row| {
        Ok(Turn {
            turn_index: row.get(0)?,
            user_message: row.get(1)?,
            assistant_response: row.get(2)?,
            timestamp: row.get::<_, String>(3).unwrap_or_default(),
        })
    })
    .map(|rows| rows.flatten().collect())
    .unwrap_or_default()
}

// ── Copilot binary ────────────────────────────────────────────────────────────

/// Find the copilot binary: prefers `~/.local/share/gh/copilot/copilot`, then PATH.
pub fn copilot_binary() -> Option<PathBuf> {
    // Check the standard gh-managed location first
    if let Some(home) = dirs::home_dir() {
        let candidate = home
            .join(".local")
            .join("share")
            .join("gh")
            .join("copilot")
            .join("copilot");
        if candidate.exists() {
            return Some(candidate);
        }
    }
    // Fall back to PATH
    which_in_path("copilot")
}

fn which_in_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var("PATH").unwrap_or_default();
    let separator = if cfg!(windows) { ';' } else { ':' };
    for dir in path_var.split(separator) {
        let candidate = PathBuf::from(dir).join(name);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Parse a flat `key: value` workspace.yaml file.
fn parse_workspace_yaml(content: &str) -> Option<CopilotSession> {
    use std::collections::HashMap;
    let mut map: HashMap<&str, &str> = HashMap::new();

    for line in content.lines() {
        if let Some((k, v)) = line.split_once(':') {
            map.insert(k.trim(), v.trim());
        }
    }

    let id = map.get("id")?.to_string();
    let cwd = map
        .get("cwd")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"));
    let git_root = map.get("git_root").map(PathBuf::from);
    let repository = map.get("repository").map(|s| s.to_string());
    let branch = map.get("branch").map(|s| s.to_string());
    let user_named = matches!(map.get("user_named"), Some(&"true"));
    let created_at = map
        .get("created_at")
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(Utc::now);
    let updated_at = map
        .get("updated_at")
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or(created_at);

    // Use the title/summary fields present in workspace.yaml before DB summaries.
    let summary = map
        .get("title")
        .or_else(|| map.get("summary"))
        .map(|s| s.to_string());

    Some(CopilotSession {
        id,
        cwd,
        git_root,
        repository,
        branch,
        summary,
        last_agent_message: None,
        user_named,
        created_at,
        updated_at,
        status: SessionStatus::Idle, // will be updated by caller
    })
}

/// Try to load the auto-generated summary for a session from the SQLite DB.
fn load_summary_from_db(db_path: &Path, session_id: &str) -> Option<String> {
    let conn = rusqlite::Connection::open(db_path).ok()?;
    conn.query_row(
        "SELECT summary FROM sessions WHERE id = ?",
        [session_id],
        |row| row.get::<_, Option<String>>(0),
    )
    .ok()
    .flatten()
    .filter(|s| !s.is_empty())
}

fn load_last_agent_message_from_db(db_path: &Path, session_id: &str) -> Option<String> {
    let conn = rusqlite::Connection::open(db_path).ok()?;
    conn.query_row(
        "SELECT assistant_response FROM turns \
         WHERE session_id = ? AND assistant_response IS NOT NULL AND assistant_response != '' \
         ORDER BY turn_index DESC LIMIT 1",
        [session_id],
        |row| row.get::<_, Option<String>>(0),
    )
    .ok()
    .flatten()
    .filter(|s| !s.trim().is_empty())
}

fn detect_session_status(
    copilot_dir: &Path,
    db_path: &Path,
    session_id: &str,
    active_tmux_sessions: &HashSet<String>,
) -> SessionStatus {
    if !active_tmux_sessions.contains(&tmux_session_name(session_id)) {
        return SessionStatus::Idle;
    }

    // The event stream records turn starts, permission prompts, and turn ends as
    // they happen, so it reflects live agent state before the summary DB catches up.
    if let Some(status) =
        detect_session_status_from_events(&event_log_path(copilot_dir, session_id))
    {
        return status;
    }

    match load_latest_turn_from_db(db_path, session_id) {
        Some((_, Some(response))) if response_indicates_error(&response) => SessionStatus::Error,
        Some((Some(user_message), assistant_response))
            if is_awaiting_response(&user_message, &assistant_response) =>
        {
            SessionStatus::Running
        }
        _ => SessionStatus::Waiting,
    }
}

fn event_log_path(copilot_dir: &Path, session_id: &str) -> PathBuf {
    session_state_dir(copilot_dir)
        .join(session_id)
        .join("events.jsonl")
}

fn detect_session_status_from_events(path: &Path) -> Option<SessionStatus> {
    fs::read_to_string(path)
        .ok()
        .and_then(|content| detect_session_status_from_event_content(&content))
}

fn detect_session_status_from_event_content(content: &str) -> Option<SessionStatus> {
    let mut saw_event = false;
    let mut active_turn: Option<String> = None;
    let mut pending_permissions = HashSet::new();
    let mut saw_error = false;

    for line in content.lines() {
        let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(event_type) = event.get("type").and_then(|value| value.as_str()) else {
            continue;
        };
        saw_event = true;

        let data = event.get("data");
        match event_type {
            "assistant.turn_start" => {
                active_turn = data
                    .and_then(|data| data.get("turnId"))
                    .and_then(|value| value.as_str())
                    .map(ToString::to_string);
                pending_permissions.clear();
                saw_error = false;
            }
            "assistant.turn_end" => {
                if active_turn.as_deref()
                    == data
                        .and_then(|data| data.get("turnId"))
                        .and_then(|value| value.as_str())
                {
                    active_turn = None;
                    pending_permissions.clear();
                }
            }
            "permission.requested" => {
                if let Some(request_id) = data
                    .and_then(|data| data.get("requestId"))
                    .and_then(|value| value.as_str())
                {
                    pending_permissions.insert(request_id.to_string());
                }
            }
            "permission.completed" => {
                if let Some(request_id) = data
                    .and_then(|data| data.get("requestId"))
                    .and_then(|value| value.as_str())
                {
                    pending_permissions.remove(request_id);
                }
            }
            "tool.execution_complete" => {
                if data
                    .and_then(|data| data.get("success"))
                    .and_then(|value| value.as_bool())
                    .is_some_and(|success| !success)
                {
                    saw_error = true;
                }
            }
            _ if event_type.ends_with(".error") => {
                saw_error = true;
            }
            _ => {}
        }
    }

    if !saw_event {
        return None;
    }
    if saw_error {
        return Some(SessionStatus::Error);
    }
    if !pending_permissions.is_empty() {
        return Some(SessionStatus::Waiting);
    }
    if active_turn.is_some() {
        return Some(SessionStatus::Running);
    }
    Some(SessionStatus::Waiting)
}

fn load_latest_turn_from_db(
    db_path: &Path,
    session_id: &str,
) -> Option<(Option<String>, Option<String>)> {
    let conn = rusqlite::Connection::open(db_path).ok()?;
    conn.query_row(
        "SELECT user_message, assistant_response \
         FROM turns WHERE session_id = ? ORDER BY turn_index DESC LIMIT 1",
        [session_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .ok()
}

fn is_awaiting_response(user_message: &str, assistant_response: &Option<String>) -> bool {
    !user_message.trim().is_empty()
        && assistant_response
            .as_deref()
            .map(str::trim)
            .unwrap_or_default()
            .is_empty()
}

fn response_indicates_error(response: &str) -> bool {
    let response = response.to_ascii_lowercase();
    [
        "something went wrong",
        "encountered an error",
        "fatal error:",
        "panic:",
    ]
    .iter()
    .any(|needle| response.contains(needle))
}

fn active_tmux_session_names() -> HashSet<String> {
    let output = Command::new("tmux")
        .arg("list-sessions")
        .arg("-F")
        .arg("#{session_name}")
        .stderr(Stdio::null())
        .output();

    let Ok(output) = output else {
        return HashSet::new();
    };
    if !output.status.success() {
        return HashSet::new();
    }

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|name| name.starts_with(TMUX_SESSION_PREFIX))
        .map(ToString::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn event_status_reports_running_for_active_turn() {
        let content = r#"{"type":"assistant.turn_start","data":{"turnId":"1"}}"#;

        assert_eq!(
            detect_session_status_from_event_content(content),
            Some(SessionStatus::Running)
        );
    }

    #[test]
    fn event_status_reports_waiting_for_pending_permission() {
        let content = r#"{"type":"assistant.turn_start","data":{"turnId":"1"}}
{"type":"permission.requested","data":{"requestId":"approve-1"}}"#;

        assert_eq!(
            detect_session_status_from_event_content(content),
            Some(SessionStatus::Waiting)
        );
    }

    #[test]
    fn event_status_reports_running_after_permission_completes() {
        let content = r#"{"type":"assistant.turn_start","data":{"turnId":"1"}}
{"type":"permission.requested","data":{"requestId":"approve-1"}}
{"type":"permission.completed","data":{"requestId":"approve-1"}}"#;

        assert_eq!(
            detect_session_status_from_event_content(content),
            Some(SessionStatus::Running)
        );
    }

    #[test]
    fn event_status_reports_waiting_after_turn_end() {
        let content = r#"{"type":"assistant.turn_start","data":{"turnId":"1"}}
{"type":"assistant.turn_end","data":{"turnId":"1"}}"#;

        assert_eq!(
            detect_session_status_from_event_content(content),
            Some(SessionStatus::Waiting)
        );
    }

    #[test]
    fn load_sessions_prefers_workspace_title_over_db_summary() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("ghmc-session-test-{unique}"));
        let session_id = "12345678-1234-1234-1234-123456789abc";
        let session_dir = root.join("session-state").join(session_id);
        fs::create_dir_all(&session_dir).unwrap();

        let timestamp = Utc::now().to_rfc3339();
        fs::write(
            session_dir.join("workspace.yaml"),
            format!(
                "id: {session_id}\n\
                 cwd: /tmp/example\n\
                 branch: feature\n\
                 title: Workspace Title\n\
                 summary: Workspace Summary\n\
                 created_at: {timestamp}\n\
                 updated_at: {timestamp}\n"
            ),
        )
        .unwrap();

        let conn = rusqlite::Connection::open(root.join("session-store.db")).unwrap();
        conn.execute(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, summary TEXT)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (id, summary) VALUES (?1, ?2)",
            (session_id, "Database Summary"),
        )
        .unwrap();

        let sessions = load_sessions(&root);

        fs::remove_dir_all(&root).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].display_name(), "Workspace Title");
    }
}
