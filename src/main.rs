mod app;
mod session;
mod terminal;
mod ui;

use anyhow::{Context, Result};
use app::{App, Mode, Panel, PendingAction};
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use session::copilot_binary;
use std::{
    io,
    path::PathBuf,
    time::{Duration, Instant},
};
use terminal::{key_to_bytes, EmbeddedTerminal};

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

fn run_event_loop<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
) -> Result<()>
where
    B::Error: Send + Sync + 'static,
{
    let tick_rate = Duration::from_millis(100); // balanced: responsive terminal output + low CPU
    let mut last_tick = Instant::now();
    let mut status_since: Option<Instant> = None;

    loop {
        resize_embedded_terminal(app, terminal.size()?);
        terminal.draw(|f| ui::draw(f, app))?;

        // ── Spawn pending embedded terminals ─────────────────────────────────
        let action = std::mem::replace(&mut app.pending_action, PendingAction::None);
        match action {
            PendingAction::None => {}
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
            PendingAction::LaunchNew { dir } => {
                let term_size = terminal.size()?;
                let (rows, cols) = embedded_terminal_size(term_size, app.terminal_fullscreen);
                match copilot_binary() {
                    Some(bin) => {
                        let dir_str = dir.to_string_lossy().to_string();
                        match EmbeddedTerminal::spawn(
                            "new".into(),
                            &bin,
                            &["-C", dir_str.as_str()],
                            Some(&dir),
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
                                app.status_message = Some(format!("Failed to launch: {e}"));
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
            app.reload();
            app.status_message = Some("Session ended".into());
            status_since = Some(Instant::now());
        }

        // ── Event handling ────────────────────────────────────────────────────
        let timeout = tick_rate
            .checked_sub(last_tick.elapsed())
            .unwrap_or(Duration::ZERO);

        if event::poll(timeout).context("Event poll failed")? {
            match event::read().context("Event read failed")? {
                Event::Key(key) => {
                    handle_key(app, key.code, key.modifiers);
                    if app.status_message.is_some() {
                        status_since = Some(Instant::now());
                    }
                }
                Event::Mouse(mouse) => handle_mouse(app, mouse.kind),
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

/// Calculate the rows/cols available for the embedded PTY given the terminal size.
fn embedded_terminal_size(term_size: ratatui::layout::Size, fullscreen: bool) -> (u16, u16) {
    let (height, width) = if fullscreen {
        (term_size.height, term_size.width)
    } else {
        // Body + footer(1), with the terminal in the right 65% detail panel.
        (
            term_size.height.saturating_sub(1),
            term_size.width * 65 / 100,
        )
    };
    // Subtract borders (2 each side) and the 1-row "LIVE" header bar.
    let rows = height.saturating_sub(3).max(1); // 2 borders + 1 live-bar
    let cols = width.saturating_sub(2).max(1); // left + right borders
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
        Mode::NewSessionDir => handle_input(app, key),
        Mode::Terminal => handle_terminal(app, key, modifiers),
    }
}

fn handle_normal(app: &mut App, key: KeyCode, modifiers: KeyModifiers) {
    if key == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL) {
        app.should_quit = true;
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
            KeyCode::Char('c') => app.toggle_current_group_collapsed(),
            KeyCode::Char('f') => app.toggle_directory_focus(),
            KeyCode::Char('o') => app.open_session_embedded(),
            KeyCode::Char('n') => app.begin_new_session(),
            KeyCode::Char('r') => app.reload(),
            KeyCode::Esc => app.clear_directory_focus(),
            _ => {}
        },
        Panel::Detail => match key {
            KeyCode::Char('q') | KeyCode::Char('Q') => app.should_quit = true,
            KeyCode::Char('j') | KeyCode::Down => app.scroll_detail_down(),
            KeyCode::Char('k') | KeyCode::Up => app.scroll_detail_up(),
            KeyCode::PageDown => app.scroll_detail_page_down(),
            KeyCode::PageUp => app.scroll_detail_page_up(),
            KeyCode::Esc | KeyCode::Char('h') | KeyCode::Left => app.focus_sessions(),
            KeyCode::Char('f') => app.toggle_directory_focus(),
            KeyCode::Char('o') => app.open_session_embedded(),
            KeyCode::Char('n') => app.begin_new_session(),
            KeyCode::Char('r') => app.reload(),
            _ => {}
        },
    }
}

fn handle_mouse(app: &mut App, kind: MouseEventKind) {
    if app.mode != Mode::Normal {
        return;
    }
    match kind {
        MouseEventKind::ScrollUp => app.scroll_detail_up(),
        MouseEventKind::ScrollDown => app.scroll_detail_down(),
        _ => {}
    }
}

fn handle_input(app: &mut App, key: KeyCode) {
    match key {
        KeyCode::Enter => app.confirm_new_session(),
        KeyCode::Esc => app.cancel_input(),
        KeyCode::Backspace => {
            app.input_buffer.pop();
        }
        KeyCode::Char(c) => {
            app.input_buffer.push(c);
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
