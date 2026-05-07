use anyhow::{Context, Result};
use std::path::PathBuf;

use crate::session::Session;

/// Persistent JSON registry of managed sessions.
///
/// Sessions are stored in `~/.gh-mission-control/sessions.json`.
pub struct Registry {
    path: PathBuf,
    sessions: Vec<Session>,
}

impl Registry {
    /// Load the registry from disk, creating an empty one if it does not exist.
    pub fn load() -> Result<Self> {
        let path = registry_path()?;
        let sessions = if path.exists() {
            let data = std::fs::read_to_string(&path)
                .with_context(|| format!("Failed to read registry at {}", path.display()))?;
            serde_json::from_str::<Vec<Session>>(&data)
                .with_context(|| "Failed to parse session registry (is the file corrupted?)")?
        } else {
            Vec::new()
        };
        Ok(Registry { path, sessions })
    }

    /// Persist the current registry state to disk.
    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create registry directory: {}", parent.display()))?;
        }
        let data = serde_json::to_string_pretty(&self.sessions)
            .context("Failed to serialize session registry")?;
        std::fs::write(&self.path, data)
            .with_context(|| format!("Failed to write registry to {}", self.path.display()))?;
        Ok(())
    }

    /// Add a new session to the registry (does not save automatically).
    pub fn add(&mut self, session: Session) {
        self.sessions.push(session);
    }

    /// Remove a session by exact ID. Returns the removed session, or `None` if not found.
    pub fn remove(&mut self, id: &str) -> Option<Session> {
        if let Some(idx) = self.sessions.iter().position(|s| s.id == id) {
            Some(self.sessions.remove(idx))
        } else {
            None
        }
    }

    /// Get a session by exact ID.
    pub fn get(&self, id: &str) -> Option<&Session> {
        self.sessions.iter().find(|s| s.id == id)
    }

    /// Get a mutable reference to a session by exact ID.
    pub fn get_mut(&mut self, id: &str) -> Option<&mut Session> {
        self.sessions.iter_mut().find(|s| s.id == id)
    }

    /// Find a session by exact ID, ID prefix (≥4 chars), or exact title.
    pub fn find(&self, query: &str) -> Option<&Session> {
        // Exact ID match first.
        if let Some(s) = self.sessions.iter().find(|s| s.id == query) {
            return Some(s);
        }
        // ID prefix match (require at least 4 chars to avoid ambiguity).
        if query.len() >= 4 {
            let matches: Vec<_> = self
                .sessions
                .iter()
                .filter(|s| s.id.starts_with(query))
                .collect();
            if matches.len() == 1 {
                return Some(matches[0]);
            }
        }
        // Exact title match (case-insensitive).
        self.sessions
            .iter()
            .find(|s| s.title.eq_ignore_ascii_case(query))
    }

    /// Find a mutable reference to a session by exact ID, ID prefix, or exact title.
    pub fn find_mut(&mut self, query: &str) -> Option<&mut Session> {
        // Resolve to an ID first so we can do a single mutable lookup.
        let id = self.find(query)?.id.clone();
        self.sessions.iter_mut().find(|s| s.id == id)
    }

    /// Return a slice of all sessions.
    pub fn sessions(&self) -> &[Session] {
        &self.sessions
    }

}

/// Resolve the path of the registry JSON file.
pub fn registry_path() -> Result<PathBuf> {
    let base = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not determine home directory"))?;
    Ok(base.join(".gh-mission-control").join("sessions.json"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{Session, Status};
    use tempfile::TempDir;

    /// Build a Registry backed by a temp directory so tests don't touch `~`.
    fn temp_registry(dir: &TempDir) -> Registry {
        let path = dir.path().join("sessions.json");
        Registry {
            path,
            sessions: Vec::new(),
        }
    }

    fn make_session(title: &str) -> Session {
        Session::new(title.to_string(), "/tmp".to_string(), "cmd".to_string())
    }

    #[test]
    fn test_add_and_find() {
        let dir = TempDir::new().unwrap();
        let mut reg = temp_registry(&dir);
        let s = make_session("Alpha");
        let id = s.id.clone();
        reg.add(s);

        assert!(reg.find(&id).is_some());
        assert!(reg.find("Alpha").is_some());
        assert!(reg.find(&id[..8]).is_some());
    }

    #[test]
    fn test_find_case_insensitive_title() {
        let dir = TempDir::new().unwrap();
        let mut reg = temp_registry(&dir);
        reg.add(make_session("MyProject"));
        assert!(reg.find("myproject").is_some());
        assert!(reg.find("MYPROJECT").is_some());
    }

    #[test]
    fn test_find_returns_none_for_unknown() {
        let dir = TempDir::new().unwrap();
        let reg = temp_registry(&dir);
        assert!(reg.find("nonexistent").is_none());
    }

    #[test]
    fn test_remove() {
        let dir = TempDir::new().unwrap();
        let mut reg = temp_registry(&dir);
        let s = make_session("Beta");
        let id = s.id.clone();
        reg.add(s);

        let removed = reg.remove(&id);
        assert!(removed.is_some());
        assert_eq!(removed.unwrap().title, "Beta");
        assert!(reg.find("Beta").is_none());
    }

    #[test]
    fn test_save_and_reload() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("sessions.json");

        {
            let mut reg = Registry { path: path.clone(), sessions: Vec::new() };
            let mut s = make_session("Gamma");
            s.status = Status::Running;
            reg.add(s);
            reg.save().unwrap();
        }

        // Reload and verify.
        let reg = Registry {
            path: path.clone(),
            sessions: serde_json::from_str(
                &std::fs::read_to_string(&path).unwrap(),
            )
            .unwrap(),
        };
        assert_eq!(reg.sessions().len(), 1);
        assert_eq!(reg.sessions()[0].title, "Gamma");
        assert_eq!(reg.sessions()[0].status, Status::Running);
    }

    #[test]
    fn test_get_mut_updates_status() {
        let dir = TempDir::new().unwrap();
        let mut reg = temp_registry(&dir);
        let s = make_session("Delta");
        let id = s.id.clone();
        reg.add(s);

        let session = reg.get_mut(&id).unwrap();
        session.status = Status::Idle;

        assert_eq!(reg.get(&id).unwrap().status, Status::Idle);
    }

    #[test]
    fn test_ambiguous_prefix_not_matched() {
        let dir = TempDir::new().unwrap();
        let mut reg = temp_registry(&dir);
        // Force two sessions with IDs that share the same prefix by patching IDs.
        let mut s1 = make_session("One");
        let mut s2 = make_session("Two");
        // Make them share a prefix.
        s1.id = "abcd1234-0000-0000-0000-000000000001".to_string();
        s2.id = "abcd5678-0000-0000-0000-000000000002".to_string();
        reg.add(s1);
        reg.add(s2);

        // "abcd" prefix matches both → should not resolve.
        assert!(reg.find("abcd").is_none());
        // Full IDs should still work.
        assert!(reg.find("abcd1234-0000-0000-0000-000000000001").is_some());
    }
}
