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

const ACTIVE_COLOR: Color = Color::Green;
const INACTIVE_COLOR: Color = Color::DarkGray;
const ACCENT_COLOR: Color = Color::Cyan;
const HEADER_COLOR: Color = Color::Magenta;
const LOAD_MORE_COLOR: Color = Color::Yellow;
const SELECTED_BG: Color = Color::Rgb(40, 56, 80);
const USER_MSG_COLOR: Color = Color::Cyan;
const AGENT_MSG_COLOR: Color = Color::White;
/// Maximum lines shown per assistant response before truncating.
const MAX_RESPONSE_LINES: usize = 20;

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();

    if app.mode == Mode::Terminal && app.terminal_fullscreen {
        if let Some(ref terminal) = app.embedded_terminal {
            draw_embedded_terminal(f, terminal, area, true);
        }
        if app.status_message.is_some() {
            draw_status_toast(f, app, area);
        }
        return;
    }

    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(3)])
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

    let block = Block::default()
        .title(" Sessions ")
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
                items.push(ListItem::new(Line::from(vec![
                    Span::styled("▸ ", Style::default().fg(HEADER_COLOR)),
                    Span::styled(
                        label,
                        Style::default()
                            .fg(HEADER_COLOR)
                            .add_modifier(Modifier::BOLD),
                    ),
                ])));
                list_idx += 1;
            }
            FlatItem::SessionEntry(idx) => {
                let session = &app.sessions[*idx];
                let is_cursor = app.cursor == flat_idx;
                let is_selected = app.selected_session == Some(*idx);

                let (status_color, status_sym) = match session.status {
                    SessionStatus::Active => (ACTIVE_COLOR, "● "),
                    SessionStatus::Inactive => (INACTIVE_COLOR, "○ "),
                };

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
            draw_embedded_terminal(f, terminal, area, false);
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
    let (status_color, status_sym) = match session.status {
        SessionStatus::Active => (ACTIVE_COLOR, "●"),
        SessionStatus::Inactive => (INACTIVE_COLOR, "○"),
    };

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
    let visible_height = layout[1].height.saturating_sub(2) as usize;
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

// ── Embedded terminal renderer ────────────────────────────────────────────────

fn draw_embedded_terminal(
    f: &mut Frame,
    term: &crate::terminal::EmbeddedTerminal,
    area: Rect,
    fullscreen: bool,
) {
    let block = Block::default()
        .title(format!(
            " {} ",
            &term.session_id[..term.session_id.len().min(20)]
        ))
        .title_style(
            Style::default()
                .fg(ACTIVE_COLOR)
                .add_modifier(Modifier::BOLD),
        )
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACTIVE_COLOR));

    let inner = block.inner(area);
    f.render_widget(block, area);

    // Split into a 1-row "LIVE" banner and the vt100 render area.
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(inner);

    // Banner row
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                " ● LIVE ",
                Style::default()
                    .fg(Color::Black)
                    .bg(ACTIVE_COLOR)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                if fullscreen {
                    "   Ctrl+F: split view   Ctrl+W: detach  "
                } else {
                    "   Ctrl+F: fullscreen   Ctrl+W: detach  "
                },
                Style::default().fg(Color::DarkGray),
            ),
        ])),
        layout[0],
    );

    // vt100 screen
    let vt100_area = layout[1];
    render_vt100_screen(f, term, vt100_area);
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
            " [Enter] Launch  [Esc] Cancel ",
            Style::default().fg(Color::Yellow),
        ),
        Mode::Terminal => (
            " [Ctrl+F] Fullscreen  [Ctrl+W] Detach  (all other keys forwarded to Copilot) ",
            Style::default().fg(ACTIVE_COLOR),
        ),
        Mode::Normal => {
            let t = match app.active_panel {
                Panel::Sessions => {
                    " [j/k] Navigate  [Enter] View/Expand  [o] Open live  [n] New  [r] Reload  [q] Quit "
                }
                Panel::Detail => {
                    " [j/k] Scroll  [o] Open live  [Esc/h] Back  [n] New  [q] Quit "
                }
            };
            (t, Style::default().fg(Color::Gray))
        }
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    f.render_widget(block, area);
    f.render_widget(
        Paragraph::new(Span::styled(text, style)).alignment(Alignment::Center),
        inner,
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
                    .bg(ACTIVE_COLOR)
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
