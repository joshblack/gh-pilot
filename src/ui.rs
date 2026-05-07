use crate::app::{App, FlatItem, Mode, Panel};
use crate::session::SessionStatus;
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
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
const PAUSED_COLOR: Color = Color::Yellow;
const ACCENT_COLOR: Color = Color::Cyan;
const HEADER_COLOR: Color = Color::Magenta;
const SELECTED_BG: Color = Color::Rgb(40, 56, 80);

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();

    // ── Outer layout: header / body / footer ────────────────────────────────
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header
            Constraint::Min(0),    // body
            Constraint::Length(3), // footer
        ])
        .split(area);

    draw_header(f, app, outer[0]);
    draw_body(f, app, outer[1]);
    draw_footer(f, app, outer[2]);

    // ── Overlays ─────────────────────────────────────────────────────────────
    match app.mode {
        Mode::NewSessionName => draw_input_popup(f, "New Session — Enter name", &app.input_buffer, area),
        Mode::NewSessionPath => draw_input_popup(f, "New Session — Enter project path (blank = cwd)", &app.input_buffer, area),
        Mode::ConfirmDelete => draw_confirm_popup(f, area),
        Mode::Normal => {}
    }

    // Status message (transient)
    if app.status_message.is_some() {
        draw_status_toast(f, app, area);
    }
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let title = Span::styled(
        " ⚡ gh-mission-control ",
        Style::default()
            .fg(ACCENT_COLOR)
            .add_modifier(Modifier::BOLD),
    );
    let info = Span::styled(
        format!(
            " {} sessions | {} active ",
            app.total_sessions(),
            app.active_count()
        ),
        Style::default().fg(Color::Gray),
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT_COLOR));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let header_layout = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Fill(1), Constraint::Length(30)])
        .split(inner);

    f.render_widget(
        Paragraph::new(title).alignment(Alignment::Left),
        header_layout[0],
    );
    f.render_widget(
        Paragraph::new(info).alignment(Alignment::Right),
        header_layout[1],
    );
}

fn draw_body(f: &mut Frame, app: &mut App, area: Rect) {
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
        .split(area);

    draw_sessions_panel(f, app, body[0]);
    draw_detail_panel(f, app, body[1]);
}

fn draw_sessions_panel(f: &mut Frame, app: &mut App, area: Rect) {
    let is_focused = app.active_panel == Panel::Sessions;
    let border_style = if is_focused {
        Style::default().fg(ACCENT_COLOR)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let block = Block::default()
        .title(" Sessions ")
        .title_style(Style::default().fg(ACCENT_COLOR).add_modifier(Modifier::BOLD))
        .borders(Borders::ALL)
        .border_style(border_style);

    let inner = block.inner(area);
    f.render_widget(block, area);

    // Build list items
    let mut items: Vec<ListItem> = Vec::new();
    let mut list_state = ListState::default();
    // Map flat_list index → list item index (skipping headers which are not selectable)
    let mut selectable_map: Vec<usize> = Vec::new(); // flat index → list item index
    let mut current_list_idx = 0;

    for (flat_idx, item) in app.flat_list.iter().enumerate() {
        match item {
            FlatItem::GroupHeader(path) => {
                // Resolve a displayable short path
                let label = short_path(path);
                let header_item = ListItem::new(Line::from(vec![
                    Span::styled("▸ ", Style::default().fg(HEADER_COLOR)),
                    Span::styled(
                        label,
                        Style::default()
                            .fg(HEADER_COLOR)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]));
                items.push(header_item);
                selectable_map.push(current_list_idx);
                current_list_idx += 1;
            }
            FlatItem::SessionEntry(idx) => {
                let session = &app.sessions[*idx];
                let is_selected = app.cursor == flat_idx;

                let (status_color, status_sym) = match session.status {
                    SessionStatus::Active => (ACTIVE_COLOR, "● "),
                    SessionStatus::Inactive => (INACTIVE_COLOR, "○ "),
                    SessionStatus::Paused => (PAUSED_COLOR, "⏸ "),
                };

                let name_style = if is_selected && is_focused {
                    Style::default()
                        .fg(Color::White)
                        .bg(SELECTED_BG)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White)
                };

                let prefix = if is_selected && is_focused { "  ❯ " } else { "    " };

                let time_str = session.created_at.format("%m/%d %H:%M").to_string();

                let entry = ListItem::new(Line::from(vec![
                    Span::raw(prefix),
                    Span::styled(status_sym, Style::default().fg(status_color)),
                    Span::styled(session.name.as_str(), name_style),
                    Span::styled(
                        format!("  {}", time_str),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
                items.push(entry);

                if is_selected {
                    list_state.select(Some(current_list_idx));
                }
                selectable_map.push(current_list_idx);
                current_list_idx += 1;
            }
        }
    }

    let list = List::new(items).highlight_style(Style::default().bg(SELECTED_BG));
    f.render_stateful_widget(list, inner, &mut list_state);
}

fn draw_detail_panel(f: &mut Frame, app: &mut App, area: Rect) {
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
        let empty = Paragraph::new("Select a session with Enter or Space")
            .style(Style::default().fg(Color::DarkGray))
            .alignment(Alignment::Center);
        let center_y = inner.height / 2;
        let centered = Rect::new(inner.x, inner.y + center_y, inner.width, 1);
        f.render_widget(empty, centered);
        return;
    };

    let session = &app.sessions[idx];

    // ── Detail layout: info card / log output ────────────────────────────────
    let block = Block::default()
        .title(format!(" {} ", session.name))
        .title_style(Style::default().fg(ACCENT_COLOR).add_modifier(Modifier::BOLD))
        .borders(Borders::ALL)
        .border_style(border_style);

    let inner = block.inner(area);
    f.render_widget(block, area);

    let detail_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(7), // info card
            Constraint::Min(1),    // log output
        ])
        .split(inner);

    // ── Info card ────────────────────────────────────────────────────────────
    let (status_color, status_sym) = match session.status {
        SessionStatus::Active => (ACTIVE_COLOR, "●"),
        SessionStatus::Inactive => (INACTIVE_COLOR, "○"),
        SessionStatus::Paused => (PAUSED_COLOR, "⏸"),
    };

    let info_lines = vec![
        Line::from(vec![
            Span::styled("  Status:   ", Style::default().fg(Color::Gray)),
            Span::styled(
                format!("{} {}", status_sym, session.status.label()),
                Style::default()
                    .fg(status_color)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("  Project:  ", Style::default().fg(Color::Gray)),
            Span::styled(
                session.project_path.as_str(),
                Style::default().fg(Color::White),
            ),
        ]),
        Line::from(vec![
            Span::styled("  Created:  ", Style::default().fg(Color::Gray)),
            Span::styled(
                session.created_at.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
                Style::default().fg(Color::White),
            ),
        ]),
        Line::from(vec![
            Span::styled("  Updated:  ", Style::default().fg(Color::Gray)),
            Span::styled(
                session.updated_at.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
                Style::default().fg(Color::White),
            ),
        ]),
        Line::from(vec![
            Span::styled("  ID:       ", Style::default().fg(Color::Gray)),
            Span::styled(
                session.id[..8].to_string() + "…",
                Style::default().fg(Color::DarkGray),
            ),
        ]),
    ];

    if let Some(desc) = &session.description {
        let mut lines = info_lines;
        lines.push(Line::from(vec![
            Span::styled("  Notes:    ", Style::default().fg(Color::Gray)),
            Span::styled(desc.as_str(), Style::default().fg(Color::White)),
        ]));
        let info_card = Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::BOTTOM)
                    .border_style(Style::default().fg(Color::DarkGray)),
            )
            .wrap(Wrap { trim: false });
        f.render_widget(info_card, detail_layout[0]);
    } else {
        let info_card = Paragraph::new(info_lines)
            .block(
                Block::default()
                    .borders(Borders::BOTTOM)
                    .border_style(Style::default().fg(Color::DarkGray)),
            )
            .wrap(Wrap { trim: false });
        f.render_widget(info_card, detail_layout[0]);
    }

    // ── Log output ────────────────────────────────────────────────────────────
    let log_content = session.read_log(&app.sessions_dir);
    let log_lines: Vec<Line> = if log_content.is_empty() {
        vec![Line::from(Span::styled(
            "  No log output yet.",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        log_content
            .lines()
            .map(|l| {
                Line::from(Span::styled(
                    format!("  {l}"),
                    Style::default().fg(Color::White),
                ))
            })
            .collect()
    };

    let total_lines = log_lines.len();
    let visible_height = detail_layout[1].height.saturating_sub(2) as usize;

    // Clamp scroll
    let max_scroll = total_lines.saturating_sub(visible_height);
    if app.log_scroll > max_scroll {
        app.log_scroll = max_scroll;
    }

    let log_title = if is_focused {
        " Output [↑/↓ scroll] "
    } else {
        " Output "
    };

    let log_block = Block::default()
        .title(log_title)
        .title_style(Style::default().fg(Color::Gray))
        .borders(Borders::NONE);

    let log_para = Paragraph::new(Text::from(log_lines))
        .block(log_block)
        .scroll((app.log_scroll as u16, 0))
        .wrap(Wrap { trim: false });

    f.render_widget(log_para, detail_layout[1]);

    // Scrollbar
    if total_lines > visible_height {
        let mut scrollbar_state =
            ScrollbarState::new(total_lines).position(app.log_scroll);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(Some("↑"))
            .end_symbol(Some("↓"));
        f.render_stateful_widget(scrollbar, detail_layout[1], &mut scrollbar_state);
    }
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let (bindings, style) = match app.mode {
        Mode::Normal => {
            let binds = match app.active_panel {
                Panel::Sessions => " [j/k] Navigate  [Enter/Space] Select  [t] Toggle status  [n] New  [d] Delete  [q] Quit ",
                Panel::Detail => " [j/k] Scroll log  [Esc/h] Back to list  [t] Toggle status  [d] Delete  [q] Quit ",
            };
            (binds, Style::default().fg(Color::Gray))
        }
        Mode::NewSessionName | Mode::NewSessionPath => (
            " [Enter] Confirm  [Esc] Cancel ",
            Style::default().fg(PAUSED_COLOR),
        ),
        Mode::ConfirmDelete => (
            " [y] Confirm delete  [n/Esc] Cancel ",
            Style::default().fg(Color::Red),
        ),
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    let inner = block.inner(area);
    f.render_widget(block, area);
    f.render_widget(
        Paragraph::new(Span::styled(bindings, style)).alignment(Alignment::Center),
        inner,
    );
}

fn draw_input_popup(f: &mut Frame, title: &str, input: &str, area: Rect) {
    let popup_area = centered_rect(60, 6, area);
    f.render_widget(Clear, popup_area);

    let block = Block::default()
        .title(format!(" {title} "))
        .title_style(Style::default().fg(ACCENT_COLOR).add_modifier(Modifier::BOLD))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(ACCENT_COLOR));

    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    let display = format!("▶ {input}█");
    let input_para = Paragraph::new(Span::styled(
        display,
        Style::default().fg(Color::White),
    ));
    f.render_widget(input_para, inner);
}

fn draw_confirm_popup(f: &mut Frame, area: Rect) {
    let popup_area = centered_rect(50, 6, area);
    f.render_widget(Clear, popup_area);

    let block = Block::default()
        .title(" Confirm Delete ")
        .title_style(Style::default().fg(Color::Red).add_modifier(Modifier::BOLD))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Red));

    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    let confirm_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1), Constraint::Length(1)])
        .split(inner);

    f.render_widget(
        Paragraph::new(Span::styled(
            "Delete this session?",
            Style::default().fg(Color::White),
        ))
        .alignment(Alignment::Center),
        confirm_layout[0],
    );
    f.render_widget(
        Paragraph::new(Span::styled(
            "This cannot be undone.",
            Style::default().fg(Color::Gray),
        ))
        .alignment(Alignment::Center),
        confirm_layout[1],
    );
    f.render_widget(
        Paragraph::new(Span::styled(
            "[y] Delete  [n] Cancel",
            Style::default().fg(Color::Yellow),
        ))
        .alignment(Alignment::Center),
        confirm_layout[2],
    );
}

fn draw_status_toast(f: &mut Frame, app: &App, area: Rect) {
    if let Some(msg) = &app.status_message {
        let width = (msg.len() + 4).min(area.width as usize) as u16;
        let toast_area = Rect::new(
            area.x + area.width.saturating_sub(width + 2),
            area.y + area.height.saturating_sub(4),
            width,
            1,
        );
        f.render_widget(Clear, toast_area);
        f.render_widget(
            Paragraph::new(Span::styled(
                format!("  {msg}  "),
                Style::default()
                    .fg(Color::Black)
                    .bg(ACTIVE_COLOR)
                    .add_modifier(Modifier::BOLD),
            )),
            toast_area,
        );
    }
}

/// Create a centered rect with percentage of width/height and minimum height.
fn centered_rect(percent_x: u16, height: u16, area: Rect) -> Rect {
    let popup_width = area.width * percent_x / 100;
    let popup_x = (area.width - popup_width) / 2 + area.x;
    let popup_y = (area.height - height) / 2 + area.y;
    Rect::new(popup_x, popup_y, popup_width, height)
}

fn short_path(path: &str) -> String {
    // Replace home dir prefix with ~
    if let Some(home) = dirs::home_dir() {
        let home_str = home.to_string_lossy().to_string();
        if path.starts_with(&home_str) {
            return path.replacen(&home_str, "~", 1);
        }
    }
    // Already uses ~ or is short enough
    path.to_string()
}
