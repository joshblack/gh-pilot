use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::ffi::OsStr;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

/// Keep generated tmux session names compact and safely below common terminal UI limits.
const TMUX_SESSION_NAME_MAX_LEN: usize = 80;

/// An embedded copilot terminal session running inside the right detail panel.
pub struct EmbeddedTerminal {
    /// Shared vt100 screen state updated by the background reader thread.
    pub parser: Arc<Mutex<vt100::Parser>>,
    /// Write bytes (keyboard input) into the PTY master.
    writer: Mutex<Box<dyn std::io::Write + Send>>,
    /// Set to `true` by the reader thread when the child process exits.
    pub child_exited: Arc<AtomicBool>,
    /// Session ID (used for display purposes).
    pub session_id: String,
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
        let copilot_command = shell_command(copilot_bin, args);

        // Attach to an existing tmux session for this copilot session, or create
        // one in the session's cwd. tmux owns the CLI process; the PTY here is
        // only the embedded client that renders inside the preview panel.
        let mut cmd = CommandBuilder::new("tmux");
        if tmux_has_session(&tmux_session) {
            cmd.arg("attach-session");
            cmd.arg("-t");
            cmd.arg(&tmux_session);
        } else {
            cmd.arg("new-session");
            cmd.arg("-s");
            cmd.arg(&tmux_session);
            if let Some(dir) = cwd {
                cmd.arg("-c");
                cmd.arg(dir);
            }
            cmd.arg(copilot_command);
        }
        // Tell copilot it's running in a color-capable terminal.
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");

        // Spawn the process inside the slave PTY, then drop the slave so that
        // we receive EOF on the master when the child exits.
        let child = slave.spawn_command(cmd)?;
        drop(slave);

        let writer = master.take_writer()?;
        let reader = master.try_clone_reader()?;

        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 1000)));
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
                        parser_clone.lock().unwrap().process(&buf[..n]);
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
        if let Ok(mut parser) = self.parser.lock() {
            parser.screen_mut().set_size(rows, cols);
        }
        self.rows = rows;
        self.cols = cols;
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

fn tmux_session_name(session_id: &str) -> String {
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
        "ghmc_{}",
        sanitized
            .chars()
            .take(TMUX_SESSION_NAME_MAX_LEN)
            .collect::<String>()
    )
}

fn shell_command(copilot_bin: &Path, args: &[impl AsRef<OsStr>]) -> String {
    let words = std::iter::once(copilot_bin.as_os_str().to_string_lossy().to_string()).chain(
        args.iter()
            .map(|arg| arg.as_ref().to_string_lossy().to_string()),
    );
    shell_words::join(words)
}

// ── Key → byte sequence mapping ──────────────────────────────────────────────

use crossterm::event::{KeyCode, KeyModifiers};

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
