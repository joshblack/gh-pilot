use crate::session::Status;
use crate::tmux;

/// Detect the current status of a managed session by inspecting its tmux pane.
///
/// If the tmux session does not exist the session is `Stopped`.
/// Otherwise the visible pane output is examined for known patterns.
pub fn detect_status(tmux_session: &str) -> Status {
    if !tmux::session_exists(tmux_session) {
        return Status::Stopped;
    }

    match tmux::capture_pane(tmux_session) {
        Ok(pane) => classify_pane(&pane),
        // If we cannot capture the pane, assume the session is running.
        Err(_) => Status::Running,
    }
}

/// Classify pane content into a `Status` using heuristic pattern matching.
///
/// The rules are intentionally simple for the MVP and ordered from most
/// specific to least specific:
///
/// 1. **Waiting** – the last non-blank line ends with a known prompt asking
///    for user confirmation (common in Claude, Gemini, etc.).
/// 2. **Idle** – the last non-blank line looks like a shell prompt (`$`, `%`,
///    `#`).
/// 3. **Running** – everything else.
pub fn classify_pane(pane: &str) -> Status {
    // Work with the last non-blank line for faster, more reliable matching.
    let last_line = pane
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim_end();

    let lower = last_line.to_lowercase();

    // --- Waiting patterns ------------------------------------------------
    // These are substrings typically found when an AI agent or CLI tool is
    // asking the user for confirmation or further input.
    const WAITING_PATTERNS: &[&str] = &[
        "do you want to proceed",
        "do you want to continue",
        "continue? [y/n]",
        "continue? (y/n)",
        "[yes/no]",
        "(yes/no)",
        "press enter to continue",
        "waiting for input",
        "y/n)",
        "(y/n)",
        "[y/n]",
        "press any key",
    ];

    for pat in WAITING_PATTERNS {
        if lower.contains(pat) {
            return Status::Waiting;
        }
    }

    // --- Idle patterns ---------------------------------------------------
    // Common shell prompts that indicate the command has finished and the
    // shell is waiting at its own prompt.
    const IDLE_SUFFIXES: &[&str] = &["$ ", "% ", "# ", "$", "%", "#"];

    for suffix in IDLE_SUFFIXES {
        if last_line.trim_end().ends_with(suffix) {
            return Status::Idle;
        }
    }

    // Default: assume the session is actively running.
    Status::Running
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_waiting_yes_no_brackets() {
        assert_eq!(
            classify_pane("Do you want to proceed? [yes/no]"),
            Status::Waiting
        );
    }

    #[test]
    fn test_classify_waiting_continue() {
        assert_eq!(
            classify_pane("Files modified. Continue? [y/n]"),
            Status::Waiting
        );
    }

    #[test]
    fn test_classify_waiting_press_enter() {
        assert_eq!(
            classify_pane("Press enter to continue"),
            Status::Waiting
        );
    }

    #[test]
    fn test_classify_idle_dollar_prompt() {
        assert_eq!(classify_pane("some output\n$ "), Status::Idle);
    }

    #[test]
    fn test_classify_idle_hash_prompt() {
        assert_eq!(classify_pane("root output\n# "), Status::Idle);
    }

    #[test]
    fn test_classify_idle_percent_prompt() {
        assert_eq!(classify_pane("zsh output\n% "), Status::Idle);
    }

    #[test]
    fn test_classify_running_active_output() {
        assert_eq!(
            classify_pane("Compiling project...\nRunning tests...\nBuilding..."),
            Status::Running
        );
    }

    #[test]
    fn test_classify_running_blank_pane() {
        assert_eq!(classify_pane(""), Status::Running);
    }

    #[test]
    fn test_classify_running_spinner() {
        assert_eq!(classify_pane("⠙ Analyzing codebase"), Status::Running);
    }

    #[test]
    fn test_classify_ignores_blank_trailing_lines() {
        // The last non-blank line is a shell prompt, so it should be Idle.
        assert_eq!(classify_pane("$ \n\n\n"), Status::Idle);
    }
}
