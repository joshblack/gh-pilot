use crate::session::{group_sessions, load_sessions, CopilotSession, SessionStatus};
use std::path::PathBuf;

// ── Enums ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Panel {
    Sessions,
    Detail,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mode {
    Normal,
    /// User is typing a directory for a new copilot session
    NewSessionDir,
}

/// Actions that require the TUI to be suspended so an external command can run.
#[derive(Debug, Clone)]
pub enum PendingAction {
    None,
    /// Launch `copilot -C <dir>` (new session)
    LaunchNew { dir: PathBuf },
    /// Resume `copilot --resume=<id>`
    ResumeSession { id: String },
}

// ── FlatItem ─────────────────────────────────────────────────────────────────

/// An item in the flat list shown in the sessions panel.
#[derive(Debug, Clone)]
pub enum FlatItem {
    /// A group header showing the cwd path
    GroupHeader(String),
    /// A session entry: index into App::sessions
    SessionEntry(usize),
}

// ── App ──────────────────────────────────────────────────────────────────────

pub struct App {
    pub sessions: Vec<CopilotSession>,
    /// Flat list of items (headers + entries) for the left panel
    pub flat_list: Vec<FlatItem>,
    /// Current cursor position in the flat list
    pub cursor: usize,
    /// Index of the session currently shown in the detail panel
    pub selected_session: Option<usize>,
    pub active_panel: Panel,
    /// Root copilot config dir (default: ~/.copilot)
    pub copilot_dir: PathBuf,
    /// Directory where mission-control was launched (default for new sessions)
    pub launch_dir: PathBuf,
    pub mode: Mode,
    pub input_buffer: String,
    pub detail_scroll: usize,
    pub should_quit: bool,
    pub status_message: Option<String>,
    pub pending_action: PendingAction,
}

impl App {
    pub fn new(copilot_dir: PathBuf, launch_dir: PathBuf) -> Self {
        let sessions = load_sessions(&copilot_dir);
        let flat_list = build_flat_list(&sessions);

        let selected_session = flat_list.iter().find_map(|item| {
            if let FlatItem::SessionEntry(idx) = item {
                Some(*idx)
            } else {
                None
            }
        });

        let cursor = flat_list
            .iter()
            .position(|item| matches!(item, FlatItem::SessionEntry(_)))
            .unwrap_or(0);

        App {
            sessions,
            flat_list,
            cursor,
            selected_session,
            active_panel: Panel::Sessions,
            copilot_dir,
            launch_dir,
            mode: Mode::Normal,
            input_buffer: String::new(),
            detail_scroll: 0,
            should_quit: false,
            status_message: None,
            pending_action: PendingAction::None,
        }
    }

    pub fn reload(&mut self) {
        self.sessions = load_sessions(&self.copilot_dir);
        self.flat_list = build_flat_list(&self.sessions);

        if self.cursor >= self.flat_list.len() {
            self.cursor = self.flat_list.len().saturating_sub(1);
        }
        self.selected_session = self.session_at_cursor();
        self.detail_scroll = 0;
    }

    // ── Navigation ────────────────────────────────────────────────────────────

    pub fn move_up(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let mut c = self.cursor - 1;
        while c > 0 && matches!(self.flat_list[c], FlatItem::GroupHeader(_)) {
            c -= 1;
        }
        if matches!(self.flat_list[c], FlatItem::GroupHeader(_)) {
            return;
        }
        self.cursor = c;
        self.selected_session = self.session_at_cursor();
        self.detail_scroll = 0;
    }

    pub fn move_down(&mut self) {
        if self.cursor + 1 >= self.flat_list.len() {
            return;
        }
        let mut c = self.cursor + 1;
        while c < self.flat_list.len() - 1 && matches!(self.flat_list[c], FlatItem::GroupHeader(_)) {
            c += 1;
        }
        if matches!(self.flat_list[c], FlatItem::GroupHeader(_)) {
            return;
        }
        self.cursor = c;
        self.selected_session = self.session_at_cursor();
        self.detail_scroll = 0;
    }

    pub fn select_current(&mut self) {
        if let Some(idx) = self.session_at_cursor() {
            self.selected_session = Some(idx);
            self.active_panel = Panel::Detail;
            self.detail_scroll = 0;
        }
    }

    pub fn focus_sessions(&mut self) {
        self.active_panel = Panel::Sessions;
    }

    pub fn scroll_detail_up(&mut self) {
        self.detail_scroll = self.detail_scroll.saturating_sub(1);
    }

    pub fn scroll_detail_down(&mut self) {
        self.detail_scroll += 1;
    }

    // ── Session actions ───────────────────────────────────────────────────────

    /// Open the prompt to launch a new copilot session.
    /// Pre-fills the launch_dir as the default directory.
    pub fn begin_new_session(&mut self) {
        self.mode = Mode::NewSessionDir;
        self.input_buffer = self.launch_dir.to_string_lossy().to_string();
    }

    /// Confirm the new session directory prompt and queue the launch action.
    pub fn confirm_new_session(&mut self) {
        let raw = self.input_buffer.trim().to_string();
        let dir = if raw.is_empty() {
            self.launch_dir.clone()
        } else {
            // Expand ~ if present
            let expanded = if raw.starts_with('~') {
                if let Some(home) = dirs::home_dir() {
                    home.join(raw.trim_start_matches("~/"))
                        .join(raw.trim_start_matches("~"))
                } else {
                    PathBuf::from(&raw)
                }
            } else {
                PathBuf::from(&raw)
            };
            expanded
        };
        self.input_buffer.clear();
        self.mode = Mode::Normal;
        self.pending_action = PendingAction::LaunchNew { dir };
    }

    pub fn cancel_input(&mut self) {
        self.mode = Mode::Normal;
        self.input_buffer.clear();
    }

    /// Queue resuming the session currently under the cursor.
    pub fn open_session(&mut self) {
        if let Some(idx) = self.session_at_cursor() {
            let id = self.sessions[idx].id.clone();
            self.pending_action = PendingAction::ResumeSession { id };
        }
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    pub fn session_at_cursor(&self) -> Option<usize> {
        self.flat_list.get(self.cursor).and_then(|item| {
            if let FlatItem::SessionEntry(idx) = item {
                Some(*idx)
            } else {
                None
            }
        })
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

// ── Build flat list ───────────────────────────────────────────────────────────

fn build_flat_list(sessions: &[CopilotSession]) -> Vec<FlatItem> {
    let groups = group_sessions(sessions);
    let mut flat = Vec::new();
    for (key, indices) in groups {
        flat.push(FlatItem::GroupHeader(key));
        for idx in indices {
            flat.push(FlatItem::SessionEntry(idx));
        }
    }
    flat
}

