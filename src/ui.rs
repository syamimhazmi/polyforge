//! Rendering: tab bar, transcript viewport, status bar, modal input,
//! centered diff-approval modal. All colors flow from the central
//! [`crate::theme::Theme`] (grok-build-inspired GrokNight default);
//! backgrounds are never filled so the UI stays readable on any
//! terminal. Truncates (never wraps) long lines so scroll offsets stay
//! exact.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, List, ListItem, Paragraph, Tabs, Wrap},
    Frame,
};

use crate::app::{App, Mode};
use crate::theme::Theme;

pub fn render(f: &mut Frame, app: &mut App) {
    let th = Theme::get(app.theme);
    // Startup (one tab still on the welcome dashboard): no tab bar. It
    // appears once the tab has content or a second tab opens.
    let tab_rows = if app.sessions.len() == 1 && shows_welcome(app) {
        0
    } else {
        1
    };
    // Turn-status row: only while the active tab is busy (steals one
    // transcript row, like grok-build's turn_status view).
    let turn_rows = u16::from(app.active().busy);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(tab_rows),
            Constraint::Min(3),
            Constraint::Length(turn_rows),
            Constraint::Length(1),
            Constraint::Length(3),
        ])
        .split(f.area());

    render_tabs(f, app, th, chunks[0]);
    render_transcript(f, app, th, chunks[1]);
    render_turn_status(f, app, th, chunks[2]);
    render_status(f, app, th, chunks[3]);
    render_input(f, app, th, chunks[4]);
    render_cmd_popup(f, app, th, chunks[4]);

    if app.active().pending_diff.is_some() {
        render_diff_modal(f, app, th);
    }
    if app.mode == Mode::Picker {
        render_picker_modal(f, app, th);
    }
    if app.mode == Mode::Sessions {
        render_sessions_modal(f, app, th);
    }
}

fn render_tabs(f: &mut Frame, app: &App, th: Theme, area: Rect) {
    let titles: Vec<Line> = app
        .sessions
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let (dot, dot_color) = if s.busy {
                ("● ", th.tab_busy)
            } else {
                ("○ ", th.muted)
            };
            let rest = format!("{}:{} ({})", s.name, s.backend.label(), i + 1);
            let style = if i == app.active {
                Style::default()
                    .fg(th.tab_active)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(th.text_secondary)
            };
            // The busy dot keeps its own color inside an inactive tab.
            Line::from(vec![
                Span::styled(dot, Style::default().fg(dot_color)),
                Span::styled(rest, style),
            ])
        })
        .collect();
    let tabs = Tabs::new(titles)
        .select(app.active)
        .style(Style::default().fg(th.text_secondary))
        .highlight_style(
            Style::default()
                .fg(th.tab_active)
                .add_modifier(Modifier::BOLD),
        );
    f.render_widget(tabs, area);
}

/// The active tab is empty, so it shows the welcome dashboard.
fn shows_welcome(app: &App) -> bool {
    app.active().lines.is_empty() && app.active().stream_draft_lines().is_empty()
}

fn render_transcript(f: &mut Frame, app: &mut App, th: Theme, area: Rect) {
    // Inner size: the block borders take 2 columns and 2 rows (wrapping at
    // the outer width clipped the last 2 chars of every full-width row).
    let width = area.width.saturating_sub(2).max(1) as usize;
    // Inner rows: the block borders take 2 (counting them hid the last two
    // transcript rows when pinned to the bottom).
    app.viewport_height = area.height.saturating_sub(2).max(1) as usize;
    app.viewport_width = width;
    // Inner origin for mouse cell → text mapping (recorded every frame).
    app.text_area = if area.width > 2 && area.height > 2 {
        Some((area.x + 1, area.y + 1, area.width - 2, area.height - 2))
    } else {
        None
    };
    // Empty tab (fresh boot, /new, /tab new): welcome dashboard instead
    // of a blank pane. The first transcript line replaces it.
    if shows_welcome(app) {
        render_welcome(f, app, th, area);
        return;
    }
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
    // Keep a bottom-pinned view pinned when the live block or the
    // turn-status row changes height; a scrolled-up view stays put.
    let total = s.total_rows + draft_rows.iter().sum::<usize>();
    let tail = total.saturating_sub(vh);
    if let Some((prev_total, prev_vh)) = s.last_geom {
        s.scroll = if s.scroll + prev_vh >= prev_total {
            tail
        } else {
            s.scroll.min(tail)
        };
    }
    s.last_geom = Some((total, vh));
    let thought_n = s.thought_block().len();
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
    // Live-turn rows get the activity rail (only while animating).
    let rail = s
        .phase
        .filter(|_| s.pending_diff.is_none())
        .zip(s.turn_since)
        .map(|(p, t)| (p, t.elapsed(), s.scroll));
    let mut live_rows: Vec<usize> = Vec::new();
    while items.len() < vh && li < n_view {
        let raw = if li < n_committed {
            s.lines[li].as_str()
        } else {
            drafts[li - n_committed].as_str()
        };
        // S3-F1 render defense: lines are clean at ingress, but never
        // trust the buffer on the way to the terminal.
        let line = crate::app::sanitize_text(raw);
        let style = match li.checked_sub(n_committed) {
            Some(0) if thought_n > 0 => Style::default().fg(th.assistant),
            Some(d) if d < thought_n => Style::default().fg(th.muted),
            _ => transcript_style(&line, th),
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
        let live = li >= n_committed || (li >= s.turn_first_line && !line.starts_with('>'));
        for chunk in chunks.iter().skip(sub) {
            if live {
                live_rows.push(items.len());
            }
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
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(th.border))
        .title(Line::from(Span::styled(
            format!(" {} transcript ", s.name),
            Style::default().fg(th.text_secondary),
        )));
    f.render_widget(List::new(items).block(block), area);
    if let Some((phase, elapsed, scroll)) = rail {
        use crate::activity::{blend, wave_brightness, Phase};
        let accent = if phase == Phase::Thinking {
            th.assistant
        } else {
            th.running
        };
        for i in live_rows {
            let y = area.y + 1 + i as u16;
            if area.width < 2 || y + 1 >= area.y + area.height {
                continue;
            }
            let color = blend(th.border, accent, wave_brightness(elapsed, scroll + i));
            f.buffer_mut()[(area.x, y)].set_symbol("┃").set_fg(color);
        }
    }
}

/// "POLYFORGE" in figlet ANSI Shadow, one glyph per letter (6 rows).
const LOGO_GLYPHS: [[&str; 6]; 9] = [
    [
        "██████╗ ",
        "██╔══██╗",
        "██████╔╝",
        "██╔═══╝ ",
        "██║     ",
        "╚═╝     ",
    ],
    [
        " ██████╗ ",
        "██╔═══██╗",
        "██║   ██║",
        "██║   ██║",
        "╚██████╔╝",
        " ╚═════╝ ",
    ],
    [
        "██╗     ",
        "██║     ",
        "██║     ",
        "██║     ",
        "███████╗",
        "╚══════╝",
    ],
    [
        "██╗   ██╗",
        "╚██╗ ██╔╝",
        " ╚████╔╝ ",
        "  ╚██╔╝  ",
        "   ██║   ",
        "   ╚═╝   ",
    ],
    [
        "███████╗",
        "██╔════╝",
        "█████╗  ",
        "██╔══╝  ",
        "██║     ",
        "╚═╝     ",
    ],
    [
        " ██████╗ ",
        "██╔═══██╗",
        "██║   ██║",
        "██║   ██║",
        "╚██████╔╝",
        " ╚═════╝ ",
    ],
    [
        "██████╗ ",
        "██╔══██╗",
        "██████╔╝",
        "██╔══██╗",
        "██║  ██║",
        "╚═╝  ╚═╝",
    ],
    [
        " ██████╗ ",
        "██╔════╝ ",
        "██║  ███╗",
        "██║   ██║",
        "╚██████╔╝",
        " ╚═════╝ ",
    ],
    [
        "███████╗",
        "██╔════╝",
        "█████╗  ",
        "██╔══╝  ",
        "███████╗",
        "╚══════╝",
    ],
];

/// Welcome dashboard for an empty tab: forge-colored logo, the tab's
/// backend + readiness, workspace, theme/keymap, and the first keys to
/// press. Centered as one block so the label column stays aligned.
fn render_welcome(f: &mut Frame, app: &App, th: Theme, area: Rect) {
    use crate::app::{BackendKind, MAX_SESSIONS};
    let s = app.active();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(th.border))
        .title(Line::from(Span::styled(
            format!(" {} welcome ", s.name),
            Style::default().fg(th.text_secondary),
        )));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();
    let logo_w: usize = LOGO_GLYPHS.iter().map(|g| g[0].chars().count()).sum();
    if (inner.width as usize) >= logo_w && inner.height >= 16 {
        // Top-down forge heat: gold → orange → red.
        let heat = [th.plan, th.plan, th.warn, th.warn, th.path, th.err];
        for (row, color) in heat.into_iter().enumerate() {
            let text: String = LOGO_GLYPHS.iter().map(|g| g[row]).collect();
            lines.push(Line::from(Span::styled(text, Style::default().fg(color))));
        }
    } else {
        lines.push(Line::from(Span::styled(
            "▲ P O L Y F O R G E",
            Style::default().fg(th.plan).add_modifier(Modifier::BOLD),
        )));
    }
    lines.push(Line::from(vec![
        Span::styled(
            "vim-modal multi-tab TUI for coding agents",
            Style::default().fg(th.text_secondary),
        ),
        Span::styled(
            concat!(" · v", env!("CARGO_PKG_VERSION")),
            Style::default().fg(th.muted),
        ),
    ]));
    lines.push(Line::default());

    let row = |label: &str, value: Vec<Span<'static>>| {
        let mut spans = vec![Span::styled(
            format!("  {label:<11}"),
            Style::default().fg(th.muted),
        )];
        spans.extend(value);
        Line::from(spans)
    };
    let (dot, state, state_color) = if app.onboarding {
        ("○", "choose a provider to start", th.warn)
    } else if s.backend == BackendKind::Mock {
        ("●", "ready (offline)", th.ok)
    } else if s.remote_id.is_some() {
        ("●", "ready", th.ok)
    } else {
        ("○", "connecting…", th.running)
    };
    lines.push(row(
        "backend",
        vec![
            Span::styled(
                s.backend.label(),
                Style::default().fg(th.skill).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("  {dot} {state}"), Style::default().fg(state_color)),
        ],
    ));
    if !app.workspace.is_empty() {
        lines.push(row(
            "workspace",
            vec![Span::styled(
                crate::app::sanitize_text(&tilde_home(&app.workspace)),
                Style::default().fg(th.path),
            )],
        ));
    }
    lines.push(row(
        "theme",
        vec![Span::styled(
            app.theme.name(),
            Style::default().fg(th.text_primary),
        )],
    ));
    lines.push(row(
        "keymap",
        vec![Span::styled(
            if app.vim { "vim" } else { "default" },
            Style::default().fg(th.text_primary),
        )],
    ));
    lines.push(row(
        "tabs",
        vec![Span::styled(
            format!("{}/{MAX_SESSIONS}", app.sessions.len()),
            Style::default().fg(th.text_primary),
        )],
    ));
    lines.push(Line::default());

    let key = |k: &'static str| Span::styled(k, Style::default().fg(th.tab_active));
    let sep = || Span::styled(" · ", Style::default().fg(th.muted));
    let desc = |d: &'static str| Span::styled(d, Style::default().fg(th.text_secondary));
    let type_key = if app.vim { "Space/i" } else { "Space/Enter" };
    lines.push(Line::from(vec![
        Span::raw("  "),
        key(type_key),
        desc(" type"),
        sep(),
        key("P"),
        desc(" provider"),
        sep(),
        key("/sessions"),
        desc(" resume"),
    ]));
    lines.push(Line::from(vec![
        Span::raw("  "),
        key("/tab new"),
        desc(" tab"),
        sep(),
        key("/theme"),
        desc(" palette"),
        sep(),
        key("/help"),
        desc(" all"),
        sep(),
        key("q"),
        desc(" quit"),
    ]));

    // Center the whole block; overflow truncates (no wrap).
    let w = lines.iter().map(Line::width).max().unwrap_or(0) as u16;
    let h = lines.len() as u16;
    let rect = Rect {
        x: inner.x + inner.width.saturating_sub(w) / 2,
        y: inner.y + inner.height.saturating_sub(h) / 2,
        width: w.min(inner.width),
        height: h.min(inner.height),
    };
    f.render_widget(Paragraph::new(lines), rect);
}

/// `$HOME/...` → `~/...` for display.
fn tilde_home(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && path.starts_with(&home) => {
            format!("~{}", &path[home.len()..])
        }
        _ => path.to_string(),
    }
}

/// Role color for one sanitized transcript line. User prompts read
/// cyan, approval outcomes carry traffic colors, Grok thoughts render
/// in the assistant accent, tool/plan lines get their own hues, and the
/// generic backend chatter stays dim — grok-build role semantics.
fn transcript_style(line: &str, th: Theme) -> Style {
    if line.starts_with('>') {
        Style::default().fg(th.user)
    } else if line.contains("approved") || line.contains('✓') || line.contains("already applied")
    {
        Style::default().fg(th.ok)
    } else if line.contains("rejected") || line.contains('✗') || line.contains("failed") {
        Style::default().fg(th.err)
    } else if line.contains("deferred") {
        Style::default().fg(th.warn)
    } else if line.starts_with("∴ Thought") || line.starts_with("grok: ∴") {
        Style::default().fg(th.assistant)
    } else if line.starts_with("grok: plan") {
        Style::default().fg(th.plan)
    } else if line.starts_with("grok: ⚙")
        || (line.starts_with("grok: ") && line.contains('[') && line.contains(']'))
    {
        Style::default().fg(th.tool)
    } else if line.starts_with("mock: ")
        || line.starts_with("muse: ")
        || line.starts_with("codex: ")
        || line.starts_with("agy: ")
        || line.starts_with("grok: ")
        || line.starts_with("claude: ")
    {
        Style::default().fg(th.muted)
    } else {
        Style::default().fg(th.text_primary)
    }
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

fn render_status(f: &mut Frame, app: &App, th: Theme, area: Rect) {
    let s = app.active();
    let total = s.total_rows;
    let mut spans = vec![
        Span::styled(
            format!(" {} ", app.mode.label()),
            Style::default()
                .fg(Color::Black)
                .bg(th.mode_color(app.mode))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" tab {}/{} {} ", app.active + 1, app.sessions.len(), s.name,),
            Style::default().fg(th.text_secondary),
        ),
    ];
    spans.push(if s.busy {
        Span::styled(
            "[busy]",
            Style::default().fg(th.running).add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled("[idle]", Style::default().fg(th.muted))
    });
    spans.extend([Span::styled(
        format!(
            "  scroll {}/{}  mouse:{} ",
            s.scroll.min(total.saturating_sub(1).max(0)) + 1,
            total.max(1),
            if app.mouse { "on" } else { "off" },
        ),
        Style::default().fg(th.muted),
    )]);
    if !app.flash.is_empty() {
        spans.push(Span::styled(
            // S3-F1: flash carries agent text (method names, errors).
            crate::app::sanitize_text(&app.flash),
            Style::default().fg(th.warn),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Turn-status row above the input (grok-build style): spinner, phase and
/// phase timer on the left, turn timer on the right; a static marker while
/// a diff awaits approval. Zero-height (skipped) when the tab is idle.
fn render_turn_status(f: &mut Frame, app: &App, th: Theme, area: Rect) {
    use crate::activity::{format_elapsed, spinner_frame, Phase};
    use std::time::Duration;
    let s = app.active();
    if area.height == 0 || !s.busy {
        return;
    }
    if s.pending_diff.is_some() {
        f.render_widget(
            Paragraph::new(Span::styled(
                " ◆ awaiting approval",
                Style::default().fg(th.warn),
            )),
            area,
        );
        return;
    }
    let now = std::time::Instant::now();
    let since = |t: Option<std::time::Instant>| t.map_or(Duration::ZERO, |t| now - t);
    let label = s.phase.unwrap_or(Phase::Thinking).label();
    let left = Line::from(vec![
        Span::styled(
            format!(" {}", spinner_frame(since(s.turn_since))),
            Style::default().fg(th.running),
        ),
        Span::styled(
            format!(" {} ", label.trim_end_matches('…')),
            Style::default().fg(th.text_secondary),
        ),
        Span::styled(
            format_elapsed(since(s.phase_since)),
            Style::default().fg(th.muted),
        ),
    ]);
    f.render_widget(Paragraph::new(left), area);
    f.render_widget(
        Paragraph::new(Span::styled(
            format!("{} ", format_elapsed(since(s.turn_since))),
            Style::default().fg(th.muted),
        ))
        .alignment(ratatui::layout::Alignment::Right),
        area,
    );
}

fn render_input(f: &mut Frame, app: &mut App, th: Theme, area: Rect) {
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
        Mode::Picker if app.onboarding => (
            " provider — (j/k move, Enter start, 1-5 quick, Esc quit) ",
            String::new(),
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
    // Focused (text-entry) modes get the brighter active chrome,
    // grok-build prompt-widget style: dim border at rest.
    let focused = matches!(app.mode, Mode::Insert | Mode::Search);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if focused { th.border_active } else { th.border }))
        .title(Line::from(Span::styled(
            title.to_string(),
            Style::default().fg(if focused { th.text_primary } else { th.muted }),
        )));
    let inner = block.inner(area);
    f.render_widget(
        Paragraph::new(content.clone())
            .style(Style::default().fg(th.text_primary))
            .block(block),
        area,
    );
    // Place the terminal cursor at the edit point in text-entry modes.
    if focused {
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
fn render_cmd_popup(f: &mut Frame, app: &App, th: Theme, input_area: Rect) {
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
                    .fg(th.tab_active)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(th.text_secondary)
            };
            Line::from(Span::styled(format!("{cursor}{template} — {desc}"), style))
        })
        .collect();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(th.border))
        .title(Line::from(Span::styled(
            " commands — Tab completes · ↑/↓ picks ",
            Style::default().fg(th.muted),
        )));
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_diff_modal(f: &mut Frame, app: &App, th: Theme) {
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
    // The modal chrome takes the risk band color so severity reads at a
    // glance (green/yellow/red); unscored approvals stay neutral.
    let band_color = s
        .approval_risk
        .as_ref()
        .map(|j| th.risk(j.band))
        .unwrap_or(th.border_active);
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
    // Filename reads in the path accent; the band tag takes the risk
    // color so severity is visible in the title alone.
    let mut title_spans = vec![
        Span::styled(" DIFF — ", Style::default().fg(th.text_secondary)),
        Span::styled(file, Style::default().fg(th.path)),
    ];
    if let Some(j) = s.approval_risk.as_ref().filter(|_| !risk.is_empty()) {
        title_spans.push(Span::styled(
            format!(" — {} ", j.band.label()),
            Style::default().fg(band_color).add_modifier(Modifier::BOLD),
        ));
    } else {
        title_spans.push(Span::raw(" "));
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(band_color))
        .title(Line::from(title_spans));
    f.render_widget(
        Paragraph::new(text)
            .style(Style::default().fg(th.text_primary))
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_picker_modal(f: &mut Frame, app: &App, th: Theme) {
    use crate::app::BackendKind;
    let area = centered(f.area(), 60, 40);
    f.render_widget(Clear, area);
    let cur = app.sessions[app.active].backend;
    let lines: Vec<Line> = BackendKind::ALL
        .iter()
        .enumerate()
        .map(|(i, (b, desc))| {
            let cursor = if i == app.picker_sel { "> " } else { "  " };
            let selected = i == app.picker_sel;
            let row = if selected {
                Style::default()
                    .fg(th.tab_active)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(th.text_primary)
            };
            let mut spans = vec![Span::styled(format!("{cursor}{}: {desc}", i + 1), row)];
            if *b == cur && !app.onboarding {
                spans.push(Span::styled(
                    " (current)",
                    if selected {
                        row
                    } else {
                        Style::default().fg(th.skill)
                    },
                ));
            }
            Line::from(spans)
        })
        .collect();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(th.border_active))
        .title(Line::from(Span::styled(
            if app.onboarding {
                " WELCOME — choose a provider to start (saved; P switches later) "
            } else {
                " PROVIDER — switch starts a fresh session "
            },
            Style::default().fg(th.text_secondary),
        )));
    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_sessions_modal(f: &mut Frame, app: &App, th: Theme) {
    use crate::app::short_id;
    use crate::store::fmt_short;
    let area = centered(f.area(), 78, 60);
    f.render_widget(Clear, area);
    // Columns mirror `grok sessions list`: id + created + updated + summary.
    // Rows arrive grouped by provider (active tab's first); each group
    // gets a header in the skill accent.
    let cur = app.active().backend.label();
    let mut lines: Vec<Line> = vec![Line::from(Span::styled(
        format!("  {}", crate::app::sanitize_text(&app.workspace)),
        Style::default().fg(th.text_secondary),
    ))];
    for (i, s) in app.sess_list.iter().enumerate() {
        if i == 0 || app.sess_list[i - 1].backend != s.backend {
            let tag = if s.backend == cur { " (this tab)" } else { "" };
            lines.push(Line::from(Span::styled(
                format!("── {}{tag} ──", s.backend),
                Style::default().fg(th.skill).add_modifier(Modifier::BOLD),
            )));
        }
        let selected = i == app.sess_sel;
        let cursor = if selected { "> " } else { "  " };
        // S3-F1: preview/title come from stored transcripts.
        let summary = if s.title.is_empty() {
            crate::app::sanitize_text(&s.preview)
        } else {
            crate::app::sanitize_text(&s.title)
        };
        let row = if selected {
            Style::default()
                .fg(th.tab_active)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(th.text_primary)
        };
        lines.push(Line::from(Span::styled(
            format!(
                "{cursor}{}: {} {}→{} {}",
                i + 1,
                short_id(&s.id),
                fmt_short(s.created_at),
                fmt_short(s.updated_at),
                summary,
            ),
            row,
        )));
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(th.border_active))
        .title(Line::from(Span::styled(
            " SESSIONS — Enter views + continues · d deletes · Esc cancels ",
            Style::default().fg(th.text_secondary),
        )));
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
    use crate::theme::ThemeKind;
    use ratatui::{backend::TestBackend, Terminal};

    fn screen_rows(app: &mut App, w: u16, h: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("terminal");
        terminal.draw(|f| render(f, app)).expect("draw");
        let buf = terminal.backend().buffer().clone();
        (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect()
    }

    #[test]
    fn turn_status_row_and_busy_marker_only_while_busy() {
        let mut app = App::new();
        app.sessions[0].lines.push("> hi".to_string());
        let rows = screen_rows(&mut app, 80, 24);
        assert!(rows.iter().any(|r| r.contains("[idle]")));
        assert!(rows.iter().all(|r| !r.contains("Thinking")));
        app.sessions[0].busy = true;
        app.sessions[0].sync_activity();
        let rows = screen_rows(&mut app, 80, 24);
        // Turn-status row sits right above the status bar.
        let status = rows.iter().position(|r| r.contains("[busy]")).expect("busy");
        assert!(rows.iter().all(|r| !r.contains("[idle]")));
        assert!(rows[status - 1].contains("Thinking 0s"), "{:?}", rows[status - 1]);
        assert!(rows[status - 1].trim_end().ends_with("0s"));
        // Live thinking block in the transcript.
        assert!(rows.iter().any(|r| r.contains("Thinking…")));
        assert!(!rows[status].contains("Thinking"));
    }

    #[test]
    fn turn_status_shows_awaiting_approval_on_pending_diff() {
        let mut app = App::new();
        app.sessions[0].lines.push("> hi".to_string());
        app.sessions[0].busy = true;
        app.sessions[0].sync_activity();
        app.sessions[0].stage_diff(crate::app::PendingDiff {
            file: "a.rs".into(),
            body: "x".into(),
        });
        let rows = screen_rows(&mut app, 80, 24);
        assert!(rows.iter().any(|r| r.contains("◆ awaiting approval")));
    }

    #[test]
    fn pinned_view_stays_pinned_when_live_block_and_turn_row_appear() {
        let mut app = App::new();
        for i in 0..60 {
            app.sessions[0].push_line(format!("line {i}"));
        }
        let _ = screen_rows(&mut app, 80, 24);
        app.stick_to_bottom();
        let _ = screen_rows(&mut app, 80, 24);
        app.sessions[0].busy = true;
        app.sessions[0].sync_activity();
        let rows = screen_rows(&mut app, 80, 24);
        // Last committed line and the live header are both visible.
        assert!(rows.iter().any(|r| r.contains("line 59")));
        assert!(rows.iter().any(|r| r.contains("Thinking…")));
        // Scrolled up: the view is not yanked when the block grows.
        app.scroll_lines(-10);
        let before = app.sessions[0].scroll;
        app.sessions[0].push_thought("a\nb\nc");
        let _ = screen_rows(&mut app, 80, 24);
        assert_eq!(app.sessions[0].scroll, before);
    }

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

    /// An empty tab shows the welcome dashboard (big logo when roomy,
    /// wordmark when cramped); the first line replaces it.
    #[test]
    fn empty_tab_shows_welcome_until_first_line() {
        let dump = |app: &mut App, w, h| {
            let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("terminal");
            terminal.draw(|f| render(f, app)).expect("draw");
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|c| c.symbol())
                .collect::<String>()
        };
        let mut app = App::new();
        app.workspace = "/tmp/w".to_string();
        let big = dump(&mut app, 100, 30);
        assert!(big.contains("██████╗"), "logo missing");
        assert!(big.contains("/tmp/w") && big.contains("ready (offline)"));
        let small = dump(&mut app, 40, 12);
        assert!(small.contains("P O L Y F O R G E"));
        app.sessions[0].push_line("> hello".to_string());
        let used = dump(&mut app, 100, 30);
        assert!(!used.contains("██████╗") && used.contains("hello"));
    }

    /// Startup hides the tab bar; content or a second tab brings it back.
    #[test]
    fn tab_bar_hidden_on_startup_welcome() {
        let top_row = |app: &mut App| {
            let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("terminal");
            terminal.draw(|f| render(f, app)).expect("draw");
            let buf = terminal.backend().buffer();
            (0..80).map(|x| buf[(x, 0)].symbol()).collect::<String>()
        };
        let mut app = App::new();
        assert!(!top_row(&mut app).contains("s1:mock"), "no tab bar at boot");
        app.sessions[0].push_line("> hello".to_string());
        assert!(top_row(&mut app).contains("s1:mock"), "content shows tabs");
        let mut app = App::new();
        app.open_tab();
        assert!(top_row(&mut app).contains("s1:mock"), "two tabs show tabs");
    }

    /// Transcript role colors follow grok-build semantics per theme.
    #[test]
    fn transcript_roles_map_to_theme() {
        let th = Theme::get(ThemeKind::GrokNight);
        let fg = |line: &str| transcript_style(line, th).fg;
        assert_eq!(fg("> hello"), Some(th.user));
        assert_eq!(fg("mock: approved edit"), Some(th.ok));
        assert_eq!(fg("codex: done ✓"), Some(th.ok));
        assert_eq!(fg("mock: rejected edit"), Some(th.err));
        assert_eq!(fg("grok: prompt failed: boom"), Some(th.err));
        assert_eq!(fg("mock: deferred"), Some(th.warn));
        assert_eq!(fg("grok: ∴ hmm"), Some(th.assistant));
        assert_eq!(fg("grok: plan (3 steps)"), Some(th.plan));
        assert_eq!(fg("grok: ⚙ read [running]"), Some(th.tool));
        assert_eq!(fg("muse: working…"), Some(th.muted));
        assert_eq!(fg("plain note"), Some(th.text_primary));
        // Outcomes win over the generic backend dimming.
        assert_eq!(fg("codex: turn failed (see flash)"), Some(th.err));
    }

    /// Every theme renders the full chrome without panicking (tiny
    /// terminal included): switching palettes can never blank the UI.
    #[test]
    fn every_theme_renders_full_chrome() {
        for kind in ThemeKind::ALL {
            let mut app = App::new();
            app.theme = kind;
            app.sessions[0].lines.push("> hello".to_string());
            app.sessions[0]
                .lines
                .push("muse: approved edit".to_string());
            app.flash = "theme note".to_string();
            for (w, h) in [(80, 24), (40, 10)] {
                let backend = TestBackend::new(w, h);
                let mut terminal = Terminal::new(backend).expect("terminal");
                terminal.draw(|f| render(f, &mut app)).expect("draw");
                let text: String = terminal
                    .backend()
                    .buffer()
                    .content()
                    .iter()
                    .map(|c| c.symbol())
                    .collect();
                assert!(text.contains("hello"), "{kind:?} dropped content");
            }
        }
    }
}
