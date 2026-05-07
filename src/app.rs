use crate::session::{group_sessions, load_sessions, seed_demo_sessions, Session, SessionStatus};
use anyhow::Result;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Panel {
    Sessions,
    Detail,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mode {
    Normal,
    /// User is typing a new session name
    NewSessionName,
    /// User is typing a new session path
    NewSessionPath,
    /// Confirm delete dialog
    ConfirmDelete,
}

pub struct App {
    pub sessions: Vec<Session>,
    /// Flat list of session indices (mirroring sessions vec) after grouping
    pub flat_list: Vec<FlatItem>,
    /// Current cursor position in the flat list
    pub cursor: usize,
    /// Index into sessions vec of the selected (detailed) session
    pub selected_session: Option<usize>,
    pub active_panel: Panel,
    pub sessions_dir: PathBuf,
    pub mode: Mode,
    pub input_buffer: String,
    pub new_session_name: Option<String>,
    pub log_scroll: usize,
    pub should_quit: bool,
    pub status_message: Option<String>,
}

/// An item in the flat list shown in the sessions panel.
#[derive(Debug, Clone)]
pub enum FlatItem {
    /// A group header (project_path)
    GroupHeader(String),
    /// A session entry: index into App::sessions
    SessionEntry(usize),
}

impl App {
    pub fn new(sessions_dir: PathBuf) -> Result<Self> {
        // Seed demo sessions if the directory is empty
        let _ = seed_demo_sessions(&sessions_dir);

        let sessions = load_sessions(&sessions_dir);
        let flat_list = build_flat_list(&sessions);

        let selected_session = flat_list.iter().find_map(|item| {
            if let FlatItem::SessionEntry(idx) = item {
                Some(*idx)
            } else {
                None
            }
        });

        // Start cursor on the first session entry
        let cursor = flat_list
            .iter()
            .position(|item| matches!(item, FlatItem::SessionEntry(_)))
            .unwrap_or(0);

        Ok(App {
            sessions,
            flat_list,
            cursor,
            selected_session,
            active_panel: Panel::Sessions,
            sessions_dir,
            mode: Mode::Normal,
            input_buffer: String::new(),
            new_session_name: None,
            log_scroll: 0,
            should_quit: false,
            status_message: None,
        })
    }

    pub fn reload(&mut self) {
        self.sessions = load_sessions(&self.sessions_dir);
        self.flat_list = build_flat_list(&self.sessions);

        // Try to keep the cursor pointing at a session entry
        if self.cursor >= self.flat_list.len() {
            self.cursor = self.flat_list.len().saturating_sub(1);
        }

        // Re-derive selected session from cursor
        self.selected_session = self.session_at_cursor();
        self.log_scroll = 0;
    }

    // ── Navigation ─────────────────────────────────────────────────────────

    pub fn move_up(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let mut new_cursor = self.cursor - 1;
        // Skip group headers
        while new_cursor > 0 {
            if matches!(self.flat_list[new_cursor], FlatItem::GroupHeader(_)) {
                new_cursor -= 1;
            } else {
                break;
            }
        }
        // If we landed on a header at position 0, don't move
        if matches!(self.flat_list[new_cursor], FlatItem::GroupHeader(_)) {
            return;
        }
        self.cursor = new_cursor;
        self.selected_session = self.session_at_cursor();
        self.log_scroll = 0;
    }

    pub fn move_down(&mut self) {
        if self.cursor + 1 >= self.flat_list.len() {
            return;
        }
        let mut new_cursor = self.cursor + 1;
        // Skip group headers
        while new_cursor < self.flat_list.len() - 1 {
            if matches!(self.flat_list[new_cursor], FlatItem::GroupHeader(_)) {
                new_cursor += 1;
            } else {
                break;
            }
        }
        if matches!(self.flat_list[new_cursor], FlatItem::GroupHeader(_)) {
            return;
        }
        self.cursor = new_cursor;
        self.selected_session = self.session_at_cursor();
        self.log_scroll = 0;
    }

    pub fn select_current(&mut self) {
        if let Some(idx) = self.session_at_cursor() {
            self.selected_session = Some(idx);
            self.active_panel = Panel::Detail;
            self.log_scroll = 0;
        }
    }

    pub fn focus_sessions(&mut self) {
        self.active_panel = Panel::Sessions;
    }

    pub fn scroll_log_up(&mut self) {
        self.log_scroll = self.log_scroll.saturating_sub(1);
    }

    pub fn scroll_log_down(&mut self) {
        self.log_scroll += 1;
    }

    // ── Session management ──────────────────────────────────────────────────

    pub fn begin_new_session(&mut self) {
        self.mode = Mode::NewSessionName;
        self.input_buffer.clear();
        self.new_session_name = None;
    }

    pub fn confirm_input(&mut self) {
        match self.mode {
            Mode::NewSessionName => {
                if !self.input_buffer.is_empty() {
                    self.new_session_name = Some(self.input_buffer.trim().to_string());
                    self.input_buffer.clear();
                    self.mode = Mode::NewSessionPath;
                }
            }
            Mode::NewSessionPath => {
                let path = if self.input_buffer.trim().is_empty() {
                    std::env::current_dir()
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_else(|_| "~".to_string())
                } else {
                    self.input_buffer.trim().to_string()
                };

                if let Some(name) = self.new_session_name.take() {
                    let session = Session::new(name, path);
                    if let Err(e) = session.save(&self.sessions_dir) {
                        self.status_message = Some(format!("Error saving session: {e}"));
                    } else {
                        self.status_message = Some("Session created".to_string());
                    }
                }
                self.input_buffer.clear();
                self.mode = Mode::Normal;
                self.reload();
            }
            _ => {}
        }
    }

    pub fn cancel_input(&mut self) {
        self.mode = Mode::Normal;
        self.input_buffer.clear();
        self.new_session_name = None;
    }

    pub fn begin_delete(&mut self) {
        if self.session_at_cursor().is_some() {
            self.mode = Mode::ConfirmDelete;
        }
    }

    pub fn confirm_delete(&mut self) {
        if let Some(idx) = self.session_at_cursor() {
            let session = self.sessions[idx].clone();
            if let Err(e) = session.delete(&self.sessions_dir) {
                self.status_message = Some(format!("Error deleting session: {e}"));
            } else {
                self.status_message = Some("Session deleted".to_string());
            }
        }
        self.mode = Mode::Normal;
        self.reload();
    }

    pub fn cancel_delete(&mut self) {
        self.mode = Mode::Normal;
    }

    pub fn toggle_status(&mut self) {
        if let Some(idx) = self.session_at_cursor() {
            let session = &mut self.sessions[idx];
            session.status = match session.status {
                SessionStatus::Active => SessionStatus::Inactive,
                SessionStatus::Inactive => SessionStatus::Active,
                SessionStatus::Paused => SessionStatus::Active,
            };
            // Save the update
            let mut updated = session.clone();
            updated.updated_at = chrono::Utc::now();
            if let Err(e) = updated.save(&self.sessions_dir) {
                self.status_message = Some(format!("Error saving session: {e}"));
            } else {
                self.sessions[idx] = updated;
            }
        }
    }

    // ── Helpers ─────────────────────────────────────────────────────────────

    pub fn session_at_cursor(&self) -> Option<usize> {
        self.flat_list.get(self.cursor).and_then(|item| {
            if let FlatItem::SessionEntry(idx) = item {
                Some(*idx)
            } else {
                None
            }
        })
    }

    #[allow(dead_code)]
    pub fn selected_log(&self) -> String {
        if let Some(idx) = self.selected_session {
            self.sessions[idx].read_log(&self.sessions_dir)
        } else {
            String::new()
        }
    }

    pub fn total_sessions(&self) -> usize {
        self.sessions.len()
    }

    pub fn active_count(&self) -> usize {
        self.sessions
            .iter()
            .filter(|s| s.status == SessionStatus::Active)
            .count()
    }
}

/// Build the flat list from sessions (groups + entries) for display in the list panel.
fn build_flat_list(sessions: &[Session]) -> Vec<FlatItem> {
    let groups = group_sessions(sessions);
    let mut flat = Vec::new();
    for (project_path, indices) in groups {
        flat.push(FlatItem::GroupHeader(project_path));
        for idx in indices {
            flat.push(FlatItem::SessionEntry(idx));
        }
    }
    flat
}
