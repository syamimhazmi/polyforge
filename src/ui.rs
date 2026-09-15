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
}

fn render_tabs(f: &mut Frame, app: &App, area: Rect) {
    let titles: Vec<Line> = app
        .sessions
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let dot = if s.busy { "● " } else { "○ " };
            let label = format!("{}{} ({})", dot, s.name, i + 1);
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
    let s = app.active();
    let end = (s.scroll + app.viewport_height).min(s.lines.len());
    let items: Vec<ListItem> = s.lines[s.scroll.min(s.lines.len())..end]
        .iter()
        .map(|l| {
            let t: String = l.chars().take(width).collect();
            let style = if l.starts_with('>') {
                Style::default().fg(Color::Cyan)
            } else if l.starts_with("mock: approved") {
                Style::default().fg(Color::Green)
            } else if l.starts_with("mock: ") {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(Span::styled(t, style)))
        })
        .collect();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} transcript ", s.name));
    f.render_widget(List::new(items).block(block), area);
}

fn render_status(f: &mut Frame, app: &App, area: Rect) {
    let s = app.active();
    let total = s.lines.len();
    let state = if s.busy { "BUSY" } else { "idle" };
    let mode_color = match app.mode {
        Mode::Normal => Color::Green,
        Mode::Insert => Color::Yellow,
        Mode::Search => Color::Magenta,
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
        Mode::Normal => (" input — NORMAL (i/a type, / search, 1-3 tabs) ", app.active().input.clone()),
        Mode::Insert => (" input — INSERT (Esc done, Enter send) ", app.active().input.clone()),
        Mode::Search => (" search — (Enter find, Esc cancel) ", format!("/{}", app.search_input)),
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
