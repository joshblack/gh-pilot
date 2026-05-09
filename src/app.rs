use crate::session::{load_sessions, refresh_session_statuses, CopilotSession, SessionStatus};
use crate::terminal::EmbeddedTerminal;
use std::collections::HashSet;
use std::path::PathBuf;

const DETAIL_PAGE_SCROLL_AMOUNT: usize = 5;

// ── Enums ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Panel {
    Sessions,
    Detail,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mode {
    Normal,
    /// User is typing a directory path for a new copilot session.
    NewSessionDir,
    /// An embedded copilot session is live in the right panel.
    Terminal,
}

/// Actions that require access to the terminal size (only available in the
/// event loop) before we can spawn an embedded PTY.
#[derive(Debug, Clone)]
pub enum PendingAction {
    None,
    /// Resume an existing session embedded in the right panel.
    OpenEmbedded {
        id: String,
        cwd: PathBuf,
    },
    /// Start a new copilot session in `dir`, embedded in the right panel.
    LaunchNew {
        dir: PathBuf,
    },
}

// ── App ──────────────────────────────────────────────────────────────────────

pub struct App {
    pub sessions: Vec<CopilotSession>,
    /// Session indices shown in the left panel.
    pub flat_list: Vec<usize>,
    /// Current cursor position in the flat list.
    pub cursor: usize,
    /// Index of the session currently shown in the detail panel.
    pub selected_session: Option<usize>,
    pub active_panel: Panel,
    /// Root copilot config dir (default: `~/.copilot`).
    pub copilot_dir: PathBuf,
    /// Directory where mission-control was launched (default for new sessions).
    pub launch_dir: PathBuf,
    pub mode: Mode,
    pub input_buffer: String,
    pub detail_scroll: usize,
    pub should_quit: bool,
    pub status_message: Option<String>,
    pub pending_action: PendingAction,
    /// Session IDs present before launching a new Copilot session.
    pub new_session_reload_baseline: Option<HashSet<String>>,
    notified_waiting_sessions: HashSet<String>,
    /// A live copilot session embedded in the right panel, if any.
    pub embedded_terminal: Option<EmbeddedTerminal>,
    /// Whether the embedded terminal is taking the full TUI area.
    pub terminal_fullscreen: bool,
}

impl App {
    pub fn new(copilot_dir: PathBuf, launch_dir: PathBuf) -> Self {
        let sessions = load_sessions(&copilot_dir);
        let flat_list = build_flat_list(&sessions);

        let selected_session = flat_list.first().copied();
        let cursor = 0;

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
            new_session_reload_baseline: None,
            notified_waiting_sessions: HashSet::new(),
            embedded_terminal: None,
            terminal_fullscreen: false,
        }
    }

    pub fn reload(&mut self) {
        self.replace_sessions(load_sessions(&self.copilot_dir));
    }

    pub fn refresh_statuses(&mut self) -> bool {
        refresh_session_statuses(&self.copilot_dir, &mut self.sessions);
        self.take_waiting_notification()
    }

    pub fn status_poll_interval(&self) -> std::time::Duration {
        if self
            .sessions
            .iter()
            .any(|session| session.status != SessionStatus::Idle)
        {
            std::time::Duration::from_secs(1)
        } else {
            std::time::Duration::from_secs(5)
        }
    }

    fn replace_sessions(&mut self, sessions: Vec<CopilotSession>) {
        let cursor_id = self
            .session_at_cursor()
            .map(|i| self.sessions[i].id.clone())
            .or_else(|| self.selected_session.map(|i| self.sessions[i].id.clone()));
        self.sessions = sessions;
        let session_ids: HashSet<&str> = self
            .sessions
            .iter()
            .map(|session| session.id.as_str())
            .collect();
        self.notified_waiting_sessions
            .retain(|id| session_ids.contains(id.as_str()));
        self.flat_list = build_flat_list(&self.sessions);

        if let Some(id) = cursor_id {
            if let Some(pos) = self
                .flat_list
                .iter()
                .position(|i| self.sessions[*i].id == id)
            {
                self.cursor = pos;
            }
        }
        if self.cursor >= self.flat_list.len() {
            self.cursor = self.flat_list.len().saturating_sub(1);
        }
        self.update_selected_from_cursor();
        self.detail_scroll = 0;
    }

    pub fn capture_new_session_reload_baseline(&mut self) {
        self.new_session_reload_baseline = Some(
            self.sessions
                .iter()
                .map(|session| session.id.clone())
                .collect(),
        );
    }

    pub fn clear_new_session_reload_watch(&mut self) {
        self.new_session_reload_baseline = None;
    }

    pub fn has_new_session_reload_watch(&self) -> bool {
        self.new_session_reload_baseline.is_some()
    }

    pub fn reload_if_new_session_created(&mut self) -> Option<String> {
        let baseline = self.new_session_reload_baseline.as_ref()?;

        let sessions = load_sessions(&self.copilot_dir);
        let new_session_id = sessions
            .iter()
            .find(|session| !baseline.contains(&session.id))
            .map(|session| session.id.clone())?;

        self.new_session_reload_baseline = None;
        self.replace_sessions(sessions);
        Some(new_session_id)
    }

    // ── Navigation ────────────────────────────────────────────────────────────

    pub fn move_up(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor -= 1;
        self.update_selected_from_cursor();
    }

    pub fn move_down(&mut self) {
        if self.cursor + 1 >= self.flat_list.len() {
            return;
        }
        self.cursor += 1;
        self.update_selected_from_cursor();
    }

    /// Select / activate the item currently under the cursor.
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

    pub fn scroll_detail_page_up(&mut self) {
        self.detail_scroll = self.detail_scroll.saturating_sub(DETAIL_PAGE_SCROLL_AMOUNT);
    }

    pub fn scroll_detail_page_down(&mut self) {
        self.detail_scroll += DETAIL_PAGE_SCROLL_AMOUNT;
    }

    // ── Session actions ───────────────────────────────────────────────────────

    /// Open the prompt to launch a new copilot session.
    pub fn begin_new_session(&mut self) {
        self.mode = Mode::NewSessionDir;
        self.input_buffer = self.default_new_session_dir().to_string_lossy().to_string();
    }

    /// Confirm the new session directory prompt and queue a launch.
    pub fn confirm_new_session(&mut self) {
        let raw = self.input_buffer.trim().to_string();
        let dir = if raw.is_empty() {
            self.launch_dir.clone()
        } else if let Some(rest) = raw.strip_prefix("~/") {
            dirs::home_dir()
                .map(|h| h.join(rest))
                .unwrap_or_else(|| PathBuf::from(&raw))
        } else if raw == "~" {
            dirs::home_dir().unwrap_or_else(|| PathBuf::from(&raw))
        } else {
            PathBuf::from(&raw)
        };
        self.input_buffer.clear();
        self.mode = Mode::Normal;
        self.pending_action = PendingAction::LaunchNew { dir };
    }

    pub fn cancel_input(&mut self) {
        self.mode = Mode::Normal;
        self.input_buffer.clear();
    }

    /// Queue opening an embedded terminal for the session under the cursor.
    pub fn open_session_embedded(&mut self) {
        if let Some(idx) = self.session_at_cursor() {
            let id = self.sessions[idx].id.clone();
            let cwd = self.sessions[idx].cwd.clone();
            self.selected_session = Some(idx);
            self.active_panel = Panel::Detail;
            self.pending_action = PendingAction::OpenEmbedded { id, cwd };
        }
    }

    /// Detach from the embedded terminal and return to normal mode.
    pub fn detach_terminal(&mut self) {
        self.embedded_terminal = None;
        self.mode = Mode::Normal;
        self.terminal_fullscreen = false;
    }

    pub fn toggle_terminal_fullscreen(&mut self) {
        self.terminal_fullscreen = !self.terminal_fullscreen;
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    pub fn session_at_cursor(&self) -> Option<usize> {
        self.flat_list.get(self.cursor).copied()
    }

    fn default_new_session_dir(&self) -> PathBuf {
        self.session_at_cursor()
            .or(self.selected_session)
            .map(|idx| self.sessions[idx].cwd.clone())
            .unwrap_or_else(|| self.launch_dir.clone())
    }

    fn take_waiting_notification(&mut self) -> bool {
        let waiting_sessions: HashSet<String> = self
            .sessions
            .iter()
            .filter(|session| session.status == SessionStatus::Waiting)
            .map(|session| session.id.clone())
            .collect();
        let should_notify = waiting_sessions
            .iter()
            .any(|id| !self.notified_waiting_sessions.contains(id));

        self.notified_waiting_sessions = waiting_sessions;
        should_notify
    }

    fn update_selected_from_cursor(&mut self) {
        if let Some(idx) = self.session_at_cursor() {
            self.selected_session = Some(idx);
            self.detail_scroll = 0;
        }
    }
}

// ── Build flat list ───────────────────────────────────────────────────────────

fn build_flat_list(sessions: &[CopilotSession]) -> Vec<usize> {
    let mut flat = Vec::new();

    for idx in 0..sessions.len() {
        flat.push(idx);
    }

    flat
}
