use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::ffi::OsStr;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, MutexGuard,
};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Keep generated tmux session names compact and safely below common terminal UI limits.
const TMUX_SESSION_NAME_MAX_LEN: usize = 80;
const TMUX_TITLE_CACHE_DURATION: Duration = Duration::from_millis(250);
const TMUX_CLIENT_FEATURES: &str = "RGB";
const COPILOT_TERM: &str = "xterm-256color";
const COPILOT_COLORTERM: &str = "truecolor";
pub(crate) const TMUX_SESSION_PREFIX: &str = "ghpilot_";
pub(crate) type TerminalParser = vt100::Parser<TerminalCallbacks>;

/// An embedded copilot terminal session running inside the right detail panel.
pub struct EmbeddedTerminal {
    /// Shared vt100 screen state updated by the background reader thread.
    parser: Arc<Mutex<TerminalParser>>,
    /// Write bytes (keyboard input) into the PTY master.
    writer: Mutex<Box<dyn std::io::Write + Send>>,
    /// Set to `true` by the reader thread when the child process exits.
    pub child_exited: Arc<AtomicBool>,
    /// Session ID (used for display purposes).
    pub session_id: String,
    /// tmux session that owns the Copilot CLI process.
    tmux_session: String,
    /// Cached title reported by tmux for the Copilot pane.
    tmux_title: Mutex<CachedTmuxTitle>,
    /// Current PTY dimensions.
    pub rows: u16,
    pub cols: u16,
    /// Keeps the master PTY file descriptor open for the lifetime of this struct.
    _master: Box<dyn portable_pty::MasterPty + Send>,
}

impl EmbeddedTerminal {
    /// Spawn `copilot_bin` with `args` inside a tmux-managed PTY of size `rows × cols`,
    /// with the working directory set to `cwd`.
    pub fn spawn(
        session_id: String,
        copilot_bin: &Path,
        args: &[impl AsRef<OsStr>],
        cwd: Option<&Path>,
        rows: u16,
        cols: u16,
    ) -> anyhow::Result<Self> {
        let pty_system = native_pty_system();
        let size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };

        let pair = pty_system.openpty(size)?;
        let slave = pair.slave;
        let master = pair.master;

        let tmux_session = tmux_session_name(&session_id);
        let copilot_command = copilot_shell_command(copilot_bin, args);

        // Attach to an existing tmux session for this copilot session, or create
        // one in the session's cwd. tmux owns the CLI process; the PTY here is
        // only the embedded client that renders inside the preview panel.
        let mut cmd = CommandBuilder::new("tmux");
        cmd.arg("-2");
        cmd.arg("-T");
        cmd.arg(TMUX_CLIENT_FEATURES);
        if tmux_has_session(&tmux_session) {
            cmd.arg("set-option");
            cmd.arg("-t");
            cmd.arg(&tmux_session);
            cmd.arg("status");
            cmd.arg("off");
            cmd.arg(";");
            cmd.arg("set-option");
            cmd.arg("-t");
            cmd.arg(&tmux_session);
            cmd.arg("mouse");
            cmd.arg("on");
            cmd.arg(";");
            cmd.arg("attach-session");
            cmd.arg("-t");
            cmd.arg(&tmux_session);
        } else {
            cmd.arg("new-session");
            cmd.arg("-d");
            cmd.arg("-s");
            cmd.arg(&tmux_session);
            if let Some(dir) = cwd {
                cmd.arg("-c");
                cmd.arg(dir);
            }
            cmd.arg(copilot_command);
            cmd.arg(";");
            cmd.arg("set-option");
            cmd.arg("-t");
            cmd.arg(&tmux_session);
            cmd.arg("status");
            cmd.arg("off");
            cmd.arg(";");
            cmd.arg("set-option");
            cmd.arg("-t");
            cmd.arg(&tmux_session);
            cmd.arg("mouse");
            cmd.arg("on");
            cmd.arg(";");
            cmd.arg("attach-session");
            cmd.arg("-t");
            cmd.arg(&tmux_session);
        }
        apply_terminal_env(&mut cmd);

        // Spawn the process inside the slave PTY, then drop the slave so that
        // we receive EOF on the master when the child exits.
        let child = slave.spawn_command(cmd)?;
        drop(slave);

        let writer = master.take_writer()?;
        let reader = master.try_clone_reader()?;

        let parser = Arc::new(Mutex::new(vt100::Parser::new_with_callbacks(
            rows,
            cols,
            1000,
            TerminalCallbacks::default(),
        )));
        let child_exited = Arc::new(AtomicBool::new(false));

        // Background reader: feeds raw PTY bytes into the vt100 parser.
        let parser_clone = Arc::clone(&parser);
        let exited_clone = Arc::clone(&child_exited);
        thread::spawn(move || {
            let mut reader = reader;
            let mut child = child;
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => {
                        exited_clone.store(true, Ordering::Relaxed);
                        break;
                    }
                    Ok(n) => {
                        lock_parser(&parser_clone).process(&buf[..n]);
                    }
                }
            }
            // Reap the child to avoid zombie processes.
            let _ = child.wait();
        });

        Ok(Self {
            parser,
            writer: Mutex::new(writer),
            child_exited,
            session_id,
            tmux_session,
            tmux_title: Mutex::new(CachedTmuxTitle::default()),
            rows,
            cols,
            _master: master,
        })
    }

    /// Send raw bytes to the PTY (keyboard input).
    pub fn write_input(&self, bytes: &[u8]) {
        if let Ok(mut w) = self.writer.lock() {
            let _ = w.write_all(bytes);
            let _ = w.flush();
        }
    }

    /// Returns `true` when the child process has exited.
    pub fn is_exited(&self) -> bool {
        self.child_exited.load(Ordering::Relaxed)
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        if self.rows == rows && self.cols == cols {
            return;
        }
        let size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        let _ = self._master.resize(size);
        self.parser().screen_mut().set_size(rows, cols);
        self.rows = rows;
        self.cols = cols;
    }

    pub(crate) fn parser(&self) -> MutexGuard<'_, TerminalParser> {
        lock_parser(&self.parser)
    }

    pub fn terminal_title(&self) -> Option<String> {
        self.tmux_terminal_title()
            .or_else(|| self.parser().callbacks().window_title.clone())
    }

    fn tmux_terminal_title(&self) -> Option<String> {
        let Ok(mut cache) = self.tmux_title.lock() else {
            return None;
        };
        if cache
            .last_checked
            .map(|last_checked| last_checked.elapsed() < TMUX_TITLE_CACHE_DURATION)
            .unwrap_or(false)
        {
            return cache.value.clone();
        }

        cache.last_checked = Some(Instant::now());
        cache.value = tmux_pane_title(&self.tmux_session);
        cache.value.clone()
    }

    /// Rename the backing tmux session to the deterministic name for `session_id`.
    ///
    /// This is used after a newly launched Copilot process writes its real
    /// session state. If the terminal is already using that deterministic tmux
    /// name, only the stored session id is updated. If another tmux session is
    /// already using the target name, this leaves the current tmux session
    /// unchanged rather than replacing an existing active session.
    pub fn reuse_as_session(&mut self, session_id: &str) -> anyhow::Result<()> {
        let target = tmux_session_name(session_id);
        if self.tmux_session == target {
            self.session_id = session_id.to_string();
            return Ok(());
        }

        if !tmux_has_session(&target) {
            let status = Command::new("tmux")
                .arg("rename-session")
                .arg("-t")
                .arg(&self.tmux_session)
                .arg(&target)
                .status()?;
            if !status.success() {
                anyhow::bail!("tmux rename-session exited with {status}");
            }
        }

        if tmux_has_session(&target) {
            self.tmux_session = target;
            self.session_id = session_id.to_string();
        }

        Ok(())
    }
}

fn lock_parser(parser: &Mutex<TerminalParser>) -> MutexGuard<'_, TerminalParser> {
    parser
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Default)]
pub(crate) struct TerminalCallbacks {
    window_title: Option<String>,
}

#[derive(Default)]
struct CachedTmuxTitle {
    last_checked: Option<Instant>,
    value: Option<String>,
}

impl vt100::Callbacks for TerminalCallbacks {
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.window_title = Some(String::from_utf8_lossy(title).to_string());
    }
}

fn tmux_has_session(tmux_session: &str) -> bool {
    Command::new("tmux")
        .arg("has-session")
        .arg("-t")
        .arg(tmux_session)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn tmux_pane_title(tmux_session: &str) -> Option<String> {
    let output = Command::new("tmux")
        .arg("display-message")
        .arg("-p")
        .arg("-t")
        .arg(tmux_session)
        .arg("#{pane_title}")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    title_from_tmux_output(&output.stdout)
}

pub fn ensure_tmux_session(
    session_id: &str,
    copilot_bin: &Path,
    args: &[impl AsRef<OsStr>],
    cwd: Option<&Path>,
) -> anyhow::Result<String> {
    let tmux_session = tmux_session_name(session_id);
    if tmux_has_session(&tmux_session) {
        return Ok(tmux_session);
    }

    let mut command = Command::new("tmux");
    command
        .arg("new-session")
        .arg("-d")
        .arg("-s")
        .arg(&tmux_session);
    if let Some(dir) = cwd {
        command.arg("-c").arg(dir);
    }
    command.arg(copilot_shell_command(copilot_bin, args));
    let status = command.status()?;
    if !status.success() {
        anyhow::bail!("tmux new-session exited with {status}");
    }

    configure_tmux_session(&tmux_session)?;
    Ok(tmux_session)
}

pub fn attach_tmux_session(tmux_session: &str) -> anyhow::Result<()> {
    let status = Command::new("tmux")
        .arg("-2")
        .arg("-T")
        .arg(TMUX_CLIENT_FEATURES)
        .arg("attach-session")
        .arg("-t")
        .arg(tmux_session)
        .status()?;
    if !status.success() {
        anyhow::bail!("tmux attach-session exited with {status}");
    }
    Ok(())
}

pub fn reuse_tmux_session(tmux_session: &str, session_id: &str) -> anyhow::Result<String> {
    let target = tmux_session_name(session_id);
    if tmux_session == target {
        return Ok(target);
    }

    if !tmux_has_session(&target) && tmux_has_session(tmux_session) {
        let status = Command::new("tmux")
            .arg("rename-session")
            .arg("-t")
            .arg(tmux_session)
            .arg(&target)
            .status()?;
        if !status.success() {
            anyhow::bail!("tmux rename-session exited with {status}");
        }
    }

    Ok(if tmux_has_session(&target) {
        target
    } else {
        tmux_session.to_string()
    })
}

fn configure_tmux_session(tmux_session: &str) -> anyhow::Result<()> {
    let status = Command::new("tmux")
        .arg("set-option")
        .arg("-t")
        .arg(tmux_session)
        .arg("status")
        .arg("off")
        .arg(";")
        .arg("set-option")
        .arg("-t")
        .arg(tmux_session)
        .arg("mouse")
        .arg("on")
        .status()?;
    if !status.success() {
        anyhow::bail!("tmux set-option exited with {status}");
    }
    Ok(())
}

fn title_from_tmux_output(output: &[u8]) -> Option<String> {
    let title = String::from_utf8_lossy(output).trim().to_string();
    (!title.is_empty()).then_some(title)
}

pub(crate) fn tmux_session_name(session_id: &str) -> String {
    let suffix = if session_id == "new" {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        format!("new_{millis}")
    } else {
        session_id.to_string()
    };

    let sanitized: String = suffix
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();

    format!(
        "{TMUX_SESSION_PREFIX}{}",
        sanitized
            .chars()
            .take(TMUX_SESSION_NAME_MAX_LEN)
            .collect::<String>()
    )
}

fn copilot_shell_command(copilot_bin: &Path, args: &[impl AsRef<OsStr>]) -> String {
    copilot_shell_command_with_env(copilot_bin, args, |name| std::env::var(name).ok())
}

fn copilot_shell_command_with_env(
    copilot_bin: &Path,
    args: &[impl AsRef<OsStr>],
    env: impl Fn(&str) -> Option<String>,
) -> String {
    let words = std::iter::once("env".to_string())
        .chain(terminal_env_assignments(env))
        .chain(std::iter::once(
            copilot_bin.as_os_str().to_string_lossy().to_string(),
        ))
        .chain(
            args.iter()
                .map(|arg| arg.as_ref().to_string_lossy().to_string()),
        );
    shell_words::join(words)
}

fn terminal_env_assignments(env: impl Fn(&str) -> Option<String>) -> Vec<String> {
    // TERM and COLORTERM get color-capable fallbacks. Terminal-emulator-specific
    // variables should only be forwarded when the host set them; otherwise we
    // would mislead Copilot about which terminal is actually rendering output.
    let terminal_env = [
        ("TERM", COPILOT_TERM),
        ("COLORTERM", COPILOT_COLORTERM),
        ("TERM_PROGRAM", ""),
        ("TERM_PROGRAM_VERSION", ""),
        ("WEZTERM_EXECUTABLE", ""),
        ("WEZTERM_PANE", ""),
        ("KITTY_WINDOW_ID", ""),
        ("VTE_VERSION", ""),
    ];

    terminal_env
        .into_iter()
        .filter_map(|(name, fallback)| {
            env(name)
                .filter(|value| !value.trim().is_empty())
                .or_else(|| (!fallback.is_empty()).then(|| fallback.to_string()))
                .map(|value| format!("{name}={value}"))
        })
        .collect()
}

fn apply_terminal_env(cmd: &mut CommandBuilder) {
    for assignment in terminal_env_assignments(|name| std::env::var(name).ok()) {
        if let Some((name, value)) = assignment.split_once('=') {
            cmd.env(name, value);
        }
    }
}

// ── Key → byte sequence mapping ──────────────────────────────────────────────

use crossterm::event::{KeyCode, KeyModifiers, MouseEvent, MouseEventKind};

/// Convert a crossterm key event into the byte sequence expected by the PTY.
pub fn key_to_bytes(code: KeyCode, modifiers: KeyModifiers) -> Vec<u8> {
    // Ctrl+letter
    if modifiers.contains(KeyModifiers::CONTROL) {
        if let KeyCode::Char(c) = code {
            let base = c.to_ascii_lowercase();
            // Ctrl+A..Z → 0x01..0x1A
            if base.is_ascii_alphabetic() {
                return vec![(base as u8) - b'a' + 1];
            }
            match base {
                '[' => return vec![0x1b],
                '\\' => return vec![0x1c],
                ']' => return vec![0x1d],
                '^' => return vec![0x1e],
                '_' => return vec![0x1f],
                _ => {}
            }
        }
    }

    match code {
        KeyCode::Char(c) => c.to_string().into_bytes(),
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => vec![0x1b, b'[', b'Z'],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => vec![0x1b, b'[', b'A'],
        KeyCode::Down => vec![0x1b, b'[', b'B'],
        KeyCode::Right => vec![0x1b, b'[', b'C'],
        KeyCode::Left => vec![0x1b, b'[', b'D'],
        KeyCode::Home => vec![0x1b, b'[', b'H'],
        KeyCode::End => vec![0x1b, b'[', b'F'],
        KeyCode::PageUp => vec![0x1b, b'[', b'5', b'~'],
        KeyCode::PageDown => vec![0x1b, b'[', b'6', b'~'],
        KeyCode::Delete => vec![0x1b, b'[', b'3', b'~'],
        KeyCode::Insert => vec![0x1b, b'[', b'2', b'~'],
        KeyCode::F(1) => vec![0x1b, b'O', b'P'],
        KeyCode::F(2) => vec![0x1b, b'O', b'Q'],
        KeyCode::F(3) => vec![0x1b, b'O', b'R'],
        KeyCode::F(4) => vec![0x1b, b'O', b'S'],
        KeyCode::F(5) => vec![0x1b, b'[', b'1', b'5', b'~'],
        KeyCode::F(6) => vec![0x1b, b'[', b'1', b'7', b'~'],
        KeyCode::F(7) => vec![0x1b, b'[', b'1', b'8', b'~'],
        KeyCode::F(8) => vec![0x1b, b'[', b'1', b'9', b'~'],
        KeyCode::F(9) => vec![0x1b, b'[', b'2', b'0', b'~'],
        KeyCode::F(10) => vec![0x1b, b'[', b'2', b'1', b'~'],
        KeyCode::F(11) => vec![0x1b, b'[', b'2', b'3', b'~'],
        KeyCode::F(12) => vec![0x1b, b'[', b'2', b'4', b'~'],
        _ => vec![],
    }
}

/// Convert mouse wheel events into xterm SGR mouse sequences for tmux.
///
/// The sequence format is `ESC [ < button ; column ; row M`; xterm coordinates
/// are 1-based, while crossterm reports 0-based terminal positions.
pub fn mouse_to_bytes(event: MouseEvent) -> Vec<u8> {
    let button = match event.kind {
        MouseEventKind::ScrollUp => 64,
        MouseEventKind::ScrollDown => 65,
        _ => return vec![],
    };
    format!(
        "\x1b[<{button};{};{}M",
        event.column.saturating_add(1),
        event.row.saturating_add(1)
    )
    .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_captures_window_title_from_osc_2() {
        let mut parser = vt100::Parser::new_with_callbacks(24, 80, 0, TerminalCallbacks::default());

        parser.process(b"\x1b]2;Fix title from Copilot CLI\x07");

        assert_eq!(
            parser.callbacks().window_title.as_deref(),
            Some("Fix title from Copilot CLI")
        );
    }

    #[test]
    fn parser_captures_window_title_from_osc_0() {
        let mut parser = vt100::Parser::new_with_callbacks(24, 80, 0, TerminalCallbacks::default());

        parser.process(b"\x1b]0;Current Copilot session\x07");

        assert_eq!(
            parser.callbacks().window_title.as_deref(),
            Some("Current Copilot session")
        );
    }

    #[test]
    fn tmux_title_output_is_trimmed_and_ignores_empty_titles() {
        assert_eq!(
            title_from_tmux_output(b"Updated Copilot Title\n").as_deref(),
            Some("Updated Copilot Title")
        );
        assert_eq!(title_from_tmux_output(b" \n"), None);
    }

    #[test]
    fn copilot_shell_command_preserves_terminal_capability_env() {
        let command = copilot_shell_command_with_env(
            Path::new("/usr/local/bin/copilot"),
            &["-C", "/tmp/project dir", "--resume=session-1"],
            |name| match name {
                "TERM" => Some("xterm-kitty".to_string()),
                "COLORTERM" => Some("truecolor".to_string()),
                "TERM_PROGRAM" => Some("kitty".to_string()),
                _ => None,
            },
        );

        assert_eq!(
            command,
            "env 'TERM=xterm-kitty' 'COLORTERM=truecolor' 'TERM_PROGRAM=kitty' /usr/local/bin/copilot -C '/tmp/project dir' '--resume=session-1'"
        );
    }

    #[test]
    fn copilot_shell_command_uses_color_fallbacks() {
        let command = copilot_shell_command_with_env(
            Path::new("/usr/local/bin/copilot"),
            &["-C", "/tmp"],
            |_| None,
        );

        assert_eq!(
            command,
            "env 'TERM=xterm-256color' 'COLORTERM=truecolor' /usr/local/bin/copilot -C /tmp"
        );
    }
}
