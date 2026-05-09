mod app;
mod session;
mod terminal;
mod ui;

use anyhow::{Context, Result};
use app::{App, Mode, Panel, PendingAction};
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
        MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{
        disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen, SetTitle,
    },
};
use ratatui::{backend::CrosstermBackend, Terminal};
use session::copilot_binary;
use std::{
    io::{self, Write},
    path::PathBuf,
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use terminal::{
    attach_tmux_session, ensure_tmux_session, key_to_bytes, mouse_to_bytes, reuse_tmux_session,
    EmbeddedTerminal,
};

fn main() -> Result<()> {
    let copilot_dir = session::copilot_dir();
    let launch_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut app = App::new(copilot_dir, launch_dir);

    enable_raw_mode().context("Failed to enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
        .context("Failed to enter alternate screen")?;

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).context("Failed to create terminal")?;
    terminal.clear()?;
    app.reload();

    let result = run_event_loop(&mut terminal, &mut app);

    disable_raw_mode().ok();
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )
    .ok();
    terminal.show_cursor().ok();

    if let Err(e) = result {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }

    Ok(())
}

fn run_event_loop<B: ratatui::backend::Backend + Write>(
    terminal: &mut Terminal<B>,
    app: &mut App,
) -> Result<()>
where
    B::Error: Send + Sync + 'static,
{
    let tick_rate = Duration::from_millis(100); // balanced: responsive terminal output + low CPU
    let mut last_tick = Instant::now();
    let mut status_since: Option<Instant> = None;
    let mut last_new_session_reload_check: Option<Instant> = None;
    let mut last_status_poll = Instant::now();
    let mut last_terminal_title = String::new();

    loop {
        app.poll_session_loads();
        app.poll_remote_log_loads();
        resize_embedded_terminal(app, terminal.size()?);
        update_terminal_title(terminal, app, &mut last_terminal_title)?;
        terminal.draw(|f| ui::draw(f, app))?;

        // ── Spawn pending embedded terminals ─────────────────────────────────
        let action = std::mem::replace(&mut app.pending_action, PendingAction::None);
        match action {
            PendingAction::None => {}
            PendingAction::OpenNative { id, cwd } => match copilot_binary() {
                Some(bin) => {
                    let cwd_arg = cwd.to_string_lossy();
                    let resume_arg = format!("--resume={id}");
                    match ensure_tmux_session(
                        &id,
                        &bin,
                        &["-C", cwd_arg.as_ref(), resume_arg.as_str()],
                        Some(&cwd),
                    )
                    .and_then(|tmux_session| {
                        run_native_tmux_session(terminal, &tmux_session)?;
                        Ok(())
                    }) {
                        Ok(()) => {
                            app.mode = Mode::Normal;
                            app.terminal_fullscreen = false;
                            app.reload();
                            app.status_message = Some("Returned from native terminal".into());
                        }
                        Err(e) => {
                            app.status_message =
                                Some(format!("Failed to open native terminal: {e}"));
                        }
                    }
                    status_since = Some(Instant::now());
                }
                None => {
                    app.status_message = Some("Copilot CLI not found (run: gh copilot)".into());
                    status_since = Some(Instant::now());
                }
            },
            PendingAction::OpenEmbedded { id, cwd } => {
                let term_size = terminal.size()?;
                let (rows, cols) = embedded_terminal_size(term_size, app.terminal_fullscreen);
                match copilot_binary() {
                    Some(bin) => {
                        let cwd_arg = cwd.to_string_lossy();
                        let resume_arg = format!("--resume={id}");
                        match EmbeddedTerminal::spawn(
                            id.clone(),
                            &bin,
                            &["-C", cwd_arg.as_ref(), resume_arg.as_str()],
                            Some(&cwd),
                            rows,
                            cols,
                        ) {
                            Ok(term) => {
                                app.embedded_terminal = Some(term);
                                app.mode = Mode::Terminal;
                                app.active_panel = Panel::Detail;
                                app.terminal_fullscreen = false;
                            }
                            Err(e) => {
                                app.status_message = Some(format!("Failed to open: {e}"));
                                status_since = Some(Instant::now());
                            }
                        }
                    }
                    None => {
                        app.status_message = Some("Copilot CLI not found (run: gh copilot)".into());
                        status_since = Some(Instant::now());
                    }
                }
            }
            PendingAction::LaunchNewNative { dir } => match copilot_binary() {
                Some(bin) => {
                    let dir_str = dir.to_string_lossy().to_string();
                    app.capture_new_session_reload_baseline();
                    match ensure_tmux_session("new", &bin, &["-C", dir_str.as_str()], Some(&dir))
                        .and_then(|tmux_session| {
                            run_native_tmux_session(terminal, &tmux_session)?;
                            Ok(tmux_session)
                        }) {
                        Ok(tmux_session) => {
                            let new_session_id = app.reload_if_new_session_created();
                            let reuse_error = new_session_id
                                .as_deref()
                                .and_then(|id| reuse_tmux_session(&tmux_session, id).err());
                            app.clear_new_session_reload_watch();
                            last_new_session_reload_check = None;
                            app.mode = Mode::Normal;
                            app.terminal_fullscreen = false;
                            app.reload();
                            app.status_message = Some(match (new_session_id, reuse_error) {
                                (Some(_), Some(e)) => {
                                    format!("New session loaded; tmux reuse failed: {e}")
                                }
                                (Some(_), None) => "New session loaded".into(),
                                (None, _) => "Returned from native terminal".into(),
                            });
                        }
                        Err(e) => {
                            app.mode = Mode::Normal;
                            app.clear_new_session_reload_watch();
                            last_new_session_reload_check = None;
                            app.status_message = Some(format!("Failed to launch: {e}"));
                        }
                    }
                    status_since = Some(Instant::now());
                }
                None => {
                    app.mode = Mode::Normal;
                    app.clear_new_session_reload_watch();
                    app.status_message = Some("Copilot CLI not found (run: gh copilot)".into());
                    status_since = Some(Instant::now());
                }
            },
            PendingAction::OpenRemoteTask { url } => {
                match open_url_in_browser(&url) {
                    Ok(()) => {
                        app.status_message = Some("Opened remote task in browser".into());
                    }
                    Err(e) => {
                        app.status_message = Some(format!("Failed to open remote task: {e}"));
                    }
                }
                status_since = Some(Instant::now());
            }
        }

        // ── Detect embedded terminal exit ─────────────────────────────────────
        let exited = app
            .embedded_terminal
            .as_ref()
            .map(|t| t.is_exited())
            .unwrap_or(false);
        if exited {
            app.embedded_terminal = None;
            app.mode = Mode::Normal;
            app.terminal_fullscreen = false;
            app.clear_new_session_reload_watch();
            last_new_session_reload_check = None;
            app.reload();
            app.status_message = Some("Session ended".into());
            status_since = Some(Instant::now());
        }
        let should_check_for_new_session = app.has_new_session_reload_watch()
            && last_new_session_reload_check
                .map(|last_check| last_check.elapsed() >= Duration::from_secs(1))
                .unwrap_or(true);
        if should_check_for_new_session {
            last_new_session_reload_check = Some(Instant::now());
            if let Some(new_session_id) = app.reload_if_new_session_created() {
                let reuse_error = app
                    .embedded_terminal
                    .as_mut()
                    .and_then(|term| term.reuse_as_session(&new_session_id).err());
                last_new_session_reload_check = None;
                app.status_message = Some(match reuse_error {
                    Some(e) => format!("New session loaded; tmux reuse failed: {e}"),
                    None => "New session loaded".into(),
                });
                status_since = Some(Instant::now());
            }
        }

        if last_status_poll.elapsed() >= app.status_poll_interval() {
            if app.refresh_statuses() {
                notify_waiting_agent();
            }
            last_status_poll = Instant::now();
        }

        // ── Event handling ────────────────────────────────────────────────────
        let timeout = tick_rate
            .checked_sub(last_tick.elapsed())
            .unwrap_or(Duration::ZERO);

        if event::poll(timeout).context("Event poll failed")? {
            match event::read().context("Event read failed")? {
                Event::Key(key) if should_handle_key_event(key.kind) => {
                    handle_key(app, key.code, key.modifiers);
                    if app.status_message.is_some() {
                        status_since = Some(Instant::now());
                    }
                }
                Event::Mouse(mouse) => handle_mouse(app, mouse),
                _ => {}
            }
        }

        // Clear status message after 3 seconds.
        if let Some(t) = status_since {
            if t.elapsed() > Duration::from_secs(3) {
                app.status_message = None;
                status_since = None;
            }
        }

        if last_tick.elapsed() >= tick_rate {
            last_tick = Instant::now();
        }

        if app.should_quit {
            break;
        }
    }

    Ok(())
}

fn update_terminal_title<B: ratatui::backend::Backend + Write>(
    terminal: &mut Terminal<B>,
    app: &App,
    last_terminal_title: &mut String,
) -> Result<()>
where
    B::Error: Send + Sync + 'static,
{
    let title = app.terminal_title();
    if title != *last_terminal_title {
        execute!(terminal.backend_mut(), SetTitle(&title)).context("Failed to update title")?;
        *last_terminal_title = title;
    }
    Ok(())
}

fn run_native_tmux_session<B: ratatui::backend::Backend + Write>(
    terminal: &mut Terminal<B>,
    tmux_session: &str,
) -> Result<()>
where
    B::Error: Send + Sync + 'static,
{
    terminal.show_cursor().ok();
    disable_raw_mode().ok();
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )
    .context("Failed to leave TUI before native terminal attach")?;

    let attach_result = attach_tmux_session(tmux_session);

    enable_raw_mode().context("Failed to re-enable raw mode")?;
    execute!(
        terminal.backend_mut(),
        EnterAlternateScreen,
        EnableMouseCapture
    )
    .context("Failed to restore TUI after native terminal attach")?;
    terminal.clear()?;

    attach_result
}

fn notify_waiting_agent() {
    let _ = io::stdout().write_all(b"\x07");
    let _ = io::stdout().flush();
}

fn open_url_in_browser(url: &str) -> Result<()> {
    let (program, args) = browser_open_command(url);
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .output()
        .with_context(|| format!("{program} failed to launch"))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if stderr.is_empty() {
            anyhow::bail!("{program} exited with {}", output.status);
        }
        anyhow::bail!("{stderr}");
    }
}

#[cfg(target_os = "macos")]
fn browser_open_command(url: &str) -> (&'static str, Vec<&str>) {
    ("open", vec![url])
}

#[cfg(target_os = "windows")]
fn browser_open_command(url: &str) -> (&'static str, Vec<&str>) {
    ("cmd", vec!["/C", "start", "", url])
}

#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
fn browser_open_command(url: &str) -> (&'static str, Vec<&str>) {
    ("xdg-open", vec![url])
}

/// Calculate the rows/cols available for the embedded PTY given the terminal size.
fn embedded_terminal_size(term_size: ratatui::layout::Size, fullscreen: bool) -> (u16, u16) {
    if fullscreen {
        return (term_size.height.max(1), term_size.width.max(1));
    }

    let area = ratatui::layout::Rect::new(0, 0, term_size.width, term_size.height);
    let outer = ratatui::layout::Layout::default()
        .direction(ratatui::layout::Direction::Vertical)
        .constraints([
            ratatui::layout::Constraint::Min(0),
            ratatui::layout::Constraint::Length(1),
        ])
        .split(area);
    let cols = ratatui::layout::Layout::default()
        .direction(ratatui::layout::Direction::Horizontal)
        .constraints([
            ratatui::layout::Constraint::Percentage(35),
            ratatui::layout::Constraint::Percentage(65),
        ])
        .split(outer[0]);
    let detail_panel = cols[1];

    // Subtract borders (2 each side).
    let rows = detail_panel.height.saturating_sub(2).max(1);
    let cols = detail_panel.width.saturating_sub(2).max(1);
    (rows, cols)
}

fn resize_embedded_terminal(app: &mut App, term_size: ratatui::layout::Size) {
    if app.mode != Mode::Terminal {
        return;
    }
    let (rows, cols) = embedded_terminal_size(term_size, app.terminal_fullscreen);
    if let Some(term) = app.embedded_terminal.as_mut() {
        term.resize(rows, cols);
    }
}

fn handle_key(app: &mut App, key: KeyCode, modifiers: KeyModifiers) {
    match app.mode {
        Mode::Normal => handle_normal(app, key, modifiers),
        Mode::NewSessionDir => handle_input(app, key, modifiers),
        Mode::LaunchingNewSession => {}
        Mode::DirectoryFilter => handle_directory_filter_input(app, key, modifiers),
        Mode::Terminal => handle_terminal(app, key, modifiers),
        Mode::Help => handle_help(app, key),
    }
}

fn should_handle_key_event(kind: KeyEventKind) -> bool {
    matches!(kind, KeyEventKind::Press | KeyEventKind::Repeat)
}

fn handle_normal(app: &mut App, key: KeyCode, modifiers: KeyModifiers) {
    if key == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL) {
        app.should_quit = true;
        return;
    }
    if key == KeyCode::Char('?') {
        app.open_help();
        return;
    }
    if key == KeyCode::Tab {
        app.next_session_filter();
        return;
    }
    if key == KeyCode::BackTab {
        app.previous_session_filter();
        return;
    }
    if key == KeyCode::Char('/') {
        app.begin_directory_filter();
        return;
    }
    if key == KeyCode::Char('u') && modifiers.contains(KeyModifiers::CONTROL) {
        app.clear_directory_filter();
        return;
    }

    match app.active_panel {
        Panel::Sessions => match key {
            KeyCode::Char('q') | KeyCode::Char('Q') => app.should_quit = true,
            KeyCode::Char('j') | KeyCode::Down => app.move_down(),
            KeyCode::Char('k') | KeyCode::Up => app.move_up(),
            KeyCode::PageDown => app.scroll_detail_page_down(),
            KeyCode::PageUp => app.scroll_detail_page_up(),
            KeyCode::Enter | KeyCode::Char(' ') => app.select_current(),
            KeyCode::Char('o') => app.open_session_native(),
            KeyCode::Char('e') => app.open_session_embedded(),
            KeyCode::Char('n') => app.begin_new_session(),
            KeyCode::Char('r') => app.reload(),
            _ => {}
        },
        Panel::Detail => match key {
            KeyCode::Char('q') | KeyCode::Char('Q') => app.should_quit = true,
            KeyCode::Char('j') | KeyCode::Down => app.scroll_detail_down(),
            KeyCode::Char('k') | KeyCode::Up => app.scroll_detail_up(),
            KeyCode::PageDown => app.scroll_detail_page_down(),
            KeyCode::PageUp => app.scroll_detail_page_up(),
            KeyCode::Esc | KeyCode::Char('h') | KeyCode::Left => app.focus_sessions(),
            KeyCode::Char('o') => app.open_session_native(),
            KeyCode::Char('e') => app.open_session_embedded(),
            KeyCode::Char('n') => app.begin_new_session(),
            KeyCode::Char('r') => app.reload(),
            _ => {}
        },
    }
}

fn handle_mouse(app: &mut App, mouse: MouseEvent) {
    match app.mode {
        Mode::Normal => match mouse.kind {
            MouseEventKind::ScrollUp => app.scroll_detail_up(),
            MouseEventKind::ScrollDown => app.scroll_detail_down(),
            _ => {}
        },
        Mode::Help => match mouse.kind {
            MouseEventKind::ScrollUp => app.scroll_help_up(),
            MouseEventKind::ScrollDown => app.scroll_help_down(),
            _ => {}
        },
        Mode::Terminal if app.terminal_fullscreen => {
            let bytes = mouse_to_bytes(mouse);
            if let (false, Some(term)) = (bytes.is_empty(), app.embedded_terminal.as_ref()) {
                term.write_input(&bytes);
            }
        }
        _ => {}
    }
}

fn handle_help(app: &mut App, key: KeyCode) {
    match key {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') => app.close_help(),
        KeyCode::Char('j') | KeyCode::Down => app.scroll_help_down(),
        KeyCode::Char('k') | KeyCode::Up => app.scroll_help_up(),
        KeyCode::PageDown => app.scroll_help_page_down(),
        KeyCode::PageUp => app.scroll_help_page_up(),
        _ => {}
    }
}

fn handle_input(app: &mut App, key: KeyCode, modifiers: KeyModifiers) {
    match key {
        KeyCode::Enter => app.confirm_new_session(),
        KeyCode::Esc => app.cancel_input(),
        KeyCode::Down => app.select_next_directory_suggestion(),
        KeyCode::Up => app.select_previous_directory_suggestion(),
        KeyCode::Backspace => {
            app.input_buffer.pop();
            app.sync_directory_suggestion_cursor();
        }
        KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.input_buffer.clear();
            app.sync_directory_suggestion_cursor();
        }
        KeyCode::Char(c) => {
            app.input_buffer.push(c);
            app.sync_directory_suggestion_cursor();
        }
        _ => {}
    }
}

fn handle_directory_filter_input(app: &mut App, key: KeyCode, modifiers: KeyModifiers) {
    match key {
        KeyCode::Enter => app.confirm_directory_filter(),
        KeyCode::Esc => app.cancel_input(),
        KeyCode::Down => app.select_next_directory_suggestion(),
        KeyCode::Up => app.select_previous_directory_suggestion(),
        KeyCode::Backspace => {
            app.input_buffer.pop();
            app.sync_directory_suggestion_cursor();
        }
        KeyCode::Char('u') if modifiers.contains(KeyModifiers::CONTROL) => {
            app.input_buffer.clear();
            app.sync_directory_suggestion_cursor();
        }
        KeyCode::Char(c) => {
            app.input_buffer.push(c);
            app.sync_directory_suggestion_cursor();
        }
        _ => {}
    }
}

fn handle_terminal(app: &mut App, key: KeyCode, modifiers: KeyModifiers) {
    // Ctrl+W detaches from the embedded session.
    if key == KeyCode::Char('w') && modifiers.contains(KeyModifiers::CONTROL) {
        app.detach_terminal();
        return;
    }
    // Ctrl+F toggles fullscreen without sending the key to Copilot.
    if key == KeyCode::Char('f') && modifiers.contains(KeyModifiers::CONTROL) {
        app.toggle_terminal_fullscreen();
        return;
    }
    // Forward all other keys as byte sequences to the PTY.
    let bytes = key_to_bytes(key, modifiers);
    if !bytes.is_empty() {
        if let Some(ref term) = app.embedded_terminal {
            term.write_input(&bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_key_presses_and_repeats() {
        assert!(should_handle_key_event(KeyEventKind::Press));
        assert!(should_handle_key_event(KeyEventKind::Repeat));
    }

    #[test]
    fn ignores_key_releases() {
        assert!(!should_handle_key_event(KeyEventKind::Release));
    }

    #[test]
    fn question_mark_opens_and_closes_help() {
        let mut app = App::new(PathBuf::from("/tmp/copilot"), PathBuf::from("/tmp"));

        handle_normal(&mut app, KeyCode::Char('?'), KeyModifiers::NONE);
        assert_eq!(app.mode, Mode::Help);

        handle_help(&mut app, KeyCode::Esc);
        assert_eq!(app.mode, Mode::Normal);
    }

    #[test]
    fn slash_opens_directory_filter_prompt() {
        let mut app = App::new(PathBuf::from("/tmp/copilot"), PathBuf::from("/tmp"));

        handle_normal(&mut app, KeyCode::Char('/'), KeyModifiers::NONE);

        assert_eq!(app.mode, Mode::DirectoryFilter);
    }

    #[test]
    fn embedded_terminal_size_matches_detail_panel_inner_area() {
        let term_size = ratatui::layout::Size {
            width: 101,
            height: 31,
        };

        assert_eq!(embedded_terminal_size(term_size, false), (28, 64));
    }

    #[test]
    fn fullscreen_embedded_terminal_uses_full_terminal_size() {
        let term_size = ratatui::layout::Size {
            width: 101,
            height: 31,
        };

        assert_eq!(embedded_terminal_size(term_size, true), (31, 101));
    }

    #[test]
    fn browser_open_command_uses_url_directly() {
        let url = "https://github.com/owner/repo/pull/42/agent-sessions/task-1";
        let (program, args) = browser_open_command(url);

        #[cfg(target_os = "macos")]
        {
            assert_eq!(program, "open");
            assert_eq!(args, vec![url]);
        }
        #[cfg(target_os = "windows")]
        {
            assert_eq!(program, "cmd");
            assert_eq!(args, vec!["/C", "start", "", url]);
        }
        #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
        {
            assert_eq!(program, "xdg-open");
            assert_eq!(args, vec![url]);
        }
    }
}
