use crate::session::{
    group_sessions, load_sessions, load_turns, session_db_path, CopilotSession, SessionStatus, Turn,
};
use crate::terminal::EmbeddedTerminal;
use chrono::{DateTime, Utc};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

/// How many sessions to show per directory group before a "Load more" item.
const MAX_SESSIONS_PER_GROUP: usize = 5;
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

// ── FlatItem ─────────────────────────────────────────────────────────────────

/// An item in the flat list shown in the sessions panel.
#[derive(Debug, Clone)]
pub enum FlatItem {
    /// A group header showing the cwd path (collapsible).
    GroupHeader(String),
    /// A session entry: index into `App::sessions`.
    SessionEntry(usize),
    /// A "… N more" item at the end of a collapsed group.
    LoadMore {
        group_key: String,
        hidden_count: usize,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum ConversationLine {
    Empty,
    NoHistory,
    UserHeader,
    UserText(String),
    AssistantHeader,
    AssistantText(String),
    Truncated,
    Separator(i64),
}

// ── App ──────────────────────────────────────────────────────────────────────

pub struct App {
    pub sessions: Vec<CopilotSession>,
    /// Flat list of items (headers + entries + load-more) for the left panel.
    pub flat_list: Vec<FlatItem>,
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
    /// Groups whose "Load more" item has been expanded.
    pub expanded_groups: HashSet<String>,
    /// Groups that are collapsed down to only their directory header.
    pub collapsed_groups: HashSet<String>,
    /// When set, only sessions in this directory group are shown.
    pub focused_group: Option<String>,
    /// Cached conversation turns by session id and session update time.
    conversation_cache: HashMap<String, (DateTime<Utc>, Vec<Turn>)>,
    /// Cached conversation preview lines by session id, session update time, and response limit.
    conversation_line_cache: HashMap<String, (DateTime<Utc>, usize, Vec<ConversationLine>)>,
    /// A live copilot session embedded in the right panel, if any.
    pub embedded_terminal: Option<EmbeddedTerminal>,
    /// Whether the embedded terminal is taking the full TUI area.
    pub terminal_fullscreen: bool,
}

impl App {
    pub fn new(copilot_dir: PathBuf, launch_dir: PathBuf) -> Self {
        let sessions = load_sessions(&copilot_dir);
        let expanded_groups = HashSet::new();
        let collapsed_groups = HashSet::new();
        let focused_group = None;
        let flat_list = build_flat_list(
            &sessions,
            &expanded_groups,
            &collapsed_groups,
            focused_group.as_deref(),
        );

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
            new_session_reload_baseline: None,
            expanded_groups,
            collapsed_groups,
            focused_group,
            conversation_cache: HashMap::new(),
            conversation_line_cache: HashMap::new(),
            embedded_terminal: None,
            terminal_fullscreen: false,
        }
    }

    pub fn reload(&mut self) {
        self.replace_sessions(load_sessions(&self.copilot_dir));
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
        self.sessions = sessions;
        self.clear_missing_focused_group();
        self.flat_list = build_flat_list(
            &self.sessions,
            &self.expanded_groups,
            &self.collapsed_groups,
            self.focused_group.as_deref(),
        );

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

    pub fn move_page_up(&mut self) {
        self.cursor = self.cursor.saturating_sub(DETAIL_PAGE_SCROLL_AMOUNT);
        self.update_selected_from_cursor();
    }

    pub fn move_page_down(&mut self) {
        if self.flat_list.is_empty() {
            return;
        }
        self.cursor = (self.cursor + DETAIL_PAGE_SCROLL_AMOUNT).min(self.flat_list.len() - 1);
        self.update_selected_from_cursor();
    }

    /// Select / activate the item currently under the cursor.
    pub fn select_current(&mut self) {
        match self.flat_list.get(self.cursor).cloned() {
            Some(FlatItem::SessionEntry(idx)) => {
                self.selected_session = Some(idx);
                self.active_panel = Panel::Detail;
                self.detail_scroll = 0;
            }
            Some(FlatItem::LoadMore { group_key, .. }) => {
                self.expand_group(&group_key);
            }
            Some(FlatItem::GroupHeader(key)) => {
                if self.collapsed_groups.contains(&key) {
                    self.collapsed_groups.remove(&key);
                    self.rebuild_flat_list_keep_cursor();
                } else {
                    self.toggle_group(&key);
                }
            }
            None => {}
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

    pub fn selected_turns(&mut self) -> Option<&[Turn]> {
        let idx = self.selected_session?;
        let session = self.sessions.get(idx)?;
        let id = session.id.clone();
        let updated_at = session.updated_at;
        let db_path = session_db_path(&self.copilot_dir);
        let is_stale = self
            .conversation_cache
            .get(&id)
            .map(|(cached_at, _)| *cached_at != updated_at)
            .unwrap_or(true);
        if is_stale {
            self.conversation_cache
                .insert(id.clone(), (updated_at, load_turns(&db_path, &id)));
        }
        self.conversation_cache
            .get(&id)
            .map(|(_, turns)| turns.as_slice())
    }

    pub fn selected_conversation_lines(
        &mut self,
        max_response_lines: usize,
    ) -> Option<&[ConversationLine]> {
        let idx = self.selected_session?;
        let session = self.sessions.get(idx)?;
        let id = session.id.clone();
        let updated_at = session.updated_at;

        let is_stale = self
            .conversation_line_cache
            .get(&id)
            .map(|(cached_at, cached_limit, _)| {
                *cached_at != updated_at || *cached_limit != max_response_lines
            })
            .unwrap_or(true);

        if is_stale {
            let lines = conversation_lines_from_turns(self.selected_turns()?, max_response_lines);
            self.conversation_line_cache
                .insert(id.clone(), (updated_at, max_response_lines, lines));
        }

        self.conversation_line_cache
            .get(&id)
            .map(|(_, _, lines)| lines.as_slice())
    }

    // ── Group expand / collapse ───────────────────────────────────────────────

    /// Expand a collapsed group (show all sessions past the initial 5).
    pub fn expand_group(&mut self, key: &str) {
        self.expanded_groups.insert(key.to_string());
        self.rebuild_flat_list_keep_cursor();
    }

    /// Toggle a group between collapsed (5 visible) and fully expanded.
    pub fn toggle_group(&mut self, key: &str) {
        if self.expanded_groups.contains(key) {
            self.expanded_groups.remove(key);
        } else {
            self.expanded_groups.insert(key.to_string());
        }
        self.rebuild_flat_list_keep_cursor();
    }

    pub fn toggle_current_group_collapsed(&mut self) {
        if let Some(key) = self.current_group_key() {
            if self.collapsed_groups.contains(&key) {
                self.collapsed_groups.remove(&key);
            } else {
                self.collapsed_groups.insert(key);
            }
            self.rebuild_flat_list_keep_cursor();
        }
    }

    pub fn toggle_directory_focus(&mut self) {
        if let Some(key) = self.current_group_key() {
            if self.focused_group.as_deref() == Some(key.as_str()) {
                self.focused_group = None;
            } else {
                self.focused_group = Some(key.clone());
                self.collapsed_groups.remove(&key);
            }
            self.rebuild_flat_list_keep_cursor();
        }
    }

    pub fn clear_directory_focus(&mut self) {
        if self.focused_group.is_some() {
            self.focused_group = None;
            self.rebuild_flat_list_keep_cursor();
        }
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
        match self.flat_list.get(self.cursor) {
            Some(FlatItem::SessionEntry(idx)) => Some(*idx),
            _ => None,
        }
    }

    pub fn current_group_key(&self) -> Option<String> {
        match self.flat_list.get(self.cursor) {
            Some(FlatItem::GroupHeader(key)) => Some(key.clone()),
            Some(FlatItem::LoadMore { group_key, .. }) => Some(group_key.clone()),
            Some(FlatItem::SessionEntry(idx)) => Some(self.sessions[*idx].group_key()),
            None => self
                .selected_session
                .map(|idx| self.sessions[idx].group_key())
                .or_else(|| self.focused_group.clone()),
        }
    }

    fn default_new_session_dir(&self) -> PathBuf {
        self.focused_group
            .as_ref()
            .map(PathBuf::from)
            .or_else(|| self.current_group_key().map(PathBuf::from))
            .unwrap_or_else(|| self.launch_dir.clone())
    }

    fn rebuild_flat_list_keep_cursor(&mut self) {
        // Try to remember which session the cursor is on.
        let cursor_id = self
            .session_at_cursor()
            .map(|i| self.sessions[i].id.clone());
        let cursor_group = self.current_group_key();
        self.clear_missing_focused_group();
        self.flat_list = build_flat_list(
            &self.sessions,
            &self.expanded_groups,
            &self.collapsed_groups,
            self.focused_group.as_deref(),
        );
        // Restore cursor to the same session if possible.
        if let Some(id) = cursor_id {
            if let Some(pos) = self.flat_list.iter().position(
                |item| matches!(item, FlatItem::SessionEntry(i) if self.sessions[*i].id == id),
            ) {
                self.cursor = pos;
            } else if let Some(group) = cursor_group.as_deref() {
                self.restore_cursor_to_group(group);
            }
        } else if let Some(group) = cursor_group.as_deref() {
            self.restore_cursor_to_group(group);
        }
        if self.cursor >= self.flat_list.len() {
            self.cursor = self.flat_list.len().saturating_sub(1);
        }
        self.update_selected_from_cursor();
    }

    fn update_selected_from_cursor(&mut self) {
        if let Some(idx) = self.session_at_cursor() {
            self.selected_session = Some(idx);
            self.detail_scroll = 0;
        }
    }

    fn restore_cursor_to_group(&mut self, group: &str) {
        self.cursor = self
            .flat_list
            .iter()
            .position(|item| matches!(item, FlatItem::GroupHeader(key) if key == group))
            .unwrap_or(self.cursor);
    }

    fn clear_missing_focused_group(&mut self) {
        if let Some(ref focused) = self.focused_group {
            if !self.sessions.iter().any(|s| s.group_key() == *focused) {
                self.focused_group = None;
            }
        }
    }
}

fn conversation_lines_from_turns(
    turns: &[Turn],
    max_response_lines: usize,
) -> Vec<ConversationLine> {
    if turns.is_empty() {
        return vec![ConversationLine::NoHistory];
    }

    let mut lines = Vec::new();
    for turn in turns {
        if let Some(ref msg) = turn.user_message {
            lines.push(ConversationLine::UserHeader);
            lines.extend(
                msg.lines()
                    .map(|line| ConversationLine::UserText(line.to_string())),
            );
            lines.push(ConversationLine::Empty);
        }
        if let Some(ref resp) = turn.assistant_response {
            lines.push(ConversationLine::AssistantHeader);
            let mut response_lines = resp.lines();
            for line in response_lines.by_ref().take(max_response_lines) {
                lines.push(ConversationLine::AssistantText(line.to_string()));
            }
            if response_lines.next().is_some() {
                lines.push(ConversationLine::Truncated);
            }
            lines.push(ConversationLine::Empty);
        }
        lines.push(ConversationLine::Separator(turn.turn_index + 1));
        lines.push(ConversationLine::Empty);
    }
    lines
}

// ── Build flat list ───────────────────────────────────────────────────────────

fn build_flat_list(
    sessions: &[CopilotSession],
    expanded: &HashSet<String>,
    collapsed: &HashSet<String>,
    focused: Option<&str>,
) -> Vec<FlatItem> {
    let groups = group_sessions(sessions);
    let mut flat = Vec::new();

    for (key, indices) in groups {
        if focused.is_some_and(|focused| focused != key) {
            continue;
        }

        let is_focused = focused == Some(key.as_str());
        let is_expanded = is_focused || expanded.contains(&key);
        let total = indices.len();
        let visible = if is_expanded {
            total
        } else {
            total.min(MAX_SESSIONS_PER_GROUP)
        };

        flat.push(FlatItem::GroupHeader(key.clone()));
        if collapsed.contains(&key) {
            continue;
        }
        for &idx in &indices[..visible] {
            flat.push(FlatItem::SessionEntry(idx));
        }
        if !is_expanded && total > MAX_SESSIONS_PER_GROUP {
            flat.push(FlatItem::LoadMore {
                group_key: key,
                hidden_count: total - visible, // = total - MAX_SESSIONS_PER_GROUP when collapsed
            });
        }
    }

    flat
}

#[cfg(test)]
mod tests {
    use super::{conversation_lines_from_turns, ConversationLine};
    use crate::session::Turn;

    #[test]
    fn conversation_lines_from_turns_handles_empty_history() {
        assert_eq!(
            conversation_lines_from_turns(&[], 20),
            vec![ConversationLine::NoHistory]
        );
    }

    #[test]
    fn conversation_lines_from_turns_truncates_long_responses() {
        let turns = vec![Turn {
            turn_index: 0,
            user_message: Some("hello".into()),
            assistant_response: Some("one\ntwo\nthree".into()),
            timestamp: String::new(),
        }];

        assert_eq!(
            conversation_lines_from_turns(&turns, 2),
            vec![
                ConversationLine::UserHeader,
                ConversationLine::UserText("hello".into()),
                ConversationLine::Empty,
                ConversationLine::AssistantHeader,
                ConversationLine::AssistantText("one".into()),
                ConversationLine::AssistantText("two".into()),
                ConversationLine::Truncated,
                ConversationLine::Empty,
                ConversationLine::Separator(1),
                ConversationLine::Empty,
            ]
        );
    }
}
