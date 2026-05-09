use crate::app::{App, Mode, Panel};
use crate::session::{load_turns, session_db_path, CopilotSession, SessionSource, SessionStatus};
use chrono::{DateTime, Utc};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{
        Block, Borders, Clear, List, ListItem, ListState, Paragraph, Scrollbar,
        ScrollbarOrientation, ScrollbarState, Wrap,
    },
    Frame,
};

const RUNNING_COLOR: Color = Color::Rgb(0x9e, 0xce, 0x6a);
const WAITING_COLOR: Color = Color::Rgb(0xe0, 0xaf, 0x68);
const IDLE_COLOR: Color = Color::Rgb(0x56, 0x5f, 0x89);
const ERROR_COLOR: Color = Color::Rgb(0xf7, 0x76, 0x8e);
const ACCENT_COLOR: Color = Color::Rgb(0x7a, 0xa2, 0xf7);
const REMOTE_COLOR: Color = Color::Rgb(0xbb, 0x9a, 0xf7);
const TERMINAL_COLOR: Color = Color::Rgb(0x7d, 0xcf, 0xff);
const BACKGROUND_COLOR: Color = Color::Rgb(0x1a, 0x1b, 0x26);
const SURFACE_COLOR: Color = BACKGROUND_COLOR;
const TEXT_COLOR: Color = Color::Rgb(0xc0, 0xca, 0xf5);
const MUTED_COLOR: Color = IDLE_COLOR;
const USER_MSG_COLOR: Color = ACCENT_COLOR;
const AGENT_MSG_COLOR: Color = TEXT_COLOR;
const MARKDOWN_TEXT_COLOR: Color = TEXT_COLOR;
const MARKDOWN_HEADING_COLOR: Color = ACCENT_COLOR;
const MARKDOWN_MARKER_COLOR: Color = WAITING_COLOR;
const MARKDOWN_CODE_COLOR: Color = RUNNING_COLOR;
const MAX_LIST_MARKER_DIGITS: usize = 9;
/// Maximum lines shown per assistant response before truncating.
const MAX_RESPONSE_LINES: usize = 20;

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();

    if app.mode == Mode::Terminal && app.terminal_fullscreen {
        if let Some(ref terminal) = app.embedded_terminal {
            render_vt100_screen(f, terminal, area);
        }
        if app.status_message.is_some() {
            draw_status_toast(f, app, area);
        }
        return;
    }

    f.render_widget(
        Block::default().style(Style::default().bg(BACKGROUND_COLOR)),
        area,
    );

    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area);

    draw_body(f, app, outer[0]);
    draw_footer(f, app, outer[1]);

    // Overlays
    if app.mode == Mode::NewSessionDir {
        draw_input_popup(
            f,
            " New Copilot Session — Directory (Enter to confirm, Esc to cancel) ",
            &app.input_buffer,
            area,
        );
    }
    if app.mode == Mode::Help {
        draw_help_popup(f, app, area);
    }

    if app.status_message.is_some() {
        draw_status_toast(f, app, area);
    }
}

fn draw_body(f: &mut Frame, app: &mut App, area: Rect) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
        .split(area);
    draw_sessions_panel(f, app, cols[0]);
    draw_detail_panel(f, app, cols[1]);
}

// ── Sessions panel (left) ─────────────────────────────────────────────────────

fn draw_sessions_panel(f: &mut Frame, app: &mut App, area: Rect) {
    let is_focused = app.active_panel == Panel::Sessions;
    let border_style = if is_focused {
        Style::default().fg(ACCENT_COLOR)
    } else {
        Style::default().fg(MUTED_COLOR)
    };

    let block = Block::default()
        .title(" Sessions ")
        .title_style(
            Style::default()
                .fg(ACCENT_COLOR)
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .style(Style::default().bg(SURFACE_COLOR))
        .border_style(border_style);

    let inner = block.inner(area);
    f.render_widget(block, area);

    if app.flat_list.is_empty() {
        let msg = Paragraph::new(Text::from(vec![
            Line::from(Span::styled(
                "No Copilot sessions found.",
                Style::default().fg(MUTED_COLOR),
            )),
            Line::from(Span::raw("")),
            Line::from(Span::styled(
                "Press [n] to start a new session.",
                Style::default().fg(MUTED_COLOR),
            )),
        ]))
        .style(Style::default().bg(SURFACE_COLOR))
        .alignment(Alignment::Center);
        let y = inner.height / 2;
        let center = Rect::new(inner.x, inner.y + y.saturating_sub(1), inner.width, 3);
        f.render_widget(msg, center);
        return;
    }

    let mut items: Vec<ListItem> = Vec::new();
    let mut list_state = ListState::default();
    let now = Utc::now();

    for (flat_idx, idx) in app.flat_list.iter().enumerate() {
        let session = &app.sessions[*idx];
        let is_cursor = app.cursor == flat_idx;
        let is_selected = app.selected_session == Some(*idx);

        let name_style = if (is_cursor && is_focused) || is_selected {
            Style::default().fg(TEXT_COLOR).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(TEXT_COLOR)
        };

        let prefix = if is_cursor && is_focused {
            "▌ "
        } else {
            "  "
        };
        items.push(ListItem::new(Text::from(vec![
            session_title_line(
                session,
                inner.width as usize,
                prefix,
                is_cursor && is_focused,
                name_style,
                now,
            ),
            session_description_line(
                session,
                inner.width as usize,
                prefix,
                is_cursor && is_focused,
            ),
        ])));

        if is_cursor {
            list_state.select(Some(flat_idx));
        }
    }

    let list = List::new(items)
        .style(Style::default().bg(SURFACE_COLOR))
        .highlight_style(Style::default());
    f.render_stateful_widget(list, inner, &mut list_state);
}

fn active_prefix(prefix: &str, is_active: bool) -> Span<'static> {
    if is_active {
        Span::styled(
            prefix.to_string(),
            Style::default()
                .fg(ACCENT_COLOR)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::raw(prefix.to_string())
    }
}

fn session_title_line(
    session: &CopilotSession,
    width: usize,
    prefix: &str,
    is_active: bool,
    name_style: Style,
    now: DateTime<Utc>,
) -> Line<'static> {
    let time = relative_time(session, now);
    let icon = session_icon(session);
    let fixed_width = prefix.chars().count() + icon.chars().count() + time.chars().count() + 1;
    let title_width = width.saturating_sub(fixed_width).max(1);
    let title = truncate_ellipsis(&single_line(&session.display_name()), title_width);
    let used_width = prefix.chars().count()
        + icon.chars().count()
        + title.chars().count()
        + time.chars().count();
    let spacer = " ".repeat(width.saturating_sub(used_width).max(1));

    let mut spans = vec![active_prefix(prefix, is_active)];
    spans.push(Span::styled(icon, session_icon_style(session, is_active)));
    spans.extend([
        Span::styled(title, name_style),
        Span::raw(spacer),
        Span::styled(time, Style::default().fg(MUTED_COLOR)),
    ]);
    Line::from(spans)
}

fn session_description_line(
    session: &CopilotSession,
    width: usize,
    prefix: &str,
    is_active: bool,
) -> Line<'static> {
    let message = session
        .last_agent_message
        .as_deref()
        .map(single_line)
        .filter(|message| !message.is_empty())
        .unwrap_or_else(|| {
            if session.source == SessionSource::Remote {
                "Remote agent task.".to_string()
            } else {
                "No agent response yet.".to_string()
            }
        });
    let prefix_width = prefix.chars().count();
    let description = truncate_ellipsis(&message, width.saturating_sub(prefix_width).max(1));

    Line::from(vec![
        active_prefix(prefix, is_active),
        Span::styled(description, Style::default().fg(MUTED_COLOR)),
    ])
}

fn relative_time(session: &CopilotSession, now: DateTime<Utc>) -> String {
    let seconds = (now - session.updated_at).num_seconds().max(0);
    if seconds < 60 {
        return format!("{seconds}s");
    }

    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes}m");
    }

    let hours = minutes / 60;
    if hours < 24 {
        return format!("{hours}h");
    }

    let days = hours / 24;
    if days < 7 {
        return format!("{days}d");
    }

    let weeks = days / 7;
    format!("{weeks}w")
}

fn single_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_ellipsis(value: &str, max_width: usize) -> String {
    if value.chars().count() <= max_width {
        return value.to_string();
    }
    if max_width <= 1 {
        return "…".to_string();
    }

    let mut truncated: String = value.chars().take(max_width - 1).collect();
    truncated.push('…');
    truncated
}

// ── Detail panel (right) ──────────────────────────────────────────────────────

fn draw_detail_panel(f: &mut Frame, app: &mut App, area: Rect) {
    // ── Embedded terminal mode ────────────────────────────────────────────────
    if app.mode == Mode::Terminal {
        if let Some(ref terminal) = app.embedded_terminal {
            draw_embedded_terminal(f, terminal, area);
            return;
        }
    }

    let is_focused = app.active_panel == Panel::Detail;
    let border_style = if is_focused {
        Style::default().fg(ACCENT_COLOR)
    } else {
        Style::default().fg(MUTED_COLOR)
    };

    let Some(idx) = app.selected_session else {
        let block = Block::default()
            .title(" Session Details ")
            .borders(Borders::ALL)
            .style(Style::default().bg(SURFACE_COLOR))
            .border_style(border_style);
        let inner = block.inner(area);
        f.render_widget(block, area);
        let msg = Paragraph::new(Text::from(vec![
            Line::from(Span::styled(
                "Select a session with j/k + Enter",
                Style::default().fg(MUTED_COLOR),
            )),
            Line::from(Span::raw("")),
            Line::from(Span::styled(
                "Press [o] to open a live terminal session",
                Style::default().fg(MUTED_COLOR),
            )),
        ]))
        .style(Style::default().bg(SURFACE_COLOR))
        .alignment(Alignment::Center);
        let cy = inner.height / 2;
        f.render_widget(
            msg,
            Rect::new(inner.x, inner.y + cy.saturating_sub(1), inner.width, 3),
        );
        return;
    };

    let session = &app.sessions[idx];
    let block = Block::default()
        .borders(Borders::ALL)
        .style(Style::default().bg(SURFACE_COLOR))
        .border_style(border_style);
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Info card
    let (status_color, status_sym) = status_display(&session.status);

    let mut info_lines = vec![
        Line::from(vec![
            Span::styled("  Title:     ", Style::default().fg(MUTED_COLOR)),
            Span::styled(
                session.display_name(),
                Style::default().fg(TEXT_COLOR).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("  Directory: ", Style::default().fg(MUTED_COLOR)),
            Span::styled(
                short_path(&session.cwd.to_string_lossy()),
                Style::default().fg(TEXT_COLOR),
            ),
        ]),
        Line::from(vec![
            Span::styled("  Status:    ", Style::default().fg(MUTED_COLOR)),
            Span::styled(
                format!("{status_sym} {}", session.status.label()),
                Style::default()
                    .fg(status_color)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
    ];

    if let Some(ref repo) = session.repository {
        info_lines.push(Line::from(vec![
            Span::styled("  Repo:      ", Style::default().fg(MUTED_COLOR)),
            Span::styled(repo.clone(), Style::default().fg(TEXT_COLOR)),
        ]));
    }
    if let Some(ref branch) = session.branch {
        info_lines.push(Line::from(vec![
            Span::styled("  Branch:    ", Style::default().fg(MUTED_COLOR)),
            Span::styled(branch.clone(), Style::default().fg(TEXT_COLOR)),
        ]));
    }
    if session.source == SessionSource::Remote {
        let source = session
            .remote_state
            .as_ref()
            .map(|state| format!("Remote agent task ({state})"))
            .unwrap_or_else(|| "Remote agent task".to_string());
        info_lines.push(Line::from(vec![
            Span::styled("  Source:    ", Style::default().fg(MUTED_COLOR)),
            Span::styled(" ", Style::default().fg(REMOTE_COLOR)),
            Span::styled(source, Style::default().fg(REMOTE_COLOR)),
        ]));
        if let Some(ref user) = session.remote_user {
            info_lines.push(Line::from(vec![
                Span::styled("  User:      ", Style::default().fg(MUTED_COLOR)),
                Span::styled(user.clone(), Style::default().fg(TEXT_COLOR)),
            ]));
        }
        if let Some(ref pull_request) = session.pull_request {
            info_lines.push(Line::from(vec![
                Span::styled("  PR:        ", Style::default().fg(MUTED_COLOR)),
                Span::styled(pull_request.clone(), Style::default().fg(TEXT_COLOR)),
            ]));
        }
        if let Some(ref url) = session.remote_url {
            info_lines.push(Line::from(vec![
                Span::styled("  URL:       ", Style::default().fg(MUTED_COLOR)),
                Span::styled(url.clone(), Style::default().fg(TEXT_COLOR)),
            ]));
        }
    } else {
        info_lines.push(Line::from(vec![
            Span::styled("  Source:    ", Style::default().fg(MUTED_COLOR)),
            Span::styled(session_icon(session), session_icon_style(session, false)),
            Span::styled(
                "Local terminal session",
                Style::default().fg(TERMINAL_COLOR),
            ),
        ]));
    }

    let info_height = (info_lines.len() as u16 + 1).min(inner.height);
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(info_height), Constraint::Min(1)])
        .split(inner);

    let info_card = Paragraph::new(info_lines)
        .block(
            Block::default()
                .borders(Borders::BOTTOM)
                .style(Style::default().bg(SURFACE_COLOR))
                .border_style(Style::default().fg(MUTED_COLOR)),
        )
        .style(Style::default().bg(SURFACE_COLOR))
        .wrap(Wrap { trim: false });
    f.render_widget(info_card, layout[0]);

    // Conversation turns
    let db_path = session_db_path(&app.copilot_dir);
    let turns = if session.source == SessionSource::Remote {
        Vec::new()
    } else {
        load_turns(&db_path, &session.id)
    };

    let mut turn_lines: Vec<Line> = Vec::new();

    if session.source == SessionSource::Remote {
        turn_lines.push(Line::from(Span::styled(
            format!(
                "  Remote agent task log: gh agent-task view {} --log",
                session.id
            ),
            Style::default().fg(REMOTE_COLOR),
        )));
        turn_lines.push(Line::from(Span::raw("")));
        match session.remote_log.as_deref() {
            Some(log) if !log.is_empty() => {
                push_markdown_lines(&mut turn_lines, log, MARKDOWN_TEXT_COLOR, None);
            }
            Some(_) => {
                turn_lines.push(Line::from(Span::styled(
                    "  No remote task log output available.",
                    Style::default().fg(MUTED_COLOR),
                )));
            }
            None => {
                turn_lines.push(Line::from(Span::styled(
                    "  Loading remote task log…",
                    Style::default().fg(MUTED_COLOR),
                )));
            }
        }
        turn_lines.push(Line::from(Span::raw("")));
        turn_lines.push(Line::from(Span::styled(
            "  Press [o] to open this task in your browser.",
            Style::default().fg(MUTED_COLOR),
        )));
    } else if turns.is_empty() {
        turn_lines.push(Line::from(Span::styled(
            "  No conversation history yet.",
            Style::default().fg(MUTED_COLOR),
        )));
        turn_lines.push(Line::from(Span::raw("")));
        turn_lines.push(Line::from(Span::styled(
            "  Press [o] to open this session in Copilot.",
            Style::default().fg(MUTED_COLOR),
        )));
    } else {
        for turn in &turns {
            if let Some(ref msg) = turn.user_message {
                turn_lines.push(Line::from(Span::styled(
                    "  You",
                    Style::default()
                        .fg(USER_MSG_COLOR)
                        .add_modifier(Modifier::BOLD),
                )));
                push_markdown_lines(&mut turn_lines, msg, MARKDOWN_TEXT_COLOR, None);
                turn_lines.push(Line::from(Span::raw("")));
            }
            if let Some(ref resp) = turn.assistant_response {
                turn_lines.push(Line::from(Span::styled(
                    "  Copilot",
                    Style::default()
                        .fg(AGENT_MSG_COLOR)
                        .add_modifier(Modifier::BOLD),
                )));
                if push_markdown_lines(
                    &mut turn_lines,
                    resp,
                    MARKDOWN_TEXT_COLOR,
                    Some(MAX_RESPONSE_LINES),
                ) {
                    turn_lines.push(Line::from(Span::styled(
                        "  … (truncated)",
                        Style::default().fg(MUTED_COLOR),
                    )));
                }
                turn_lines.push(Line::from(Span::raw("")));
            }
            turn_lines.push(Line::from(Span::styled(
                format!("  ─── Turn {} ", turn.turn_index + 1),
                Style::default().fg(MUTED_COLOR),
            )));
            turn_lines.push(Line::from(Span::raw("")));
        }
    }

    let total_lines = turn_lines.len();
    let visible_height = layout[1].height as usize;
    let max_scroll = total_lines.saturating_sub(visible_height);
    if app.detail_scroll > max_scroll {
        app.detail_scroll = max_scroll;
    }

    let log_title = if is_focused && session.source == SessionSource::Remote {
        " Preview [k/j scroll, o=open browser] "
    } else if is_focused && !turns.is_empty() {
        " Conversation [k/j scroll, o=open live] "
    } else {
        " Conversation "
    };

    let turns_para = Paragraph::new(Text::from(turn_lines))
        .block(
            Block::default()
                .title(log_title)
                .title_style(Style::default().fg(MUTED_COLOR))
                .style(Style::default().bg(SURFACE_COLOR))
                .borders(Borders::NONE),
        )
        .style(Style::default().bg(SURFACE_COLOR))
        .scroll((app.detail_scroll as u16, 0))
        .wrap(Wrap { trim: false });
    f.render_widget(turns_para, layout[1]);

    if total_lines > visible_height {
        let mut scroll_state = ScrollbarState::new(total_lines).position(app.detail_scroll);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("↑"))
            .end_symbol(Some("↓"));
        f.render_stateful_widget(scrollbar, layout[1], &mut scroll_state);
    }
}

fn push_markdown_lines(
    lines: &mut Vec<Line<'static>>,
    text: &str,
    base_color: Color,
    max_lines: Option<usize>,
) -> bool {
    let mut in_code_block = false;
    let mut truncated = false;

    for (index, line) in text.lines().enumerate() {
        if max_lines.is_some_and(|max| index >= max) {
            truncated = true;
            break;
        }
        lines.push(markdown_line("  ", line, base_color, &mut in_code_block));
    }

    truncated
}

fn markdown_line(
    prefix: &str,
    line: &str,
    base_color: Color,
    in_code_block: &mut bool,
) -> Line<'static> {
    let base_style = Style::default().fg(base_color);
    let mut spans = vec![Span::styled(prefix.to_string(), base_style)];
    let trimmed = line.trim_start();
    let leading = line.len() - trimmed.len();
    let leading_ws = &line[..leading];

    if is_code_fence(trimmed) {
        spans.push(Span::styled(
            line.to_string(),
            Style::default()
                .fg(MARKDOWN_CODE_COLOR)
                .add_modifier(Modifier::BOLD),
        ));
        *in_code_block = !*in_code_block;
        return Line::from(spans);
    }

    if *in_code_block {
        spans.push(Span::styled(
            line.to_string(),
            Style::default().fg(MARKDOWN_CODE_COLOR),
        ));
        return Line::from(spans);
    }

    if is_heading(trimmed) {
        spans.push(Span::styled(
            line.to_string(),
            Style::default()
                .fg(MARKDOWN_HEADING_COLOR)
                .add_modifier(Modifier::BOLD),
        ));
        return Line::from(spans);
    }

    if let Some(rest) = trimmed.strip_prefix('>') {
        spans.push(Span::styled(leading_ws.to_string(), base_style));
        spans.push(Span::styled(
            ">".to_string(),
            Style::default()
                .fg(MARKDOWN_MARKER_COLOR)
                .add_modifier(Modifier::BOLD),
        ));
        append_inline_markdown(spans, rest, base_color)
    } else if let Some((marker, rest)) = split_list_marker(trimmed) {
        spans.push(Span::styled(leading_ws.to_string(), base_style));
        spans.push(Span::styled(
            marker.to_string(),
            Style::default()
                .fg(MARKDOWN_MARKER_COLOR)
                .add_modifier(Modifier::BOLD),
        ));
        append_inline_markdown(spans, rest, base_color)
    } else {
        append_inline_markdown(spans, line, base_color)
    }
}

fn append_inline_markdown(
    mut spans: Vec<Span<'static>>,
    text: &str,
    base_color: Color,
) -> Line<'static> {
    let mut rest = text;
    let base_style = Style::default().fg(base_color);

    while let Some(start) = rest.find('`') {
        let (before, after_start) = rest.split_at(start);
        if !before.is_empty() {
            append_emphasis_markdown(&mut spans, before, base_color);
        }

        let after_tick = &after_start[1..];
        if let Some(end) = after_tick.find('`') {
            let (code, after_end) = after_tick.split_at(end);
            spans.push(Span::styled(
                format!("`{code}`"),
                Style::default().fg(MARKDOWN_CODE_COLOR),
            ));
            rest = &after_end[1..];
        } else {
            spans.push(Span::styled(after_start.to_string(), base_style));
            rest = "";
        }
    }

    if !rest.is_empty() {
        append_emphasis_markdown(&mut spans, rest, base_color);
    }

    Line::from(spans)
}

fn append_emphasis_markdown(spans: &mut Vec<Span<'static>>, text: &str, base_color: Color) {
    let mut rest = text;
    let base_style = Style::default().fg(base_color);

    while let Some((start, marker, end)) = find_emphasis(rest) {
        let (before, emphasized) = rest.split_at(start);
        if !before.is_empty() {
            spans.push(Span::styled(before.to_string(), base_style));
        }

        let token_len = marker.len();
        let (content, after_content) = emphasized[token_len..].split_at(end);
        let mut style = base_style;
        if token_len >= 2 {
            style = style.add_modifier(Modifier::BOLD);
        }
        if token_len == 1 || token_len == 3 {
            style = style.add_modifier(Modifier::ITALIC);
        }
        spans.push(Span::styled(content.to_string(), style));
        rest = &after_content[token_len..];
    }

    if !rest.is_empty() {
        spans.push(Span::styled(rest.to_string(), base_style));
    }
}

fn find_emphasis(text: &str) -> Option<(usize, &'static str, usize)> {
    const EMPHASIS_MARKERS: [&str; 6] = ["***", "___", "**", "__", "*", "_"];

    let mut index = 0;
    while index < text.len() {
        let rest = &text[index..];
        for marker in EMPHASIS_MARKERS {
            if !rest.starts_with(marker) {
                continue;
            }
            let after_marker = &rest[marker.len()..];
            if let Some(end) = after_marker.find(marker) {
                if end > 0 {
                    return Some((index, marker, end));
                }
            }
        }
        index += rest.chars().next().map_or(1, char::len_utf8);
    }

    None
}

fn is_code_fence(trimmed: &str) -> bool {
    trimmed.starts_with("```") || trimmed.starts_with("~~~")
}

fn is_heading(trimmed: &str) -> bool {
    let hashes = trimmed.chars().take_while(|ch| *ch == '#').count();
    (1..=6).contains(&hashes)
        && trimmed
            .as_bytes()
            .get(hashes)
            .is_some_and(|byte| byte.is_ascii_whitespace())
}

fn split_list_marker(trimmed: &str) -> Option<(&str, &str)> {
    if trimmed.len() >= 2 {
        let marker = &trimmed[..1];
        if matches!(marker, "-" | "*" | "+") && trimmed[1..].starts_with(char::is_whitespace) {
            return Some((&trimmed[..2], &trimmed[2..]));
        }
    }

    let marker_end = trimmed.find('.')?;
    if marker_end == 0
        || marker_end > MAX_LIST_MARKER_DIGITS
        || !trimmed[..marker_end].chars().all(|ch| ch.is_ascii_digit())
        || !trimmed[marker_end + 1..].starts_with(char::is_whitespace)
    {
        return None;
    }

    Some((&trimmed[..marker_end + 2], &trimmed[marker_end + 2..]))
}

fn status_display(status: &SessionStatus) -> (Color, &'static str) {
    match status {
        SessionStatus::Running => (RUNNING_COLOR, "●"),
        SessionStatus::Waiting => (WAITING_COLOR, "◐"),
        SessionStatus::Idle => (IDLE_COLOR, "○"),
        SessionStatus::Error => (ERROR_COLOR, "✕"),
    }
}

fn session_icon(session: &CopilotSession) -> &'static str {
    match session.source {
        SessionSource::Local => " ",
        SessionSource::Remote => " ",
    }
}

fn session_icon_style(session: &CopilotSession, is_active: bool) -> Style {
    let color = match session.source {
        SessionSource::Local => TERMINAL_COLOR,
        SessionSource::Remote => REMOTE_COLOR,
    };
    let style = Style::default().fg(color);
    if is_active {
        style.add_modifier(Modifier::BOLD)
    } else {
        style
    }
}

// ── Embedded terminal renderer ────────────────────────────────────────────────

fn draw_embedded_terminal(f: &mut Frame, term: &crate::terminal::EmbeddedTerminal, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .style(Style::default().bg(SURFACE_COLOR))
        .border_style(Style::default().fg(RUNNING_COLOR));

    let inner = block.inner(area);
    f.render_widget(block, area);

    render_vt100_screen(f, term, inner);
}

fn render_vt100_screen(f: &mut Frame, term: &crate::terminal::EmbeddedTerminal, area: Rect) {
    let parser = term.parser.lock().unwrap();
    let screen = parser.screen();

    let rows = area.height as usize;
    let cols = area.width as usize;

    let mut lines: Vec<Line> = Vec::with_capacity(rows);

    for row in 0..rows {
        let mut spans: Vec<Span> = Vec::new();
        let mut cur_style = Style::default();
        let mut cur_text = String::new();

        for col in 0..cols {
            let (ch, style) = match screen.cell(row as u16, col as u16) {
                Some(cell) => {
                    let c = cell.contents();
                    // Avoid an allocation for the common blank-cell case.
                    let ch = if c.is_empty() {
                        " ".to_string()
                    } else {
                        c.to_string()
                    };
                    (ch, cell_to_ratatui_style(cell))
                }
                None => (" ".to_string(), Style::default()),
            };

            if style == cur_style {
                cur_text.push_str(&ch);
            } else {
                if !cur_text.is_empty() {
                    spans.push(Span::styled(std::mem::take(&mut cur_text), cur_style));
                }
                cur_text = ch; // ch is already a String
                cur_style = style;
            }
        }
        if !cur_text.is_empty() {
            spans.push(Span::styled(cur_text, cur_style));
        }
        lines.push(Line::from(spans));
    }

    // Render the screen content.
    f.render_widget(Paragraph::new(Text::from(lines)), area);

    // Position the cursor.
    let (cursor_row, cursor_col) = screen.cursor_position();
    if !screen.hide_cursor() && cursor_col < area.width && cursor_row < area.height {
        f.set_cursor_position(Position {
            x: area.x + cursor_col,
            y: area.y + cursor_row,
        });
    }
}

fn cell_to_ratatui_style(cell: &vt100::Cell) -> Style {
    let fg = vt100_color_to_ratatui(cell.fgcolor());
    let bg = vt100_color_to_ratatui(cell.bgcolor());
    let mut style = Style::default().fg(fg).bg(bg);
    if cell.bold() {
        style = style.add_modifier(Modifier::BOLD);
    }
    if cell.italic() {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if cell.underline() {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if cell.inverse() {
        style = style.add_modifier(Modifier::REVERSED);
    }
    style
}

fn vt100_color_to_ratatui(color: vt100::Color) -> Color {
    match color {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(n) => Color::Indexed(n),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

// ── Footer ────────────────────────────────────────────────────────────────────

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let (text, style) = match app.mode {
        Mode::NewSessionDir => (
            "Launch: Enter  Cancel: Esc".to_string(),
            Style::default().fg(WAITING_COLOR),
        ),
        Mode::Terminal => (
            "Fullscreen: Ctrl+F  Detach: Ctrl+W  Input: forwarded to Copilot".to_string(),
            Style::default().fg(RUNNING_COLOR),
        ),
        Mode::Help => (
            "Scroll: j/k  Close: Esc/q  Help: ?".to_string(),
            Style::default().fg(MUTED_COLOR),
        ),
        Mode::Normal => (footer_text(app), Style::default().fg(MUTED_COLOR)),
    };

    f.render_widget(
        Paragraph::new(Span::styled(text, style))
            .style(Style::default().bg(BACKGROUND_COLOR))
            .alignment(Alignment::Center),
        area,
    );
}

fn footer_text(app: &App) -> String {
    footer_shortcuts(app)
        .into_iter()
        .map(|(action, key)| format!("{action}: {key}"))
        .collect::<Vec<_>>()
        .join("  ")
}

fn footer_shortcuts(app: &App) -> Vec<(&'static str, &'static str)> {
    match app.active_panel {
        Panel::Sessions => {
            let mut shortcuts = vec![("Navigate", "j/k")];
            if app.flat_list.get(app.cursor).is_some() {
                shortcuts.push(("View", "Enter"));
                shortcuts.push(("Open", "o"));
            }
            shortcuts.push(("New", "n"));
            shortcuts.push(("Help", "?"));
            shortcuts
        }
        Panel::Detail => {
            let mut shortcuts = vec![("Scroll", "j/k"), ("Back", "h/Esc")];
            if app.selected_session.is_some() {
                shortcuts.push(("Open", "o"));
            }
            shortcuts.push(("New", "n"));
            shortcuts.push(("Help", "?"));
            shortcuts
        }
    }
}

// ── Overlays ──────────────────────────────────────────────────────────────────

fn draw_input_popup(f: &mut Frame, title: &str, input: &str, area: Rect) {
    let popup = centered_rect(70, 5, area);
    f.render_widget(Clear, popup);
    let block = Block::default()
        .title(title)
        .title_style(
            Style::default()
                .fg(ACCENT_COLOR)
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .style(Style::default().bg(SURFACE_COLOR))
        .border_style(Style::default().fg(ACCENT_COLOR));
    let inner = block.inner(popup);
    f.render_widget(block, popup);
    f.render_widget(
        Paragraph::new(Span::styled(
            format!("▶ {input}█"),
            Style::default().fg(TEXT_COLOR),
        ))
        .style(Style::default().bg(SURFACE_COLOR)),
        inner,
    );
}

fn draw_help_popup(f: &mut Frame, app: &App, area: Rect) {
    let popup_height = area.height.saturating_sub(4).clamp(1, 24);
    let popup = centered_rect(70, popup_height, area);
    f.render_widget(Clear, popup);

    let block = Block::default()
        .title(" Shortcuts — j/k or scroll, Esc to close ")
        .title_style(
            Style::default()
                .fg(ACCENT_COLOR)
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .style(Style::default().bg(SURFACE_COLOR))
        .border_style(Style::default().fg(ACCENT_COLOR));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    let lines = help_lines();
    let total_lines = lines.len();
    let visible_height = inner.height as usize;
    let max_scroll = total_lines.saturating_sub(visible_height);
    let help_scroll = app.help_scroll.min(max_scroll);

    let help = Paragraph::new(Text::from(lines))
        .style(Style::default().bg(SURFACE_COLOR))
        .scroll((help_scroll as u16, 0))
        .wrap(Wrap { trim: false });
    f.render_widget(help, inner);

    if total_lines > visible_height {
        let mut scroll_state = ScrollbarState::new(total_lines).position(help_scroll);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("↑"))
            .end_symbol(Some("↓"));
        f.render_stateful_widget(scrollbar, inner, &mut scroll_state);
    }
}

fn help_lines() -> Vec<Line<'static>> {
    vec![
        help_heading("Global"),
        help_shortcut("?", "Show or hide this shortcut help"),
        help_shortcut("q", "Quit from normal mode"),
        help_shortcut("Ctrl+C", "Quit"),
        Line::from(""),
        help_heading("Sessions panel"),
        help_shortcut("j / ↓", "Move selection down"),
        help_shortcut("k / ↑", "Move selection up"),
        help_shortcut("Enter / Space", "View the selected session"),
        help_shortcut("o", "Open the selected session in Copilot"),
        help_shortcut("n", "Launch a new Copilot session"),
        help_shortcut("r", "Reload sessions from disk"),
        Line::from(""),
        help_heading("Detail panel"),
        help_shortcut("j / ↓", "Scroll conversation down"),
        help_shortcut("k / ↑", "Scroll conversation up"),
        help_shortcut("PageDown / PageUp", "Scroll conversation by page"),
        help_shortcut("h / ← / Esc", "Return to the sessions panel"),
        help_shortcut("o", "Open the selected session in Copilot"),
        help_shortcut("n", "Launch a new Copilot session"),
        help_shortcut("r", "Reload sessions from disk"),
        Line::from(""),
        help_heading("Embedded terminal"),
        help_shortcut("Ctrl+F", "Toggle fullscreen"),
        help_shortcut("Ctrl+W", "Detach from the embedded session"),
        help_shortcut("Mouse", "Forwarded to Copilot while fullscreen"),
        Line::from(""),
        help_heading("New session prompt"),
        help_shortcut("Enter", "Launch in the entered directory"),
        help_shortcut("Esc", "Cancel"),
        help_shortcut("Type", "Edit the directory path"),
        Line::from(""),
        help_heading("Shortcut help"),
        help_shortcut("j / ↓ / scroll", "Scroll down"),
        help_shortcut("k / ↑ / scroll", "Scroll up"),
        help_shortcut("PageDown / PageUp", "Scroll by page"),
        help_shortcut("Esc / q / ?", "Close"),
    ]
}

fn help_heading(text: &'static str) -> Line<'static> {
    Line::from(Span::styled(
        format!("  {text}"),
        Style::default()
            .fg(ACCENT_COLOR)
            .add_modifier(Modifier::BOLD),
    ))
}

fn help_shortcut(key: &'static str, action: &'static str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("  {key:<18}"), Style::default().fg(ACCENT_COLOR)),
        Span::styled(action, Style::default().fg(TEXT_COLOR)),
    ])
}

fn draw_status_toast(f: &mut Frame, app: &App, area: Rect) {
    if let Some(msg) = &app.status_message {
        let width = (msg.len() + 4).min(area.width as usize) as u16;
        let toast = Rect::new(
            area.x + area.width.saturating_sub(width + 2),
            area.y + area.height.saturating_sub(4),
            width,
            1,
        );
        f.render_widget(Clear, toast);
        f.render_widget(
            Paragraph::new(Span::styled(
                format!("  {msg}  "),
                Style::default()
                    .fg(BACKGROUND_COLOR)
                    .bg(RUNNING_COLOR)
                    .add_modifier(Modifier::BOLD),
            )),
            toast,
        );
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn centered_rect(percent_x: u16, height: u16, area: Rect) -> Rect {
    let w = (area.width * percent_x / 100).min(area.width);
    let h = height.min(area.height);
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    Rect::new(x, y, w, h)
}

fn short_path(path: &str) -> String {
    if let Some(home) = dirs::home_dir() {
        let home_str = home.to_string_lossy().to_string();
        if path.starts_with(&home_str) {
            return path.replacen(&home_str, "~", 1);
        }
    }
    path.to_string()
}
