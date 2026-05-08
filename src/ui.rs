use crate::app::{App, FlatItem, Mode, Panel};
use crate::session::{load_turns, session_db_path, SessionStatus};
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

const RUNNING_COLOR: Color = Color::Green;
const WAITING_COLOR: Color = Color::Yellow;
const IDLE_COLOR: Color = Color::DarkGray;
const ERROR_COLOR: Color = Color::Red;
const ACCENT_COLOR: Color = Color::Cyan;
const HEADER_COLOR: Color = Color::Magenta;
const LOAD_MORE_COLOR: Color = Color::Yellow;
const SELECTED_BG: Color = Color::Rgb(40, 56, 80);
const USER_MSG_COLOR: Color = Color::Cyan;
const AGENT_MSG_COLOR: Color = Color::White;
const MARKDOWN_HEADING_COLOR: Color = Color::Magenta;
const MARKDOWN_MARKER_COLOR: Color = Color::Yellow;
const MARKDOWN_CODE_COLOR: Color = Color::Green;
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
        Style::default().fg(Color::DarkGray)
    };

    let title = if let Some(group) = app.focused_group.as_deref() {
        format!(" Sessions — {} ", short_path(group))
    } else {
        " Sessions ".to_string()
    };
    let block = Block::default()
        .title(title)
        .title_style(
            Style::default()
                .fg(ACCENT_COLOR)
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .border_style(border_style);

    let inner = block.inner(area);
    f.render_widget(block, area);

    if app.flat_list.is_empty() {
        let msg = Paragraph::new(Text::from(vec![
            Line::from(Span::styled(
                "No Copilot sessions found.",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(Span::raw("")),
            Line::from(Span::styled(
                "Press [n] to start a new session.",
                Style::default().fg(Color::Gray),
            )),
        ]))
        .alignment(Alignment::Center);
        let y = inner.height / 2;
        let center = Rect::new(inner.x, inner.y + y.saturating_sub(1), inner.width, 3);
        f.render_widget(msg, center);
        return;
    }

    let mut items: Vec<ListItem> = Vec::new();
    let mut list_state = ListState::default();
    let mut list_idx = 0usize;

    for (flat_idx, item) in app.flat_list.iter().enumerate() {
        match item {
            FlatItem::GroupHeader(path) => {
                let label = short_path(path);
                let is_cursor = app.cursor == flat_idx;
                let is_collapsed = app.collapsed_groups.contains(path);
                let is_focused_group = app.focused_group.as_deref() == Some(path.as_str());
                let prefix = if is_cursor && is_focused {
                    "❯ "
                } else {
                    "  "
                };
                let marker = if is_collapsed { "▸ " } else { "▾ " };
                let focus_suffix = if is_focused_group { "  focused" } else { "" };
                let style = if is_cursor && is_focused {
                    Style::default()
                        .fg(HEADER_COLOR)
                        .bg(SELECTED_BG)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                        .fg(HEADER_COLOR)
                        .add_modifier(Modifier::BOLD)
                };
                items.push(ListItem::new(Line::from(vec![
                    Span::raw(prefix),
                    Span::styled(marker, style),
                    Span::styled(label, style),
                    Span::styled(focus_suffix, Style::default().fg(Color::DarkGray)),
                ])));
                if is_cursor {
                    list_state.select(Some(list_idx));
                }
                list_idx += 1;
            }
            FlatItem::SessionEntry(idx) => {
                let session = &app.sessions[*idx];
                let is_cursor = app.cursor == flat_idx;
                let is_selected = app.selected_session == Some(*idx);

                let (status_color, status_sym) = status_display(&session.status);

                let name = session.display_name();
                let time_str = session.updated_at.format("%m/%d %H:%M").to_string();
                let prefix = if is_cursor && is_focused {
                    "  ❯ "
                } else {
                    "    "
                };

                let name_style = if is_cursor && is_focused {
                    Style::default()
                        .fg(Color::White)
                        .bg(SELECTED_BG)
                        .add_modifier(Modifier::BOLD)
                } else if is_selected {
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White)
                };

                items.push(ListItem::new(Line::from(vec![
                    Span::raw(prefix),
                    Span::styled(status_sym, Style::default().fg(status_color)),
                    Span::raw(" "),
                    Span::styled(name, name_style),
                    Span::styled(
                        format!("  {time_str}"),
                        Style::default().fg(Color::DarkGray),
                    ),
                ])));

                if is_cursor {
                    list_state.select(Some(list_idx));
                }
                list_idx += 1;
            }
            FlatItem::LoadMore { hidden_count, .. } => {
                let is_cursor = app.cursor == flat_idx;
                let prefix = if is_cursor && is_focused {
                    "  ❯ "
                } else {
                    "    "
                };
                let style = if is_cursor && is_focused {
                    Style::default()
                        .fg(LOAD_MORE_COLOR)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(LOAD_MORE_COLOR)
                };
                items.push(ListItem::new(Line::from(vec![
                    Span::raw(prefix),
                    Span::styled(format!("… {hidden_count} more  [Enter to expand]"), style),
                ])));
                if is_cursor {
                    list_state.select(Some(list_idx));
                }
                list_idx += 1;
            }
        }
    }

    let list = List::new(items).highlight_style(Style::default().bg(SELECTED_BG));
    f.render_stateful_widget(list, inner, &mut list_state);
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
        Style::default().fg(Color::DarkGray)
    };

    let Some(idx) = app.selected_session else {
        let block = Block::default()
            .title(" Session Details ")
            .borders(Borders::ALL)
            .border_style(border_style);
        let inner = block.inner(area);
        f.render_widget(block, area);
        let msg = Paragraph::new(Text::from(vec![
            Line::from(Span::styled(
                "Select a session with j/k + Enter",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(Span::raw("")),
            Line::from(Span::styled(
                "Press [o] to open a live terminal session",
                Style::default().fg(Color::DarkGray),
            )),
        ]))
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
        .title(format!(" {} ", session.display_name()))
        .title_style(
            Style::default()
                .fg(ACCENT_COLOR)
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .border_style(border_style);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(7), Constraint::Min(1)])
        .split(inner);

    // Info card
    let (status_color, status_sym) = status_display(&session.status);

    let mut info_lines = vec![
        Line::from(vec![
            Span::styled("  Status:    ", Style::default().fg(Color::Gray)),
            Span::styled(
                format!("{status_sym} {}", session.status.label()),
                Style::default()
                    .fg(status_color)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("  Directory: ", Style::default().fg(Color::Gray)),
            Span::styled(
                short_path(&session.cwd.to_string_lossy()),
                Style::default().fg(Color::White),
            ),
        ]),
    ];

    if let Some(ref repo) = session.repository {
        info_lines.push(Line::from(vec![
            Span::styled("  Repo:      ", Style::default().fg(Color::Gray)),
            Span::styled(repo.clone(), Style::default().fg(Color::White)),
        ]));
    }
    if let Some(ref branch) = session.branch {
        info_lines.push(Line::from(vec![
            Span::styled("  Branch:    ", Style::default().fg(Color::Gray)),
            Span::styled(branch.clone(), Style::default().fg(Color::White)),
        ]));
    }
    info_lines.push(Line::from(vec![
        Span::styled("  Updated:   ", Style::default().fg(Color::Gray)),
        Span::styled(
            session.updated_at.format("%Y-%m-%d %H:%M UTC").to_string(),
            Style::default().fg(Color::White),
        ),
    ]));
    info_lines.push(Line::from(vec![
        Span::styled("  ID:        ", Style::default().fg(Color::Gray)),
        Span::styled(
            format!("{}…", &session.id[..8]),
            Style::default().fg(Color::DarkGray),
        ),
    ]));

    let info_card = Paragraph::new(info_lines)
        .block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(Style::default().fg(Color::DarkGray)),
        )
        .wrap(Wrap { trim: false });
    f.render_widget(info_card, layout[0]);

    // Conversation turns
    let db_path = session_db_path(&app.copilot_dir);
    let turns = load_turns(&db_path, &session.id);

    let mut turn_lines: Vec<Line> = Vec::new();

    if turns.is_empty() {
        turn_lines.push(Line::from(Span::styled(
            "  No conversation history yet.",
            Style::default().fg(Color::DarkGray),
        )));
        turn_lines.push(Line::from(Span::raw("")));
        turn_lines.push(Line::from(Span::styled(
            "  Press [o] to open this session in Copilot.",
            Style::default().fg(Color::DarkGray),
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
                push_markdown_lines(&mut turn_lines, msg, AGENT_MSG_COLOR, None);
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
                    AGENT_MSG_COLOR,
                    Some(MAX_RESPONSE_LINES),
                ) {
                    turn_lines.push(Line::from(Span::styled(
                        "  … (truncated)",
                        Style::default().fg(Color::DarkGray),
                    )));
                }
                turn_lines.push(Line::from(Span::raw("")));
            }
            turn_lines.push(Line::from(Span::styled(
                format!("  ─── Turn {} ", turn.turn_index + 1),
                Style::default().fg(Color::DarkGray),
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

    let log_title = if is_focused && !turns.is_empty() {
        " Conversation [k/j scroll, o=open live] "
    } else {
        " Conversation "
    };

    let turns_para = Paragraph::new(Text::from(turn_lines))
        .block(
            Block::default()
                .title(log_title)
                .title_style(Style::default().fg(Color::Gray))
                .borders(Borders::NONE),
        )
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
        spans.push(Span::styled(format!("{marker}{content}{marker}"), style));
        rest = &after_content[token_len..];
    }

    if !rest.is_empty() {
        spans.push(Span::styled(rest.to_string(), base_style));
    }
}

fn find_emphasis(text: &str) -> Option<(usize, &'static str, usize)> {
    const EMPHASIS_MARKERS: [&str; 6] = ["***", "___", "**", "__", "*", "_"];

    for (index, _) in text.char_indices() {
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

// ── Embedded terminal renderer ────────────────────────────────────────────────

fn draw_embedded_terminal(f: &mut Frame, term: &crate::terminal::EmbeddedTerminal, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
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
            "Launch: Enter  Cancel: Esc",
            Style::default().fg(Color::Yellow),
        ),
        Mode::Terminal => (
            "Fullscreen: Ctrl+F  Detach: Ctrl+W  Input: forwarded to Copilot",
            Style::default().fg(RUNNING_COLOR),
        ),
        Mode::Normal => {
            let t = match app.active_panel {
                Panel::Sessions => {
                    "Navigate: j/k  Preview scroll: PageUp/PageDown  View/Expand: Enter  Focus dir: f  Collapse dir: c  Open: o  New: n  Reload: r  Quit: q"
                }
                Panel::Detail => {
                    "Scroll: j/k  Back: Esc/h  Focus dir: f  Open: o  New: n  Reload: r  Quit: q"
                }
            };
            (t, Style::default().fg(Color::Gray))
        }
    };

    f.render_widget(
        Paragraph::new(Span::styled(text, style)).alignment(Alignment::Center),
        area,
    );
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
        .border_style(Style::default().fg(ACCENT_COLOR));
    let inner = block.inner(popup);
    f.render_widget(block, popup);
    f.render_widget(
        Paragraph::new(Span::styled(
            format!("▶ {input}█"),
            Style::default().fg(Color::White),
        )),
        inner,
    );
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
                    .fg(Color::Black)
                    .bg(RUNNING_COLOR)
                    .add_modifier(Modifier::BOLD),
            )),
            toast,
        );
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn centered_rect(percent_x: u16, height: u16, area: Rect) -> Rect {
    let w = area.width * percent_x / 100;
    let x = (area.width - w) / 2 + area.x;
    let y = (area.height - height) / 2 + area.y;
    Rect::new(x, y, w, height)
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
