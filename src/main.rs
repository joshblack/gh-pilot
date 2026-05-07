mod app;
mod session;
mod terminal;
mod ui;

use anyhow::{Context, Result};
use app::{App, Mode, Panel, PendingAction};
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use session::copilot_binary;
use terminal::{key_to_bytes, EmbeddedTerminal};
use std::{
    io,
    path::PathBuf,
    time::{Duration, Instant},
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
        terminal.draw(|f| ui::draw(f, app))?;

        // ── Spawn pending embedded terminals ─────────────────────────────────
        let action = std::mem::replace(&mut app.pending_action, PendingAction::None);
        match action {
            PendingAction::None => {}
            PendingAction::OpenEmbedded { id, cwd } => {
                let term_size = terminal.size()?;
                let (rows, cols) = embedded_terminal_size(term_size);
                match copilot_binary() {
                    Some(bin) => {
                        let resume_arg = format!("--resume={id}");
                        match EmbeddedTerminal::spawn(
                            id.clone(),
                            &bin,
                            &[resume_arg.as_str()],
                            Some(&cwd),
                            rows,
                            cols,
                        ) {
                            Ok(term) => {
                                app.embedded_terminal = Some(term);
                                app.mode = Mode::Terminal;
                                app.active_panel = Panel::Detail;
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
                let (rows, cols) = embedded_terminal_size(term_size);
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
            app.reload();
            app.status_message = Some("Session ended".into());
            status_since = Some(Instant::now());
        }

        // ── Event handling ────────────────────────────────────────────────────
        let timeout = tick_rate
            .checked_sub(last_tick.elapsed())
            .unwrap_or(Duration::ZERO);

        if event::poll(timeout).context("Event poll failed")? {
            if let Event::Key(key) = event::read().context("Event read failed")? {
                handle_key(app, key.code, key.modifiers);
                if app.status_message.is_some() {
                    status_since = Some(Instant::now());
                }
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
fn embedded_terminal_size(term_size: ratatui::layout::Size) -> (u16, u16) {
    // Outer layout: header(3) + body + footer(3) → body_height = total - 6
    let body_height = term_size.height.saturating_sub(6);
    // Right panel is 65% of total width.
    let panel_width = term_size.width * 65 / 100;
    // Subtract borders (2 each side) and the 1-row "LIVE" header bar.
    let rows = body_height.saturating_sub(3).max(1); // 2 borders + 1 live-bar
    let cols = panel_width.saturating_sub(2).max(1); // left + right borders
    (rows, cols)
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
            KeyCode::Enter | KeyCode::Char(' ') => app.select_current(),
            KeyCode::Char('o') => app.open_session_embedded(),
            KeyCode::Char('n') => app.begin_new_session(),
            KeyCode::Char('r') => app.reload(),
            _ => {}
        },
        Panel::Detail => match key {
            KeyCode::Char('q') | KeyCode::Char('Q') => app.should_quit = true,
            KeyCode::Char('j') | KeyCode::Down => app.scroll_detail_down(),
            KeyCode::Char('k') | KeyCode::Up => app.scroll_detail_up(),
            KeyCode::Esc | KeyCode::Char('h') | KeyCode::Left => app.focus_sessions(),
            KeyCode::Char('o') => app.open_session_embedded(),
            KeyCode::Char('n') => app.begin_new_session(),
            KeyCode::Char('r') => app.reload(),
            _ => {}
        },
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
    // Forward all other keys as byte sequences to the PTY.
    let bytes = key_to_bytes(key, modifiers);
    if !bytes.is_empty() {
        if let Some(ref term) = app.embedded_terminal {
            term.write_input(&bytes);
        }
    }
}

