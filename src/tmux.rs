use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};

use crate::status::{self, SessionStatus};

pub const SESSION_PREFIX: &str = "gh-pilot__";

const PROJECT_OPTION: &str = "@gh-pilot-project";
const CREATED_OPTION: &str = "@gh-pilot-created";
const COMMAND_OPTION: &str = "@gh-pilot-command";
const LAST_SEEN_OPTION: &str = "@gh-pilot-last-seen";

#[derive(Debug, Clone)]
pub struct ManagedSession {
    pub name: String,
    pub display_name: String,
    pub project_dir: PathBuf,
    pub is_current_project: bool,
    pub status: SessionStatus,
    pub last_activity: Option<SystemTime>,
    pub has_bell: bool,
    pub pane_dead: bool,
}

#[derive(Debug, Default)]
pub struct StatusCache {
    content_hashes: HashMap<String, u64>,
}

impl StatusCache {
    pub fn remove(&mut self, name: &str) {
        self.content_hashes.remove(name);
    }

    pub fn clear(&mut self) {
        self.content_hashes.clear();
    }
}

pub enum LaunchKind {
    Local { remote_enabled: bool },
    Connect { id: String, remote_enabled: bool },
}

pub fn create_session(project_dir: &Path, launch: LaunchKind) -> Result<ManagedSession> {
    ensure_command("tmux")?;
    ensure_command("copilot")?;

    let name = unique_session_name(project_dir)?;
    let command = launch.command()?;

    let output = Command::new("tmux")
        .args(["new-session", "-d", "-s", &name, "-c"])
        .arg(project_dir)
        .arg(&command)
        .output()
        .with_context(|| format!("failed to start tmux session {name}"))?;
    check_output(output, "tmux new-session")?;

    tmux_status(
        Command::new("tmux")
            .args(["set-option", "-q", "-t", &name, PROJECT_OPTION])
            .arg(project_dir),
        "tmux set project metadata",
    )?;
    tmux_status(
        Command::new("tmux").args([
            "set-option",
            "-q",
            "-t",
            &name,
            CREATED_OPTION,
            &unix_now().to_string(),
        ]),
        "tmux set created metadata",
    )?;
    tmux_status(
        Command::new("tmux").args([
            "set-option",
            "-q",
            "-t",
            &name,
            COMMAND_OPTION,
            command.as_str(),
        ]),
        "tmux set command metadata",
    )?;
    tmux_status(
        Command::new("tmux").args([
            "set-option",
            "-q",
            "-t",
            &name,
            LAST_SEEN_OPTION,
            &unix_now().to_string(),
        ]),
        "tmux set last-seen metadata",
    )?;
    tmux_status(
        Command::new("tmux").args([
            "set-window-option",
            "-q",
            "-t",
            &format!("{name}:0"),
            "remain-on-exit",
            "on",
        ]),
        "tmux enable remain-on-exit",
    )?;
    tmux_status(
        Command::new("tmux").args([
            "set-window-option",
            "-q",
            "-t",
            &format!("{name}:0"),
            "monitor-bell",
            "on",
        ]),
        "tmux enable bell monitoring",
    )?;
    tmux_status(
        Command::new("tmux").args(["set-option", "-q", "-t", &name, "bell-action", "any"]),
        "tmux set bell action",
    )?;
    tmux_status(
        Command::new("tmux").args(["set-option", "-q", "-t", &name, "status-left", ""]),
        "tmux hide generated session name",
    )?;
    tmux_status(
        Command::new("tmux").args(["set-option", "-q", "-t", &name, "status-left-length", "0"]),
        "tmux hide generated session name length",
    )?;

    Ok(ManagedSession {
        display_name: display_name(&name),
        name,
        project_dir: project_dir.to_path_buf(),
        is_current_project: true,
        status: SessionStatus::Busy,
        last_activity: Some(SystemTime::now()),
        has_bell: false,
        pane_dead: false,
    })
}

pub fn list_sessions(current_dir: &Path, cache: &mut StatusCache) -> Result<Vec<ManagedSession>> {
    let output = Command::new("tmux")
        .args([
            "list-sessions",
            "-F",
            "#{session_name}\t#{@gh-pilot-project}\t#{@gh-pilot-created}\t#{@gh-pilot-last-seen}",
        ])
        .output()
        .context("failed to list tmux sessions")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("no server running") {
            return Ok(Vec::new());
        }
        bail!("tmux list-sessions failed: {}", stderr.trim());
    }

    let mut sessions = Vec::new();
    let now = SystemTime::now();

    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut fields = line.splitn(4, '\t');
        let Some(name) = fields.next() else {
            continue;
        };
        if !name.starts_with(SESSION_PREFIX) {
            continue;
        }

        let project_field = fields.next().unwrap_or_default();
        let metadata_project = (!project_field.is_empty()).then(|| PathBuf::from(project_field));
        let _created_at = fields.next();
        let last_seen = fields.next().and_then(parse_tmux_time);

        let window_info = window_info(name).unwrap_or_default();
        let project_dir = metadata_project
            .or_else(|| window_info.current_path.clone())
            .unwrap_or_else(|| PathBuf::from("."));

        let content = if window_info.pane_dead {
            String::new()
        } else {
            capture_pane(name).unwrap_or_default()
        };

        let hash = status::content_hash(&content);
        let previous_hash = cache.content_hashes.insert(name.to_owned(), hash);
        let content_changed = previous_hash.is_some_and(|previous| previous != hash);
        let activity_recent = window_info
            .last_activity
            .and_then(|last| now.duration_since(last).ok())
            .is_some_and(|age| age <= Duration::from_secs(4));
        let seen = match (window_info.last_activity, last_seen) {
            (Some(last_activity), Some(last_seen)) => last_activity <= last_seen,
            _ => false,
        };

        let detected_status = if window_info.pane_dead {
            SessionStatus::Idle
        } else {
            status::detect_status(&content, content_changed, activity_recent, seen)
        };

        sessions.push(ManagedSession {
            name: name.to_owned(),
            display_name: session_display_name(name, window_info.title.as_deref()),
            project_dir: project_dir.clone(),
            is_current_project: same_directory(&project_dir, current_dir),
            status: detected_status,
            last_activity: window_info.last_activity,
            has_bell: window_info.has_bell,
            pane_dead: window_info.pane_dead,
        });
    }

    sessions.sort_by(|a, b| {
        a.status
            .sort_rank()
            .cmp(&b.status.sort_rank())
            .then_with(|| b.last_activity.cmp(&a.last_activity))
            .then_with(|| a.display_name.cmp(&b.display_name))
    });

    Ok(sessions)
}

pub fn attach_session(name: &str) -> Result<()> {
    let status = Command::new("tmux")
        .args(["attach-session", "-t", name])
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("failed to attach tmux session {name}"))?;

    if !status.success() {
        bail!("tmux attach-session exited with {status}");
    }

    Ok(())
}

pub fn mark_seen(name: &str) -> Result<()> {
    let activity = window_info(name)?
        .last_activity
        .unwrap_or_else(SystemTime::now);
    let secs = activity
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .to_string();
    tmux_status(
        Command::new("tmux").args(["set-option", "-q", "-t", name, LAST_SEEN_OPTION, &secs]),
        "tmux mark session seen",
    )
}

pub fn kill_session(name: &str) -> Result<()> {
    tmux_status(
        Command::new("tmux").args(["kill-session", "-t", name]),
        "tmux kill-session",
    )
}

pub fn kill_sessions<'a>(names: impl IntoIterator<Item = &'a str>) -> Result<()> {
    for name in names {
        kill_session(name)?;
    }
    Ok(())
}

fn capture_pane(name: &str) -> Result<String> {
    let output = Command::new("tmux")
        .args(["capture-pane", "-p", "-e", "-J", "-S", "-120", "-t", name])
        .output()
        .with_context(|| format!("failed to capture tmux pane {name}"))?;
    check_output(output, "tmux capture-pane")
}

#[derive(Default)]
struct WindowInfo {
    last_activity: Option<SystemTime>,
    pane_dead: bool,
    has_bell: bool,
    current_path: Option<PathBuf>,
    title: Option<String>,
}

fn window_info(name: &str) -> Result<WindowInfo> {
    let output = Command::new("tmux")
        .args([
            "display-message",
            "-p",
            "-t",
            name,
            "#{window_activity}\t#{pane_dead}\t#{pane_current_path}\t#{window_flags}\t#{pane_title}",
        ])
        .output()
        .with_context(|| format!("failed to inspect tmux session {name}"))?;
    let info = check_output(output, "tmux display-message")?;
    let mut fields = info.trim_end().splitn(5, '\t');
    let last_activity = fields.next().and_then(parse_tmux_time);
    let pane_dead = fields.next().is_some_and(|field| field == "1");
    let current_path = fields
        .next()
        .filter(|field| !field.is_empty())
        .map(PathBuf::from);
    let has_bell = fields.next().is_some_and(|flags| flags.contains('!'));
    let title = fields.next().and_then(sanitize_title);

    Ok(WindowInfo {
        last_activity,
        pane_dead,
        has_bell,
        current_path,
        title,
    })
}

fn parse_tmux_time(field: &str) -> Option<SystemTime> {
    field
        .parse::<u64>()
        .ok()
        .map(|secs| UNIX_EPOCH + Duration::from_secs(secs))
}

fn unique_session_name(project_dir: &Path) -> Result<String> {
    let slug = project_dir
        .file_name()
        .and_then(|name| name.to_str())
        .map(slugify)
        .filter(|slug| !slug.is_empty())
        .unwrap_or_else(|| "project".to_owned());

    for attempt in 0..100 {
        let candidate = format!(
            "{SESSION_PREFIX}{slug}__{}__{}",
            unix_now(),
            std::process::id() + attempt
        );
        if !session_exists(&candidate)? {
            return Ok(candidate);
        }
    }

    Err(anyhow!("could not create a unique tmux session name"))
}

fn session_exists(name: &str) -> Result<bool> {
    let output = Command::new("tmux")
        .args(["has-session", "-t", name])
        .output()
        .with_context(|| format!("failed to check tmux session {name}"))?;

    Ok(output.status.success())
}

fn display_name(name: &str) -> String {
    let generated = name.strip_prefix(SESSION_PREFIX).unwrap_or(name);
    generated
        .split("__")
        .next()
        .unwrap_or(generated)
        .replace('-', " ")
}

fn session_display_name(name: &str, title: Option<&str>) -> String {
    title
        .and_then(sanitize_title)
        .filter(|title| title != name && !title.starts_with(SESSION_PREFIX))
        .unwrap_or_else(|| display_name(name))
}

fn sanitize_title(title: &str) -> Option<String> {
    let title = title
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect::<String>();
    let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
    (!title.is_empty()).then_some(title)
}

fn slugify(input: &str) -> String {
    let mut slug = input
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();

    while slug.contains("--") {
        slug = slug.replace("--", "-");
    }

    slug.trim_matches('-').chars().take(32).collect()
}

fn same_directory(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

fn ensure_command(command: &str) -> Result<()> {
    let status = Command::new("sh")
        .args(["-c", &format!("command -v {command} >/dev/null 2>&1")])
        .status()
        .with_context(|| format!("failed to check for {command}"))?;

    if status.success() {
        Ok(())
    } else {
        bail!("{command} was not found in PATH");
    }
}

fn tmux_status(command: &mut Command, context: &str) -> Result<()> {
    let output = command.output().with_context(|| context.to_owned())?;
    check_output(output, context).map(|_| ())
}

fn check_output(output: Output, context: &str) -> Result<String> {
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("{context} failed: {}", stderr.trim());
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl LaunchKind {
    fn command(&self) -> Result<String> {
        match self {
            Self::Local { remote_enabled } => {
                if *remote_enabled {
                    Ok("exec copilot --remote".to_owned())
                } else {
                    Ok("exec copilot".to_owned())
                }
            }
            Self::Connect { id, remote_enabled } => {
                validate_session_id(id)?;
                if *remote_enabled {
                    Ok(format!("exec copilot --remote --resume={id}"))
                } else {
                    Ok(format!("exec copilot --resume={id}"))
                }
            }
        }
    }
}

fn validate_session_id(id: &str) -> Result<()> {
    if id.is_empty() {
        bail!("remote session id cannot be empty");
    }

    if id
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | ':'))
    {
        Ok(())
    } else {
        bail!("remote session id contains unsupported characters");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugifies_project_names() {
        assert_eq!(slugify("My Project.rs"), "my-project-rs");
    }

    #[test]
    fn validates_remote_ids() {
        assert!(validate_session_id("abc-123_def:ghi.jkl").is_ok());
        assert!(validate_session_id("abc;rm").is_err());
    }

    #[test]
    fn connects_remote_tasks_with_direct_resume() {
        let command = LaunchKind::Connect {
            id: "85320a43-6021-40ac-bb41-d8f1ab2b9372".to_owned(),
            remote_enabled: false,
        }
        .command()
        .unwrap();

        assert_eq!(
            command,
            "exec copilot --resume=85320a43-6021-40ac-bb41-d8f1ab2b9372"
        );
    }
}
