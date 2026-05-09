use crate::terminal::{tmux_session_name, TMUX_SESSION_PREFIX};
use chrono::{DateTime, Duration, Utc};
use std::collections::HashSet;
use std::fs;
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const REMOTE_AGENT_GROUP: &str = "Remote agent tasks";
const REMOTE_LOG_PREVIEW_BYTES: usize = 128 * 1024;
const REMOTE_LOG_PREVIEW_LINES: usize = 200;

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

#[derive(Debug, Clone, PartialEq)]
pub enum SessionSource {
    Local,
    Remote,
}

// ── Session model ────────────────────────────────────────────────────────────

/// A Copilot CLI session read from `~/.copilot/session-state/<id>/workspace.yaml`.
#[derive(Debug, Clone)]
pub struct CopilotSession {
    pub id: String,
    pub source: SessionSource,
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
    pub remote_state: Option<String>,
    pub remote_url: Option<String>,
    pub remote_user: Option<String>,
    pub pull_request: Option<String>,
    pub remote_log: Option<String>,
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
    sessions.extend(load_remote_agent_tasks());
    sessions.sort_by_key(|session| std::cmp::Reverse(session.updated_at));
    sessions
}

/// Refresh status for already-loaded sessions using active gh-pilot tmux sessions.
pub fn refresh_session_statuses(copilot_dir: &Path, sessions: &mut [CopilotSession]) {
    let db_path = session_db_path(copilot_dir);
    let active_tmux_sessions = active_tmux_session_names();

    for session in sessions {
        if session.source == SessionSource::Remote {
            continue;
        }
        session.status =
            detect_session_status(copilot_dir, &db_path, &session.id, &active_tmux_sessions);
    }
}

fn load_remote_agent_tasks() -> Vec<CopilotSession> {
    let fields = [
        "completedAt",
        "createdAt",
        "id",
        "name",
        "pullRequestNumber",
        "pullRequestState",
        "pullRequestTitle",
        "pullRequestUrl",
        "repository",
        "state",
        "updatedAt",
        "user",
    ]
    .join(",");
    let output = Command::new("gh")
        .args([
            "agent-task",
            "list",
            "--json",
            fields.as_str(),
            "--limit",
            "100",
        ])
        .stderr(Stdio::null())
        .output();

    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }

    let tasks = serde_json::from_slice::<Vec<serde_json::Value>>(&output.stdout)
        .ok()
        .unwrap_or_default();

    tasks
        .into_iter()
        .filter_map(remote_task_from_json)
        .collect()
}

fn remote_task_from_json(task: serde_json::Value) -> Option<CopilotSession> {
    let id = value_text(task.get("id"))?;
    let repository = value_text(task.get("repository"));
    let name = value_text(task.get("name"));
    let state = value_text(task.get("state"));
    let created_at = value_text(task.get("createdAt"))
        .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(Utc::now);
    let updated_at = value_text(task.get("updatedAt"))
        .or_else(|| value_text(task.get("completedAt")))
        .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or(created_at);
    let pull_request = format_pull_request(
        task.get("pullRequestNumber"),
        task.get("pullRequestTitle"),
        task.get("pullRequestState"),
    );
    let remote_description = pull_request.clone().or_else(|| {
        state
            .clone()
            .map(|state| format!("Remote agent task ({state})"))
    });

    let pull_request_number = value_text(task.get("pullRequestNumber")).or_else(|| {
        value_text(task.get("pullRequestUrl")).and_then(|url| pull_request_number_from_url(&url))
    });
    let remote_url = remote_task_url(repository.as_deref(), pull_request_number.as_deref(), &id);

    Some(CopilotSession {
        id,
        source: SessionSource::Remote,
        cwd: repository
            .as_deref()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(REMOTE_AGENT_GROUP)),
        git_root: None,
        repository,
        branch: None,
        summary: name,
        last_agent_message: remote_description,
        user_named: false,
        created_at,
        updated_at,
        status: remote_status(state.as_deref()),
        remote_state: state,
        remote_url,
        remote_user: value_text(task.get("user")),
        pull_request,
        remote_log: None,
    })
}

fn remote_task_url(
    repository: Option<&str>,
    pull_request_number: Option<&str>,
    id: &str,
) -> Option<String> {
    let repository = repository?;
    let pull_request_number = pull_request_number?;
    let (owner, repo) = repository.split_once('/')?;
    if owner.is_empty() || repo.is_empty() || pull_request_number.is_empty() || id.is_empty() {
        return None;
    }
    Some(format!(
        "https://github.com/{owner}/{repo}/pull/{pull_request_number}/agent-sessions/{id}"
    ))
}

fn pull_request_number_from_url(url: &str) -> Option<String> {
    let number = url.split("/pull/").nth(1)?.split('/').next()?;
    if number.is_empty() {
        return None;
    }
    Some(number.to_string())
}

pub fn load_remote_task_log(session_id: &str) -> String {
    let mut child = match Command::new("gh")
        .args(["agent-task", "view", session_id, "--log"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return "Failed to run `gh agent-task view --log`.".to_string(),
    };

    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return "Failed to read remote task log.".to_string();
    };
    let stderr_handle = child.stderr.take().map(|mut stderr| {
        std::thread::spawn(move || {
            let mut output = String::new();
            let _ = stderr.read_to_string(&mut output);
            output
        })
    });

    let (output, truncated) = match read_remote_log_preview(stdout) {
        Ok(result) => result,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = collect_remote_log_stderr(stderr_handle);
            return "Failed to read remote task log.".to_string();
        }
    };

    if truncated {
        let _ = child.kill();
        let _ = child.wait();
        let _ = collect_remote_log_stderr(stderr_handle);
    } else {
        let status = match child.wait() {
            Ok(status) => status,
            Err(_) => {
                let _ = collect_remote_log_stderr(stderr_handle);
                return "Failed to load remote task log.".to_string();
            }
        };
        if !status.success() {
            let stderr = sanitize_remote_log(&collect_remote_log_stderr(stderr_handle));
            let stderr = stderr.trim();
            if stderr.is_empty() {
                return format!("Failed to load remote task log: {status}");
            }
            return format!("Failed to load remote task log: {stderr}");
        }
        let _ = collect_remote_log_stderr(stderr_handle);
    }

    let mut log = sanitize_remote_log(&String::from_utf8_lossy(&output))
        .trim()
        .to_string();
    if truncated {
        if !log.is_empty() {
            log.push('\n');
        }
        log.push_str("… (log preview truncated)");
    }
    log
}

fn read_remote_log_preview(stdout: impl Read) -> io::Result<(Vec<u8>, bool)> {
    let mut reader = BufReader::new(stdout);
    let mut output = Vec::new();
    let mut bytes = 0;
    let mut lines = 0;

    loop {
        if bytes >= REMOTE_LOG_PREVIEW_BYTES || lines >= REMOTE_LOG_PREVIEW_LINES {
            return Ok((output, true));
        }

        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            return Ok((output, false));
        }

        let mut consumed = 0;
        for &byte in buffer {
            if bytes >= REMOTE_LOG_PREVIEW_BYTES || lines >= REMOTE_LOG_PREVIEW_LINES {
                break;
            }
            output.push(byte);
            bytes += 1;
            consumed += 1;
            if byte == b'\n' {
                lines += 1;
            }
        }

        reader.consume(consumed);
    }
}

fn collect_remote_log_stderr(handle: Option<std::thread::JoinHandle<String>>) -> String {
    handle
        .and_then(|handle| handle.join().ok())
        .unwrap_or_default()
}

fn sanitize_remote_log(log: &str) -> String {
    log.chars()
        .filter(|ch| ch == &'\n' || ch == &'\t' || !ch.is_control())
        .collect()
}

fn value_text(value: Option<&serde_json::Value>) -> Option<String> {
    match value? {
        serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Object(map) => ["login", "name", "nameWithOwner", "url"]
            .into_iter()
            .find_map(|key| map.get(key).and_then(|value| value_text(Some(value)))),
        _ => None,
    }
}

fn format_pull_request(
    number: Option<&serde_json::Value>,
    title: Option<&serde_json::Value>,
    state: Option<&serde_json::Value>,
) -> Option<String> {
    let number = value_text(number)?;
    let title = value_text(title);
    let state = value_text(state);
    let mut label = format!("#{number}");
    if let Some(title) = title {
        label.push_str(&format!(" {title}"));
    }
    if let Some(state) = state {
        label.push_str(&format!(" ({state})"));
    }
    Some(label)
}

fn remote_status(state: Option<&str>) -> SessionStatus {
    match state.unwrap_or_default().to_ascii_lowercase().as_str() {
        "queued" | "pending" | "waiting" => SessionStatus::Waiting,
        "in_progress" | "in-progress" | "running" | "started" => SessionStatus::Running,
        "failed" | "failure" | "error" | "errored" | "cancelled" | "canceled" | "timed_out" => {
            SessionStatus::Error
        }
        _ => SessionStatus::Idle,
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
        source: SessionSource::Local,
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
        remote_state: None,
        remote_url: None,
        remote_user: None,
        pull_request: None,
        remote_log: None,
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
            "assistant.turn_end"
                if active_turn.as_deref()
                    == data
                        .and_then(|data| data.get("turnId"))
                        .and_then(|value| value.as_str()) =>
            {
                active_turn = None;
                pending_permissions.clear();
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
            "tool.execution_complete"
                if data
                    .and_then(|data| data.get("success"))
                    .and_then(|value| value.as_bool())
                    .is_some_and(|success| !success) =>
            {
                saw_error = true;
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
    fn remote_task_from_json_builds_remote_session() {
        let task = serde_json::json!({
            "id": "task-1",
            "name": "Fix remote bug",
            "repository": { "nameWithOwner": "owner/repo" },
            "state": "in_progress",
            "createdAt": "2026-05-08T10:00:00Z",
            "updatedAt": "2026-05-08T11:00:00Z",
            "pullRequestNumber": 42,
            "pullRequestTitle": "Fix remote bug",
            "pullRequestState": "OPEN",
            "pullRequestUrl": "https://github.com/owner/repo/pull/42",
            "user": { "login": "octocat" }
        });

        let session = remote_task_from_json(task).expect("remote task should parse");

        assert_eq!(session.source, SessionSource::Remote);
        assert_eq!(session.status, SessionStatus::Running);
        assert_eq!(session.repository.as_deref(), Some("owner/repo"));
        assert_eq!(session.display_name(), "Fix remote bug");
        assert_eq!(session.remote_user.as_deref(), Some("octocat"));
        assert_eq!(
            session.remote_url.as_deref(),
            Some("https://github.com/owner/repo/pull/42/agent-sessions/task-1")
        );
        assert_eq!(
            session.pull_request.as_deref(),
            Some("#42 Fix remote bug (OPEN)")
        );
    }

    #[test]
    fn remote_task_url_requires_owner_repo_and_pull_request() {
        assert_eq!(
            remote_task_url(Some("owner/repo"), Some("42"), "task-1").as_deref(),
            Some("https://github.com/owner/repo/pull/42/agent-sessions/task-1")
        );
        assert_eq!(remote_task_url(None, Some("42"), "task-1"), None);
        assert_eq!(remote_task_url(Some("repo"), Some("42"), "task-1"), None);
        assert_eq!(remote_task_url(Some("owner/"), Some("42"), "task-1"), None);
        assert_eq!(remote_task_url(Some("owner/repo"), None, "task-1"), None);
        assert_eq!(
            remote_task_url(Some("owner/repo"), Some(""), "task-1"),
            None
        );
        assert_eq!(remote_task_url(Some("owner/repo"), Some("42"), ""), None);
    }

    #[test]
    fn remote_task_url_uses_pull_request_url_when_number_missing() {
        let task = serde_json::json!({
            "id": "task-1",
            "repository": "owner/repo",
            "pullRequestUrl": "https://github.com/owner/repo/pull/42",
        });

        let session = remote_task_from_json(task).expect("remote task should parse");

        assert_eq!(
            session.remote_url.as_deref(),
            Some("https://github.com/owner/repo/pull/42/agent-sessions/task-1")
        );
    }

    #[test]
    fn remote_log_preview_limits_lines() {
        let log = (0..REMOTE_LOG_PREVIEW_LINES + 10)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();

        let (preview, truncated) = read_remote_log_preview(log.as_bytes()).unwrap();

        assert!(truncated);
        assert_eq!(
            String::from_utf8(preview).unwrap().lines().count(),
            REMOTE_LOG_PREVIEW_LINES
        );
    }

    #[test]
    fn remote_log_preview_limits_bytes() {
        let log = vec![b'a'; REMOTE_LOG_PREVIEW_BYTES + 10];

        let (preview, truncated) = read_remote_log_preview(log.as_slice()).unwrap();

        assert!(truncated);
        assert_eq!(preview.len(), REMOTE_LOG_PREVIEW_BYTES);
    }

    #[test]
    fn sanitize_remote_log_removes_control_characters() {
        let sanitized = sanitize_remote_log("start\u{1b}[31m red\u{0} text\nnext\tline");

        assert_eq!(sanitized, "start[31m red text\nnext\tline");
        assert!(!sanitized.contains('\u{1b}'));
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
        let root = std::env::temp_dir().join(format!("gh-pilot-session-test-{unique}"));
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

        let display_name = load_sessions(&root)
            .into_iter()
            .find(|session| session.id == session_id)
            .expect("local session should be loaded")
            .display_name();

        fs::remove_dir_all(&root).unwrap();
        assert_eq!(display_name, "Workspace Title");
    }
}
