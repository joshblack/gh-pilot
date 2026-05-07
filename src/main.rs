mod app;
mod session;
mod ui;

use anyhow::{Context, Result};
use app::{App, Mode, Panel, PendingAction};
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{
        disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
    },
};
use ratatui::{backend::CrosstermBackend, Terminal};
use session::copilot_binary;
use std::{
    io,
    path::PathBuf,
    process::Command,
    time::{Duration, Instant},
};

fn main() -> Result<()> {
    // Determine the copilot config dir (~/.copilot by default)
    let copilot_dir = session::copilot_dir();

    // The launch directory defaults to where mission-control is run from
    let launch_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    let mut app = App::new(copilot_dir, launch_dir);

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
    let mut status_since: Option<Instant> = None;

    loop {
        terminal.draw(|f| ui::draw(f, app))?;

        // Handle any pending action (launch/resume copilot) before processing events
        let action = std::mem::replace(&mut app.pending_action, PendingAction::None);
        match action {
            PendingAction::None => {}
            PendingAction::LaunchNew { dir } => {
                run_copilot_suspended(terminal, &["-C", &dir.to_string_lossy()])?;
                app.reload();
                app.status_message = Some("Session launched".to_string());
                status_since = Some(Instant::now());
            }
            PendingAction::ResumeSession { id } => {
                let resume_arg = format!("--resume={id}");
                run_copilot_suspended(terminal, &[&resume_arg])?;
                app.reload();
                app.status_message = Some("Session resumed".to_string());
                status_since = Some(Instant::now());
            }
        }

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

/// Suspend the TUI, run copilot with the given args, then restore the TUI.
fn run_copilot_suspended<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    args: &[&str],
) -> Result<()> {
    // Use stdout directly for crossterm commands (doesn't require B: Write)
    let mut stdout = io::stdout();

    // Restore the terminal for the child process
    disable_raw_mode().context("Failed to disable raw mode")?;
    execute!(stdout, LeaveAlternateScreen, DisableMouseCapture)
        .context("Failed to leave alternate screen")?;
    terminal.show_cursor().ok();

    let status = if let Some(binary) = copilot_binary() {
        Command::new(&binary).args(args).status()
    } else {
        // Fall back to gh copilot
        Command::new("gh")
            .arg("copilot")
            .arg("--")
            .args(args)
            .status()
    };

    match status {
        Ok(s) if !s.success() => {
            // Non-zero exit is fine (user may have quit copilot normally)
        }
        Err(e) => {
            eprintln!("\nFailed to launch Copilot: {e}");
            eprintln!("Make sure the Copilot CLI is installed: gh copilot");
            // Give the user time to read the message
            std::thread::sleep(Duration::from_secs(2));
        }
        _ => {}
    }

    // Re-enter the TUI
    enable_raw_mode().context("Failed to re-enable raw mode")?;
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
        .context("Failed to re-enter alternate screen")?;
    terminal.clear().ok();

    Ok(())
}

fn handle_key(app: &mut App, key: KeyCode, modifiers: KeyModifiers) {
    match app.mode {
        Mode::Normal => handle_normal(app, key, modifiers),
        Mode::NewSessionDir => handle_input(app, key),
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
            KeyCode::Char('o') => app.open_session(),
            KeyCode::Char('n') => app.begin_new_session(),
            KeyCode::Char('r') => app.reload(),
            _ => {}
        },
        Panel::Detail => match key {
            KeyCode::Char('q') | KeyCode::Char('Q') => app.should_quit = true,
            KeyCode::Char('j') | KeyCode::Down => app.scroll_detail_down(),
            KeyCode::Char('k') | KeyCode::Up => app.scroll_detail_up(),
            KeyCode::Esc | KeyCode::Char('h') | KeyCode::Left => app.focus_sessions(),
            KeyCode::Char('o') => app.open_session(),
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
