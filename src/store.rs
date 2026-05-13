use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use rusqlite::{Connection, params};

use crate::status::SessionStatus;
use crate::tmux::ManagedSession;

pub struct SessionStore {
    connection: Connection,
}

#[derive(Debug, Clone)]
pub struct CachedRemoteSession {
    pub id: String,
    pub display_name: String,
    pub project_dir: PathBuf,
    pub repository: Option<String>,
    pub status: SessionStatus,
    pub state: String,
    pub updated_at: Option<String>,
    pub url: Option<String>,
    pub pr_url: Option<String>,
}

impl SessionStore {
    pub fn open() -> Result<Self> {
        let path = database_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }

        let connection = Connection::open(&path)
            .with_context(|| format!("failed to open session cache {}", path.display()))?;
        let store = Self { connection };
        store.initialize()?;
        Ok(store)
    }

    pub fn load_local(&self, current_dir: &Path) -> Result<Vec<ManagedSession>> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT name, display_name, project_dir, status, last_activity, has_bell, pane_dead
                 FROM local_sessions
                 ORDER BY status_rank, last_activity DESC, display_name ASC",
            )
            .context("failed to load cached local sessions")?;

        let sessions = statement
            .query_map([], |row| {
                let project_dir = PathBuf::from(row.get::<_, String>(2)?);
                Ok(ManagedSession {
                    name: row.get(0)?,
                    display_name: row.get(1)?,
                    is_current_project: same_path(&project_dir, current_dir),
                    project_dir,
                    status: SessionStatus::from_label(&row.get::<_, String>(3)?),
                    last_activity: row
                        .get::<_, Option<i64>>(4)?
                        .and_then(system_time_from_unix),
                    has_bell: row.get::<_, i64>(5)? != 0,
                    pane_dead: row.get::<_, i64>(6)? != 0,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to read cached local sessions")?;

        Ok(sessions)
    }

    pub fn replace_local(&mut self, sessions: &[ManagedSession]) -> Result<()> {
        let transaction = self
            .connection
            .transaction()
            .context("failed to update cached local sessions")?;
        transaction.execute("DELETE FROM local_sessions", [])?;
        {
            let mut insert = transaction.prepare(
                "INSERT INTO local_sessions (
                    name, display_name, project_dir, status, status_rank, last_activity,
                    has_bell, pane_dead, updated_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?;

            let updated_at = unix_now();
            for session in sessions {
                insert.execute(params![
                    session.name,
                    session.display_name,
                    session.project_dir.to_string_lossy(),
                    session.status.label(),
                    session.status.sort_rank(),
                    session.last_activity.and_then(unix_from_system_time),
                    bool_to_i64(session.has_bell),
                    bool_to_i64(session.pane_dead),
                    updated_at,
                ])?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn load_remote(&self, current_dir: &Path) -> Result<Vec<CachedRemoteSession>> {
        let project_dir = current_dir.to_string_lossy();
        let mut statement = self
            .connection
            .prepare(
                "SELECT id, display_name, project_dir, repository, status, state, updated_at, url, pr_url
                 FROM remote_sessions
                 WHERE project_dir = ?1 AND repository IS NOT NULL
                 ORDER BY status_rank, updated_at DESC, display_name ASC",
            )
            .context("failed to load cached remote sessions")?;

        let sessions = statement
            .query_map([project_dir.as_ref()], |row| {
                Ok(CachedRemoteSession {
                    id: row.get(0)?,
                    display_name: row.get(1)?,
                    project_dir: PathBuf::from(row.get::<_, String>(2)?),
                    repository: row.get(3)?,
                    status: SessionStatus::from_label(&row.get::<_, String>(4)?),
                    state: row.get(5)?,
                    updated_at: row.get(6)?,
                    url: row.get(7)?,
                    pr_url: row.get(8)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to read cached remote sessions")?;

        Ok(sessions)
    }

    pub fn replace_remote(
        &mut self,
        current_dir: &Path,
        sessions: &[CachedRemoteSession],
    ) -> Result<()> {
        let project_dir = current_dir.to_string_lossy();
        let transaction = self
            .connection
            .transaction()
            .context("failed to update cached remote sessions")?;
        transaction.execute(
            "DELETE FROM remote_sessions WHERE project_dir = ?1",
            [project_dir.as_ref()],
        )?;
        {
            let mut insert = transaction.prepare(
                "INSERT INTO remote_sessions (
                    id, project_dir, display_name, repository, status, status_rank, state, updated_at, url, pr_url, cached_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            )?;

            let cached_at = unix_now();
            for session in sessions {
                insert.execute(params![
                    session.id,
                    session.project_dir.to_string_lossy(),
                    session.display_name,
                    session.repository,
                    session.status.label(),
                    session.status.sort_rank(),
                    session.state,
                    session.updated_at,
                    session.url,
                    session.pr_url,
                    cached_at,
                ])?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn load_recent_paths(&self, limit: usize) -> Result<Vec<PathBuf>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut statement = self
            .connection
            .prepare(
                "SELECT path
                 FROM recent_paths
                 ORDER BY last_used DESC, path ASC
                 LIMIT ?1",
            )
            .context("failed to load recent paths")?;

        let paths = statement
            .query_map([limit], |row| Ok(PathBuf::from(row.get::<_, String>(0)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to read recent paths")?;

        Ok(paths)
    }

    pub fn load_removed_projects(&self) -> Result<Vec<PathBuf>> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT path
                 FROM removed_projects
                 ORDER BY removed_at DESC, path ASC",
            )
            .context("failed to load removed projects")?;

        let paths = statement
            .query_map([], |row| Ok(PathBuf::from(row.get::<_, String>(0)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to read removed projects")?;

        Ok(paths)
    }

    pub fn record_recent_path(&mut self, path: &Path, limit: usize) -> Result<()> {
        let transaction = self
            .connection
            .transaction()
            .context("failed to record recent path")?;
        transaction.execute(
            "INSERT INTO recent_paths (path, last_used)
             VALUES (?1, ?2)
             ON CONFLICT(path) DO UPDATE SET last_used = excluded.last_used",
            params![path.to_string_lossy(), unix_now()],
        )?;
        transaction.execute(
            "DELETE FROM removed_projects WHERE path = ?1",
            [path.to_string_lossy().as_ref()],
        )?;
        transaction.execute(
            "DELETE FROM recent_paths
             WHERE path NOT IN (
                 SELECT path FROM recent_paths ORDER BY last_used DESC, path ASC LIMIT ?1
             )",
            [i64::try_from(limit).unwrap_or(i64::MAX)],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn remove_project(&mut self, path: &Path) -> Result<()> {
        let project_dir = path.to_string_lossy();
        let transaction = self
            .connection
            .transaction()
            .context("failed to remove project")?;
        transaction.execute(
            "DELETE FROM remote_sessions WHERE project_dir = ?1",
            [project_dir.as_ref()],
        )?;
        transaction.execute(
            "DELETE FROM recent_paths WHERE path = ?1",
            [project_dir.as_ref()],
        )?;
        transaction.execute(
            "INSERT INTO removed_projects (path, removed_at)
             VALUES (?1, ?2)
             ON CONFLICT(path) DO UPDATE SET removed_at = excluded.removed_at",
            params![project_dir.as_ref(), unix_now()],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn restore_project(&mut self, path: &Path) -> Result<()> {
        self.connection
            .execute(
                "DELETE FROM removed_projects WHERE path = ?1",
                [path.to_string_lossy().as_ref()],
            )
            .context("failed to restore project")?;
        Ok(())
    }

    fn initialize(&self) -> Result<()> {
        self.connection
            .execute_batch(
                "
                CREATE TABLE IF NOT EXISTS local_sessions (
                    name TEXT PRIMARY KEY,
                    display_name TEXT NOT NULL,
                    project_dir TEXT NOT NULL,
                    status TEXT NOT NULL,
                    status_rank INTEGER NOT NULL,
                    last_activity INTEGER,
                    has_bell INTEGER NOT NULL,
                    pane_dead INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL
                );

                CREATE TABLE IF NOT EXISTS remote_sessions (
                    id TEXT NOT NULL,
                    project_dir TEXT NOT NULL,
                    display_name TEXT NOT NULL,
                    repository TEXT,
                    status TEXT NOT NULL,
                    status_rank INTEGER NOT NULL,
                    state TEXT NOT NULL,
                    updated_at TEXT,
                    url TEXT,
                    pr_url TEXT,
                    cached_at INTEGER NOT NULL,
                    PRIMARY KEY (id, project_dir)
                );

                CREATE TABLE IF NOT EXISTS recent_paths (
                    path TEXT PRIMARY KEY,
                    last_used INTEGER NOT NULL
                );

                CREATE TABLE IF NOT EXISTS removed_projects (
                    path TEXT PRIMARY KEY,
                    removed_at INTEGER NOT NULL
                );
                ",
            )
            .context("failed to initialize session cache")?;
        self.ensure_column("remote_sessions", "url", "TEXT")?;
        self.ensure_column("remote_sessions", "pr_url", "TEXT")?;
        self.ensure_column("remote_sessions", "repository", "TEXT")?;
        Ok(())
    }

    fn ensure_column(&self, table: &str, column: &str, definition: &str) -> Result<()> {
        let mut statement = self
            .connection
            .prepare(&format!("PRAGMA table_info({table})"))
            .with_context(|| format!("failed to inspect {table} schema"))?;
        let exists = statement
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .iter()
            .any(|name| name == column);
        if !exists {
            self.connection
                .execute(
                    &format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"),
                    [],
                )
                .with_context(|| format!("failed to add {column} to {table}"))?;
        }
        Ok(())
    }
}

fn database_path() -> Result<PathBuf> {
    let config_home = match env::var_os("XDG_CONFIG_HOME") {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        _ => home_dir()?.join(".config"),
    };
    Ok(config_home.join("gh-pilot").join("sessions.sqlite3"))
}

fn home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("HOME is not set; cannot locate gh-pilot config directory"))
}

fn unix_now() -> i64 {
    unix_from_system_time(SystemTime::now()).unwrap_or_default()
}

fn unix_from_system_time(time: SystemTime) -> Option<i64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())
}

fn system_time_from_unix(secs: i64) -> Option<SystemTime> {
    u64::try_from(secs)
        .ok()
        .map(|secs| UNIX_EPOCH + Duration::from_secs(secs))
}

fn bool_to_i64(value: bool) -> i64 {
    if value { 1 } else { 0 }
}

fn same_path(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}
