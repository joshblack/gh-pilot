mod app;
mod session;
mod ui;

use anyhow::{Context, Result};
use app::{App, Mode, Panel};
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{
        disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
    },
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::{
    io,
    path::PathBuf,
    time::{Duration, Instant},
};

fn sessions_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("gh-mission-control")
        .join("sessions")
}

fn main() -> Result<()> {
    let sessions_dir = sessions_dir();

    // Allow --sessions-dir <path> override
    let args: Vec<String> = std::env::args().collect();
    let sessions_dir = if let Some(idx) = args.iter().position(|a| a == "--sessions-dir") {
        args.get(idx + 1)
            .map(PathBuf::from)
            .unwrap_or(sessions_dir)
    } else {
        sessions_dir
    };

    let mut app = App::new(sessions_dir).context("Failed to initialize application")?;

    // Setup terminal
    enable_raw_mode().context("Failed to enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
        .context("Failed to enter alternate screen")?;

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).context("Failed to create terminal")?;
    terminal.clear()?;

    let result = run_event_loop(&mut terminal, &mut app);

    // Restore terminal
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
    let tick_rate = Duration::from_millis(250);
    let mut last_tick = Instant::now();
    // Track when to clear the status message
    let mut status_since: Option<Instant> = None;

    loop {
        terminal.draw(|f| ui::draw(f, app))?;

        let timeout = tick_rate
            .checked_sub(last_tick.elapsed())
            .unwrap_or(Duration::ZERO);

        if event::poll(timeout).context("Event poll failed")? {
            if let Event::Key(key) = event::read().context("Event read failed")? {
                handle_key(app, key.code, key.modifiers);

                // Track status message display time
                if app.status_message.is_some() {
                    status_since = Some(Instant::now());
                }
            }
        }

        // Clear status message after 2 seconds
        if let Some(t) = status_since {
            if t.elapsed() > Duration::from_secs(2) {
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

fn handle_key(app: &mut App, key: KeyCode, modifiers: KeyModifiers) {
    match app.mode {
        Mode::Normal => handle_normal(app, key, modifiers),
        Mode::NewSessionName | Mode::NewSessionPath => handle_input(app, key),
        Mode::ConfirmDelete => handle_confirm_delete(app, key),
    }
}

fn handle_normal(app: &mut App, key: KeyCode, modifiers: KeyModifiers) {
    // Ctrl+C / q always quit
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
            KeyCode::Char('n') => app.begin_new_session(),
            KeyCode::Char('d') => app.begin_delete(),
            KeyCode::Char('t') => app.toggle_status(),
            KeyCode::Char('r') => app.reload(),
            _ => {}
        },
        Panel::Detail => match key {
            KeyCode::Char('q') | KeyCode::Char('Q') => app.should_quit = true,
            KeyCode::Char('j') | KeyCode::Down => app.scroll_log_down(),
            KeyCode::Char('k') | KeyCode::Up => app.scroll_log_up(),
            KeyCode::Esc | KeyCode::Char('h') | KeyCode::Left => app.focus_sessions(),
            KeyCode::Char('d') => app.begin_delete(),
            KeyCode::Char('t') => app.toggle_status(),
            KeyCode::Char('r') => app.reload(),
            _ => {}
        },
    }
}

fn handle_input(app: &mut App, key: KeyCode) {
    match key {
        KeyCode::Enter => app.confirm_input(),
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

fn handle_confirm_delete(app: &mut App, key: KeyCode) {
    match key {
        KeyCode::Char('y') | KeyCode::Char('Y') => app.confirm_delete(),
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => app.cancel_delete(),
        _ => {}
    }
}
