use chrono::{DateTime, Utc};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Keep generated tmux session names aligned with the terminal module's limit.
const TMUX_SESSION_NAME_MAX_LEN: usize = 80;

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
    pub repository: Option<String>,
    /// Current git branch
    pub branch: Option<String>,
    /// Auto-generated or user-provided summary / name
    pub summary: Option<String>,
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
    /// Priority: user summary → branch → last cwd component → id prefix.
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

    /// Key used for grouping (the cwd path, shortened for display).
    pub fn group_key(&self) -> String {
        self.cwd.to_string_lossy().to_string()
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
                // Try to enrich with summary from SQLite
                session.summary = load_summary_from_db(&db_path, &session.id).or(session.summary);
                session.status = detect_session_status(&db_path, &session.id);
                sessions.push(session);
            }
        }
    }

    // Sort newest-first by updated_at
    sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    sessions
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

/// Group sessions by their `cwd`, preserving newest-first order within each group.
/// Returns `(group_key, [indices into sessions])` pairs.
pub fn group_sessions(sessions: &[CopilotSession]) -> Vec<(String, Vec<usize>)> {
    let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
    let mut group_index: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();

    for (i, session) in sessions.iter().enumerate() {
        let key = session.group_key();
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

    // summary_count can guide us but the actual summary comes from the DB
    let summary = None;

    Some(CopilotSession {
        id,
        cwd,
        git_root,
        repository,
        branch,
        summary,
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

fn detect_session_status(db_path: &Path, session_id: &str) -> SessionStatus {
    if !is_session_active(session_id) {
        return SessionStatus::Idle;
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

/// Check if a copilot session is currently active by scanning running processes.
/// On Linux this reads /proc/*/cmdline; returns false on other platforms.
fn is_session_active(session_id: &str) -> bool {
    if tmux_has_session(session_id) {
        return true;
    }

    #[cfg(target_os = "linux")]
    {
        let proc = match fs::read_dir("/proc") {
            Ok(e) => e,
            Err(_) => return false,
        };
        for entry in proc.flatten() {
            let cmdline_path = entry.path().join("cmdline");
            if let Ok(bytes) = fs::read(&cmdline_path) {
                // cmdline args are NUL-separated
                let cmdline = String::from_utf8_lossy(&bytes);
                if cmdline.contains(session_id) {
                    return true;
                }
            }
        }
        false
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = session_id;
        false
    }
}

fn tmux_has_session(session_id: &str) -> bool {
    Command::new("tmux")
        .arg("has-session")
        .arg("-t")
        .arg(tmux_session_name(session_id))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn tmux_session_name(session_id: &str) -> String {
    let sanitized: String = session_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(TMUX_SESSION_NAME_MAX_LEN)
        .collect();
    format!("ghmc_{sanitized}")
}
