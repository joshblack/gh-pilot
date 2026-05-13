use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::io::{self, Stdout};
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use crossterm::cursor::{MoveToColumn, MoveUp};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    Clear as TerminalClear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, SetTitle,
    disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use tokio::process::Command;
use tokio::runtime::Runtime;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use crate::status::SessionStatus;
use crate::store::{CachedRemoteSession, SessionStore};
use crate::tmux::{self, LaunchKind, ManagedSession, StatusCache};

const RECENT_PATH_LIMIT: usize = 20;

pub struct AppOptions {
    pub connect: Option<String>,
    pub remote_enabled: bool,
}

pub fn run(current_dir: PathBuf, options: AppOptions) -> Result<()> {
    let mut app = App::new(current_dir, options.remote_enabled)?;

    if let Some(id) = options.connect {
        let session = tmux::create_session(
            &app.current_dir,
            LaunchKind::Connect {
                id,
                remote_enabled: app.remote_enabled,
            },
        )?;
        app.pending_open = Some(session.name);
    }

    let mut terminal = init_terminal()?;
    app.start_remote_load();
    let result = run_loop(&mut terminal, &mut app);
    restore_terminal(&mut terminal)?;
    result
}

struct App {
    current_dir: PathBuf,
    sessions: Vec<ManagedSession>,
    remote_sessions: Vec<RemoteSession>,
    remote_state: RemoteLoadState,
    remote_rx: Option<UnboundedReceiver<RemoteLoadResult>>,
    remote_request_projects: BTreeSet<String>,
    runtime: Runtime,
    selected_row: usize,
    active_filter: SessionFilter,
    collapsed_groups: BTreeSet<String>,
    cache: StatusCache,
    store: SessionStore,
    message: String,
    confirm_close_all: bool,
    input_mode: Option<InputMode>,
    pending_open: Option<String>,
    remote_enabled: bool,
    shortcut_help_open: bool,
    shortcut_help_scroll: u16,
    recent_paths: Vec<PathBuf>,
    removed_projects: BTreeSet<String>,
    project_labels: BTreeMap<String, String>,
}

enum InputMode {
    NewSessionDirectory {
        buffer: String,
        remote_enabled: bool,
        completion_prefix: Option<String>,
        completion_index: usize,
    },
    NewProjectDirectory {
        buffer: String,
        completion_prefix: Option<String>,
        completion_index: usize,
    },
}

impl App {
    fn new(current_dir: PathBuf, remote_enabled: bool) -> Result<Self> {
        let store = SessionStore::open()?;
        let sessions = store.load_local(&current_dir)?;
        let recent_paths = store.load_recent_paths(RECENT_PATH_LIMIT)?;
        let removed_projects = store
            .load_removed_projects()?
            .into_iter()
            .map(|path| path_key(&path))
            .collect::<BTreeSet<_>>();
        let tracked_projects = tracked_project_dirs(
            &current_dir,
            &sessions,
            &[],
            &recent_paths,
            &removed_projects,
        );
        let mut remote_sessions = Vec::new();
        for project_dir in &tracked_projects {
            remote_sessions.extend(
                store
                    .load_remote(project_dir)?
                    .into_iter()
                    .map(RemoteSession::from),
            );
        }
        let project_labels = project_labels(&tracked_projects);
        let remote_state = if remote_sessions.is_empty() {
            RemoteLoadState::NotStarted
        } else {
            RemoteLoadState::Loaded
        };

        Ok(Self {
            current_dir,
            sessions,
            remote_sessions,
            remote_state,
            remote_rx: None,
            remote_request_projects: BTreeSet::new(),
            runtime: Runtime::new().context("failed to initialize async runtime")?,
            selected_row: 0,
            active_filter: SessionFilter::All,
            collapsed_groups: BTreeSet::new(),
            cache: StatusCache::default(),
            store,
            message: String::new(),
            confirm_close_all: false,
            input_mode: None,
            pending_open: None,
            remote_enabled,
            shortcut_help_open: false,
            shortcut_help_scroll: 0,
            recent_paths,
            removed_projects,
            project_labels,
        })
    }

    fn refresh(&mut self) -> Result<()> {
        let sessions = tmux::list_sessions(&self.current_dir, &mut self.cache)?;
        self.store.replace_local(&sessions)?;
        self.sessions = sessions;
        self.refresh_project_labels();
        self.clamp_selection();
        Ok(())
    }

    fn clamp_selection(&mut self) {
        let total_rows = self.tree_rows().len();
        if total_rows == 0 {
            self.selected_row = 0;
        } else if self.selected_row >= total_rows {
            self.selected_row = total_rows - 1;
        }
    }

    fn start_remote_load(&mut self) -> bool {
        if self.remote_rx.is_some() {
            return false;
        }

        let (sender, receiver) = unbounded_channel();
        let project_dirs = self.tracked_project_dirs();
        if project_dirs.is_empty() {
            self.remote_state = RemoteLoadState::Empty;
            return false;
        }
        self.remote_request_projects = project_dirs.iter().map(|path| path_key(path)).collect();
        self.remote_state = RemoteLoadState::Loading;
        self.remote_rx = Some(receiver);

        self.runtime.spawn(async move {
            let result = list_remote_sessions(project_dirs)
                .await
                .map_err(|error| error.to_string());
            let _ = sender.send(result);
        });

        true
    }

    fn poll_remote_load(&mut self) -> Result<()> {
        let Some(receiver) = &mut self.remote_rx else {
            return Ok(());
        };

        let completed = match receiver.try_recv() {
            Ok(result) => Some(result),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => None,
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                Some(Err("remote session loader stopped".to_owned()))
            }
        };

        if let Some(result) = completed {
            self.remote_rx = None;
            let active_project_keys = self
                .tracked_project_dirs()
                .into_iter()
                .map(|path| path_key(&path))
                .collect::<BTreeSet<_>>();
            let request_still_relevant = self
                .remote_request_projects
                .iter()
                .any(|key| active_project_keys.contains(key));
            self.remote_request_projects.clear();
            if !request_still_relevant {
                self.remote_state = if self.remote_sessions.is_empty() {
                    RemoteLoadState::Empty
                } else {
                    RemoteLoadState::Loaded
                };
                self.clamp_selection();
                return Ok(());
            }
            match result {
                Ok(mut sessions) => {
                    sessions.retain(|session| {
                        active_project_keys.contains(&path_key(&session.project_dir))
                    });
                    for project_dir in self.tracked_project_dirs() {
                        let cached = sessions
                            .iter()
                            .filter(|session| same_path(&session.project_dir, &project_dir))
                            .map(RemoteSession::cache_entry)
                            .collect::<Vec<_>>();
                        self.store.replace_remote(&project_dir, &cached)?;
                    }
                    self.remote_sessions = sessions;
                    self.remote_state = if self.remote_sessions.is_empty() {
                        RemoteLoadState::Empty
                    } else {
                        RemoteLoadState::Loaded
                    };
                }
                Err(error) => {
                    self.remote_state = RemoteLoadState::Failed(error);
                }
            }
            self.clamp_selection();
        }

        Ok(())
    }

    fn select_next(&mut self) {
        let total_rows = self.tree_rows().len();
        if total_rows > 0 {
            self.selected_row = (self.selected_row + 1).min(total_rows - 1);
        }
    }

    fn select_previous(&mut self) {
        self.selected_row = self.selected_row.saturating_sub(1);
    }

    fn selected_session(&self) -> Option<&ManagedSession> {
        match self.selected_tree_row() {
            Some(TreeRow::LocalSession { session_index, .. }) => self.sessions.get(session_index),
            _ => None,
        }
    }

    fn selected_remote_session(&self) -> Option<&RemoteSession> {
        match self.selected_tree_row() {
            Some(TreeRow::RemoteSession { remote_index, .. }) => {
                self.remote_sessions.get(remote_index)
            }
            _ => None,
        }
    }

    fn selected_group(&self) -> Option<ProjectGroup> {
        let groups = self.project_groups();
        match self.selected_tree_row() {
            Some(TreeRow::Group { group_index }) => groups.get(group_index).cloned(),
            Some(TreeRow::RemoteGroup { group_index, .. }) => groups.get(group_index).cloned(),
            Some(TreeRow::LocalSession { group_index, .. }) => groups.get(group_index).cloned(),
            Some(TreeRow::RemoteSession { group_index, .. }) => groups.get(group_index).cloned(),
            Some(TreeRow::Placeholder { group_index, .. }) => groups.get(group_index).cloned(),
            None => None,
        }
    }

    fn selected_tree_row(&self) -> Option<TreeRow> {
        self.tree_rows().get(self.selected_row).cloned()
    }

    fn tree_rows(&self) -> Vec<TreeRow> {
        tree_rows(
            &self.project_groups(),
            &self.collapsed_groups,
            self.active_filter,
        )
    }

    fn project_groups(&self) -> Vec<ProjectGroup> {
        project_groups(
            &self.sessions,
            &self.remote_sessions,
            &self.tracked_project_dirs(),
            &self.project_labels,
            self.active_filter,
            &self.remote_state,
            &self.current_dir,
        )
    }

    fn tracked_project_dirs(&self) -> Vec<PathBuf> {
        tracked_project_dirs(
            &self.current_dir,
            &self.sessions,
            &self.remote_sessions,
            &self.recent_paths,
            &self.removed_projects,
        )
    }

    fn refresh_project_labels(&mut self) {
        for project_dir in self.tracked_project_dirs() {
            let key = path_key(&project_dir);
            self.project_labels
                .entry(key)
                .or_insert_with(|| project_label(&project_dir));
        }
    }

    fn cycle_filter(&mut self) {
        self.active_filter = self.active_filter.next();
        self.selected_row = 0;
    }

    fn cycle_filter_previous(&mut self) {
        self.active_filter = self.active_filter.previous();
        self.selected_row = 0;
    }

    fn collapse_selected_group(&mut self) {
        let Some(row) = self.selected_tree_row() else {
            return;
        };
        let groups = self.project_groups();
        let group_index = row.group_index();
        let Some(group) = groups.get(group_index) else {
            return;
        };

        match row {
            TreeRow::LocalSession {
                under_remote: false,
                ..
            }
            | TreeRow::RemoteSession {
                under_remote: false,
                ..
            }
            | TreeRow::Placeholder {
                under_remote: false,
                ..
            } => {
                self.select_group_row(group_index);
            }
            TreeRow::LocalSession {
                under_remote: true, ..
            }
            | TreeRow::RemoteSession {
                under_remote: true, ..
            }
            | TreeRow::Placeholder {
                under_remote: true, ..
            } => {
                self.select_remote_group_row(group_index);
            }
            TreeRow::RemoteGroup { .. }
                if !self
                    .collapsed_groups
                    .contains(&remote_group_key(&group.key)) =>
            {
                self.collapsed_groups.insert(remote_group_key(&group.key));
                self.select_remote_group_row(group_index);
            }
            _ => {
                self.collapsed_groups.insert(group.key.clone());
                self.select_group_row(group_index);
            }
        }
    }

    fn expand_selected_group(&mut self) {
        let Some(row) = self.selected_tree_row() else {
            return;
        };
        let groups = self.project_groups();
        let group_index = row.group_index();
        let Some(group) = groups.get(group_index) else {
            return;
        };

        match row {
            TreeRow::RemoteGroup { .. }
            | TreeRow::LocalSession {
                under_remote: true, ..
            }
            | TreeRow::RemoteSession {
                under_remote: true, ..
            } => {
                self.collapsed_groups.remove(&remote_group_key(&group.key));
                self.select_remote_group_row(group_index);
            }
            _ => {
                self.collapsed_groups.remove(&group.key);
                self.select_group_row(group_index);
            }
        }
    }

    fn select_group_row(&mut self, group_index: usize) {
        if let Some(row_index) = self.tree_rows().iter().position(
            |row| matches!(row, TreeRow::Group { group_index: index } if *index == group_index),
        ) {
            self.selected_row = row_index;
        }
    }

    fn select_remote_group_row(&mut self, group_index: usize) {
        if let Some(row_index) = self.tree_rows().iter().position(
            |row| matches!(row, TreeRow::RemoteGroup { group_index: index, .. } if *index == group_index),
        ) {
            self.selected_row = row_index;
        }
    }

    fn open_selected(&mut self) {
        if self.toggle_selected_parent() {
            return;
        }

        if let Some(session) = self.selected_session() {
            self.pending_open = Some(session.name.clone());
        } else if let Some(remote) = self.selected_remote_session() {
            match tmux::create_session(
                &self.current_dir,
                LaunchKind::Connect {
                    id: remote.id.clone(),
                    remote_enabled: false,
                },
            ) {
                Ok(session) => {
                    self.message = format!("opened remote {}", remote.display_name);
                    self.pending_open = Some(session.name);
                    if let Err(error) = self.refresh() {
                        self.message = error.to_string();
                    }
                }
                Err(error) => {
                    self.message = error.to_string();
                }
            }
        } else {
            self.message = "select a session to open".to_owned();
        }
    }

    fn toggle_selected_parent(&mut self) -> bool {
        let Some(row) = self.selected_tree_row() else {
            return false;
        };
        let groups = self.project_groups();
        let group_index = row.group_index();
        let Some(group) = groups.get(group_index) else {
            return false;
        };

        match row {
            TreeRow::Group { .. } if !group.entries.is_empty() => {
                if self.collapsed_groups.contains(&group.key) {
                    self.collapsed_groups.remove(&group.key);
                } else {
                    self.collapsed_groups.insert(group.key.clone());
                }
                self.select_group_row(group_index);
                true
            }
            TreeRow::RemoteGroup { .. } if !group.remote_entries().is_empty() => {
                let key = remote_group_key(&group.key);
                if self.collapsed_groups.contains(&key) {
                    self.collapsed_groups.remove(&key);
                } else {
                    self.collapsed_groups.insert(key);
                }
                self.select_remote_group_row(group_index);
                true
            }
            _ => false,
        }
    }

    fn new_session(&mut self) -> Result<()> {
        let project_dir = self
            .selected_group()
            .map(|group| group.path)
            .unwrap_or_else(|| self.current_dir.clone());
        self.new_session_in_dir(project_dir, self.new_session_remote_enabled())
    }

    fn new_session_in_dir(&mut self, project_dir: PathBuf, remote_enabled: bool) -> Result<()> {
        let project_dir = normalize_project_dir(&self.current_dir, &project_dir)?;
        self.create_session_in_dir(project_dir, remote_enabled)
    }

    fn create_session_in_dir(&mut self, project_dir: PathBuf, remote_enabled: bool) -> Result<()> {
        self.store.restore_project(&project_dir)?;
        self.removed_projects.remove(&path_key(&project_dir));
        let session = tmux::create_session(&project_dir, LaunchKind::Local { remote_enabled })?;
        self.message = if remote_enabled {
            format!("created remote {}", session.display_name)
        } else {
            format!("created {}", session.display_name)
        };
        self.pending_open = Some(session.name);
        self.refresh()
    }

    fn record_recent_path(&mut self, project_dir: &Path) -> Result<()> {
        self.store
            .record_recent_path(project_dir, RECENT_PATH_LIMIT)?;
        self.recent_paths = self.store.load_recent_paths(RECENT_PATH_LIMIT)?;
        self.removed_projects.remove(&path_key(project_dir));
        self.refresh_project_labels();
        Ok(())
    }

    fn add_project(&mut self, project_dir: PathBuf) -> Result<()> {
        let project_dir = normalize_project_dir(&self.current_dir, &project_dir)?;
        self.record_recent_path(&project_dir)?;
        let label = self
            .project_labels
            .get(&path_key(&project_dir))
            .cloned()
            .unwrap_or_else(|| project_label(&project_dir));
        self.message = format!("added project {label}");
        self.clamp_selection();
        Ok(())
    }

    fn prompt_new_session_dir(&mut self, remote_enabled: bool) {
        self.input_mode = Some(InputMode::NewSessionDirectory {
            buffer: String::new(),
            remote_enabled,
            completion_prefix: None,
            completion_index: 0,
        });
        self.message = if remote_enabled {
            "enter repo path for remote session".to_owned()
        } else {
            "enter repo path for local session".to_owned()
        };
    }

    fn prompt_new_project_dir(&mut self) {
        self.input_mode = Some(InputMode::NewProjectDirectory {
            buffer: String::new(),
            completion_prefix: None,
            completion_index: 0,
        });
        self.message = "enter project repo path".to_owned();
    }

    fn handle_input_key(&mut self, code: KeyCode) -> Result<bool> {
        let recent_paths = self.recent_paths.clone();
        let mut create_session = None;
        let mut add_project = None;
        let Some(mode) = &mut self.input_mode else {
            return Ok(false);
        };

        match mode {
            InputMode::NewSessionDirectory {
                buffer,
                remote_enabled,
                completion_prefix,
                completion_index,
            } => match code {
                KeyCode::Esc => {
                    self.input_mode = None;
                    self.message = "new repo session cancelled".to_owned();
                }
                KeyCode::Enter => {
                    let path = PathBuf::from(buffer.trim());
                    let remote_enabled = *remote_enabled;
                    self.input_mode = None;
                    create_session = Some((path, remote_enabled));
                }
                KeyCode::Backspace => {
                    buffer.pop();
                    *completion_prefix = None;
                    *completion_index = 0;
                }
                KeyCode::Tab | KeyCode::BackTab => {
                    complete_path_input(
                        buffer,
                        completion_prefix,
                        completion_index,
                        &recent_paths,
                        matches!(code, KeyCode::BackTab),
                    );
                }
                KeyCode::Char(ch) => {
                    buffer.push(ch);
                    *completion_prefix = None;
                    *completion_index = 0;
                }
                _ => {}
            },
            InputMode::NewProjectDirectory {
                buffer,
                completion_prefix,
                completion_index,
            } => match code {
                KeyCode::Esc => {
                    self.input_mode = None;
                    self.message = "new project cancelled".to_owned();
                }
                KeyCode::Enter => {
                    let path = PathBuf::from(buffer.trim());
                    self.input_mode = None;
                    add_project = Some(path);
                }
                KeyCode::Backspace => {
                    buffer.pop();
                    *completion_prefix = None;
                    *completion_index = 0;
                }
                KeyCode::Tab | KeyCode::BackTab => {
                    complete_path_input(
                        buffer,
                        completion_prefix,
                        completion_index,
                        &recent_paths,
                        matches!(code, KeyCode::BackTab),
                    );
                }
                KeyCode::Char(ch) => {
                    buffer.push(ch);
                    *completion_prefix = None;
                    *completion_index = 0;
                }
                _ => {}
            },
        }

        if let Some((path, remote_enabled)) = create_session {
            let project_dir = normalize_project_dir(&self.current_dir, &path)?;
            self.record_recent_path(&project_dir)?;
            self.input_mode = None;
            self.create_session_in_dir(project_dir, remote_enabled)?;
            self.start_remote_load();
        }
        if let Some(path) = add_project {
            self.add_project(path)?;
            self.start_remote_load();
        }

        Ok(true)
    }

    fn new_session_remote_enabled(&self) -> bool {
        self.remote_enabled
            || self
                .selected_tree_row()
                .is_some_and(|row| row.is_remote_context())
    }

    fn close_selected(&mut self) -> Result<()> {
        let Some(session) = self.selected_session() else {
            if self.selected_remote_session().is_some() {
                self.message = "remote sessions cannot be closed locally".to_owned();
            } else {
                self.message = "select a session to close".to_owned();
            }
            return Ok(());
        };
        let name = session.name.clone();
        let display_name = session.display_name.clone();
        tmux::kill_session(&name)?;
        self.cache.remove(&name);
        self.message = format!("closed {display_name}");
        self.refresh()
    }

    fn remove_selected_project(&mut self) -> Result<()> {
        let Some(group) = self.selected_group() else {
            self.message = "select a project to remove".to_owned();
            return Ok(());
        };

        let session_names = self
            .sessions
            .iter()
            .filter(|session| same_path(&session.project_dir, &group.path))
            .map(|session| session.name.clone())
            .collect::<Vec<_>>();
        tmux::kill_sessions(session_names.iter().map(String::as_str))?;
        for name in &session_names {
            self.cache.remove(name);
        }

        self.store.remove_project(&group.path)?;
        self.recent_paths = self.store.load_recent_paths(RECENT_PATH_LIMIT)?;
        self.removed_projects.insert(path_key(&group.path));
        self.remote_sessions
            .retain(|session| !same_path(&session.project_dir, &group.path));
        self.project_labels.remove(&path_key(&group.path));
        self.collapsed_groups.remove(&group.key);
        self.collapsed_groups.remove(&remote_group_key(&group.key));
        self.message = if session_names.is_empty() {
            format!("removed {}", group.display_name)
        } else {
            format!(
                "removed {} and closed {} session(s)",
                group.display_name,
                session_names.len()
            )
        };
        self.refresh()
    }

    fn open_selected_remote_url(&mut self) {
        let Some(remote) = self.selected_remote_session() else {
            self.message = "select a remote session to open in the browser".to_owned();
            return;
        };
        let id = remote.id.clone();
        let display_name = remote.display_name.clone();
        let url = remote.url.clone();

        let result = if let Some(url) = &url {
            open_url(url)
        } else {
            StdCommand::new("gh")
                .args(["agent-task", "view", &id, "--web"])
                .current_dir(&self.current_dir)
                .status()
                .with_context(|| format!("failed to open remote {}", id))
        };

        match result {
            Ok(status) if status.success() => {
                self.message = format!("opened remote {}", display_name);
            }
            Ok(status) => {
                self.message = format!("remote browser opener exited with {status}");
            }
            Err(error) => {
                self.message = error.to_string();
            }
        }
    }

    fn open_selected_remote_pr(&mut self) {
        let Some(remote) = self.selected_remote_session() else {
            self.message = "select a remote session to open its pull request".to_owned();
            return;
        };
        let Some(url) = remote.pr_url.clone() else {
            self.message = format!("{} has no pull request", remote.display_name);
            return;
        };
        let display_name = remote.display_name.clone();

        match open_url(&url) {
            Ok(status) if status.success() => {
                self.message = format!("opened pull request for {display_name}");
            }
            Ok(status) => {
                self.message = format!("pull request opener exited with {status}");
            }
            Err(error) => {
                self.message = error.to_string();
            }
        }
    }

    fn close_all(&mut self) -> Result<()> {
        let names = self
            .sessions
            .iter()
            .map(|session| session.name.as_str())
            .collect::<Vec<_>>();
        let count = names.len();
        tmux::kill_sessions(names)?;
        self.cache.clear();
        self.confirm_close_all = false;
        self.message = format!("closed {count} managed session(s)");
        self.refresh()
    }

    fn open_shortcut_help(&mut self) {
        self.shortcut_help_open = true;
        self.shortcut_help_scroll = 0;
    }

    fn close_shortcut_help(&mut self) {
        self.shortcut_help_open = false;
        self.shortcut_help_scroll = 0;
    }

    fn scroll_shortcut_help_up(&mut self, amount: u16) {
        self.shortcut_help_scroll = self.shortcut_help_scroll.saturating_sub(amount);
    }

    fn scroll_shortcut_help_down(&mut self, amount: u16) {
        self.shortcut_help_scroll = self
            .shortcut_help_scroll
            .saturating_add(amount)
            .min(shortcut_help_lines().len().saturating_sub(1) as u16);
    }

    fn clamp_shortcut_help_scroll(&mut self, visible_lines: u16) {
        let max_scroll = (shortcut_help_lines().len() as u16).saturating_sub(visible_lines);
        self.shortcut_help_scroll = self.shortcut_help_scroll.min(max_scroll);
    }

    fn handle_shortcut_help_key(&mut self, code: KeyCode) -> bool {
        if !self.shortcut_help_open {
            return false;
        }

        match code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') => self.close_shortcut_help(),
            KeyCode::Up | KeyCode::Char('k') => self.scroll_shortcut_help_up(1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_shortcut_help_down(1),
            KeyCode::PageUp => self.scroll_shortcut_help_up(6),
            KeyCode::PageDown => self.scroll_shortcut_help_down(6),
            KeyCode::Home => self.shortcut_help_scroll = 0,
            KeyCode::End => self.scroll_shortcut_help_down(u16::MAX),
            _ => {}
        }
        true
    }

    fn handle_shortcut_help_mouse(&mut self, kind: MouseEventKind) -> bool {
        if !self.shortcut_help_open {
            return false;
        }

        match kind {
            MouseEventKind::ScrollUp => self.scroll_shortcut_help_up(3),
            MouseEventKind::ScrollDown => self.scroll_shortcut_help_down(3),
            _ => {}
        }
        true
    }
}

fn run_loop(terminal: &mut Terminal<CrosstermBackend<Stdout>>, app: &mut App) -> Result<()> {
    let mut last_refresh = UNIX_EPOCH;
    let mut last_remote_refresh = SystemTime::now();

    loop {
        terminal.draw(|frame| render(frame, app))?;

        if let Some(name) = app.pending_open.take() {
            suspend_terminal(terminal)?;
            let attach_result = tmux::attach_session(&name);
            resume_terminal(terminal)?;

            if let Err(error) = attach_result {
                app.message = error.to_string();
            } else {
                if let Err(error) = tmux::mark_seen(&name) {
                    app.message = format!("returned from tmux; could not mark seen: {error}");
                } else {
                    app.message = "returned from tmux".to_owned();
                }
            }
            app.refresh()?;
            last_refresh = SystemTime::now();
            continue;
        }

        if event::poll(Duration::from_millis(UI_TICK_MS))? {
            let key = match event::read()? {
                Event::Key(key) => key,
                Event::Mouse(mouse) => {
                    app.handle_shortcut_help_mouse(mouse.kind);
                    continue;
                }
                _ => continue,
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }

            if app.handle_input_key(key.code)? {
                continue;
            }

            if app.handle_shortcut_help_key(key.code) {
                continue;
            }

            if key.code == KeyCode::Char('?') {
                app.open_shortcut_help();
                continue;
            }

            if app.confirm_close_all {
                match key.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') => app.close_all()?,
                    KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                        app.confirm_close_all = false;
                        app.message = "close all cancelled".to_owned();
                    }
                    _ => {}
                }
                continue;
            }

            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => break,
                KeyCode::BackTab => app.cycle_filter_previous(),
                KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => {
                    app.cycle_filter_previous();
                }
                KeyCode::Tab => app.cycle_filter(),
                KeyCode::Left => app.collapse_selected_group(),
                KeyCode::Right => app.expand_selected_group(),
                KeyCode::Down | KeyCode::Char('j') => app.select_next(),
                KeyCode::Up | KeyCode::Char('k') => app.select_previous(),
                KeyCode::Enter | KeyCode::Char('o') => app.open_selected(),
                KeyCode::Char('n') => app.new_session()?,
                KeyCode::Char('N') => app.prompt_new_session_dir(false),
                KeyCode::Char('P') => app.prompt_new_project_dir(),
                KeyCode::Char('d') => app.remove_selected_project()?,
                KeyCode::Char('x') => app.close_selected()?,
                KeyCode::Char('X') => {
                    if app.sessions.is_empty() {
                        app.message = "no managed sessions to close".to_owned();
                    } else {
                        app.confirm_close_all = true;
                    }
                }
                KeyCode::Char('r') => {
                    app.refresh()?;
                    if app.start_remote_load() {
                        last_remote_refresh = SystemTime::now();
                        app.message = "refreshing".to_owned();
                    } else {
                        app.message = "refresh already running".to_owned();
                    }
                }
                KeyCode::Char('w') => app.open_selected_remote_url(),
                KeyCode::Char('p') => app.open_selected_remote_pr(),
                _ => {}
            }
            continue;
        }

        app.poll_remote_load()?;

        if last_refresh.elapsed().unwrap_or_default() >= Duration::from_secs(2) {
            app.refresh()?;
            last_refresh = SystemTime::now();
        }

        if last_remote_refresh.elapsed().unwrap_or_default() >= Duration::from_secs(60)
            && app.start_remote_load()
        {
            last_remote_refresh = SystemTime::now();
        }
    }

    Ok(())
}

fn render(frame: &mut ratatui::Frame<'_>, app: &mut App) {
    frame.render_widget(
        Block::default().style(Style::default().bg(TOKYO_BG)),
        frame.area(),
    );

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(5),
            Constraint::Length(2),
        ])
        .split(frame.area());

    let groups = app.project_groups();
    let rows = tree_rows(&groups, &app.collapsed_groups, app.active_filter);
    let counts = StatusCounts::from_sources(&app.sessions, &app.remote_sessions);

    let header = Paragraph::new(header_lines(app, counts))
        .style(Style::default().fg(TOKYO_FG))
        .block(chrome_block());
    frame.render_widget(header, chunks[0]);

    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(49), Constraint::Percentage(51)])
        .split(chunks[1]);

    render_sessions(frame, app, &groups, &rows, body[0]);
    render_preview(frame, app, body[1]);

    let footer = Paragraph::new(footer_text(app, chunks[2].width as usize))
        .style(
            Style::default()
                .fg(TOKYO_COMMENT)
                .add_modifier(Modifier::ITALIC),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(footer, chunks[2]);

    if app.shortcut_help_open {
        render_shortcut_help(frame, app);
    }
}

fn footer_text(app: &App, width: usize) -> String {
    if let Some(mode) = &app.input_mode {
        let label = match mode {
            InputMode::NewSessionDirectory { remote_enabled, .. } => {
                if *remote_enabled {
                    "Start remote session in repo"
                } else {
                    "Start local session in repo"
                }
            }
            InputMode::NewProjectDirectory { .. } => "Add project repo",
        };
        let buffer = match mode {
            InputMode::NewSessionDirectory { buffer, .. }
            | InputMode::NewProjectDirectory { buffer, .. } => buffer,
        };
        return truncate(
            &format!("{label}: {buffer}█  • Tab complete • Enter create • Esc cancel"),
            width,
        );
    }

    if app.confirm_close_all {
        return footer_line(
            Some("Close all managed sessions? y confirm • n/Esc cancel"),
            width,
            &["? help"],
        );
    }

    footer_line(
        (!app.message.is_empty()).then_some(app.message.as_str()),
        width,
        &footer_shortcuts(app),
    )
}

fn footer_shortcuts(app: &App) -> Vec<&'static str> {
    let mut shortcuts = vec![
        "? help",
        "⇥/⇤ filters",
        "n new",
        "N new local",
        "P new project",
        "r refresh",
    ];
    let mut has_project = false;

    match app.selected_tree_row() {
        Some(TreeRow::Group { .. }) => {
            has_project = true;
            shortcuts.push("←/→ tree");
            shortcuts.push("Enter toggle");
        }
        Some(TreeRow::RemoteGroup { .. }) => {
            has_project = true;
            shortcuts.push("←/→ tree");
            shortcuts.push("Enter toggle");
        }
        Some(TreeRow::LocalSession { .. }) => {
            has_project = true;
            shortcuts.push("Enter open");
            shortcuts.push("x close");
        }
        Some(TreeRow::RemoteSession { remote_index, .. }) => {
            has_project = true;
            shortcuts.push("Enter open");
            shortcuts.push("w web");
            if app
                .remote_sessions
                .get(remote_index)
                .is_some_and(|session| session.pr_url.is_some())
            {
                shortcuts.push("p PR");
            }
        }
        Some(TreeRow::Placeholder { .. }) => {
            has_project = true;
        }
        None => {}
    }

    if has_project {
        shortcuts.push("d remove project");
    }
    if !app.sessions.is_empty() {
        shortcuts.push("X close all");
    }
    shortcuts.push("q quit");
    shortcuts
}

fn footer_line(prefix: Option<&str>, width: usize, shortcuts: &[&str]) -> String {
    if width == 0 {
        return String::new();
    }

    let mut output = String::new();
    if let Some(prefix) = prefix {
        output = fit_footer_prefix(prefix, width, shortcuts.first().copied());
    }

    for shortcut in shortcuts {
        if append_footer_part(&mut output, shortcut, width) {
            continue;
        }
        if output.is_empty() {
            return truncate(shortcut, width);
        }
        break;
    }

    output
}

fn fit_footer_prefix(prefix: &str, width: usize, first_shortcut: Option<&str>) -> String {
    let Some(shortcut) = first_shortcut else {
        return truncate(prefix, width);
    };

    let reserved = shortcut.chars().count() + " • ".chars().count();
    if width > reserved + 3 {
        truncate(prefix, width - reserved)
    } else {
        truncate(prefix, width)
    }
}

fn append_footer_part(output: &mut String, part: &str, width: usize) -> bool {
    let separator = if output.is_empty() { "" } else { " • " };
    let next_width = output.chars().count() + separator.chars().count() + part.chars().count();
    if next_width > width {
        return false;
    }

    output.push_str(separator);
    output.push_str(part);
    true
}

fn render_shortcut_help(frame: &mut ratatui::Frame<'_>, app: &mut App) {
    let lines = shortcut_help_lines();
    let area = shortcut_help_area(frame.area(), lines.len() as u16);
    app.clamp_shortcut_help_scroll(area.height.saturating_sub(2));

    let block = Block::default()
        .title(Span::styled(
            " SHORTCUTS ",
            Style::default().fg(TOKYO_CYAN).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(TOKYO_BORDER))
        .style(Style::default().bg(TOKYO_BG).fg(TOKYO_FG));

    let help = Paragraph::new(lines)
        .block(block)
        .scroll((app.shortcut_help_scroll, 0))
        .wrap(Wrap { trim: false });

    frame.render_widget(Clear, area);
    frame.render_widget(help, area);
}

fn shortcut_help_area(area: Rect, content_lines: u16) -> Rect {
    let width = area.width.saturating_sub(4).clamp(1, 78);
    let height = content_lines
        .saturating_add(2)
        .min(18)
        .min(area.height.saturating_sub(2).max(1));

    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

fn shortcut_help_lines() -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(Span::styled(
            "? / Esc / q close   ↑↓ or j/k scroll   PgUp/PgDn page   mouse wheel scroll",
            Style::default().fg(TOKYO_COMMENT),
        )),
        Line::raw(""),
        shortcut_help_section("Navigation"),
        shortcut_help_item("↑/↓ or j/k", "select previous/next row"),
        shortcut_help_item("←/→", "collapse or expand the selected project tree"),
        shortcut_help_item("Tab / Shift+Tab", "cycle status filters"),
        shortcut_help_item(
            "Enter or o",
            "open a session, or toggle a project/remote group",
        ),
        Line::raw(""),
        shortcut_help_section("Sessions"),
        shortcut_help_item("n", "start a Copilot session in the selected project"),
        shortcut_help_item(
            "N",
            "start a local Copilot session in another repo/directory",
        ),
        shortcut_help_item(
            "P",
            "add another repo/directory as a project without starting a session",
        ),
        shortcut_help_item(
            "Tab / Shift+Tab",
            "complete or cycle recent repo paths in the prompt",
        ),
        shortcut_help_item(
            "d",
            "remove the selected project and close its managed local sessions",
        ),
        shortcut_help_item("x", "close the selected local managed session"),
        shortcut_help_item("X", "close all local managed sessions after confirmation"),
        shortcut_help_item("w", "open the selected remote session in the browser"),
        shortcut_help_item(
            "p",
            "open the selected remote session's pull request when present",
        ),
        shortcut_help_item("r", "refresh local sessions and remote tasks"),
        Line::raw(""),
        shortcut_help_section("Prompts"),
        shortcut_help_item(
            "Enter",
            "create the session or project while entering a repo path",
        ),
        shortcut_help_item(
            "Esc",
            "cancel a prompt, confirmation, or this shortcut list",
        ),
        shortcut_help_item("y / n", "confirm or cancel close-all"),
        shortcut_help_item("q", "quit gh-pilot when the shortcut list is closed"),
    ];

    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "Remote rows open locally through copilot --resume=<id>.",
        Style::default().fg(TOKYO_COMMENT),
    )));
    lines
}

fn shortcut_help_section(label: &'static str) -> Line<'static> {
    Line::from(Span::styled(
        label,
        Style::default()
            .fg(TOKYO_PURPLE)
            .add_modifier(Modifier::BOLD),
    ))
}

fn shortcut_help_item(keys: &'static str, description: &'static str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{keys:<16}"), Style::default().fg(TOKYO_BLUE)),
        Span::styled(description, Style::default().fg(TOKYO_FG)),
    ])
}

fn render_sessions(
    frame: &mut ratatui::Frame<'_>,
    app: &App,
    groups: &[ProjectGroup],
    rows: &[TreeRow],
    area: ratatui::layout::Rect,
) {
    let block = section_block("SESSIONS");

    if rows.is_empty() {
        let empty = Paragraph::new(format!(
            "No {} sessions.\n\nPress Tab to change filters or n to start Copilot.",
            app.active_filter.label().to_lowercase()
        ))
        .style(Style::default().fg(TOKYO_COMMENT))
        .block(block)
        .wrap(Wrap { trim: true });
        frame.render_widget(empty, area);
        return;
    };

    let items = rows
        .iter()
        .map(|row| {
            let content_width = area.width.saturating_sub(2) as usize;
            let loading_frame = loading_frame();
            tree_item(
                row,
                groups,
                &app.sessions,
                &app.remote_sessions,
                &app.collapsed_groups,
                content_width,
                loading_frame,
            )
        })
        .collect::<Vec<_>>();

    let mut state = ListState::default();
    state.select(Some(app.selected_row));

    let list = List::new(items)
        .block(block)
        .highlight_symbol(Span::styled("▌ ", Style::default().fg(TOKYO_BLUE)))
        .highlight_style(Style::default());
    frame.render_stateful_widget(list, area, &mut state);
}

fn render_preview(frame: &mut ratatui::Frame<'_>, app: &App, area: ratatui::layout::Rect) {
    let selected_session = app.selected_session();
    let selected_remote = app.selected_remote_session();
    let selected_group = app.selected_group();

    let lines = match (selected_session, selected_remote, selected_group) {
        (Some(session), _, Some(group)) => session_preview(session, &group),
        (_, Some(remote), Some(group)) => remote_preview(remote, &group),
        (None, None, Some(group)) => group_preview(&group, &app.sessions, &app.remote_sessions),
        _ => vec![Line::from(Span::styled(
            "Select a project or session.",
            Style::default().fg(TOKYO_COMMENT),
        ))],
    };

    let preview = Paragraph::new(lines)
        .style(Style::default().fg(TOKYO_FG))
        .block(section_block("PREVIEW").borders(Borders::LEFT | Borders::TOP))
        .wrap(Wrap { trim: false });
    frame.render_widget(preview, area);
}

fn init_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode().context("failed to enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        SetTitle("gh pilot"),
        EnterAlternateScreen,
        EnableMouseCapture
    )
    .context("failed to enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend).context("failed to initialize terminal")
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode().context("failed to disable raw mode")?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )
    .context("failed to leave alternate screen")?;
    terminal.show_cursor().context("failed to show cursor")
}

fn suspend_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    terminal.show_cursor().context("failed to show cursor")?;
    disable_raw_mode().context("failed to disable raw mode")?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )
    .context("failed to leave alternate screen")
}

fn resume_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    enable_raw_mode().context("failed to enable raw mode")?;
    execute!(
        terminal.backend_mut(),
        MoveToColumn(0),
        TerminalClear(ClearType::CurrentLine),
        MoveUp(1),
        MoveToColumn(0),
        TerminalClear(ClearType::CurrentLine),
        SetTitle("gh pilot"),
        EnterAlternateScreen,
        EnableMouseCapture
    )
    .context("failed to enter alternate screen")?;
    terminal.clear().context("failed to clear terminal")
}

#[derive(Debug, Clone)]
struct ProjectGroup {
    key: String,
    display_name: String,
    path: PathBuf,
    is_current: bool,
    entries: Vec<GroupEntry>,
    counts: StatusCounts,
    has_bell: bool,
    remote_loading: bool,
    last_activity: Option<u128>,
}

impl ProjectGroup {
    fn record_activity(&mut self, activity: Option<u128>) {
        self.last_activity = self.last_activity.max(activity);
    }

    fn local_entries(&self) -> Vec<GroupEntry> {
        self.entries
            .iter()
            .filter(|entry| {
                matches!(
                    entry,
                    GroupEntry::Local(_) | GroupEntry::Placeholder(PlaceholderKind::LocalEmpty)
                )
            })
            .cloned()
            .collect()
    }

    fn remote_entries(&self) -> Vec<GroupEntry> {
        self.entries
            .iter()
            .filter(|entry| {
                matches!(
                    entry,
                    GroupEntry::Remote(_)
                        | GroupEntry::RemoteLocal(_)
                        | GroupEntry::Placeholder(PlaceholderKind::RemoteLoading)
                        | GroupEntry::Placeholder(PlaceholderKind::RemoteEmpty)
                        | GroupEntry::Placeholder(PlaceholderKind::RemoteError(_))
                )
            })
            .cloned()
            .collect()
    }
}

#[derive(Debug, Clone)]
enum TreeRow {
    Group {
        group_index: usize,
    },
    RemoteGroup {
        group_index: usize,
        is_last: bool,
    },
    LocalSession {
        group_index: usize,
        session_index: usize,
        is_last: bool,
        is_remote: bool,
        under_remote: bool,
    },
    RemoteSession {
        group_index: usize,
        remote_index: usize,
        is_last: bool,
        under_remote: bool,
    },
    Placeholder {
        group_index: usize,
        kind: PlaceholderKind,
        is_last: bool,
        under_remote: bool,
    },
}

impl TreeRow {
    fn group_index(&self) -> usize {
        match self {
            Self::Group { group_index }
            | Self::RemoteGroup { group_index, .. }
            | Self::LocalSession { group_index, .. }
            | Self::RemoteSession { group_index, .. }
            | Self::Placeholder { group_index, .. } => *group_index,
        }
    }

    fn is_remote_context(&self) -> bool {
        match self {
            Self::RemoteGroup { .. } | Self::RemoteSession { .. } => true,
            Self::LocalSession {
                is_remote,
                under_remote,
                ..
            } => *is_remote || *under_remote,
            Self::Placeholder {
                kind, under_remote, ..
            } => {
                *under_remote
                    || matches!(
                        kind,
                        PlaceholderKind::RemoteLoading
                            | PlaceholderKind::RemoteEmpty
                            | PlaceholderKind::RemoteError(_)
                    )
            }
            Self::Group { .. } => false,
        }
    }
}

#[derive(Debug, Clone)]
enum GroupEntry {
    Local(usize),
    RemoteLocal(usize),
    Remote(usize),
    Placeholder(PlaceholderKind),
}

#[derive(Debug, Clone)]
enum PlaceholderKind {
    LocalEmpty,
    RemoteLoading,
    RemoteEmpty,
    RemoteError(String),
}

#[derive(Debug, Clone)]
struct RemoteSession {
    id: String,
    display_name: String,
    project_dir: PathBuf,
    repository: Option<String>,
    status: SessionStatus,
    state: String,
    updated_at: Option<String>,
    url: Option<String>,
    pr_url: Option<String>,
}

impl RemoteSession {
    fn cache_entry(&self) -> CachedRemoteSession {
        CachedRemoteSession {
            id: self.id.clone(),
            display_name: self.display_name.clone(),
            project_dir: self.project_dir.clone(),
            repository: self.repository.clone(),
            status: self.status,
            state: self.state.clone(),
            updated_at: self.updated_at.clone(),
            url: self.url.clone(),
            pr_url: self.pr_url.clone(),
        }
    }
}

impl From<CachedRemoteSession> for RemoteSession {
    fn from(session: CachedRemoteSession) -> Self {
        Self {
            id: session.id,
            display_name: session.display_name,
            project_dir: session.project_dir,
            repository: session.repository,
            status: session.status,
            state: session.state,
            updated_at: session.updated_at,
            url: session.url,
            pr_url: session.pr_url,
        }
    }
}

type RemoteLoadResult = std::result::Result<Vec<RemoteSession>, String>;

#[derive(Debug, Clone)]
enum RemoteLoadState {
    NotStarted,
    Loading,
    Loaded,
    Empty,
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionFilter {
    All,
    Waiting,
    Done,
    Busy,
    Idle,
    Remote,
}

impl SessionFilter {
    const ALL: [Self; 6] = [
        Self::All,
        Self::Waiting,
        Self::Done,
        Self::Busy,
        Self::Idle,
        Self::Remote,
    ];

    fn next(self) -> Self {
        match self {
            Self::All => Self::Waiting,
            Self::Waiting => Self::Done,
            Self::Done => Self::Busy,
            Self::Busy => Self::Idle,
            Self::Idle => Self::Remote,
            Self::Remote => Self::All,
        }
    }

    fn previous(self) -> Self {
        match self {
            Self::All => Self::Remote,
            Self::Waiting => Self::All,
            Self::Done => Self::Waiting,
            Self::Busy => Self::Done,
            Self::Idle => Self::Busy,
            Self::Remote => Self::Idle,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Waiting => "Waiting",
            Self::Done => "Done",
            Self::Busy => "Busy",
            Self::Idle => "Idle",
            Self::Remote => "Remote",
        }
    }

    fn matches_local(self, status: SessionStatus) -> bool {
        match self {
            Self::All => true,
            Self::Waiting => status == SessionStatus::Waiting,
            Self::Done => status == SessionStatus::Done,
            Self::Busy => status == SessionStatus::Busy,
            Self::Idle => status == SessionStatus::Idle,
            Self::Remote => false,
        }
    }

    fn matches_remote(self, status: SessionStatus) -> bool {
        match self {
            Self::All | Self::Remote => true,
            Self::Waiting => status == SessionStatus::Waiting,
            Self::Done => status == SessionStatus::Done,
            Self::Busy => status == SessionStatus::Busy,
            Self::Idle => status == SessionStatus::Idle,
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct StatusCounts {
    waiting: usize,
    done: usize,
    busy: usize,
    idle: usize,
    remote: usize,
    local: usize,
}

impl StatusCounts {
    fn from_sources(sessions: &[ManagedSession], remote_sessions: &[RemoteSession]) -> Self {
        let mut counts = Self::default();
        for session in sessions {
            counts.add(session.status);
            if session.is_remote {
                counts.remote += 1;
            } else {
                counts.local += 1;
            }
        }
        for session in remote_sessions {
            counts.add(session.status);
            counts.remote += 1;
        }
        counts
    }

    fn add(&mut self, status: SessionStatus) {
        match status {
            SessionStatus::Waiting => self.waiting += 1,
            SessionStatus::Done => self.done += 1,
            SessionStatus::Busy => self.busy += 1,
            SessionStatus::Idle => self.idle += 1,
        }
    }
}

fn project_groups(
    sessions: &[ManagedSession],
    remote_sessions: &[RemoteSession],
    tracked_projects: &[PathBuf],
    project_labels: &BTreeMap<String, String>,
    filter: SessionFilter,
    remote_state: &RemoteLoadState,
    current_dir: &Path,
) -> Vec<ProjectGroup> {
    let mut groups = BTreeMap::<String, ProjectGroup>::new();

    if matches!(filter, SessionFilter::All | SessionFilter::Remote) {
        for project_dir in tracked_projects {
            let key = path_key(project_dir);
            groups.entry(key.clone()).or_insert_with(|| ProjectGroup {
                key: key.clone(),
                display_name: project_labels
                    .get(&key)
                    .cloned()
                    .unwrap_or_else(|| fallback_project_label(project_dir)),
                path: project_dir.clone(),
                is_current: same_path(project_dir, current_dir),
                entries: Vec::new(),
                counts: StatusCounts::default(),
                has_bell: false,
                remote_loading: false,
                last_activity: None,
            });
        }
    }

    for (index, session) in sessions.iter().enumerate() {
        if session.is_remote {
            if !filter.matches_remote(session.status) {
                continue;
            }
        } else if !filter.matches_local(session.status) {
            continue;
        }

        let key = path_key(&session.project_dir);
        let entry = groups.entry(key).or_insert_with(|| ProjectGroup {
            key: path_key(&session.project_dir),
            display_name: project_labels
                .get(&path_key(&session.project_dir))
                .cloned()
                .unwrap_or_else(|| fallback_project_label(&session.project_dir)),
            path: session.project_dir.clone(),
            is_current: session.is_current_project,
            entries: Vec::new(),
            counts: StatusCounts::default(),
            has_bell: false,
            remote_loading: false,
            last_activity: None,
        });

        entry.is_current |= session.is_current_project;
        entry.has_bell |= session.has_bell;
        entry.counts.add(session.status);
        if session.is_remote {
            entry.counts.remote += 1;
        } else {
            entry.counts.local += 1;
        }
        entry.record_activity(
            session
                .last_activity
                .and_then(activity_key_from_system_time),
        );
        if session.is_remote {
            entry.entries.push(GroupEntry::RemoteLocal(index));
        } else {
            entry.entries.push(GroupEntry::Local(index));
        }
    }

    for (index, session) in remote_sessions.iter().enumerate() {
        if !filter.matches_remote(session.status) {
            continue;
        }

        let key = path_key(&session.project_dir);
        let entry = groups.entry(key).or_insert_with(|| ProjectGroup {
            key: path_key(&session.project_dir),
            display_name: project_labels
                .get(&path_key(&session.project_dir))
                .cloned()
                .unwrap_or_else(|| fallback_project_label(&session.project_dir)),
            path: session.project_dir.clone(),
            is_current: same_path(&session.project_dir, current_dir),
            entries: Vec::new(),
            counts: StatusCounts::default(),
            has_bell: false,
            remote_loading: false,
            last_activity: None,
        });

        entry.counts.add(session.status);
        entry.counts.remote += 1;
        entry.record_activity(remote_activity_key(session));
        entry.entries.push(GroupEntry::Remote(index));
    }

    if matches!(filter, SessionFilter::All | SessionFilter::Remote) {
        let placeholder = match remote_state {
            RemoteLoadState::NotStarted | RemoteLoadState::Loading => {
                Some(PlaceholderKind::RemoteLoading)
            }
            RemoteLoadState::Empty if matches!(filter, SessionFilter::Remote) => {
                Some(PlaceholderKind::RemoteEmpty)
            }
            RemoteLoadState::Failed(error) => Some(PlaceholderKind::RemoteError(error.clone())),
            RemoteLoadState::Loaded | RemoteLoadState::Empty => None,
        };

        if let Some(kind) = placeholder {
            for project_dir in tracked_projects {
                let key = path_key(project_dir);
                let entry = groups.entry(key.clone()).or_insert_with(|| ProjectGroup {
                    key: key.clone(),
                    display_name: project_labels
                        .get(&key)
                        .cloned()
                        .unwrap_or_else(|| fallback_project_label(project_dir)),
                    path: project_dir.clone(),
                    is_current: same_path(project_dir, current_dir),
                    entries: Vec::new(),
                    counts: StatusCounts::default(),
                    has_bell: false,
                    remote_loading: false,
                    last_activity: None,
                });
                if matches!(kind, PlaceholderKind::RemoteLoading) {
                    entry.remote_loading = true;
                }
                if !matches!(kind, PlaceholderKind::RemoteLoading)
                    && entry.remote_entries().is_empty()
                {
                    entry.entries.push(GroupEntry::Placeholder(kind.clone()));
                }
            }
        }
    }

    if matches!(filter, SessionFilter::All) {
        for group in groups.values_mut() {
            if group.local_entries().is_empty() {
                group
                    .entries
                    .insert(0, GroupEntry::Placeholder(PlaceholderKind::LocalEmpty));
            }
        }
    }

    let mut groups = groups.into_values().collect::<Vec<_>>();
    groups.sort_by(|a, b| {
        b.last_activity
            .cmp(&a.last_activity)
            .then_with(|| b.is_current.cmp(&a.is_current))
            .then_with(|| a.display_name.cmp(&b.display_name))
    });
    groups
}

fn tree_rows(
    groups: &[ProjectGroup],
    collapsed_groups: &BTreeSet<String>,
    filter: SessionFilter,
) -> Vec<TreeRow> {
    let mut rows = Vec::new();
    for (group_index, group) in groups.iter().enumerate() {
        rows.push(TreeRow::Group { group_index });
        if collapsed_groups.contains(&group.key) {
            continue;
        }

        let local_entries = group.local_entries();
        let remote_entries = group.remote_entries();
        let has_remote_subtree =
            !remote_entries.is_empty() && !matches!(filter, SessionFilter::Remote);

        for (position, entry) in local_entries.iter().cloned().enumerate() {
            let is_last = position + 1 == local_entries.len() && !has_remote_subtree;
            match entry {
                GroupEntry::Local(session_index) => rows.push(TreeRow::LocalSession {
                    group_index,
                    session_index,
                    is_last,
                    is_remote: false,
                    under_remote: false,
                }),
                GroupEntry::Placeholder(kind) => rows.push(TreeRow::Placeholder {
                    group_index,
                    kind,
                    is_last,
                    under_remote: false,
                }),
                GroupEntry::Remote(_) | GroupEntry::RemoteLocal(_) => {}
            }
        }

        if has_remote_subtree {
            rows.push(TreeRow::RemoteGroup {
                group_index,
                is_last: true,
            });
            if collapsed_groups.contains(&remote_group_key(&group.key)) {
                continue;
            }
        }

        for (position, entry) in remote_entries.iter().cloned().enumerate() {
            let is_last = position + 1 == remote_entries.len();
            match entry {
                GroupEntry::RemoteLocal(session_index) => rows.push(TreeRow::LocalSession {
                    group_index,
                    session_index,
                    is_last,
                    is_remote: true,
                    under_remote: has_remote_subtree,
                }),
                GroupEntry::Remote(remote_index) => rows.push(TreeRow::RemoteSession {
                    group_index,
                    remote_index,
                    is_last,
                    under_remote: has_remote_subtree,
                }),
                GroupEntry::Placeholder(kind) => rows.push(TreeRow::Placeholder {
                    group_index,
                    kind,
                    is_last,
                    under_remote: has_remote_subtree,
                }),
                GroupEntry::Local(_) => {}
            }
        }
    }
    rows
}

fn tree_item(
    row: &TreeRow,
    groups: &[ProjectGroup],
    sessions: &[ManagedSession],
    remote_sessions: &[RemoteSession],
    collapsed_groups: &BTreeSet<String>,
    content_width: usize,
    loading_frame: usize,
) -> ListItem<'static> {
    match row.clone() {
        TreeRow::Group { group_index } => {
            let group = &groups[group_index];
            let bell = if group.has_bell { " 🔔" } else { "" };
            let loading = if group.remote_loading {
                format!(" {}", loading_icon(loading_frame))
            } else {
                String::new()
            };
            let icon = if collapsed_groups.contains(&group.key) {
                "▸ "
            } else {
                "▾ "
            };
            let name_width = content_width
                .saturating_sub(icon.chars().count())
                .saturating_sub(bell.chars().count())
                .saturating_sub(loading.chars().count());
            let line = Line::from(vec![
                Span::styled(icon, Style::default().fg(TOKYO_BLUE)),
                Span::styled(
                    truncate(&group.display_name, name_width),
                    Style::default().fg(TOKYO_CYAN).add_modifier(Modifier::BOLD),
                ),
                Span::styled(bell.to_owned(), Style::default().fg(TOKYO_YELLOW)),
                Span::styled(loading, Style::default().fg(TOKYO_YELLOW)),
            ]);
            ListItem::new(line)
        }
        TreeRow::RemoteGroup {
            group_index,
            is_last,
        } => {
            let group = &groups[group_index];
            let icon = if collapsed_groups.contains(&remote_group_key(&group.key)) {
                "▸ "
            } else {
                "▾ "
            };
            let branch = if is_last { "  └─ " } else { "  ├─ " };
            let count = format!(" ({})", group.counts.remote);
            let label_prefix = format!("{} ", remote_icon());
            let name_width = content_width
                .saturating_sub(branch.chars().count())
                .saturating_sub(icon.chars().count())
                .saturating_sub(label_prefix.chars().count())
                .saturating_sub(count.chars().count());
            let line = Line::from(vec![
                Span::styled(branch, Style::default().fg(TOKYO_COMMENT)),
                Span::styled(icon, Style::default().fg(TOKYO_PURPLE)),
                Span::styled(
                    format!("{}{}", label_prefix, truncate("remote", name_width)),
                    Style::default()
                        .fg(TOKYO_PURPLE)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(count, Style::default().fg(TOKYO_COMMENT)),
            ]);
            ListItem::new(line)
        }
        TreeRow::LocalSession {
            session_index,
            is_last,
            under_remote,
            ..
        } => {
            let session = &sessions[session_index];
            let branch = match (under_remote, is_last) {
                (true, true) => "    └─ ",
                (true, false) => "    ├─ ",
                (false, true) => "  └─ ",
                (false, false) => "  ├─ ",
            };
            let bell = if session.has_bell { " 🔔" } else { "" };
            let exited = if session.pane_dead { " exited" } else { "" };
            let suffix = " copilot";
            let name_width = content_width
                .saturating_sub(branch.chars().count())
                .saturating_sub(status_icon(session.status).chars().count())
                .saturating_sub(1)
                .saturating_sub(suffix.chars().count())
                .saturating_sub(bell.chars().count())
                .saturating_sub(exited.chars().count());
            let line = Line::from(vec![
                Span::styled(branch, Style::default().fg(TOKYO_COMMENT)),
                Span::styled(status_icon(session.status), status_style(session.status)),
                Span::raw(" "),
                Span::styled(
                    truncate(&session.display_name, name_width),
                    Style::default().fg(TOKYO_FG),
                ),
                Span::styled(suffix, Style::default().fg(TOKYO_PURPLE)),
                Span::styled(bell.to_owned(), Style::default().fg(TOKYO_YELLOW)),
                Span::styled(exited.to_owned(), Style::default().fg(TOKYO_RED)),
            ]);
            ListItem::new(line)
        }
        TreeRow::RemoteSession {
            remote_index,
            is_last,
            under_remote,
            ..
        } => {
            let session = &remote_sessions[remote_index];
            let branch = match (under_remote, is_last) {
                (true, true) => "    └─ ",
                (true, false) => "    ├─ ",
                (false, true) => "  └─ ",
                (false, false) => "  ├─ ",
            };
            let icon = remote_status_icon(session);
            let name_width = content_width
                .saturating_sub(branch.chars().count())
                .saturating_sub(icon.chars().count())
                .saturating_sub(1);
            let line = Line::from(vec![
                Span::styled(branch, Style::default().fg(TOKYO_COMMENT)),
                Span::styled(icon, remote_status_style(session)),
                Span::raw(" "),
                Span::styled(
                    truncate(&session.display_name, name_width),
                    Style::default().fg(TOKYO_FG),
                ),
            ]);
            ListItem::new(line)
        }
        TreeRow::Placeholder {
            kind,
            is_last,
            under_remote,
            ..
        } => {
            let branch = match (under_remote, is_last) {
                (true, true) => "    └─ ",
                (true, false) => "    ├─ ",
                (false, true) => "  └─ ",
                (false, false) => "  ├─ ",
            };
            let (icon, label, style) = placeholder_display(&kind, loading_frame);
            let line = Line::from(vec![
                Span::styled(branch, Style::default().fg(TOKYO_COMMENT)),
                Span::styled(icon, style),
                Span::raw(" "),
                Span::styled(label, Style::default().fg(TOKYO_COMMENT)),
            ]);
            ListItem::new(line)
        }
    }
}

fn header_lines(app: &App, counts: StatusCounts) -> Vec<Line<'static>> {
    vec![
        Line::from(vec![Span::styled(
            "gh pilot",
            Style::default().fg(TOKYO_BLUE).add_modifier(Modifier::BOLD),
        )]),
        filter_line(app.active_filter, counts, app.remote_enabled),
    ]
}

fn filter_line(
    active_filter: SessionFilter,
    counts: StatusCounts,
    remote_enabled: bool,
) -> Line<'static> {
    let mut spans = Vec::new();
    for filter in SessionFilter::ALL {
        if !spans.is_empty() {
            spans.push(Span::raw("  "));
        }
        spans.push(filter_span(filter, active_filter, counts));
    }
    if remote_enabled {
        spans.push(Span::styled(
            "   remote on",
            Style::default().fg(TOKYO_COMMENT),
        ));
    }
    Line::from(spans)
}

fn filter_span(
    filter: SessionFilter,
    active_filter: SessionFilter,
    counts: StatusCounts,
) -> Span<'static> {
    let label = match filter {
        SessionFilter::All => " All ".to_owned(),
        SessionFilter::Waiting => {
            format!(
                " {} {} ",
                status_icon(SessionStatus::Waiting),
                counts.waiting
            )
        }
        SessionFilter::Done => format!(" {} {} ", status_icon(SessionStatus::Done), counts.done),
        SessionFilter::Busy => format!(" {} {} ", status_icon(SessionStatus::Busy), counts.busy),
        SessionFilter::Idle => format!(" {} {} ", status_icon(SessionStatus::Idle), counts.idle),
        SessionFilter::Remote => format!(" {} {} ", remote_icon(), counts.remote),
    };
    let color = filter_color(filter);

    if filter == active_filter {
        Span::styled(
            label,
            Style::default()
                .fg(TOKYO_BG)
                .bg(color)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(label, Style::default().fg(color))
    }
}

fn filter_color(filter: SessionFilter) -> Color {
    match filter {
        SessionFilter::All => TOKYO_BLUE,
        SessionFilter::Waiting => TOKYO_YELLOW,
        SessionFilter::Done => TOKYO_GREEN,
        SessionFilter::Busy => TOKYO_BLUE,
        SessionFilter::Idle => TOKYO_COMMENT,
        SessionFilter::Remote => TOKYO_PURPLE,
    }
}

fn group_preview(
    group: &ProjectGroup,
    sessions: &[ManagedSession],
    remote_sessions: &[RemoteSession],
) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(vec![
            Span::styled("📁 ", Style::default().fg(TOKYO_FG)),
            Span::styled(
                group.display_name.clone(),
                Style::default().fg(TOKYO_CYAN).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::raw(""),
        Line::from(Span::styled(
            group_count_text(group.counts),
            Style::default().fg(TOKYO_FG).add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::styled(
                status_icon(SessionStatus::Waiting),
                status_style(SessionStatus::Waiting),
            ),
            Span::styled(
                format!(" {} waiting", group.counts.waiting),
                Style::default().fg(TOKYO_YELLOW),
            ),
            Span::styled("  ", Style::default()),
            Span::styled(
                status_icon(SessionStatus::Done),
                status_style(SessionStatus::Done),
            ),
            Span::styled(
                format!(" {} done", group.counts.done),
                Style::default().fg(TOKYO_GREEN),
            ),
        ]),
        Line::raw(""),
        separator_line("Sessions"),
    ];

    for entry in &group.entries {
        match entry {
            GroupEntry::Local(index) | GroupEntry::RemoteLocal(index) => {
                let session = &sessions[*index];
                lines.push(Line::from(vec![
                    Span::styled("  ", Style::default()),
                    Span::styled(status_icon(session.status), status_style(session.status)),
                    Span::raw(" "),
                    Span::styled(
                        truncate(&session.display_name, 28),
                        Style::default().fg(TOKYO_FG),
                    ),
                    Span::raw(" "),
                    Span::styled("copilot", Style::default().fg(TOKYO_PURPLE)),
                    Span::styled(
                        if session.has_bell { " 🔔" } else { "" },
                        Style::default().fg(TOKYO_YELLOW),
                    ),
                ]));
            }
            GroupEntry::Remote(index) => {
                let session = &remote_sessions[*index];
                lines.push(Line::from(vec![
                    Span::styled("  ", Style::default()),
                    Span::styled(remote_status_icon(session), remote_status_style(session)),
                    Span::raw(" "),
                    Span::styled(
                        truncate(&session.display_name, 28),
                        Style::default().fg(TOKYO_FG),
                    ),
                    Span::raw(" "),
                    Span::styled(
                        format!("{} remote", remote_icon()),
                        Style::default().fg(TOKYO_PURPLE),
                    ),
                ]));
            }
            GroupEntry::Placeholder(kind) => {
                let (icon, label, style) = placeholder_display(kind, loading_frame());
                lines.push(Line::from(vec![
                    Span::styled("  ", Style::default()),
                    Span::styled(icon, style),
                    Span::raw(" "),
                    Span::styled(label, Style::default().fg(TOKYO_COMMENT)),
                ]));
            }
        }
    }

    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "Enter open • n new in project • N new local repo • P new project • x close • X close all • w web",
        Style::default()
            .fg(TOKYO_COMMENT)
            .add_modifier(Modifier::ITALIC),
    )));

    lines
}

fn remote_preview(session: &RemoteSession, group: &ProjectGroup) -> Vec<Line<'static>> {
    vec![
        Line::from(vec![
            Span::styled(remote_status_icon(session), remote_status_style(session)),
            Span::raw(" "),
            Span::styled(
                session.display_name.clone(),
                Style::default().fg(TOKYO_FG).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(
                format!("{} remote", remote_icon()),
                Style::default().fg(TOKYO_PURPLE),
            ),
        ]),
        Line::raw(""),
        Line::from(vec![
            Span::styled("📁 ", Style::default().fg(TOKYO_FG)),
            Span::styled(group.display_name.clone(), Style::default().fg(TOKYO_CYAN)),
        ]),
        Line::raw(""),
        Line::from(vec![
            Span::styled("state   ", Style::default().fg(TOKYO_COMMENT)),
            Span::styled(session.state.clone(), status_style(session.status)),
        ]),
        Line::from(vec![
            Span::styled("updated ", Style::default().fg(TOKYO_COMMENT)),
            Span::styled(
                session.updated_at.clone().unwrap_or_else(|| "-".to_owned()),
                Style::default().fg(TOKYO_FG),
            ),
        ]),
        Line::from(vec![
            Span::styled("url     ", Style::default().fg(TOKYO_COMMENT)),
            Span::styled(
                session.url.clone().unwrap_or_else(|| "-".to_owned()),
                Style::default().fg(TOKYO_FG),
            ),
        ]),
        Line::from(vec![
            Span::styled("pr      ", Style::default().fg(TOKYO_COMMENT)),
            Span::styled(
                session.pr_url.clone().unwrap_or_else(|| "-".to_owned()),
                Style::default().fg(TOKYO_FG),
            ),
        ]),
        Line::raw(""),
        separator_line("Actions"),
        Line::from(Span::styled(
            "Enter open locally with copilot --resume • w session • p pull request",
            Style::default()
                .fg(TOKYO_COMMENT)
                .add_modifier(Modifier::ITALIC),
        )),
    ]
}

fn session_preview(session: &ManagedSession, group: &ProjectGroup) -> Vec<Line<'static>> {
    vec![
        Line::from(vec![
            Span::styled(status_icon(session.status), status_style(session.status)),
            Span::raw(" "),
            Span::styled(
                session.display_name.clone(),
                Style::default().fg(TOKYO_FG).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled("copilot", Style::default().fg(TOKYO_PURPLE)),
        ]),
        Line::raw(""),
        Line::from(vec![
            Span::styled("📁 ", Style::default().fg(TOKYO_FG)),
            Span::styled(group.display_name.clone(), Style::default().fg(TOKYO_CYAN)),
        ]),
        Line::from(Span::styled(
            group.path.display().to_string(),
            Style::default().fg(TOKYO_COMMENT),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::styled("status  ", Style::default().fg(TOKYO_COMMENT)),
            Span::styled(session.status.label(), status_style(session.status)),
        ]),
        Line::from(vec![
            Span::styled("last    ", Style::default().fg(TOKYO_COMMENT)),
            Span::styled(
                format_age(session.last_activity),
                Style::default().fg(TOKYO_FG),
            ),
        ]),
        Line::from(vec![
            Span::styled("bell    ", Style::default().fg(TOKYO_COMMENT)),
            Span::styled(
                if session.has_bell { "yes 🔔" } else { "no" },
                if session.has_bell {
                    Style::default().fg(TOKYO_YELLOW)
                } else {
                    Style::default().fg(TOKYO_COMMENT)
                },
            ),
        ]),
        Line::raw(""),
        separator_line("Actions"),
        Line::from(Span::styled(
            "Enter open fullscreen • x close • X close all",
            Style::default()
                .fg(TOKYO_COMMENT)
                .add_modifier(Modifier::ITALIC),
        )),
    ]
}

fn separator_line(label: &'static str) -> Line<'static> {
    Line::from(vec![
        Span::styled("──────────── ", Style::default().fg(TOKYO_BORDER)),
        Span::styled(
            label,
            Style::default().fg(TOKYO_FG).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" ────────────", Style::default().fg(TOKYO_BORDER)),
    ])
}

fn placeholder_display(kind: &PlaceholderKind, loading_frame: usize) -> (String, String, Style) {
    match kind {
        PlaceholderKind::LocalEmpty => (
            "○".to_owned(),
            "empty - no local sessions yet".to_owned(),
            Style::default().fg(TOKYO_COMMENT),
        ),
        PlaceholderKind::RemoteLoading => (
            loading_icon(loading_frame).to_owned(),
            "refreshing remote sessions...".to_owned(),
            Style::default().fg(TOKYO_YELLOW),
        ),
        PlaceholderKind::RemoteEmpty => (
            "○".to_owned(),
            "empty - no remote sessions for this repo".to_owned(),
            Style::default().fg(TOKYO_COMMENT),
        ),
        PlaceholderKind::RemoteError(error) => (
            "!".to_owned(),
            format!("remote sessions unavailable: {}", truncate(error, 48)),
            Style::default().fg(TOKYO_RED),
        ),
    }
}

fn loading_frame() -> usize {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .checked_div(SPINNER_FRAME_MS)
        .unwrap_or_default() as usize
}

fn loading_icon(frame: usize) -> &'static str {
    const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    FRAMES[frame % FRAMES.len()]
}

fn status_icon(status: SessionStatus) -> &'static str {
    match status {
        SessionStatus::Waiting => "◐",
        SessionStatus::Done => "◆",
        SessionStatus::Busy => "●",
        SessionStatus::Idle => "○",
    }
}

fn remote_icon() -> &'static str {
    ""
}

fn remote_status_icon(session: &RemoteSession) -> &'static str {
    if is_cancelled_remote_state(&session.state) {
        "×"
    } else {
        status_icon(session.status)
    }
}

fn remote_status_style(session: &RemoteSession) -> Style {
    if is_cancelled_remote_state(&session.state) {
        Style::default().fg(TOKYO_RED).add_modifier(Modifier::BOLD)
    } else {
        status_style(session.status)
    }
}

fn status_style(status: SessionStatus) -> Style {
    match status {
        SessionStatus::Waiting => Style::default()
            .fg(TOKYO_YELLOW)
            .add_modifier(Modifier::BOLD),
        SessionStatus::Done => Style::default()
            .fg(TOKYO_GREEN)
            .add_modifier(Modifier::BOLD),
        SessionStatus::Busy => Style::default().fg(TOKYO_BLUE),
        SessionStatus::Idle => Style::default().fg(TOKYO_COMMENT),
    }
}

fn chrome_block() -> Block<'static> {
    Block::default().style(Style::default().bg(TOKYO_BG).fg(TOKYO_FG))
}

fn section_block(title: &'static str) -> Block<'static> {
    Block::default()
        .title(Span::styled(
            title,
            Style::default().fg(TOKYO_CYAN).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::TOP)
        .border_style(Style::default().fg(TOKYO_BORDER))
        .style(Style::default().bg(TOKYO_BG).fg(TOKYO_FG))
}

fn format_age(time: Option<SystemTime>) -> String {
    let Some(time) = time else {
        return "-".to_owned();
    };
    let age = SystemTime::now()
        .duration_since(time)
        .unwrap_or_else(|_| Duration::from_secs(0));
    let secs = age.as_secs();

    if secs < 60 {
        format!("{secs}s")
    } else if secs < 60 * 60 {
        format!("{}m", secs / 60)
    } else if secs < 60 * 60 * 24 {
        format!("{}h", secs / 60 / 60)
    } else {
        format!("{}d", secs / 60 / 60 / 24)
    }
}

fn activity_key_from_system_time(time: SystemTime) -> Option<u128> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_millis())
}

fn remote_activity_key(session: &RemoteSession) -> Option<u128> {
    session
        .updated_at
        .as_deref()
        .and_then(parse_rfc3339_utc_millis)
}

fn parse_rfc3339_utc_millis(input: &str) -> Option<u128> {
    let timestamp = input.strip_suffix('Z')?;
    let timestamp = timestamp
        .split_once('.')
        .map_or(timestamp, |(head, _)| head);
    let (date, time) = timestamp.split_once('T')?;
    let mut date_parts = date.split('-');
    let year = date_parts.next()?.parse::<i64>().ok()?;
    let month = date_parts.next()?.parse::<i64>().ok()?;
    let day = date_parts.next()?.parse::<i64>().ok()?;
    let mut time_parts = time.split(':');
    let hour = time_parts.next()?.parse::<i64>().ok()?;
    let minute = time_parts.next()?.parse::<i64>().ok()?;
    let second = time_parts.next()?.parse::<i64>().ok()?;

    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        || !(0..=60).contains(&second)
    {
        return None;
    }

    let days = days_from_civil(year, month, day);
    let seconds = days
        .checked_mul(86_400)?
        .checked_add(hour.checked_mul(3_600)?)?
        .checked_add(minute.checked_mul(60)?)?
        .checked_add(second)?;
    u128::try_from(seconds).ok().map(|seconds| seconds * 1_000)
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_prime = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

const TOKYO_BG: Color = Color::Rgb(0x1a, 0x1b, 0x26);
const TOKYO_FG: Color = Color::Rgb(0xc0, 0xca, 0xf5);
const TOKYO_COMMENT: Color = Color::Rgb(0x56, 0x5f, 0x89);
const TOKYO_BORDER: Color = Color::Rgb(0x41, 0x48, 0x68);
const TOKYO_BLUE: Color = Color::Rgb(0x7a, 0xa2, 0xf7);
const TOKYO_CYAN: Color = Color::Rgb(0x7d, 0xcf, 0xff);
const TOKYO_GREEN: Color = Color::Rgb(0x9e, 0xce, 0x6a);
const TOKYO_YELLOW: Color = Color::Rgb(0xe0, 0xaf, 0x68);
const TOKYO_RED: Color = Color::Rgb(0xf7, 0x76, 0x8e);
const TOKYO_PURPLE: Color = Color::Rgb(0xbb, 0x9a, 0xf7);

const UI_TICK_MS: u64 = 80;
const SPINNER_FRAME_MS: u128 = 80;

fn truncate(input: &str, width: usize) -> String {
    let char_count = input.chars().count();
    if char_count <= width {
        return input.to_owned();
    }
    if width == 0 {
        String::new()
    } else if width == 1 {
        "~".to_owned()
    } else {
        let mut output = input.chars().take(width - 1).collect::<String>();
        output.push('~');
        output
    }
}

fn tracked_project_dirs(
    current_dir: &Path,
    sessions: &[ManagedSession],
    remote_sessions: &[RemoteSession],
    recent_paths: &[PathBuf],
    removed_projects: &BTreeSet<String>,
) -> Vec<PathBuf> {
    let mut projects = BTreeMap::new();
    insert_project_dir(&mut projects, current_dir.to_path_buf(), removed_projects);
    for session in sessions {
        insert_project_dir(&mut projects, session.project_dir.clone(), removed_projects);
    }
    for session in remote_sessions {
        insert_project_dir(&mut projects, session.project_dir.clone(), removed_projects);
    }
    for path in recent_paths {
        insert_project_dir(&mut projects, path.clone(), removed_projects);
    }
    projects.into_values().collect()
}

fn insert_project_dir(
    projects: &mut BTreeMap<String, PathBuf>,
    path: PathBuf,
    removed_projects: &BTreeSet<String>,
) {
    let key = path_key(&path);
    if !removed_projects.contains(&key) {
        projects.entry(key).or_insert(path);
    }
}

fn project_labels(project_dirs: &[PathBuf]) -> BTreeMap<String, String> {
    project_dirs
        .iter()
        .map(|path| (path_key(path), project_label(path)))
        .collect()
}

fn project_label(project_dir: &Path) -> String {
    github_repo_from_remote(project_dir).unwrap_or_else(|| fallback_project_label(project_dir))
}

fn fallback_project_label(project_dir: &Path) -> String {
    project_dir
        .file_name()
        .and_then(|name| name.to_str())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| project_dir.display().to_string())
}

fn path_key(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

fn same_path(left: &Path, right: &Path) -> bool {
    path_key(left) == path_key(right)
}

fn github_repo_from_remote(project_dir: &Path) -> Option<String> {
    let output = StdCommand::new("git")
        .args(["config", "--get", "remote.origin.url"])
        .current_dir(project_dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let remote = String::from_utf8_lossy(&output.stdout);
    parse_github_remote(remote.trim())
}

fn parse_github_remote(remote: &str) -> Option<String> {
    let remote = remote.trim_end_matches(".git");
    let path = remote
        .strip_prefix("git@github.com:")
        .or_else(|| remote.strip_prefix("https://github.com/"))
        .or_else(|| remote.strip_prefix("http://github.com/"))
        .or_else(|| remote.strip_prefix("ssh://git@github.com/"))?;
    let mut parts = path.split('/');
    let owner = parts.next()?.trim();
    let repo = parts.next()?.trim();
    if owner.is_empty() || repo.is_empty() {
        None
    } else {
        Some(format!("{owner}/{repo}"))
    }
}

fn complete_path_input(
    buffer: &mut String,
    completion_prefix: &mut Option<String>,
    completion_index: &mut usize,
    recent_paths: &[PathBuf],
    reverse: bool,
) {
    let active_completion = completion_prefix.is_some();
    let prefix = completion_prefix
        .clone()
        .unwrap_or_else(|| buffer.trim().to_owned());
    let matches = recent_path_matches(&prefix, recent_paths);
    if matches.is_empty() {
        return;
    }

    let index = if active_completion {
        let offset = if reverse {
            matches.len().saturating_sub(1)
        } else {
            1
        };
        completion_index.saturating_add(offset) % matches.len()
    } else if reverse {
        matches.len().saturating_sub(1)
    } else {
        0
    };

    *completion_prefix = Some(prefix);
    *completion_index = index;
    *buffer = matches[index].clone();
}

fn recent_path_matches(prefix: &str, recent_paths: &[PathBuf]) -> Vec<String> {
    let mut matches = Vec::new();
    for path in recent_paths {
        let display = prompt_path(path);
        let absolute = path.to_string_lossy();
        if (prefix.is_empty() || display.starts_with(prefix) || absolute.starts_with(prefix))
            && !matches.contains(&display)
        {
            matches.push(display);
        }
    }
    matches
}

fn prompt_path(path: &Path) -> String {
    let Some(home) = home_dir_path() else {
        return path.display().to_string();
    };

    let Ok(relative) = path.strip_prefix(&home) else {
        return path.display().to_string();
    };

    if relative.as_os_str().is_empty() {
        "~".to_owned()
    } else {
        format!("~/{}", relative.display())
    }
}

fn normalize_project_dir(current_dir: &Path, input: &Path) -> Result<PathBuf> {
    if input.as_os_str().is_empty() {
        anyhow::bail!("repo path cannot be empty");
    }

    let input = expand_home_path(input)?;
    let path = if input.is_absolute() {
        input
    } else {
        current_dir.join(input)
    };
    if !path.is_dir() {
        anyhow::bail!("{} is not a directory", path.display());
    }
    path.canonicalize()
        .with_context(|| format!("failed to resolve {}", path.display()))
}

fn expand_home_path(input: &Path) -> Result<PathBuf> {
    let input = input.to_string_lossy();
    if input == "~" {
        return home_dir_path().context("HOME is not set; cannot resolve ~");
    }

    if let Some(suffix) = input.strip_prefix("~/") {
        return Ok(home_dir_path()
            .context("HOME is not set; cannot resolve ~")?
            .join(suffix));
    }

    Ok(PathBuf::from(input.as_ref()))
}

fn home_dir_path() -> Option<PathBuf> {
    env::var_os("HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
}

fn group_count_text(counts: StatusCounts) -> String {
    match (counts.local, counts.remote) {
        (0, 0) => "empty".to_owned(),
        (local, 0) => format!("{local} local session(s)"),
        (0, remote) => format!("{remote} remote session(s)"),
        (local, remote) => format!("{local} local session(s), {remote} remote session(s)"),
    }
}

fn remote_group_key(group_key: &str) -> String {
    format!("{group_key}::remote")
}

async fn list_remote_sessions(project_dirs: Vec<PathBuf>) -> Result<Vec<RemoteSession>> {
    let mut sessions = Vec::new();
    let mut errors = Vec::new();
    let mut seen = BTreeSet::new();

    for project_dir in project_dirs {
        if github_repo_from_remote(&project_dir).is_none() {
            continue;
        }

        match list_remote_sessions_for_project(project_dir).await {
            Ok(project_sessions) => {
                for session in project_sessions {
                    if seen.insert(session.id.clone()) {
                        sessions.push(session);
                    }
                }
            }
            Err(error) => errors.push(error.to_string()),
        }
    }

    if sessions.is_empty() && !errors.is_empty() {
        anyhow::bail!("{}", errors.join("; "));
    }

    Ok(sessions)
}

async fn list_remote_sessions_for_project(current_dir: PathBuf) -> Result<Vec<RemoteSession>> {
    let repo = github_repo_from_remote(&current_dir);
    let output = Command::new("gh")
        .current_dir(&current_dir)
        .args([
            "agent-task",
            "list",
            "--limit",
            "50",
            "--json",
            "id,name,pullRequestUrl,repository,state,updatedAt",
        ])
        .output()
        .await
        .context("failed to list remote agent tasks")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("gh agent-task list failed: {}", stderr.trim());
    }

    let tasks: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("failed to parse gh agent-task output")?;
    let Some(tasks) = tasks.as_array() else {
        return Ok(Vec::new());
    };

    let mut sessions = Vec::new();
    for task in tasks {
        let Some(task_repo) = matching_task_repository(task, repo.as_deref()) else {
            continue;
        };

        let Some(id) = task.get("id").and_then(|value| value.as_str()) else {
            continue;
        };
        let name = task
            .get("name")
            .and_then(|value| value.as_str())
            .unwrap_or("Remote Copilot session");
        let state = task
            .get("state")
            .and_then(|value| value.as_str())
            .unwrap_or("unknown")
            .to_owned();

        sessions.push(RemoteSession {
            id: id.to_owned(),
            display_name: truncate(name, 48),
            project_dir: current_dir.clone(),
            repository: Some(task_repo),
            status: remote_status(&state),
            state,
            updated_at: task
                .get("updatedAt")
                .and_then(|value| value.as_str())
                .map(ToOwned::to_owned),
            url: remote_session_url(task, id),
            pr_url: pull_request_url(task),
        });
    }

    Ok(sessions)
}

fn matching_task_repository(task: &serde_json::Value, repo: Option<&str>) -> Option<String> {
    let repo = repo?;
    let task_repo = task_repository(task)?;
    (task_repo == repo).then_some(task_repo)
}

fn task_repository(task: &serde_json::Value) -> Option<String> {
    match task.get("repository")? {
        serde_json::Value::String(repo) => Some(repo.clone()),
        serde_json::Value::Object(repo) => repo
            .get("nameWithOwner")
            .or_else(|| repo.get("fullName"))
            .or_else(|| repo.get("name"))
            .and_then(|value| value.as_str())
            .map(ToOwned::to_owned),
        _ => None,
    }
}

fn remote_session_url(task: &serde_json::Value, id: &str) -> Option<String> {
    pull_request_url(task).map(|url| format!("{}/agent-sessions/{}", url.trim_end_matches('/'), id))
}

fn pull_request_url(task: &serde_json::Value) -> Option<String> {
    task.get("pullRequestUrl")
        .and_then(|value| value.as_str())
        .filter(|url| !url.is_empty())
        .map(ToOwned::to_owned)
}

fn open_url(url: &str) -> Result<std::process::ExitStatus> {
    #[cfg(target_os = "macos")]
    {
        StdCommand::new("open")
            .arg(url)
            .status()
            .with_context(|| format!("failed to open {}", url))
    }

    #[cfg(target_os = "windows")]
    {
        StdCommand::new("rundll32")
            .args(["url.dll,FileProtocolHandler", url])
            .status()
            .with_context(|| format!("failed to open {}", url))
    }

    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        StdCommand::new("xdg-open")
            .arg(url)
            .status()
            .with_context(|| format!("failed to open {}", url))
    }
}

fn remote_status(state: &str) -> SessionStatus {
    match normalize_remote_state(state).as_str() {
        "completed" | "complete" | "done" | "succeeded" | "success" => SessionStatus::Done,
        "cancelled" | "canceled" | "cancelling" | "canceling" => SessionStatus::Idle,
        "queued"
        | "pending"
        | "waiting"
        | "blocked"
        | "waiting_for_input"
        | "waiting_for_user"
        | "input_required"
        | "user_input_required"
        | "requires_action" => SessionStatus::Waiting,
        "in_progress" | "running" | "busy" | "started" => SessionStatus::Busy,
        _ => SessionStatus::Idle,
    }
}

fn is_cancelled_remote_state(state: &str) -> bool {
    matches!(
        normalize_remote_state(state).as_str(),
        "cancelled" | "canceled" | "cancelling" | "canceling"
    )
}

fn normalize_remote_state(state: &str) -> String {
    state
        .trim()
        .chars()
        .map(|ch| match ch {
            '-' | ' ' => '_',
            _ => ch.to_ascii_lowercase(),
        })
        .collect()
}

#[cfg(test)]
mod app_tests {
    use super::*;

    #[test]
    fn recent_path_matches_use_prefix_order() {
        let paths = vec![
            PathBuf::from("/tmp/gh-pilot-alpha"),
            PathBuf::from("/tmp/gh-pilot-beta"),
            PathBuf::from("/tmp/other"),
        ];

        assert_eq!(
            recent_path_matches("/tmp/gh-pilot", &paths),
            vec![
                "/tmp/gh-pilot-alpha".to_owned(),
                "/tmp/gh-pilot-beta".to_owned()
            ]
        );
    }

    #[test]
    fn path_completion_cycles_recent_matches() {
        let paths = vec![
            PathBuf::from("/tmp/gh-pilot-alpha"),
            PathBuf::from("/tmp/gh-pilot-beta"),
        ];
        let mut buffer = "/tmp/gh-pilot".to_owned();
        let mut completion_prefix = None;
        let mut completion_index = 0;

        complete_path_input(
            &mut buffer,
            &mut completion_prefix,
            &mut completion_index,
            &paths,
            false,
        );
        assert_eq!(buffer, "/tmp/gh-pilot-alpha");

        complete_path_input(
            &mut buffer,
            &mut completion_prefix,
            &mut completion_index,
            &paths,
            false,
        );
        assert_eq!(buffer, "/tmp/gh-pilot-beta");
    }

    #[test]
    fn parses_github_remote_names() {
        assert_eq!(
            parse_github_remote("git@github.com:joshblack/gh-pilot.git"),
            Some("joshblack/gh-pilot".to_owned())
        );
        assert_eq!(
            parse_github_remote("https://github.com/primer/react.git"),
            Some("primer/react".to_owned())
        );
        assert_eq!(parse_github_remote("https://example.com/a/b.git"), None);
    }

    #[test]
    fn tracked_projects_include_current_and_recent_paths() {
        let current = PathBuf::from("/tmp/current-project");
        let recent = vec![PathBuf::from("/tmp/other-project")];
        let projects = tracked_project_dirs(&current, &[], &[], &recent, &BTreeSet::new());

        assert_eq!(projects.len(), 2);
        assert!(projects.iter().any(|path| path == &current));
        assert!(projects.iter().any(|path| path == &recent[0]));
    }

    #[test]
    fn tracked_projects_exclude_removed_paths() {
        let current = PathBuf::from("/tmp/current-project");
        let recent = vec![PathBuf::from("/tmp/other-project")];
        let removed = [path_key(&current)].into_iter().collect::<BTreeSet<_>>();
        let projects = tracked_project_dirs(&current, &[], &[], &recent, &removed);

        assert_eq!(projects, recent);
    }

    #[test]
    fn parses_remote_activity_timestamp() {
        assert_eq!(
            parse_rfc3339_utc_millis("1970-01-02T00:00:00Z"),
            Some(86_400_000)
        );
        assert_eq!(
            parse_rfc3339_utc_millis("2026-05-13T03:48:44.901774124Z"),
            Some(1_778_644_124_000)
        );
    }

    #[test]
    fn remote_task_matching_filters_terminal_mirrors() {
        let terminal_mirror = serde_json::json!({
            "repository": null,
        });
        let repo_task = serde_json::json!({
            "repository": "joshblack/gh-pilot",
        });
        let other_repo_task = serde_json::json!({
            "repository": {
                "nameWithOwner": "primer/react",
            },
        });

        assert!(matching_task_repository(&terminal_mirror, Some("joshblack/gh-pilot")).is_none());
        assert_eq!(
            matching_task_repository(&repo_task, Some("joshblack/gh-pilot")),
            Some("joshblack/gh-pilot".to_owned())
        );
        assert!(matching_task_repository(&other_repo_task, Some("joshblack/gh-pilot")).is_none());
        assert!(matching_task_repository(&repo_task, None).is_none());
    }

    #[test]
    fn remote_status_detects_waiting_for_input_states() {
        assert_eq!(remote_status("waiting_for_input"), SessionStatus::Waiting);
        assert_eq!(remote_status("WAITING FOR USER"), SessionStatus::Waiting);
        assert_eq!(remote_status("requires-action"), SessionStatus::Waiting);
    }

    #[test]
    fn remote_managed_sessions_are_grouped_under_remote_subtree() {
        let project_dir = PathBuf::from("/tmp/current-project");
        let sessions = vec![ManagedSession {
            name: "gh-pilot__current-project__1__1".to_owned(),
            display_name: "current project".to_owned(),
            project_dir: project_dir.clone(),
            is_current_project: true,
            is_remote: true,
            status: SessionStatus::Busy,
            last_activity: None,
            has_bell: false,
            pane_dead: false,
        }];
        let groups = project_groups(
            &sessions,
            &[],
            std::slice::from_ref(&project_dir),
            &BTreeMap::new(),
            SessionFilter::All,
            &RemoteLoadState::Loaded,
            &project_dir,
        );
        let rows = tree_rows(&groups, &BTreeSet::new(), SessionFilter::All);

        assert!(matches!(
            rows.get(1),
            Some(TreeRow::Placeholder {
                kind: PlaceholderKind::LocalEmpty,
                ..
            })
        ));
        assert!(matches!(rows.get(2), Some(TreeRow::RemoteGroup { .. })));
        assert!(matches!(
            rows.get(3),
            Some(TreeRow::LocalSession {
                session_index: 0,
                is_remote: true,
                under_remote: true,
                ..
            })
        ));
        assert_eq!(groups[0].counts.local, 0);
        assert_eq!(groups[0].counts.remote, 1);
    }
}
