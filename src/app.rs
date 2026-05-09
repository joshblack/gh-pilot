use crate::session::{
    load_remote_task_log, load_sessions, refresh_session_statuses_with_cache, CopilotSession,
    SessionSource, SessionStatus, SessionStatusCache,
};
use crate::terminal::EmbeddedTerminal;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};

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
    /// User is typing text to filter sessions by directory.
    DirectoryFilter,
    /// An embedded copilot session is live in the right panel.
    Terminal,
    /// Shortcut help popup is open.
    Help,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SessionFilter {
    All,
    Running,
    Pending,
    Remote,
}

impl SessionFilter {
    fn next(self) -> Self {
        match self {
            SessionFilter::All => SessionFilter::Running,
            SessionFilter::Running => SessionFilter::Pending,
            SessionFilter::Pending => SessionFilter::Remote,
            SessionFilter::Remote => SessionFilter::All,
        }
    }

    fn previous(self) -> Self {
        match self {
            SessionFilter::All => SessionFilter::Remote,
            SessionFilter::Running => SessionFilter::All,
            SessionFilter::Pending => SessionFilter::Running,
            SessionFilter::Remote => SessionFilter::Pending,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SessionFilterCounts {
    pub all: usize,
    pub running: usize,
    pub pending: usize,
    pub remote: usize,
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
    /// Open a remote agent task in the browser.
    OpenRemoteTask {
        url: String,
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
    /// Directory where pilot was launched (default for new sessions).
    pub launch_dir: PathBuf,
    pub mode: Mode,
    pub input_buffer: String,
    pub session_filter: SessionFilter,
    pub directory_filter: String,
    pub detail_scroll: usize,
    pub help_scroll: usize,
    pub should_quit: bool,
    pub status_message: Option<String>,
    pub pending_action: PendingAction,
    /// Session IDs present before launching a new Copilot session.
    pub new_session_reload_baseline: Option<HashSet<String>>,
    notified_waiting_sessions: HashSet<String>,
    status_cache: SessionStatusCache,
    /// A live copilot session embedded in the right panel, if any.
    pub embedded_terminal: Option<EmbeddedTerminal>,
    /// Whether the embedded terminal is taking the full TUI area.
    pub terminal_fullscreen: bool,
    /// Sends completed remote log loads from background workers to the main loop.
    remote_log_sender: Sender<(String, String)>,
    /// Receives completed remote log loads without blocking the UI.
    remote_log_receiver: Receiver<(String, String)>,
    /// Remote session IDs whose logs are currently loading in the background.
    remote_logs_loading: HashSet<String>,
    /// Sends completed session loads from background workers to the main loop.
    session_load_sender: Sender<(u64, Vec<CopilotSession>)>,
    /// Receives completed session loads without blocking startup or rendering.
    session_load_receiver: Receiver<(u64, Vec<CopilotSession>)>,
    /// Whether a session list load is currently in flight.
    sessions_loading: bool,
    /// Monotonically increasing token used to ignore stale background loads.
    session_load_generation: u64,
}

impl App {
    pub fn new(copilot_dir: PathBuf, launch_dir: PathBuf) -> Self {
        let (remote_log_sender, remote_log_receiver) = mpsc::channel();
        let (session_load_sender, session_load_receiver) = mpsc::channel();

        App {
            sessions: Vec::new(),
            flat_list: Vec::new(),
            cursor: 0,
            selected_session: None,
            active_panel: Panel::Sessions,
            copilot_dir,
            launch_dir,
            mode: Mode::Normal,
            input_buffer: String::new(),
            session_filter: SessionFilter::All,
            directory_filter: String::new(),
            detail_scroll: 0,
            help_scroll: 0,
            should_quit: false,
            status_message: None,
            pending_action: PendingAction::None,
            new_session_reload_baseline: None,
            notified_waiting_sessions: HashSet::new(),
            status_cache: SessionStatusCache::default(),
            embedded_terminal: None,
            terminal_fullscreen: false,
            remote_log_sender,
            remote_log_receiver,
            remote_logs_loading: HashSet::new(),
            session_load_sender,
            session_load_receiver,
            sessions_loading: false,
            session_load_generation: 0,
        }
    }

    pub fn reload(&mut self) {
        self.session_load_generation = self.session_load_generation.wrapping_add(1);
        self.sessions_loading = true;
        let generation = self.session_load_generation;
        let copilot_dir = self.copilot_dir.clone();
        let sender = self.session_load_sender.clone();
        std::thread::spawn(move || {
            let sessions = load_sessions(&copilot_dir);
            drop(sender.send((generation, sessions)));
        });
    }

    pub fn poll_session_loads(&mut self) {
        while let Ok((generation, sessions)) = self.session_load_receiver.try_recv() {
            if generation == self.session_load_generation {
                self.sessions_loading = false;
                self.replace_sessions(sessions);
            }
        }
    }

    pub fn is_loading_sessions(&self) -> bool {
        self.sessions_loading
    }

    pub fn refresh_statuses(&mut self) -> bool {
        refresh_session_statuses_with_cache(
            &self.copilot_dir,
            &mut self.sessions,
            &mut self.status_cache,
        );
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

    fn replace_sessions(&mut self, mut sessions: Vec<CopilotSession>) {
        let cursor_id = self
            .session_at_cursor()
            .map(|i| self.sessions[i].id.clone())
            .or_else(|| self.selected_session.map(|i| self.sessions[i].id.clone()));
        for session in &mut sessions {
            if session.source == SessionSource::Remote {
                session.remote_log = self
                    .sessions
                    .iter()
                    .find(|existing| existing.id == session.id)
                    .and_then(|existing| existing.remote_log.clone());
            }
        }
        self.sessions = sessions;
        let session_ids: HashSet<&str> = self
            .sessions
            .iter()
            .map(|session| session.id.as_str())
            .collect();
        self.notified_waiting_sessions
            .retain(|id| session_ids.contains(id.as_str()));
        self.status_cache.retain_sessions(&session_ids);
        self.flat_list =
            build_flat_list(&self.sessions, self.session_filter, &self.directory_filter);

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
        if self.active_panel == Panel::Detail {
            self.load_selected_remote_preview();
        }
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

        self.session_load_generation = self.session_load_generation.wrapping_add(1);
        self.sessions_loading = false;
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
            self.load_selected_remote_preview();
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

    pub fn open_help(&mut self) {
        self.mode = Mode::Help;
        self.help_scroll = 0;
    }

    pub fn close_help(&mut self) {
        self.mode = Mode::Normal;
        self.help_scroll = 0;
    }

    pub fn scroll_help_up(&mut self) {
        self.help_scroll = self.help_scroll.saturating_sub(1);
    }

    pub fn scroll_help_down(&mut self) {
        self.help_scroll += 1;
    }

    pub fn scroll_help_page_up(&mut self) {
        self.help_scroll = self.help_scroll.saturating_sub(DETAIL_PAGE_SCROLL_AMOUNT);
    }

    pub fn scroll_help_page_down(&mut self) {
        self.help_scroll += DETAIL_PAGE_SCROLL_AMOUNT;
    }

    pub fn next_session_filter(&mut self) {
        self.session_filter = self.session_filter.next();
        self.apply_session_filters();
    }

    pub fn previous_session_filter(&mut self) {
        self.session_filter = self.session_filter.previous();
        self.apply_session_filters();
    }

    pub fn begin_directory_filter(&mut self) {
        self.mode = Mode::DirectoryFilter;
        self.input_buffer = self.directory_filter.clone();
    }

    pub fn confirm_directory_filter(&mut self) {
        self.directory_filter = self.input_buffer.trim().to_string();
        self.input_buffer.clear();
        self.mode = Mode::Normal;
        self.apply_session_filters();
    }

    pub fn clear_directory_filter(&mut self) {
        self.directory_filter.clear();
        self.apply_session_filters();
    }

    pub fn session_filter_counts(&self) -> SessionFilterCounts {
        count_session_filters(&self.sessions, &self.directory_filter)
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
            if self.sessions[idx].source == SessionSource::Remote {
                self.selected_session = Some(idx);
                self.active_panel = Panel::Detail;
                self.load_selected_remote_preview();
                match self.sessions[idx].remote_url.clone() {
                    Some(url) => self.pending_action = PendingAction::OpenRemoteTask { url },
                    None => self.status_message = Some("Remote task URL not available".into()),
                }
                return;
            }
            let id = self.sessions[idx].id.clone();
            let cwd = self.sessions[idx].cwd.clone();
            self.selected_session = Some(idx);
            self.active_panel = Panel::Detail;
            self.pending_action = PendingAction::OpenEmbedded { id, cwd };
        }
    }

    /// Starts loading the selected remote task log in the background if needed.
    fn load_selected_remote_preview(&mut self) {
        let Some(idx) = self.selected_session else {
            return;
        };
        if self.sessions[idx].source != SessionSource::Remote
            || self.sessions[idx].remote_log.is_some()
            || self.remote_logs_loading.contains(&self.sessions[idx].id)
        {
            return;
        }

        let id = self.sessions[idx].id.clone();
        self.remote_logs_loading.insert(id.clone());
        let sender = self.remote_log_sender.clone();
        std::thread::spawn(move || {
            let log = load_remote_task_log(&id);
            drop(sender.send((id, log)));
        });
    }

    pub fn poll_remote_log_loads(&mut self) {
        while let Ok((id, log)) = self.remote_log_receiver.try_recv() {
            self.remote_logs_loading.remove(&id);
            if let Some(session) = self.sessions.iter_mut().find(|session| session.id == id) {
                session.remote_log = Some(log);
            }
        }
    }

    pub fn is_remote_log_loading(&self, id: &str) -> bool {
        self.remote_logs_loading.contains(id)
    }

    /// Detach from the embedded terminal and return to normal mode.
    pub fn detach_terminal(&mut self) {
        self.embedded_terminal = None;
        self.mode = Mode::Normal;
        self.terminal_fullscreen = false;
        self.status_message = Some("Detached; Copilot continues in tmux".into());
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
        } else {
            self.selected_session = None;
        }
    }

    fn apply_session_filters(&mut self) {
        let selected_id = self
            .selected_session
            .map(|idx| self.sessions[idx].id.clone());
        self.flat_list =
            build_flat_list(&self.sessions, self.session_filter, &self.directory_filter);

        self.cursor = selected_id
            .and_then(|id| {
                self.flat_list
                    .iter()
                    .position(|idx| self.sessions[*idx].id == id)
            })
            .unwrap_or(0);
        if self.cursor >= self.flat_list.len() {
            self.cursor = self.flat_list.len().saturating_sub(1);
        }
        self.selected_session = self.session_at_cursor();
        self.detail_scroll = 0;
    }
}

// ── Build flat list ───────────────────────────────────────────────────────────

fn build_flat_list(
    sessions: &[CopilotSession],
    session_filter: SessionFilter,
    directory_filter: &str,
) -> Vec<usize> {
    let mut flat = Vec::new();

    for (idx, session) in sessions.iter().enumerate() {
        if matches_directory_filter(session, directory_filter)
            && matches_session_filter(session, session_filter)
        {
            flat.push(idx);
        }
    }

    flat
}

fn count_session_filters(
    sessions: &[CopilotSession],
    directory_filter: &str,
) -> SessionFilterCounts {
    sessions
        .iter()
        .filter(|session| matches_directory_filter(session, directory_filter))
        .fold(SessionFilterCounts::default(), |mut counts, session| {
            counts.all += 1;
            match &session.status {
                SessionStatus::Running => counts.running += 1,
                SessionStatus::Waiting => counts.pending += 1,
                _ => {}
            }
            if is_remote_session(session) {
                counts.remote += 1;
            }
            counts
        })
}

fn matches_session_filter(session: &CopilotSession, session_filter: SessionFilter) -> bool {
    match session_filter {
        SessionFilter::All => true,
        SessionFilter::Running => session.status == SessionStatus::Running,
        SessionFilter::Pending => session.status == SessionStatus::Waiting,
        SessionFilter::Remote => is_remote_session(session),
    }
}

fn matches_directory_filter(session: &CopilotSession, directory_filter: &str) -> bool {
    let filters = directory_filter_candidates(directory_filter);
    filters.is_empty()
        || filters
            .iter()
            .any(|filter| session_matches_directory_filter(session, filter))
}

fn session_matches_directory_filter(session: &CopilotSession, filter: &str) -> bool {
    path_matches_filter(&session.cwd, filter)
        || session
            .git_root
            .as_ref()
            .is_some_and(|git_root| path_matches_filter(git_root, filter))
        || session
            .repository
            .as_ref()
            .is_some_and(|repository| repository_matches_filter(repository, filter))
}

fn path_matches_filter(path: &Path, filter: &str) -> bool {
    normalize_filter_text(&path.to_string_lossy()).contains(filter)
}

fn repository_matches_filter(repository: &str, filter: &str) -> bool {
    let repository = normalize_filter_text(repository);
    repository.contains(filter) || filter_contains_repository_path(filter, &repository)
}

fn filter_contains_repository_path(filter: &str, repository: &str) -> bool {
    if !repository.contains('/') {
        return false;
    }

    let repository_path = format!("/{repository}");
    filter.ends_with(&repository_path) || filter.contains(&format!("{repository_path}/"))
}

fn directory_filter_candidates(directory_filter: &str) -> Vec<String> {
    let filter = directory_filter.trim();
    if filter.is_empty() {
        return Vec::new();
    }

    let mut candidates = Vec::new();
    let normalized = normalize_filter_text(filter);
    if !normalized.is_empty() {
        candidates.push(normalized);
    }
    if let Some(expanded) = expand_home_directory_filter(filter) {
        let expanded = normalize_filter_text(&expanded.to_string_lossy());
        if !expanded.is_empty() && !candidates.contains(&expanded) {
            candidates.push(expanded);
        }
    }
    candidates
}

fn expand_home_directory_filter(filter: &str) -> Option<PathBuf> {
    if filter == "~" {
        return dirs::home_dir();
    }

    let rest = filter
        .strip_prefix("~/")
        .or_else(|| filter.strip_prefix("~\\"))?;
    dirs::home_dir().map(|home| home.join(rest))
}

fn normalize_filter_text(value: &str) -> String {
    value
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_ascii_lowercase()
}

fn is_remote_session(session: &CopilotSession) -> bool {
    session.source == SessionSource::Remote
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn app_with_sessions(sessions: Vec<CopilotSession>) -> App {
        let (remote_log_sender, remote_log_receiver) = mpsc::channel();
        let (session_load_sender, session_load_receiver) = mpsc::channel();
        let flat_list = build_flat_list(&sessions, SessionFilter::All, "");
        let selected_session = flat_list.first().copied();
        App {
            flat_list,
            cursor: 0,
            selected_session,
            sessions,
            active_panel: Panel::Sessions,
            copilot_dir: PathBuf::from("/tmp/copilot"),
            launch_dir: PathBuf::from("/tmp"),
            mode: Mode::Normal,
            input_buffer: String::new(),
            session_filter: SessionFilter::All,
            directory_filter: String::new(),
            detail_scroll: 0,
            help_scroll: 0,
            should_quit: false,
            status_message: None,
            pending_action: PendingAction::None,
            new_session_reload_baseline: None,
            notified_waiting_sessions: HashSet::new(),
            status_cache: SessionStatusCache::default(),
            embedded_terminal: None,
            terminal_fullscreen: false,
            remote_log_sender,
            remote_log_receiver,
            remote_logs_loading: HashSet::new(),
            session_load_sender,
            session_load_receiver,
            sessions_loading: false,
            session_load_generation: 0,
        }
    }

    fn session(id: &str, source: SessionSource) -> CopilotSession {
        session_with_details(id, source, "/tmp", SessionStatus::Idle, None, None)
    }

    fn session_with_details(
        id: &str,
        source: SessionSource,
        cwd: &str,
        status: SessionStatus,
        git_root: Option<&str>,
        repository: Option<&str>,
    ) -> CopilotSession {
        CopilotSession {
            id: id.to_string(),
            source,
            cwd: PathBuf::from(cwd),
            git_root: git_root.map(PathBuf::from),
            repository: repository.map(ToString::to_string),
            branch: None,
            summary: None,
            last_agent_message: None,
            user_named: false,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            status,
            remote_state: None,
            remote_url: None,
            remote_user: None,
            pull_request: None,
            remote_log: None,
        }
    }

    #[test]
    fn filters_sessions_by_status_and_directory() {
        let sessions = vec![
            session_with_details(
                "running",
                SessionSource::Local,
                "/work/alpha",
                SessionStatus::Running,
                Some("/work/alpha"),
                Some("owner/alpha"),
            ),
            session_with_details(
                "pending",
                SessionSource::Local,
                "/work/beta",
                SessionStatus::Waiting,
                Some("/work/beta"),
                Some("owner/beta"),
            ),
            session_with_details(
                "remote",
                SessionSource::Remote,
                "/remote/alpha",
                SessionStatus::Idle,
                None,
                Some("owner/remote"),
            ),
        ];

        assert_eq!(
            build_flat_list(&sessions, SessionFilter::Running, "alpha"),
            vec![0]
        );
        assert_eq!(
            build_flat_list(&sessions, SessionFilter::Pending, ""),
            vec![1]
        );
        assert_eq!(
            build_flat_list(&sessions, SessionFilter::Remote, "alpha"),
            vec![2]
        );
    }

    #[test]
    fn counts_filters_after_directory_filter() {
        let sessions = vec![
            session_with_details(
                "running",
                SessionSource::Local,
                "/work/alpha",
                SessionStatus::Running,
                None,
                None,
            ),
            session_with_details(
                "pending",
                SessionSource::Local,
                "/work/alpha",
                SessionStatus::Waiting,
                None,
                None,
            ),
            session_with_details(
                "remote",
                SessionSource::Remote,
                "/remote/beta",
                SessionStatus::Idle,
                None,
                Some("owner/beta"),
            ),
        ];

        let counts = count_session_filters(&sessions, "alpha");

        assert_eq!(
            counts,
            SessionFilterCounts {
                all: 2,
                running: 1,
                pending: 1,
                remote: 0,
            }
        );
    }

    #[test]
    fn directory_filter_expands_home_prefix() {
        let home = dirs::home_dir().expect("home directory should be available");
        let cwd = home.join("gh/primer/react");
        let sessions = vec![session_with_details(
            "local",
            SessionSource::Local,
            cwd.to_str().expect("path should be utf-8"),
            SessionStatus::Idle,
            None,
            Some("primer/react"),
        )];

        assert_eq!(
            build_flat_list(&sessions, SessionFilter::All, "~/gh/primer/react"),
            vec![0]
        );
    }

    #[test]
    fn directory_filter_matches_remote_session_repository() {
        let sessions = vec![session_with_details(
            "remote",
            SessionSource::Remote,
            "primer/react",
            SessionStatus::Idle,
            None,
            Some("primer/react"),
        )];

        assert_eq!(
            build_flat_list(
                &sessions,
                SessionFilter::Remote,
                "/Users/octocat/gh/primer/react"
            ),
            vec![0]
        );
    }

    #[test]
    fn directory_filter_does_not_match_partial_repository_path() {
        let sessions = vec![session_with_details(
            "remote",
            SessionSource::Remote,
            "owner/react",
            SessionStatus::Idle,
            None,
            Some("owner/react"),
        )];

        assert!(build_flat_list(&sessions, SessionFilter::Remote, "/tmp/react/src").is_empty());
    }

    #[test]
    fn app_new_starts_with_empty_non_loading_session_list() {
        let app = App::new(PathBuf::from("/tmp/copilot"), PathBuf::from("/tmp"));

        assert!(app.sessions.is_empty());
        assert!(app.flat_list.is_empty());
        assert_eq!(app.selected_session, None);
        assert!(!app.is_loading_sessions());
    }

    #[test]
    fn reload_starts_background_session_load() {
        let mut app = App::new(PathBuf::from("/tmp/copilot"), PathBuf::from("/tmp"));

        app.reload();

        assert!(app.is_loading_sessions());
    }

    #[test]
    fn poll_session_loads_replaces_sessions() {
        let mut app = App::new(PathBuf::from("/tmp/copilot"), PathBuf::from("/tmp"));
        app.sessions_loading = true;
        app.session_load_sender
            .send((
                app.session_load_generation,
                vec![session("local", SessionSource::Local)],
            ))
            .unwrap();

        app.poll_session_loads();

        assert!(!app.is_loading_sessions());
        assert_eq!(app.sessions.len(), 1);
        assert_eq!(app.selected_session, Some(0));
    }

    #[test]
    fn moving_cursor_to_remote_task_does_not_load_log() {
        let mut app = app_with_sessions(vec![
            session("local", SessionSource::Local),
            session("remote", SessionSource::Remote),
        ]);

        app.move_down();

        assert_eq!(app.selected_session, Some(1));
        assert!(!app.is_remote_log_loading("remote"));
        assert!(app.sessions[1].remote_log.is_none());
    }

    #[test]
    fn poll_remote_log_loads_updates_matching_session() {
        let mut app = app_with_sessions(vec![session("remote", SessionSource::Remote)]);
        app.remote_logs_loading.insert("remote".to_string());

        app.remote_log_sender
            .send(("remote".to_string(), "log output".to_string()))
            .unwrap();
        app.poll_remote_log_loads();

        assert!(!app.is_remote_log_loading("remote"));
        assert_eq!(app.sessions[0].remote_log.as_deref(), Some("log output"));
    }

    #[test]
    fn opening_remote_task_uses_task_url() {
        let mut remote = session("remote", SessionSource::Remote);
        remote.remote_url =
            Some("https://github.com/owner/repo/pull/42/agent-sessions/remote".to_string());
        let mut app = app_with_sessions(vec![remote]);

        app.open_session_embedded();

        match app.pending_action {
            PendingAction::OpenRemoteTask { ref url } => {
                assert_eq!(
                    url,
                    "https://github.com/owner/repo/pull/42/agent-sessions/remote"
                );
            }
            _ => panic!("expected remote task to open in browser"),
        }
    }
}
