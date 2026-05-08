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

const RUNNING_COLOR: Color = Color::Rgb(0x9e, 0xce, 0x6a);
const WAITING_COLOR: Color = Color::Rgb(0xe0, 0xaf, 0x68);
const IDLE_COLOR: Color = Color::Rgb(0x56, 0x5f, 0x89);
const ERROR_COLOR: Color = Color::Rgb(0xf7, 0x76, 0x8e);
const ACCENT_COLOR: Color = Color::Rgb(0x7a, 0xa2, 0xf7);
const GROUP_COLOR: Color = Color::Rgb(0x7d, 0xcf, 0xff);
const BACKGROUND_COLOR: Color = Color::Rgb(0x1a, 0x1b, 0x26);
const SURFACE_COLOR: Color = Color::Rgb(0x24, 0x28, 0x3b);
const TEXT_COLOR: Color = Color::Rgb(0xc0, 0xca, 0xf5);
const MUTED_COLOR: Color = IDLE_COLOR;
const SELECTED_BG: Color = ACCENT_COLOR;
const SELECTED_FG: Color = BACKGROUND_COLOR;
const USER_MSG_COLOR: Color = ACCENT_COLOR;
const AGENT_MSG_COLOR: Color = TEXT_COLOR;
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
                let count = group_session_count(app, path).to_string();
                let style = if is_cursor && is_focused {
                    Style::default()
                        .fg(SELECTED_FG)
                        .bg(SELECTED_BG)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                        .fg(GROUP_COLOR)
                        .add_modifier(Modifier::BOLD)
                };
                let label_width =
                    prefix.chars().count() + marker.chars().count() + label.chars().count();
                let suffix_width = focus_suffix.chars().count() + count.chars().count();
                let spacer = " ".repeat(
                    (inner.width as usize)
                        .saturating_sub(label_width + suffix_width)
                        .max(1),
                );
                items.push(ListItem::new(Line::from(vec![
                    Span::raw(prefix),
                    Span::styled(marker, style),
                    Span::styled(label, style),
                    Span::styled(focus_suffix, Style::default().fg(MUTED_COLOR)),
                    Span::raw(spacer),
                    Span::styled(count, Style::default().fg(MUTED_COLOR)),
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
                        .fg(SELECTED_FG)
                        .bg(SELECTED_BG)
                        .add_modifier(Modifier::BOLD)
                } else if is_selected {
                    Style::default().fg(TEXT_COLOR).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(TEXT_COLOR)
                };

                items.push(ListItem::new(Line::from(vec![
                    Span::raw(prefix),
                    Span::styled(status_sym, Style::default().fg(status_color)),
                    Span::raw(" "),
                    Span::styled(name, name_style),
                    Span::styled(format!("  {time_str}"), Style::default().fg(MUTED_COLOR)),
                ])));

                if is_cursor {
                    list_state.select(Some(list_idx));
                }
                list_idx += 1;
            }
        }
    }

    let list = List::new(items)
        .style(Style::default().bg(SURFACE_COLOR))
        .highlight_style(Style::default().fg(SELECTED_FG).bg(SELECTED_BG));
    f.render_stateful_widget(list, inner, &mut list_state);
}

fn group_session_count(app: &App, group_key: &str) -> usize {
    app.sessions
        .iter()
        .filter(|session| session.group_key() == group_key)
        .count()
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
        .title(format!(" {} ", session.display_name()))
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

    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(7), Constraint::Min(1)])
        .split(inner);

    // Info card
    let (status_color, status_sym) = status_display(&session.status);

    let mut info_lines = vec![
        Line::from(vec![
            Span::styled("  Status:    ", Style::default().fg(MUTED_COLOR)),
            Span::styled(
                format!("{status_sym} {}", session.status.label()),
                Style::default()
                    .fg(status_color)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("  Directory: ", Style::default().fg(MUTED_COLOR)),
            Span::styled(
                short_path(&session.cwd.to_string_lossy()),
                Style::default().fg(TEXT_COLOR),
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
    info_lines.push(Line::from(vec![
        Span::styled("  Updated:   ", Style::default().fg(MUTED_COLOR)),
        Span::styled(
            session.updated_at.format("%Y-%m-%d %H:%M UTC").to_string(),
            Style::default().fg(TEXT_COLOR),
        ),
    ]));
    info_lines.push(Line::from(vec![
        Span::styled("  ID:        ", Style::default().fg(MUTED_COLOR)),
        Span::styled(
            format!("{}…", &session.id[..8]),
            Style::default().fg(MUTED_COLOR),
        ),
    ]));

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
    let turns = load_turns(&db_path, &session.id);

    let mut turn_lines: Vec<Line> = Vec::new();

    if turns.is_empty() {
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
                for line in msg.lines() {
                    turn_lines.push(Line::from(Span::styled(
                        format!("  {line}"),
                        Style::default().fg(USER_MSG_COLOR),
                    )));
                }
                turn_lines.push(Line::from(Span::raw("")));
            }
            if let Some(ref resp) = turn.assistant_response {
                turn_lines.push(Line::from(Span::styled(
                    "  Copilot",
                    Style::default()
                        .fg(AGENT_MSG_COLOR)
                        .add_modifier(Modifier::BOLD),
                )));
                for line in resp.lines().take(MAX_RESPONSE_LINES) {
                    turn_lines.push(Line::from(Span::styled(
                        format!("  {line}"),
                        Style::default().fg(AGENT_MSG_COLOR),
                    )));
                }
                if resp.lines().count() > MAX_RESPONSE_LINES {
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

    let log_title = if is_focused && !turns.is_empty() {
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
            "Launch: Enter  Cancel: Esc",
            Style::default().fg(WAITING_COLOR),
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
            (t, Style::default().fg(MUTED_COLOR))
        }
    };

    f.render_widget(
        Paragraph::new(Span::styled(text, style))
            .style(Style::default().bg(BACKGROUND_COLOR))
            .alignment(Alignment::Center),
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
