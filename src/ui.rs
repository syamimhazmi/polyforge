//! M1 rendering: tab bar, transcript viewport, status bar, modal input,
//! centered diff-approval modal. Truncates (never wraps) long lines so
//! scroll offsets stay exact.

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Tabs, Wrap},
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
    render_cmd_popup(f, app, chunks[3]);

    if app.active().pending_diff.is_some() {
        render_diff_modal(f, app);
    }
    if app.mode == Mode::Picker {
        render_picker_modal(f, app);
    }
    if app.mode == Mode::Sessions {
        render_sessions_modal(f, app);
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
    // Inner origin for mouse cell → text mapping (recorded every frame).
    app.text_area = if area.width > 2 && area.height > 2 {
        Some((area.x + 1, area.y + 1, area.width - 2, area.height - 2))
    } else {
        None
    };
    let vh = app.viewport_height;
    let sel = app.sel;
    let s = app.active_mut();
    s.ensure_cache(width);
    // Committed lines + live Grok stream drafts (thought / open answer).
    let drafts = s.stream_draft_lines();
    let draft_rows: Vec<usize> = drafts
        .iter()
        .map(|l| crate::app::wrap_rows(l, width))
        .collect();
    let n_committed = s.lines.len();
    let n_view = n_committed + drafts.len();
    // Walk the height cache to the first visible row (grok-build-style
    // virtualized window: only visible rows are laid out, never the tail).
    let mut li = 0usize;
    let mut consumed = 0usize;
    while li < n_view {
        let rows = if li < n_committed {
            s.row_cache[li]
        } else {
            draft_rows[li - n_committed]
        };
        if consumed + rows > s.scroll {
            break;
        }
        consumed += rows;
        li += 1;
    }
    let mut sub = s.scroll.saturating_sub(consumed);
    let mut items = Vec::new();
    while items.len() < vh && li < n_view {
        let raw = if li < n_committed {
            s.lines[li].as_str()
        } else {
            drafts[li - n_committed].as_str()
        };
        // S3-F1 render defense: lines are clean at ingress, but never
        // trust the buffer on the way to the terminal.
        let line = crate::app::sanitize_text(raw);
        let style = if line.starts_with('>') {
            Style::default().fg(Color::Cyan)
        } else if line.starts_with("mock: approved") || line.starts_with("muse: approved") {
            Style::default().fg(Color::Green)
        } else if line.starts_with("mock: ")
            || line.starts_with("muse: ")
            || line.starts_with("grok: ∴")
        {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default()
        };
        let line_len = line.chars().count();
        // Drafts are not mouse-selectable (no stable line index in `lines`).
        let span = if li < n_committed {
            sel.and_then(|sel| sel.span_on_line(li, line_len))
        } else {
            None
        };
        let chunks = crate::app::Session::wrap_line(&line, width);
        let mut coff = chunks
            .iter()
            .take(sub)
            .map(|c| c.chars().count())
            .sum::<usize>();
        for chunk in chunks.iter().skip(sub) {
            let spans = match span {
                Some((ss, se)) => hl_spans(chunk, coff, ss, se, style),
                None => vec![Span::styled(chunk.clone(), style)],
            };
            items.push(ListItem::new(Line::from(spans)));
            if items.len() >= vh {
                break;
            }
            coff += chunk.chars().count();
        }
        li += 1;
        sub = 0;
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} transcript ", s.name));
    f.render_widget(List::new(items).block(block), area);
}

/// Split one wrapped chunk into plain/highlighted spans for the selected
/// char range [ss, se). `coff` is the chunk's first-char index in the line.
fn hl_spans(chunk: &str, coff: usize, ss: usize, se: usize, style: Style) -> Vec<Span<'static>> {
    let chars: Vec<char> = chunk.chars().collect();
    let len = chars.len();
    let s = ss.saturating_sub(coff).min(len);
    let e = se.saturating_sub(coff).min(len);
    if s >= e {
        return vec![Span::styled(chunk.to_string(), style)];
    }
    let mut out = Vec::new();
    if s > 0 {
        out.push(Span::styled(chars[..s].iter().collect::<String>(), style));
    }
    out.push(Span::styled(
        chars[s..e].iter().collect::<String>(),
        style.add_modifier(Modifier::REVERSED),
    ));
    if e < len {
        out.push(Span::styled(chars[e..].iter().collect::<String>(), style));
    }
    out
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
        Mode::Sessions => Color::Blue,
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
            // S3-F1: flash carries agent text (method names, errors).
            crate::app::sanitize_text(&app.flash),
        )),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn render_input(f: &mut Frame, app: &mut App, area: Rect) {
    let normal_title = if app.vim {
        " input — NORMAL·vim (j/k move, Space/i/a type, / search, P provider, R fresh) "
    } else {
        " input — NORMAL (arrows move, Space/Enter type, / search, P provider, R fresh) "
    };
    let (title, content) = match app.mode {
        Mode::Normal => (normal_title, app.active().input.clone()),
        Mode::Insert => (
            " input — INSERT (Esc done, Enter send · ↑/↓ pick · Tab completes) ",
            app.active().input.clone(),
        ),
        Mode::Search => (
            " search — (Enter find, Esc cancel) ",
            format!("/{}", app.search_input),
        ),
        Mode::Picker => (
            " provider — (j/k move, Enter switch, 1-5 quick, Esc cancel) ",
            String::new(),
        ),
        Mode::Sessions => (
            " sessions — (j/k move, Enter view + continue, d delete, 1-9 quick, Esc cancel) ",
            String::new(),
        ),
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    f.render_widget(Paragraph::new(content.clone()).block(block), area);
    // Place the terminal cursor at the edit point in text-entry modes.
    if matches!(app.mode, Mode::Insert | Mode::Search) {
        let col = match app.mode {
            Mode::Insert => char_count(
                &app.active().input[..char_byte_index(&app.active().input, app.active().cursor)],
            ),
            _ => {
                1 + char_count(
                    &app.search_input[..char_byte_index(&app.search_input, app.search_cursor)],
                )
            }
        };
        f.set_cursor_position((inner.x + col as u16, inner.y));
    }
}

/// Slash-command suggestions above the input (Insert mode, input starts
/// with `/`). Plain bordered list reusing the picker highlight; the
/// highlight follows `cmd_sel` (Tab accepts, Up/Down moves).
fn render_cmd_popup(f: &mut Frame, app: &App, input_area: Rect) {
    if app.mode != Mode::Insert {
        return;
    }
    let matches = app.slash_matches();
    if matches.is_empty() {
        return;
    }
    let show = matches.len().min(6);
    let height = (show as u16 + 2).min(input_area.y);
    if height < 3 {
        return; // no room above the input on a tiny terminal
    }
    let area = Rect {
        x: input_area.x,
        y: input_area.y - height,
        width: input_area.width,
        height,
    };
    f.render_widget(Clear, area);
    let sel = app.cmd_sel.min(matches.len() - 1);
    let lines: Vec<Line> = matches
        .iter()
        .take(show)
        .enumerate()
        .map(|(row, &i)| {
            let (template, _, desc) = crate::app::SLASH_COMMANDS[i];
            let cursor = if row == sel { "> " } else { "  " };
            let style = if row == sel {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            Line::from(Span::styled(format!("{cursor}{template} — {desc}"), style))
        })
        .collect();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" commands — Tab completes · ↑/↓ picks ");
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_diff_modal(f: &mut Frame, app: &App) {
    let area = centered(f.area(), 76, 60);
    f.render_widget(Clear, area);
    let s = app.active();
    let Some(diff) = s.pending_diff.as_ref() else {
        return;
    };
    let risk = match s.approval_risk.as_ref() {
        Some(j) => j.summary_line(),
        None if s.risk_spawned_gen.is_some() => "risk … scoring".to_string(),
        None => String::new(),
    };
    // S3-F1 render defense: the card body/file are agent-controlled.
    let body = crate::app::sanitize_text(&diff.body);
    let file = crate::app::sanitize_text(&diff.file);
    let text = if risk.is_empty() {
        format!(
            "{}\n\n{}",
            body, "y approve · n reject · a approve-all · q later"
        )
    } else {
        format!(
            "{}\n\n{}\n\n{}",
            body, risk, "y approve · n reject · a approve-all · q later"
        )
    };
    let title = if risk.is_empty() {
        format!(" DIFF — {} ", file)
    } else if let Some(j) = s.approval_risk.as_ref() {
        format!(" DIFF — {} — {} ", file, j.band.label())
    } else {
        format!(" DIFF — {} ", file)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
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
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
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
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_sessions_modal(f: &mut Frame, app: &App) {
    use crate::app::short_id;
    use crate::store::fmt_short;
    let area = centered(f.area(), 78, 60);
    f.render_widget(Clear, area);
    // Columns mirror `grok sessions list`: id + created + updated + summary.
    let lines: Vec<Line> = app
        .sess_list
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let cursor = if i == app.sess_sel { "> " } else { "  " };
            // S3-F1: preview/title come from stored transcripts.
            let summary = if s.title.is_empty() {
                crate::app::sanitize_text(&s.preview)
            } else {
                crate::app::sanitize_text(&s.title)
            };
            let style = if i == app.sess_sel {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            Line::from(Span::styled(
                format!(
                    "{cursor}{}: [{}] {} {}→{} {}",
                    i + 1,
                    s.backend,
                    short_id(&s.id),
                    fmt_short(s.created_at),
                    fmt_short(s.updated_at),
                    summary,
                ),
                style,
            ))
        })
        .collect();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" SESSIONS — Enter views + continues · d deletes · Esc cancels ");
    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use ratatui::{Terminal, backend::TestBackend};

    /// S3-F1: even a raw line that bypassed `push_line` renders inert —
    /// no control byte may reach the terminal buffer, while visible text
    /// and hyperlink labels survive without their URL.
    #[test]
    fn render_neutralizes_injected_line() {
        let mut app = App::new();
        app.sessions[0]
            .lines
            .push("\x1b[2J\x1b]8;;http://evil\x07pwned\x00".to_string());
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|f| render(f, &mut app)).expect("draw");
        let buf = terminal.backend().buffer();
        for cell in buf.content() {
            for ch in cell.symbol().chars() {
                assert!(
                    !ch.is_control(),
                    "control char reached terminal buffer: {ch:?}"
                );
            }
        }
        let text: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("pwned"));
        assert!(!text.contains("evil"));
    }
}
