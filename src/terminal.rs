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
pub(crate) const TMUX_SESSION_PREFIX: &str = "ghpilot_";
pub(crate) type TerminalParser = vt100::Parser<TerminalCallbacks>;

/// An embedded copilot terminal session running inside the right detail panel.
pub struct EmbeddedTerminal {
    /// Shared vt100 screen state updated by the background reader thread.
    parser: Arc<Mutex<TerminalParser>>,
    progress_event: Arc<Mutex<Option<Vec<u8>>>>,
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
        let copilot_command = shell_command(copilot_bin, args);

        // Attach to an existing tmux session for this copilot session, or create
        // one in the session's cwd. tmux owns the CLI process; the PTY here is
        // only the embedded client that renders inside the preview panel.
        let mut cmd = CommandBuilder::new("tmux");
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
        // Tell copilot it's running in a color-capable terminal.
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");

        // Spawn the process inside the slave PTY, then drop the slave so that
        // we receive EOF on the master when the child exits.
        let child = slave.spawn_command(cmd)?;
        drop(slave);

        let writer = master.take_writer()?;
        let reader = master.try_clone_reader()?;

        let progress_event = Arc::new(Mutex::new(None));
        let parser = Arc::new(Mutex::new(vt100::Parser::new_with_callbacks(
            rows,
            cols,
            1000,
            TerminalCallbacks::new(Arc::clone(&progress_event)),
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
            progress_event,
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

    pub(crate) fn take_progress_event(&self) -> Option<Vec<u8>> {
        self.progress_event
            .lock()
            .ok()
            .and_then(|mut progress_event| progress_event.take())
    }

    pub(crate) fn clear_progress_event(&self) {
        if let Ok(mut progress_event) = self.progress_event.lock() {
            *progress_event = None;
        }
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

pub(crate) struct TerminalCallbacks {
    progress_event: Arc<Mutex<Option<Vec<u8>>>>,
    window_title: Option<String>,
    palette: TerminalPalette,
}

impl TerminalCallbacks {
    fn new(progress_event: Arc<Mutex<Option<Vec<u8>>>>) -> Self {
        Self {
            progress_event,
            window_title: None,
            palette: TerminalPalette::default(),
        }
    }

    pub(crate) fn palette(&self) -> &TerminalPalette {
        &self.palette
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TerminalPalette {
    pub default_fg: Option<vt100::Color>,
    pub default_bg: Option<vt100::Color>,
    indexed: Vec<Option<vt100::Color>>,
}

impl TerminalPalette {
    pub(crate) fn indexed_color(&self, index: u8) -> Option<vt100::Color> {
        self.indexed.get(index as usize).copied().flatten()
    }

    fn set_indexed_color(&mut self, index: u8, color: vt100::Color) {
        if self.indexed.len() <= index as usize {
            self.indexed.resize(index as usize + 1, None);
        }
        self.indexed[index as usize] = Some(color);
    }

    fn reset_indexed_color(&mut self, index: u8) {
        if let Some(color) = self.indexed.get_mut(index as usize) {
            *color = None;
        }
    }
}

impl vt100::Callbacks for TerminalCallbacks {
    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.window_title = Some(String::from_utf8_lossy(title).to_string());
    }

    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
        if let Some(sequence) = osc_progress_sequence(params) {
            if let Ok(mut progress_event) = self.progress_event.lock() {
                *progress_event = Some(sequence);
            }
        } else {
            update_palette_from_osc(&mut self.palette, params);
        }
    }
}

#[derive(Default)]
struct CachedTmuxTitle {
    last_checked: Option<Instant>,
    value: Option<String>,
}

fn osc_progress_sequence(params: &[&[u8]]) -> Option<Vec<u8>> {
    if !(3..=4).contains(&params.len()) || params[0] != b"9" || params[1] != b"4" {
        return None;
    }

    for param in &params[2..] {
        if param.is_empty() || param.len() > 3 || !param.iter().all(u8::is_ascii_digit) {
            return None;
        }
    }

    let mut sequence = b"\x1b]9;4".to_vec();
    for param in &params[2..] {
        sequence.push(b';');
        sequence.extend_from_slice(param);
    }
    sequence.push(b'\x07');
    Some(sequence)
}

fn update_palette_from_osc(palette: &mut TerminalPalette, params: &[&[u8]]) {
    match params {
        [b"10", color] => palette.default_fg = parse_osc_color(color),
        [b"11", color] => palette.default_bg = parse_osc_color(color),
        [b"110"] => palette.default_fg = None,
        [b"111"] => palette.default_bg = None,
        [b"4", colors @ ..] => {
            for pair in colors.chunks_exact(2) {
                let Some(index) = parse_u8(pair[0]) else {
                    continue;
                };
                let Some(color) = parse_osc_color(pair[1]) else {
                    continue;
                };
                palette.set_indexed_color(index, color);
            }
        }
        [b"104"] => palette.indexed.clear(),
        [b"104", indices @ ..] => {
            for index in indices {
                if let Some(index) = parse_u8(index) {
                    palette.reset_indexed_color(index);
                }
            }
        }
        _ => {}
    }
}

fn parse_u8(value: &[u8]) -> Option<u8> {
    std::str::from_utf8(value).ok()?.parse().ok()
}

fn parse_osc_color(color: &[u8]) -> Option<vt100::Color> {
    if let Some(hex) = color.strip_prefix(b"#") {
        return parse_hex_color_components(hex);
    }

    let components = color.strip_prefix(b"rgb:")?;
    let mut components = components.split(|b| *b == b'/');
    let red = parse_hex_color_component(components.next()?)?;
    let green = parse_hex_color_component(components.next()?)?;
    let blue = parse_hex_color_component(components.next()?)?;
    components
        .next()
        .is_none()
        .then_some(vt100::Color::Rgb(red, green, blue))
}

fn parse_hex_color_components(hex: &[u8]) -> Option<vt100::Color> {
    let component_len = match hex.len() {
        3 | 6 | 9 | 12 => hex.len() / 3,
        _ => return None,
    };

    let red = parse_hex_color_component(&hex[..component_len])?;
    let green = parse_hex_color_component(&hex[component_len..component_len * 2])?;
    let blue = parse_hex_color_component(&hex[component_len * 2..])?;
    Some(vt100::Color::Rgb(red, green, blue))
}

fn parse_hex_color_component(hex: &[u8]) -> Option<u8> {
    if hex.is_empty() || hex.len() > 4 {
        return None;
    }

    let value = hex.iter().try_fold(0u32, |value, digit| {
        Some((value << 4) | (*digit as char).to_digit(16)?)
    })?;
    let max = (1u32 << (hex.len() * 4)) - 1;
    Some(((value * 255) / max) as u8)
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

fn shell_command(copilot_bin: &Path, args: &[impl AsRef<OsStr>]) -> String {
    let words = std::iter::once(copilot_bin.as_os_str().to_string_lossy().to_string()).chain(
        args.iter()
            .map(|arg| arg.as_ref().to_string_lossy().to_string()),
    );
    shell_words::join(words)
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
    fn captures_osc_progress_event() {
        let progress_event = Arc::new(Mutex::new(None));
        let mut parser = vt100::Parser::new_with_callbacks(
            1,
            1,
            0,
            TerminalCallbacks::new(Arc::clone(&progress_event)),
        );

        parser.process(b"\x1b]9;4;1;50\x07");

        assert_eq!(
            *progress_event.lock().unwrap(),
            Some(b"\x1b]9;4;1;50\x07".to_vec())
        );
    }

    #[test]
    fn ignores_non_progress_osc_events() {
        assert_eq!(osc_progress_sequence(&[b"2", b"title"]), None);
    }

    #[test]
    fn captures_dynamic_default_terminal_colors() {
        let mut parser = vt100::Parser::new_with_callbacks(24, 80, 0, test_terminal_callbacks());

        parser.process(b"\x1b]10;#c0caf5\x07\x1b]11;rgb:1a/1b/26\x07");

        assert_eq!(
            parser.callbacks().palette().default_fg,
            Some(vt100::Color::Rgb(0xc0, 0xca, 0xf5))
        );
        assert_eq!(
            parser.callbacks().palette().default_bg,
            Some(vt100::Color::Rgb(0x1a, 0x1b, 0x26))
        );
    }

    #[test]
    fn captures_dynamic_indexed_terminal_colors() {
        let mut parser = vt100::Parser::new_with_callbacks(24, 80, 0, test_terminal_callbacks());

        parser.process(b"\x1b]4;7;#c0caf5;8;rgb:56/5f/89\x07");

        assert_eq!(
            parser.callbacks().palette().indexed_color(7),
            Some(vt100::Color::Rgb(0xc0, 0xca, 0xf5))
        );
        assert_eq!(
            parser.callbacks().palette().indexed_color(8),
            Some(vt100::Color::Rgb(0x56, 0x5f, 0x89))
        );
    }

    #[test]
    fn rejects_non_numeric_progress_params() {
        assert_eq!(osc_progress_sequence(&[b"9", b"4", b"1", b"50\x1b"]), None);
    }

    #[test]
    fn parser_captures_window_title_from_osc_2() {
        let mut parser = vt100::Parser::new_with_callbacks(24, 80, 0, test_terminal_callbacks());

        parser.process(b"\x1b]2;Fix title from Copilot CLI\x07");

        assert_eq!(
            parser.callbacks().window_title.as_deref(),
            Some("Fix title from Copilot CLI")
        );
    }

    #[test]
    fn parser_captures_window_title_from_osc_0() {
        let mut parser = vt100::Parser::new_with_callbacks(24, 80, 0, test_terminal_callbacks());

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

    fn test_terminal_callbacks() -> TerminalCallbacks {
        TerminalCallbacks::new(Arc::new(Mutex::new(None)))
    }
}
