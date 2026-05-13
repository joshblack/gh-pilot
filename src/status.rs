use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    Waiting,
    Done,
    Busy,
    Idle,
}

impl SessionStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Done => "done",
            Self::Busy => "busy",
            Self::Idle => "idle",
        }
    }

    pub fn from_label(label: &str) -> Self {
        match label {
            "waiting" => Self::Waiting,
            "done" => Self::Done,
            "busy" => Self::Busy,
            _ => Self::Idle,
        }
    }

    pub fn sort_rank(self) -> u8 {
        match self {
            Self::Waiting => 0,
            Self::Done => 1,
            Self::Busy => 2,
            Self::Idle => 3,
        }
    }
}

pub fn content_hash(content: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    strip_ansi(content).trim().hash(&mut hasher);
    hasher.finish()
}

pub fn detect_status(
    content: &str,
    content_changed: bool,
    activity_recent: bool,
    _seen: bool,
) -> SessionStatus {
    let clean = strip_ansi(content);

    if has_busy_indicator(&clean) {
        return SessionStatus::Busy;
    }

    if has_attention_prompt(&clean) {
        return SessionStatus::Waiting;
    }

    if has_input_prompt(&clean) {
        return SessionStatus::Waiting;
    }

    if content_changed || activity_recent {
        return SessionStatus::Busy;
    }

    SessionStatus::Idle
}

fn has_busy_indicator(content: &str) -> bool {
    let recent = recent_non_empty_lines(content, 15).join("\n");
    let lower = recent.to_lowercase();

    let busy_phrases = [
        "ctrl+c to interrupt",
        "esc to interrupt",
        "thinking...",
        "thinking…",
        "working...",
        "working…",
        "generating...",
        "generating…",
        "analyzing...",
        "analyzing…",
        "running tool",
        "running command",
        "calling tool",
        "waiting for tool",
        "streaming response",
        "searching files",
        "reading file",
        "editing file",
        "running tests",
    ];

    if busy_phrases.iter().any(|phrase| lower.contains(phrase)) {
        return true;
    }

    let spinner_chars = [
        "⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏", "✳", "✽", "✶", "✢", "⣾", "⣽", "⣻", "⢿",
        "⡿", "⣟", "⣯", "⣷",
    ];

    spinner_chars.iter().any(|spinner| recent.contains(spinner))
}

fn has_attention_prompt(content: &str) -> bool {
    let recent_lines = recent_non_empty_lines(content, 12);
    let recent = recent_lines.join("\n");
    let lower = recent.to_lowercase();

    let waiting_phrases = [
        "waiting for input",
        "waiting for user",
        "action required",
        "do you want",
        "allow once",
        "allow always",
        "run this command",
        "continue?",
        "proceed?",
        "(y/n)",
        "(y/n)",
        "[y/n]",
        "[y/n]",
        "yes/no",
        "use arrow keys",
    ];

    if waiting_phrases.iter().any(|phrase| lower.contains(phrase)) {
        return true;
    }

    false
}

fn has_input_prompt(content: &str) -> bool {
    let recent_lines = recent_non_empty_lines(content, 12);
    let Some(last_line) = recent_lines.last() else {
        return false;
    };
    let last_line = last_line.trim().replace('\u{00a0}', " ");
    let lower = last_line.to_lowercase();

    let done_phrases = [
        "how can i help",
        "what can i help",
        "what would you like",
        "ask anything",
        "enter a prompt",
        "type your message",
        "press enter",
    ];

    if done_phrases.iter().any(|phrase| lower.contains(phrase)) {
        return true;
    }

    matches!(last_line.as_str(), ">" | "?" | "❯" | "›")
        || last_line.starts_with("> ")
        || last_line.starts_with("❯ ")
        || last_line.starts_with("› ")
}

fn recent_non_empty_lines(content: &str, count: usize) -> Vec<String> {
    let mut lines = content
        .lines()
        .rev()
        .filter_map(|line| {
            let trimmed = line.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_owned())
        })
        .take(count)
        .collect::<Vec<_>>();
    lines.reverse();
    lines
}

pub fn strip_ansi(content: &str) -> String {
    if !content.as_bytes().contains(&0x1b) && !content.as_bytes().contains(&0x9b) {
        return content.to_owned();
    }

    let mut output = String::with_capacity(content.len());
    let bytes = content.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == 0x1b {
            if i + 1 < bytes.len() && bytes[i + 1] == b'[' {
                i += 2;
                while i < bytes.len() {
                    let byte = bytes[i];
                    i += 1;
                    if byte.is_ascii_alphabetic() {
                        break;
                    }
                }
                continue;
            }

            if i + 1 < bytes.len() && bytes[i + 1] == b']' {
                i += 2;
                while i < bytes.len() {
                    if bytes[i] == 0x07 {
                        i += 1;
                        break;
                    }
                    if i + 1 < bytes.len() && bytes[i] == 0x1b && bytes[i + 1] == b'\\' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
                continue;
            }

            i += 2;
            continue;
        }

        if bytes[i] == 0x9b {
            i += 1;
            while i < bytes.len() {
                let byte = bytes[i];
                i += 1;
                if byte.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }

        if let Some(ch) = content[i..].chars().next() {
            output.push(ch);
            i += ch.len_utf8();
        } else {
            break;
        }
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_busy_before_prompt() {
        let content = "❯ previous prompt\n⠋ Thinking... ctrl+c to interrupt";
        assert_eq!(
            detect_status(content, false, false, false),
            SessionStatus::Busy
        );
    }

    #[test]
    fn prompt_history_does_not_mark_active_output_done() {
        let content = "What would you like to do?\n❯ explain this file\nReading file src/main.rs";
        assert_eq!(
            detect_status(content, false, false, false),
            SessionStatus::Busy
        );
    }

    #[test]
    fn prompt_history_without_recent_activity_is_idle() {
        let content = "What would you like to do?\n❯ explain this file\nSummary output";
        assert_eq!(
            detect_status(content, false, false, false),
            SessionStatus::Idle
        );
    }

    #[test]
    fn detects_input_prompt_as_waiting() {
        assert_eq!(
            detect_status("What would you like to do?\n❯", false, false, false),
            SessionStatus::Waiting
        );
        assert_eq!(
            detect_status("What would you like to do?\n❯", false, false, true),
            SessionStatus::Waiting
        );
    }

    #[test]
    fn detects_attention_prompt() {
        assert_eq!(
            detect_status("Run this command?\n❯ Yes", false, false, false),
            SessionStatus::Waiting
        );
    }

    #[test]
    fn detects_activity_as_busy() {
        assert_eq!(
            detect_status("summary output", true, true, false),
            SessionStatus::Busy
        );
        assert_eq!(
            detect_status("summary output", true, false, false),
            SessionStatus::Busy
        );
        assert_eq!(
            detect_status("summary output", false, true, false),
            SessionStatus::Busy
        );
    }

    #[test]
    fn static_token_counts_are_not_busy() {
        assert_eq!(
            detect_status("Done. 1200 tokens used.", false, false, false),
            SessionStatus::Idle
        );
    }

    #[test]
    fn strips_ansi_sequences() {
        assert_eq!(strip_ansi("\x1b[31mhello\x1b[0m"), "hello");
    }
}
