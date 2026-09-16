//! M1 rendering: tab bar, transcript viewport, status bar, modal input,
//! centered diff-approval modal. Truncates (never wraps) long lines so
//! scroll offsets stay exact.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Tabs, Wrap},
    Frame,
};

use crate::app::{App, Mode};

pub fn render(f: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(3),
        ])
        .split(f.area());

    render_tabs(f, app, chunks[0]);
    render_transcript(f, app, chunks[1]);
    render_status(f, app, chunks[2]);
    render_input(f, app, chunks[3]);

    if app.active().pending_diff.is_some() {
        render_diff_modal(f, app);
    }
    if app.mode == Mode::Picker {
        render_picker_modal(f, app);
    }
}

fn render_tabs(f: &mut Frame, app: &App, area: Rect) {
    let titles: Vec<Line> = app
        .sessions
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let dot = if s.busy { "● " } else { "○ " };
            let label = format!("{}{}:{} ({})", dot, s.name, s.backend.label(), i + 1);
            Line::from(label)
        })
        .collect();
    let tabs = Tabs::new(titles)
        .select(app.active)
        .style(Style::default().fg(Color::White))
        .highlight_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
    f.render_widget(tabs, area);
}

fn render_transcript(f: &mut Frame, app: &mut App, area: Rect) {
    let width = area.width.max(1) as usize;
    app.viewport_height = area.height.max(1) as usize;
    app.viewport_width = width;
    let vh = app.viewport_height;
    let s = app.active_mut();
    s.ensure_cache(width);
    // Walk the height cache to the first visible row (grok-build-style
    // virtualized window: only visible rows are laid out, never the tail).
    let mut li = 0usize;
    let mut consumed = 0usize;
    while li < s.lines.len() && consumed + s.row_cache[li] <= s.scroll {
        consumed += s.row_cache[li];
        li += 1;
    }
    let mut sub = s.scroll.saturating_sub(consumed);
    let mut items = Vec::new();
    while items.len() < vh && li < s.lines.len() {
        let line = &s.lines[li];
        let style = if line.starts_with('>') {
            Style::default().fg(Color::Cyan)
        } else if line.starts_with("mock: approved") || line.starts_with("muse: approved") {
            Style::default().fg(Color::Green)
        } else if line.starts_with("mock: ") || line.starts_with("muse: ") {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default()
        };
        for chunk in crate::app::Session::wrap_line(line, width).iter().skip(sub) {
            items.push(ListItem::new(Line::from(Span::styled(
                chunk.clone(),
                style,
            ))));
            if items.len() >= vh {
                break;
            }
        }
        li += 1;
        sub = 0;
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} transcript ", s.name));
    f.render_widget(List::new(items).block(block), area);
}

fn render_status(f: &mut Frame, app: &App, area: Rect) {
    let s = app.active();
    let total = s.total_rows;
    let state = if s.busy { "BUSY" } else { "idle" };
    let mode_color = match app.mode {
        Mode::Normal => Color::Green,
        Mode::Insert => Color::Yellow,
        Mode::Search => Color::Magenta,
        Mode::Picker => Color::Cyan,
    };
    let line = Line::from(vec![
        Span::styled(
            format!(" {} ", app.mode.label()),
            Style::default()
                .fg(Color::Black)
                .bg(mode_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            " tab {}/{} {} [{}]  scroll {}/{}  mouse:{}  {}",
            app.active + 1,
            app.sessions.len(),
            s.name,
            state,
            s.scroll.min(total.saturating_sub(1).max(0)) + 1,
            total.max(1),
            if app.mouse { "on" } else { "off" },
            app.flash,
        )),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn render_input(f: &mut Frame, app: &mut App, area: Rect) {
    let (title, content) = match app.mode {
        Mode::Normal => (" input — NORMAL (i/a type, / search, P provider, 1-3 tabs) ", app.active().input.clone()),
        Mode::Insert => (" input — INSERT (Esc done, Enter send) ", app.active().input.clone()),
        Mode::Search => (" search — (Enter find, Esc cancel) ", format!("/{}", app.search_input)),
        Mode::Picker => (" provider — (j/k move, Enter switch, 1-3 quick, Esc cancel) ", String::new()),
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    f.render_widget(Paragraph::new(content.clone()).block(block), area);
    // Place the terminal cursor at the edit point in text-entry modes.
    if matches!(app.mode, Mode::Insert | Mode::Search) {
        let col = match app.mode {
            Mode::Insert => char_count(&app.active().input[..char_byte_index(&app.active().input, app.active().cursor)]),
            _ => 1 + char_count(&app.search_input[..char_byte_index(&app.search_input, app.search_cursor)]),
        };
        f.set_cursor_position((inner.x + col as u16, inner.y));
    }
}

fn render_diff_modal(f: &mut Frame, app: &App) {
    let area = centered(f.area(), 76, 60);
    f.render_widget(Clear, area);
    let s = app.active();
    let Some(diff) = s.pending_diff.as_ref() else {
        return;
    };
    let text = format!("{}\n\n{}", diff.body, "y approve · n reject · a approve-all · q later");
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" DIFF — {} ", diff.file))
        .style(Style::default().bg(Color::Black));
    f.render_widget(
        Paragraph::new(text).block(block).wrap(Wrap { trim: false }),
        area,
    );
}

fn render_picker_modal(f: &mut Frame, app: &App) {
    use crate::app::BackendKind;
    let area = centered(f.area(), 60, 40);
    f.render_widget(Clear, area);
    let cur = app.sessions[app.active].backend;
    let lines: Vec<Line> = BackendKind::ALL
        .iter()
        .enumerate()
        .map(|(i, (b, desc))| {
            let cursor = if i == app.picker_sel { "> " } else { "  " };
            let here = if *b == cur { " (current)" } else { "" };
            let style = if i == app.picker_sel {
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            Line::from(Span::styled(
                format!("{cursor}{}: {desc}{here}", i + 1),
                style,
            ))
        })
        .collect();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" PROVIDER — switch starts a fresh session ");
    f.render_widget(
        Paragraph::new(lines).block(block).wrap(Wrap { trim: false }),
        area,
    );
}

fn centered(area: Rect, pct_x: u16, pct_y: u16) -> Rect {
    let h = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - pct_x) / 2),
            Constraint::Percentage(pct_x),
            Constraint::Percentage((100 - pct_x) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - pct_y) / 2),
            Constraint::Percentage(pct_y),
            Constraint::Percentage((100 - pct_y) / 2),
        ])
        .split(h[1])[1]
}

fn char_count(s: &str) -> usize {
    s.chars().count()
}

fn char_byte_index(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .map(|(b, _)| b)
        .nth(char_idx)
        .unwrap_or(s.len())
}
