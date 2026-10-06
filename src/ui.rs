//! Rendering: header row, borderless transcript, turn-status row, rounded
//! input box, shortcuts row, modal popups. All colors flow from the central
//! [`crate::theme::Theme`] (grok-build-inspired GrokNight default). The
//! only background fills are the user-prompt block and code rows; the
//! terminal's own background is never painted. Truncates (never wraps)
//! long chrome so layout stays exact; transcript lines wrap at the text
//! width and share one height/scroll model.
//!
//! Transcript text goes through ONE seam, [`display_line`]: it maps a raw
//! stored line to the text that is wrapped, selected and copied plus its
//! styled spans. The height cache ([`line_rows`]), the renderer, the
//! selection highlight and mouse/copy mapping all consume it, so they
//! cannot drift apart.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Padding, Paragraph, Wrap},
    Frame,
};

use crate::app::{App, Mode};
use crate::markdown::{self, MdKind};
use crate::theme::Theme;

/// Transcript margins: the rail sits in column 2, text starts at column 5,
/// and both the transcript and the chrome keep a 2-column right margin.
const RAIL_COL: u16 = 2;
const TEXT_COL: u16 = 5;
const RIGHT_MARGIN: u16 = 2;
const CHROME_MARGIN: u16 = 2;

pub fn render(f: &mut Frame, app: &mut App) {
    let th = Theme::get(app.theme);
    // Startup (one tab still on the welcome dashboard): no header. It
    // appears once the tab has content or a second tab opens.
    let header_rows = if app.sessions.len() == 1 && shows_welcome(app) {
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
            Constraint::Length(header_rows),
            Constraint::Min(3),
            Constraint::Length(turn_rows),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(f.area());

    render_header(f, app, th, chunks[0]);
    render_transcript(f, app, th, chunks[1]);
    render_turn_status(f, app, th, chunks[2]);
    let input_area = inset(chunks[3], CHROME_MARGIN);
    render_input(f, app, th, input_area);
    render_cmd_popup(f, app, th, input_area);
    render_shortcuts(f, app, th, chunks[4]);

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

/// Shrink `area` by `m` columns on each side (never below width 1).
fn inset(area: Rect, m: u16) -> Rect {
    let w = area.width.saturating_sub(2 * m).max(1).min(area.width);
    let x = area.x + (area.width - w).min(m);
    Rect::new(x, area.y, w, area.height)
}

/// `$HOME` → `~`, then every component but the last cut to its first char
/// (`/p/t/c/-/e/scratchpad/grokui`).
fn short_cwd(path: &str) -> String {
    let full = tilde_home(path);
    let parts: Vec<&str> = full.split('/').collect();
    let last = parts.len().saturating_sub(1);
    parts
        .iter()
        .enumerate()
        .map(|(i, p)| {
            if i == last {
                (*p).to_string()
            } else {
                p.chars().take(1).collect()
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Header row: shortened workspace on the left, compact tabs (and the
/// active tab's token count) on the right. Inactive tabs drop to `○2`
/// short form when the row is tight; nothing ever overflows.
fn render_header(f: &mut Frame, app: &App, th: Theme, area: Rect) {
    if area.height == 0 {
        return;
    }
    let area = inset(area, CHROME_MARGIN);
    let width = area.width as usize;
    let n = |s: &str| s.chars().count();
    let sep = Span::styled(" │ ", Style::default().fg(th.sep));
    let tokens = app
        .active()
        .tokens
        .filter(|&t| t > 0)
        .map(|t| crate::activity::format_tokens_short(t).replace(".0k", "k"));
    let tab = |i: usize, short: bool| -> Vec<Span<'static>> {
        let s = &app.sessions[i];
        let (dot, dot_color) = if s.busy {
            ("●", th.tab_busy)
        } else {
            ("○", th.muted)
        };
        let label = if i == app.active || !short {
            format!("{}:{}", s.name, s.backend.label())
        } else {
            format!("{}", i + 1)
        };
        let style = if i == app.active {
            Style::default()
                .fg(th.tab_active)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(th.text_secondary)
        };
        vec![
            Span::styled(dot, Style::default().fg(dot_color)),
            Span::styled(format!(" {label}"), style),
        ]
    };
    let build = |short: bool, with_tokens: bool| -> Vec<Span<'static>> {
        let mut spans = Vec::new();
        for i in 0..app.sessions.len() {
            if i > 0 {
                spans.push(sep.clone());
            }
            spans.extend(tab(i, short));
        }
        if let (true, Some(t)) = (with_tokens, tokens.as_ref()) {
            spans.push(sep.clone());
            spans.push(Span::styled(t.to_lowercase(), Style::default().fg(th.muted)));
        }
        spans
    };
    let w_of = |spans: &[Span]| spans.iter().map(|s| n(&s.content)).sum::<usize>();
    let right = [(false, true), (true, true), (true, false)]
        .into_iter()
        .map(|(short, tok)| build(short, tok))
        .find(|s| w_of(s) <= width)
        .unwrap_or_else(|| {
            // Last resort: the active tab alone, clipped.
            let mut s = tab(app.active, false);
            let mut budget = width;
            s.retain_mut(|sp| {
                if budget == 0 {
                    return false;
                }
                let c = n(&sp.content);
                if c > budget {
                    sp.content = sp.content.chars().take(budget).collect::<String>().into();
                }
                budget -= n(&sp.content);
                true
            });
            s
        });
    let right_w = w_of(&right);
    let left_w = width.saturating_sub(right_w + 2);
    let cwd = crate::app::sanitize_text(&short_cwd(&app.workspace));
    let cwd: String = if n(&cwd) > left_w {
        cwd.chars().take(left_w).collect()
    } else {
        cwd
    };
    f.render_widget(
        Paragraph::new(Span::styled(cwd, Style::default().fg(th.text_secondary))),
        area,
    );
    let rw = right_w as u16;
    f.render_widget(
        Paragraph::new(Line::from(right)),
        Rect::new(area.x + area.width - rw.min(area.width), area.y, rw.min(area.width), 1),
    );
}

/// The active tab is empty, so it shows the welcome dashboard.
fn shows_welcome(app: &App) -> bool {
    app.active().lines.is_empty() && app.active().stream_draft_lines().is_empty()
}

/// One transcript line as it is shown. `text` is what wraps, selects and
/// copies; `spans` style exactly those chars. Every row is preceded by
/// `indent` columns (the first carries `lead`), and `fill` paints a
/// background across the whole row band.
pub struct DisplayLine {
    pub text: String,
    pub spans: Vec<Span<'static>>,
    pub indent: usize,
    pub lead: Option<Span<'static>>,
    pub fill: Option<Color>,
    /// The fill starts at the text column (code rows) instead of the rail
    /// column (prompt block).
    pub fill_text: bool,
    /// Horizontal rule: `text` is one `─` the renderer stretches.
    pub rule: bool,
}

/// Markdown kind of a line about to be stored after one of kind `prev`:
/// answer lines (plain text role) and anything inside an open fence get
/// markdown, prefixed system lines never do.
pub fn md_kind(line: &str, prev: Option<MdKind>) -> MdKind {
    let in_fence = prev.is_some_and(MdKind::in_fence);
    let th = Theme::groknight();
    let system = line.starts_with('>')
        || line.starts_with("◆ ")
        || line.starts_with("Worked for ")
        || line.starts_with("■ ")
        || line.starts_with("--- ")
        || ["mock: ", "muse: ", "codex: ", "agy: ", "grok: ", "claude: "]
            .iter()
            .any(|p| line.starts_with(p));
    // Keyword-colored lines (approved/failed/...) are not answer prose,
    // but inside a fence a code line may say "failed".
    let plain = transcript_style(line, th) == Style::default().fg(th.text_primary);
    if system || (!in_fence && !plain) {
        MdKind::Plain
    } else {
        markdown::classify(line, in_fence)
    }
}

/// THE raw-line → display mapping (sanitizes on the way, S3-F1). Height
/// cache, render, selection highlight and copy all go through here.
pub fn display_line(raw: &str, kind: MdKind, th: Theme) -> DisplayLine {
    let line = crate::app::sanitize_text(raw);
    if let Some(body) = line.strip_prefix('>') {
        // User prompt: `❯ text` on the user block, continuation rows
        // indented to align after the marker.
        let body = body.strip_prefix(' ').unwrap_or(body).to_string();
        let bg = Style::default().bg(th.user_bg);
        return DisplayLine {
            spans: vec![Span::styled(body.clone(), bg.fg(th.text_primary))],
            text: body,
            indent: 2,
            lead: Some(Span::styled("❯ ", bg.fg(th.user))),
            fill: Some(th.user_bg),
            fill_text: false,
            rule: false,
        };
    }
    if kind != MdKind::Plain {
        let (text, spans) = markdown::md_display(&line, kind, th);
        let code = kind.is_code_row();
        return DisplayLine {
            text,
            spans,
            indent: 0,
            lead: None,
            fill: code.then_some(th.code_bg),
            fill_text: code,
            rule: kind == MdKind::Text && markdown::is_rule(&line),
        };
    }
    let style = transcript_style(&line, th);
    let spans = match thought_split(&line) {
        Some((mark, word, rest)) => vec![
            Span::styled(mark, style),
            Span::styled(word, style.add_modifier(Modifier::BOLD)),
            Span::styled(rest, style),
        ],
        None => vec![Span::styled(line.clone(), style)],
    };
    DisplayLine {
        text: line,
        spans,
        indent: 0,
        lead: None,
        fill: None,
        fill_text: false,
        rule: false,
    }
}

/// `◆ Thought for 6s` / `◆ Thinking…` → ("◆ ", "Thought", " for 6s").
fn thought_split(line: &str) -> Option<(String, String, String)> {
    let rest = line.strip_prefix("◆ ")?;
    let word = ["Thought", "Thinking"]
        .into_iter()
        .find(|w| rest.starts_with(w))?;
    Some((
        "◆ ".to_string(),
        word.to_string(),
        rest[word.len()..].to_string(),
    ))
}

/// Wrapped-row count of `raw` at the transcript text `width`.
pub fn line_rows(raw: &str, kind: MdKind, width: usize) -> usize {
    let d = display_line(raw, kind, Theme::groknight());
    crate::app::wrap_rows(&d.text, width.saturating_sub(d.indent).max(1))
}

/// Chars [a, b) of a span run, styles kept.
fn slice_spans(spans: &[Span<'static>], a: usize, b: usize) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    for sp in spans {
        let len = sp.content.chars().count();
        let (s, e) = (a.max(pos), b.min(pos + len));
        if s < e {
            let t: String = sp.content.chars().skip(s - pos).take(e - s).collect();
            out.push(Span::styled(t, sp.style));
        }
        pos += len;
        if pos >= b {
            break;
        }
    }
    out
}

/// Reverse-video chars [s, e) of a span run (selection highlight).
fn reverse_range(spans: Vec<Span<'static>>, s: usize, e: usize) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    for sp in spans {
        let chars: Vec<char> = sp.content.chars().collect();
        let (lo, hi) = (pos, pos + chars.len());
        let p1 = s.clamp(lo, hi);
        let p2 = e.clamp(lo, hi).max(p1);
        for (a, b, rev) in [(lo, p1, false), (p1, p2, true), (p2, hi, false)] {
            if a < b {
                let style = if rev {
                    sp.style.add_modifier(Modifier::REVERSED)
                } else {
                    sp.style
                };
                out.push(Span::styled(
                    chars[a - lo..b - lo].iter().collect::<String>(),
                    style,
                ));
            }
        }
        pos = hi;
    }
    out
}

fn render_transcript(f: &mut Frame, app: &mut App, th: Theme, area: Rect) {
    // Text column: rail at col 2, text from col 5, 2-col right margin. The
    // wrap width, height cache, scroll math and mouse mapping all use it.
    let text_x = area.x + TEXT_COL.min(area.width.saturating_sub(1));
    let text_w = (area.x + area.width).saturating_sub(text_x + RIGHT_MARGIN).max(1);
    let width = text_w as usize;
    app.viewport_height = area.height.max(1) as usize;
    app.viewport_width = width;
    // Origin for mouse cell → text mapping (recorded every frame).
    app.text_area = if area.height > 0 && text_x < area.x + area.width {
        Some((text_x, area.y, text_w.min(area.x + area.width - text_x), area.height))
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
    // Committed lines + live stream drafts (thought / open answer).
    let drafts = s.stream_draft_lines();
    let draft_kinds = s.stream_draft_kinds();
    let draft_rows: Vec<usize> = drafts
        .iter()
        .zip(&draft_kinds)
        .map(|(l, k)| line_rows(l, *k, width))
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
    // (row line, background band) per visible row.
    let mut items: Vec<(Line<'static>, Option<Color>, bool)> = Vec::new();
    // Live-turn rows get the activity rail (only while animating).
    let rail = s
        .phase
        .filter(|_| s.pending_diff.is_none())
        .zip(s.turn_since)
        .map(|(p, t)| (p, t.elapsed(), s.scroll));
    let mut live_rows: Vec<usize> = Vec::new();
    while items.len() < vh && li < n_view {
        let (raw, kind) = if li < n_committed {
            (s.lines[li].as_str(), s.kind_at(li))
        } else {
            (drafts[li - n_committed].as_str(), draft_kinds[li - n_committed])
        };
        let mut d = display_line(raw, kind, th);
        // Live thought text under the header reads muted.
        if matches!(li.checked_sub(n_committed), Some(k) if k >= 1 && k < thought_n) {
            let m = Style::default().fg(th.muted);
            d.spans = vec![Span::styled(d.text.clone(), m)];
        }
        let line_len = d.text.chars().count();
        // Drafts are not mouse-selectable (no stable line index in `lines`).
        let span = if li < n_committed {
            sel.and_then(|sel| sel.span_on_line(li, line_len))
        } else {
            None
        };
        let chunks = crate::app::Session::wrap_line(&d.text, width.saturating_sub(d.indent).max(1));
        let mut coff = chunks
            .iter()
            .take(sub)
            .map(|c| c.chars().count())
            .sum::<usize>();
        let live = li >= n_committed
            || (li >= s.turn_first_line && (d.fill.is_none() || d.fill_text));
        let pad_style = d.fill.map(|bg| Style::default().bg(bg)).unwrap_or_default();
        for (ci, chunk) in chunks.iter().enumerate().skip(sub) {
            if live {
                live_rows.push(items.len());
            }
            let clen = chunk.chars().count();
            let mut spans = if d.rule {
                let style = d.spans.first().map(|s| s.style).unwrap_or_default();
                vec![Span::styled("─".repeat(width), style)]
            } else {
                slice_spans(&d.spans, coff, coff + clen)
            };
            if let Some((ss, se)) = span {
                let (a, b) = (ss.saturating_sub(coff).min(clen), se.saturating_sub(coff).min(clen));
                if d.rule {
                    spans = reverse_range(spans, 0, width);
                } else if a < b {
                    spans = reverse_range(spans, a, b);
                }
            }
            if d.indent > 0 {
                spans.insert(
                    0,
                    match (&d.lead, ci) {
                        (Some(lead), 0) => lead.clone(),
                        _ => Span::styled(" ".repeat(d.indent), pad_style),
                    },
                );
            }
            items.push((Line::from(spans), d.fill, d.fill_text));
            if items.len() >= vh {
                break;
            }
            coff += clen;
        }
        li += 1;
        sub = 0;
    }
    let buf_w = area.width.saturating_sub(RAIL_COL + RIGHT_MARGIN);
    for (i, (line, fill, fill_text)) in items.into_iter().enumerate() {
        let y = area.y + i as u16;
        if let Some(bg) = fill {
            let (x, w) = if fill_text {
                (text_x, text_w.min(area.x + area.width - text_x))
            } else {
                (area.x + RAIL_COL, buf_w)
            };
            f.buffer_mut().set_style(Rect::new(x, y, w, 1), Style::default().bg(bg));
        }
        f.render_widget(Paragraph::new(line), Rect::new(text_x, y, text_w.min(area.x + area.width - text_x), 1));
    }
    if let Some((phase, elapsed, scroll)) = rail {
        use crate::activity::{blend, wave_brightness, Phase};
        let accent = if phase == Phase::Thinking {
            th.assistant
        } else {
            th.running
        };
        for i in live_rows {
            let y = area.y + i as u16;
            if area.width <= RAIL_COL || y >= area.y + area.height {
                continue;
            }
            let color = blend(th.border, accent, wave_brightness(elapsed, scroll + i));
            f.buffer_mut()[(area.x + RAIL_COL, y)]
                .set_symbol("┃")
                .set_fg(color);
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
    } else if s.remote_id.is_some() || s.session_deferred {
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
/// lavender, approval outcomes carry traffic colors, thoughts and the
/// `Worked for` line stay muted, tool/plan lines get their own hues, and
/// the generic backend chatter stays dim — grok-build role semantics.
fn transcript_style(line: &str, th: Theme) -> Style {
    if line.starts_with('>') {
        Style::default().fg(th.user)
    } else if line.starts_with("◆ Thought")
        || line.starts_with("◆ Thinking")
        || line.starts_with("Worked for ")
    {
        Style::default().fg(th.muted)
    } else if line.contains("approved") || line.contains('✓') || line.contains("already applied")
    {
        Style::default().fg(th.ok)
    } else if line.contains("rejected") || line.contains('✗') || line.contains("failed") {
        Style::default().fg(th.err)
    } else if line.contains("deferred") {
        Style::default().fg(th.warn)
    } else if line.starts_with("grok: ∴") {
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

/// `(key, description)` hints for the current mode (the lists the input
/// box title used to carry). Empty description = a bare label.
fn mode_hints(app: &App) -> Vec<(&'static str, &'static str)> {
    let busy = app.active().busy;
    match app.mode {
        Mode::Normal => {
            let mut v = if app.vim {
                vec![("j/k", "move"), ("Space/i/a", "type"), ("/", "search")]
            } else {
                vec![("↑/↓", "move"), ("Space/Enter", "type"), ("/", "search")]
            };
            if busy {
                v.push(("Esc", "stop"));
            }
            v.extend([("P", "provider"), ("R", "fresh"), ("q", "quit")]);
            v
        }
        Mode::Insert => vec![
            ("Enter", "send"),
            ("Esc", "normal"),
            ("/", "commands"),
            ("↑/↓", "pick"),
            ("Tab", "complete"),
        ],
        Mode::Search => vec![("Enter", "find"), ("Esc", "cancel")],
        Mode::Picker if app.onboarding => vec![
            ("j/k", "move"),
            ("Enter", "start"),
            ("1-5", "quick"),
            ("Esc", "quit"),
        ],
        Mode::Picker => vec![
            ("j/k", "move"),
            ("Enter", "switch"),
            ("1-5", "quick"),
            ("Esc", "cancel"),
        ],
        Mode::Sessions => vec![
            ("j/k", "move"),
            ("Enter", "view + continue"),
            ("d", "delete"),
            ("1-9", "quick"),
            ("Esc", "cancel"),
        ],
    }
}

/// Shortcuts row under the input box: mode badge and Grok-style key hints
/// on the left; busy state, scroll position, mouse and the flash message
/// on the right. Tight rows drop the hints first, then the right-side
/// extras; the flash is kept last.
fn render_shortcuts(f: &mut Frame, app: &App, th: Theme, area: Rect) {
    let s = app.active();
    let width = area.width as usize;
    let n = |s: &str| s.chars().count();
    let badge = format!(" {} ", app.mode.label());
    let badge_span = Span::styled(
        badge.clone(),
        Style::default()
            .fg(Color::Black)
            .bg(th.mode_color(app.mode))
            .add_modifier(Modifier::BOLD),
    );
    // Right side, most expendable last: mouse, scroll, busy, then flash.
    let total = s.total_rows;
    let flash = crate::app::sanitize_text(&app.flash); // S3-F1: agent text
    let mut right: Vec<(String, Style)> = Vec::new();
    if !flash.is_empty() {
        right.push((flash, Style::default().fg(th.warn)));
    }
    right.push(if s.busy {
        (
            "[busy]".to_string(),
            Style::default().fg(th.running).add_modifier(Modifier::BOLD),
        )
    } else {
        ("[idle]".to_string(), Style::default().fg(th.muted))
    });
    right.push((
        format!("{}/{}", s.scroll.min(total.saturating_sub(1)) + 1, total.max(1)),
        Style::default().fg(th.muted),
    ));
    right.push((
        format!("mouse:{}", if app.mouse { "on" } else { "off" }),
        Style::default().fg(th.muted),
    ));
    let right_w = |r: &[(String, Style)]| {
        r.iter().map(|(t, _)| n(t) + 2).sum::<usize>()
    };
    let has_flash = !app.flash.is_empty();
    while right.len() > 1 && n(&badge) + right_w(&right) > width {
        right.pop();
    }
    // A flash wider than the row is clipped instead of overflowing.
    if let Some((t, _)) = right.first_mut().filter(|_| has_flash) {
        let room = width.saturating_sub(n(&badge) + 2);
        if n(t) > room {
            *t = t.chars().take(room).collect();
        }
    }
    let rw = right_w(&right).min(width);
    // Left hints get what is left; whole hints only, dropped from the end.
    let sep = "  │  ";
    let mut budget = width.saturating_sub(n(&badge) + 1 + rw + 2);
    let mut left = vec![badge_span, Span::raw(" ")];
    for (i, (key, desc)) in mode_hints(app).into_iter().enumerate() {
        let need = n(key) + 1 + n(desc) + if i > 0 { n(sep) } else { 0 };
        if need > budget {
            break;
        }
        budget -= need;
        if i > 0 {
            left.push(Span::styled(sep, Style::default().fg(th.sep)));
        }
        left.push(Span::styled(
            key,
            Style::default()
                .fg(th.text_secondary)
                .add_modifier(Modifier::BOLD),
        ));
        left.push(Span::styled(format!(":{desc}"), Style::default().fg(th.muted)));
    }
    f.render_widget(Paragraph::new(Line::from(left)), area);
    let mut spans = Vec::new();
    for (i, (t, st)) in right.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(t, st));
    }
    spans.push(Span::raw("  "));
    f.render_widget(
        Paragraph::new(Line::from(spans)),
        Rect::new(area.x + area.width - rw as u16, area.y, rw as u16, 1),
    );
}

/// Turn-status row above the input (grok-build style): spinner, phase and
/// phase timer on the left; turn timer, context tokens and a `[stop]`
/// button on the right. A static marker while a diff awaits approval.
/// Zero-height (skipped) when the tab is idle. Records the button cells in
/// `app.stop_hit` for mouse clicks (None whenever it is not drawn).
fn render_turn_status(f: &mut Frame, app: &mut App, th: Theme, area: Rect) {
    use crate::activity::{format_secs, format_tokens_short, spinner_frame, Phase};
    use std::time::Duration;
    app.stop_hit = None;
    let s = app.active();
    if area.height == 0 || !s.busy {
        return;
    }
    if s.pending_diff.is_some() {
        f.render_widget(
            Paragraph::new(Span::styled(
                "    ◆ awaiting approval",
                Style::default().fg(th.warn),
            )),
            area,
        );
        return;
    }
    let now = std::time::Instant::now();
    let since = |t: Option<std::time::Instant>| t.map_or(Duration::ZERO, |t| now - t);
    // While stopping the left timer counts the stop, not the phase.
    let (label, phase_t) = match s.stopping {
        Some(t) => ("Stopping…", since(Some(t))),
        None => (
            s.phase.unwrap_or(Phase::Thinking).label(),
            since(s.phase_since),
        ),
    };
    let tokens = s
        .tokens
        .filter(|&n| n > 0)
        .map(|n| format!("⇣{}", format_tokens_short(n)));
    let spinner = format!("{TURN_INDENT}{}", spinner_frame(since(s.turn_since)));
    let row = plan_turn_row(
        area.width as usize,
        label,
        &format_secs(phase_t),
        &format_secs(since(s.turn_since)),
        tokens.as_deref(),
    );
    let mut left = vec![Span::styled(spinner, Style::default().fg(th.running))];
    left.push(Span::styled(
        format!(" {}", row.label),
        Style::default().fg(th.text_secondary),
    ));
    if let Some(t) = row.timer {
        left.push(Span::styled(format!(" {t}"), Style::default().fg(th.muted)));
    }
    f.render_widget(Paragraph::new(Line::from(left)), area);
    let Some(pre) = row.right else {
        return;
    };
    // pre (muted) + [stop] + the right margin, right-aligned.
    let pre_w = if pre.is_empty() { 0 } else { pre.chars().count() as u16 + 1 };
    let w = pre_w + STOP_LABEL.len() as u16 + RIGHT_MARGIN;
    let x = area.x + area.width - w;
    let mut spans = Vec::new();
    if !pre.is_empty() {
        spans.push(Span::styled(format!("{pre} "), Style::default().fg(th.muted)));
    }
    spans.push(Span::styled(
        STOP_LABEL,
        Style::default().fg(th.text_secondary),
    ));
    f.render_widget(
        Paragraph::new(Line::from(spans)),
        Rect::new(x, area.y, w, 1),
    );
    app.stop_hit = Some(Rect::new(
        x + pre_w,
        area.y,
        STOP_LABEL.len() as u16,
        1,
    ));
}

const STOP_LABEL: &str = "[stop]";
/// Turn row left edge: the spinner sits at column 4, like Grok.
const TURN_INDENT: &str = "    ";

/// What fits on the turn row at a given width.
struct TurnRow {
    /// Phase label, truncated when the row is tight.
    label: String,
    /// Phase timer; dropped before the label gives way further.
    timer: Option<String>,
    /// Right side before `[stop]` ("1m 05s ⇣12.3k", possibly empty).
    /// None = not even `[stop]` fits, so the right side is omitted.
    right: Option<String>,
}

/// Column plan: the right side (turn timer, tokens, `[stop]`) is placed
/// first and sheds tokens, then the turn timer, when narrow; the left gets
/// the rest and truncates its label first, then drops the phase timer.
/// The two sides never overlap.
fn plan_turn_row(
    width: usize,
    label: &str,
    phase_t: &str,
    turn: &str,
    tokens: Option<&str>,
) -> TurnRow {
    let n = |s: &str| s.chars().count();
    // Indent + spinner cell + a gap column between the sides.
    const LEFT_FIXED: usize = TURN_INDENT.len() + 2;
    const RIGHT_PAD: usize = RIGHT_MARGIN as usize;
    let full = match tokens {
        Some(t) => format!("{turn} {t}"),
        None => turn.to_string(),
    };
    let right = [full, turn.to_string(), String::new()]
        .into_iter()
        // pre + space + [stop] + margin (no space when pre is empty)
        .find(|pre| {
            let w = if pre.is_empty() { 0 } else { n(pre) + 1 } + STOP_LABEL.len() + RIGHT_PAD;
            w + LEFT_FIXED <= width
        });
    let right_w = right.as_ref().map_or(0, |pre| {
        (if pre.is_empty() { 0 } else { n(pre) + 1 }) + STOP_LABEL.len() + RIGHT_PAD
    });
    // Columns left for " label" + " timer" after the spinner.
    let avail = width.saturating_sub(right_w + LEFT_FIXED);
    let clip = |s: &str, max: usize| -> String {
        if n(s) <= max {
            s.to_string()
        } else if max == 0 {
            String::new()
        } else {
            let keep = max.saturating_sub(1);
            format!("{}…", s.chars().take(keep).collect::<String>())
        }
    };
    let with_timer = 1 + n(label) + 1 + n(phase_t);
    let (label, timer) = if with_timer <= avail {
        (label.to_string(), Some(phase_t.to_string()))
    } else if avail >= 1 + 3 + 1 + n(phase_t) {
        // Label gives way first, keeping at least 3 columns of it.
        (clip(label, avail - 1 - 1 - n(phase_t)), Some(phase_t.to_string()))
    } else {
        (clip(label, avail.saturating_sub(1)), None)
    };
    TurnRow {
        label,
        timer,
        right,
    }
}

fn render_input(f: &mut Frame, app: &mut App, th: Theme, area: Rect) {
    let content = match app.mode {
        Mode::Normal | Mode::Insert => app.active().input.clone(),
        Mode::Search => format!("/{}", app.search_input),
        Mode::Picker | Mode::Sessions => String::new(),
    };
    // Focused (text-entry) modes get the brighter active chrome,
    // grok-build prompt-widget style: dim border at rest.
    let focused = matches!(app.mode, Mode::Insert | Mode::Search);
    // Bottom-right title, Grok style: `backend · tab ─`.
    let s = app.active();
    let title = Line::from(vec![
        Span::styled(s.backend.label(), Style::default().fg(th.subtle)),
        Span::styled(" · ", Style::default().fg(th.dot)),
        Span::styled(s.name.clone(), Style::default().fg(th.muted)),
        Span::styled(" ─", Style::default().fg(if focused { th.border_active } else { th.border })),
    ]);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if focused { th.border_active } else { th.border }))
        .padding(Padding::left(1))
        .title_bottom(title.right_aligned());
    let inner = block.inner(area);
    f.render_widget(block, area);
    let mut spans = Vec::new();
    if matches!(app.mode, Mode::Normal | Mode::Insert | Mode::Search) {
        spans.push(Span::styled(INPUT_PROMPT, Style::default().fg(th.user)));
    }
    spans.push(Span::styled(content, Style::default().fg(th.text_primary)));
    f.render_widget(Paragraph::new(Line::from(spans)), inner);
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
        f.set_cursor_position((inner.x + INPUT_PROMPT.chars().count() as u16 + col as u16, inner.y));
    }
}

const INPUT_PROMPT: &str = "❯ ";

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
        // Turn-status row sits above the input box; the shortcuts row is last.
        let status = rows.iter().position(|r| r.contains("[busy]")).expect("busy");
        assert_eq!(status, rows.len() - 1);
        assert!(rows.iter().all(|r| !r.contains("[idle]")));
        let turn = rows.iter().position(|r| r.contains("[stop]")).expect("turn row");
        assert_eq!(turn, rows.len() - 5, "turn row, 3-row box, shortcuts");
        assert!(rows[turn].starts_with("    "), "{:?}", rows[turn]);
        assert_eq!(rows[turn].chars().nth(4).map(|c| c.is_alphanumeric()), Some(false));
        assert!(rows[turn].contains("Thinking… 0."), "{:?}", rows[turn]);
        // The right side is the turn timer, then the stop button + margin.
        assert!(rows[turn].ends_with("s [stop]  "), "{:?}", rows[turn]);
        // Live thinking block in the transcript.
        assert!(rows.iter().any(|r| r.contains("◆ Thinking…")));
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

    fn turn_row(app: &mut App, w: u16) -> String {
        let rows = screen_rows(app, w, 24);
        rows[rows.len() - 5].clone()
    }

    fn busy_app() -> App {
        let mut app = App::new();
        app.sessions[0].lines.push("> hi".to_string());
        app.sessions[0].busy = true;
        app.sessions[0].sync_activity();
        app
    }

    #[test]
    fn turn_row_shows_tokens_and_stop_button_at_the_right_end() {
        let mut app = busy_app();
        app.sessions[0].tokens = Some(1234);
        let row = turn_row(&mut app, 80);
        assert!(row.ends_with("⇣1.23k [stop]  "), "{row:?}");
        assert!(row.contains("Thinking"));
        let hit = app.stop_hit.expect("button recorded");
        let cells: String = row.chars().skip(hit.x as usize).take(hit.width as usize).collect();
        assert_eq!(cells, "[stop]");
        // No tokens known: no arrow, button still there.
        let mut app = busy_app();
        let row = turn_row(&mut app, 80);
        assert!(!row.contains('⇣'), "{row:?}");
        assert!(row.ends_with("s [stop]  "), "{row:?}");
        let mut app = busy_app();
        app.sessions[0].tokens = Some(0);
        assert!(!turn_row(&mut app, 80).contains('⇣'));
    }

    #[test]
    fn stopping_label_and_clock_replace_the_phase() {
        let mut app = busy_app();
        app.sessions[0].stopping = Some(std::time::Instant::now());
        let row = turn_row(&mut app, 80);
        assert!(row.contains("Stopping…"), "{row:?}");
        assert!(!row.contains("Thinking"), "{row:?}");
        assert!(row.trim_end().ends_with("[stop]"));
        assert!(row.starts_with("    "), "{row:?}");
    }

    #[test]
    fn stop_hit_only_while_the_button_is_drawn() {
        let mut app = App::new();
        app.sessions[0].lines.push("> hi".to_string());
        let _ = screen_rows(&mut app, 80, 24);
        assert!(app.stop_hit.is_none());
        app.sessions[0].busy = true;
        app.sessions[0].sync_activity();
        let _ = screen_rows(&mut app, 80, 24);
        assert!(app.stop_hit.is_some());
        app.sessions[0].stage_diff(crate::app::PendingDiff {
            file: "a.rs".into(),
            body: "x".into(),
        });
        let rows = screen_rows(&mut app, 80, 24);
        assert!(rows.iter().all(|r| !r.contains("[stop]")));
        assert!(app.stop_hit.is_none(), "no button under a pending card");
        app.sessions[0].busy = false;
        app.sessions[0].clear_diff();
        let _ = screen_rows(&mut app, 80, 24);
        assert!(app.stop_hit.is_none());
    }

    #[test]
    fn turn_row_sides_never_overlap_when_narrow() {
        let n = |s: &str| s.chars().count();
        for w in 6..=100usize {
            let r = plan_turn_row(w, "Responding", "12s", "1m 05s", Some("⇣12.3k"));
            let left = 4 + 1 + 1 + n(&r.label) + r.timer.as_ref().map_or(0, |t| 1 + n(t));
            let right = r.right.as_ref().map_or(0, |p| {
                (if p.is_empty() { 0 } else { n(p) + 1 }) + STOP_LABEL.len() + 2
            });
            assert!(left + right <= w, "w={w} left={left} right={right} {:?}", r.label);
        }
        // Shedding order: tokens go before the turn timer, timer before label.
        let r = plan_turn_row(40, "Responding", "12s", "1m 05s", Some("⇣12.3k"));
        assert_eq!(r.right.as_deref(), Some("1m 05s ⇣12.3k"));
        assert_eq!(r.timer.as_deref(), Some("12s"));
        let r = plan_turn_row(23, "Responding", "12s", "1m 05s", Some("⇣12.3k"));
        assert_eq!(r.right.as_deref(), Some("1m 05s"));
        let r = plan_turn_row(16, "Responding", "12s", "1m 05s", Some("⇣12.3k"));
        assert_eq!(r.right.as_deref(), Some(""));
        let r = plan_turn_row(8, "Responding", "12s", "1m 05s", None);
        assert_eq!(r.right, None);
    }

    fn screen_buf(app: &mut App, w: u16, h: u16) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("terminal");
        terminal.draw(|f| render(f, app)).expect("draw");
        terminal.backend().buffer().clone()
    }

    #[test]
    fn short_cwd_abbreviates_all_but_the_last_component() {
        assert_eq!(short_cwd("/private/tmp/claude/scratchpad/grokui"), "/p/t/c/s/grokui");
        assert_eq!(short_cwd("/tmp/w"), "/t/w");
        assert_eq!(short_cwd(""), "");
        if let Ok(home) = std::env::var("HOME").map(|h| h.trim_end_matches('/').to_string())
            && !home.is_empty()
        {
            assert_eq!(
                short_cwd(&format!("{home}/Code/polyforge")),
                "~/C/polyforge"
            );
        }
    }

    #[test]
    fn header_shows_short_cwd_tabs_and_tokens() {
        let mut app = App::new();
        app.workspace = "/tmp/work/proj".to_string();
        app.sessions[0].push_line("hello".to_string());
        app.open_tab();
        app.active = 0;
        app.sessions[0].tokens = Some(28_000);
        app.sessions[1].busy = true;
        let rows = screen_rows(&mut app, 100, 30);
        assert!(rows[0].starts_with("  /t/w/proj"), "{:?}", rows[0]);
        assert!(rows[0].contains("○ s1:mock │ ● s2:mock │ 28k"), "{:?}", rows[0]);
        // Tight row: inactive tabs shorten to their index, never overflow.
        let rows = screen_rows(&mut app, 30, 12);
        assert!(rows[0].contains("○ s1:mock │ ● 2"), "{:?}", rows[0]);
        assert_eq!(rows[0].chars().count(), 30);
    }

    #[test]
    fn transcript_text_starts_at_col_five_and_prompt_gets_user_bg() {
        let th = Theme::get(ThemeKind::GrokNight);
        let mut app = App::new();
        app.sessions[0].push_line("> hi there".to_string());
        app.sessions[0].push_line("answer text".to_string());
        let buf = screen_buf(&mut app, 60, 20);
        let row = |y: u16| (0..60).map(|x| buf[(x, y)].symbol()).collect::<String>();
        // Header is row 0; the prompt is row 1, the answer row 2.
        assert_eq!(&row(1)[..2], "  ");
        assert!(row(1).chars().skip(5).collect::<String>().starts_with("❯ hi there"), "{:?}", row(1));
        assert!(row(2).chars().skip(5).collect::<String>().starts_with("answer text"), "{:?}", row(2));
        assert_eq!(buf[(5, 1)].fg, th.user);
        // Fill spans col 2 to the right margin; nothing outside it.
        for x in 2..58 {
            assert_eq!(buf[(x, 1)].bg, th.user_bg, "col {x}");
        }
        assert_ne!(buf[(1, 1)].bg, th.user_bg);
        assert_ne!(buf[(58, 1)].bg, th.user_bg);
        assert_ne!(buf[(10, 2)].bg, th.user_bg, "answer rows are not filled");
    }

    #[test]
    fn wrapped_prompt_indents_continuation_and_copies_original_text() {
        let mut app = App::new();
        let prompt = "aaaa bbbb cccc dddd eeee ffff gggg hhhh";
        app.sessions[0].push_line(format!("> {prompt}"));
        app.sessions[0].push_line("plain answer that also wraps around the narrow view".to_string());
        // 20 cols: text width 13 (col 5 .. 18-... margin), prompt text 11.
        let rows = screen_rows(&mut app, 20, 20);
        assert!(rows[1].contains("❯ aaaa bbbb"), "{:?}", rows[1]);
        assert!(rows[2].starts_with("       "), "continuation aligns after ❯: {:?}", rows[2]);
        assert_eq!(app.viewport_width, 20 - 5 - 2);
        // Height cache matches what was drawn.
        let s = &app.sessions[0];
        assert_eq!(s.row_cache[0], crate::app::wrap_rows(prompt, app.viewport_width - 2));
        // Drag over the whole transcript copies the original text.
        let (ax, ay, _, _) = app.text_area.expect("text area");
        assert!(app.sel_begin(ax, ay));
        app.sel_extend(ax + 12, ay + app.sessions[0].total_rows as u16 - 1);
        let copied = app.take_selected_text().expect("selection");
        let mut lines = copied.lines();
        assert_eq!(lines.next(), Some(prompt));
        assert!(lines.next().expect("answer").starts_with("plain answer"));
    }

    #[test]
    fn input_box_has_rounded_border_prompt_and_bottom_right_title() {
        let mut app = App::new();
        app.sessions[0].push_line("hi".to_string());
        app.mode = Mode::Insert;
        app.sessions[0].input = "abc".to_string();
        app.sessions[0].cursor = 3;
        let rows = screen_rows(&mut app, 100, 30);
        let top = rows.iter().position(|r| r.contains('╭') && r.contains('╮')).expect("box top");
        assert!(rows[top].starts_with("  ╭") && rows[top].trim_end().ends_with('╮'));
        assert!(rows[top + 1].starts_with("  │ ❯ abc"), "{:?}", rows[top + 1]);
        assert!(
            rows[top + 2].trim_end().ends_with("mock · s1 ─╯"),
            "{:?}",
            rows[top + 2]
        );
        assert!(rows.iter().all(|r| !r.contains("input —")), "keys left the title");
    }

    #[test]
    fn shortcuts_row_lists_mode_keys_and_esc_stop_only_while_busy() {
        let mut app = App::new();
        app.sessions[0].push_line("> hi".to_string());
        app.vim = true;
        let rows = screen_rows(&mut app, 100, 30);
        let last = rows.last().expect("row");
        assert!(last.contains(" NORMAL "), "{last:?}");
        assert!(last.contains("j/k:move  │  Space/i/a:type"), "{last:?}");
        assert!(last.contains("[idle]") && last.contains("mouse:"), "{last:?}");
        assert!(!last.contains("Esc:stop"), "{last:?}");
        app.sessions[0].busy = true;
        app.sessions[0].sync_activity();
        let rows = screen_rows(&mut app, 100, 30);
        let last = rows.last().expect("row");
        assert!(last.contains("Esc:stop") && last.contains("[busy]"), "{last:?}");
        // Narrow: hints give way first, the right side stays.
        let rows = screen_rows(&mut app, 40, 12);
        let last = rows.last().expect("row");
        assert!(last.contains("[busy]") && last.chars().count() == 40, "{last:?}");
        // INSERT hints.
        app.mode = Mode::Insert;
        let rows = screen_rows(&mut app, 100, 30);
        assert!(rows.last().expect("row").contains("Enter:send  │  Esc:normal"));
    }

    fn push_answer(app: &mut App, lines: &[&str]) {
        app.sessions[0].push_line("> q".to_string());
        for l in lines {
            app.sessions[0].push_line((*l).to_string());
        }
    }

    #[test]
    fn markdown_answer_renders_stripped_with_code_rows_on_code_bg() {
        let th = Theme::get(ThemeKind::GrokNight);
        let mut app = App::new();
        push_answer(
            &mut app,
            &[
                "## Title", "some **bold** and `code`", "- item one", "1. step one", "---",
                "```rust", "let x = 1;", "```", "after",
            ],
        );
        let rows = screen_rows(&mut app, 100, 30);
        let buf = screen_buf(&mut app, 100, 30);
        let at = |needle: &str| rows.iter().position(|r| r.contains(needle)).expect(needle);
        assert!(rows[at("Title")].starts_with("     Title"), "{:?}", rows[at("Title")]);
        assert!(rows.iter().all(|r| !r.contains("##") && !r.contains("**")));
        assert!(rows[at("some bold and code")].starts_with("     some bold and code"));
        assert!(rows[at("• item one")].starts_with("     • item one"));
        assert!(rows[at("1. step one")].starts_with("     1. step one"));
        assert!(rows[at("───")].contains(&"─".repeat(93)), "rule fills the text width");
        let (code, blank_open, close) = (at("let x = 1;"), at("let x = 1;") - 1, at("let x = 1;") + 1);
        for y in [code, blank_open, close] {
            for x in 5..98 {
                assert_eq!(buf[(x, y as u16)].bg, th.code_bg, "row {y} col {x}");
            }
            assert_ne!(buf[(98, y as u16)].bg, th.code_bg);
        }
        assert_ne!(buf[(10, at("after") as u16)].bg, th.code_bg);
        assert_eq!(buf[(5, at("Title") as u16)].fg, th.user);
        assert_eq!(buf[(15, at("some bold") as u16)].fg, th.text_primary);
    }

    #[test]
    fn fence_state_survives_pushes_replace_and_cache_rebuild() {
        let mut app = App::new();
        push_answer(&mut app, &["```", "# raw", "```", "# head"]);
        let s = &app.sessions[0];
        let k = |i| s.kind_at(i);
        assert_eq!(
            [k(0), k(1), k(2), k(3), k(4)],
            [MdKind::Plain, MdKind::FenceOpen, MdKind::Code, MdKind::FenceClose, MdKind::Text]
        );
        let lines = s.lines.clone();
        let kinds = s.kinds.clone();
        let mut s2 = crate::app::Session::new("x");
        s2.replace_lines(lines, 30);
        assert_eq!(s2.kinds, kinds);
        assert_eq!(s2.kinds.len(), s2.lines.len());
        // An open fence continues into the live answer draft.
        s2.replace_lines(vec!["```".to_string()], 30);
        s2.draft_answer = "# x".to_string();
        assert_eq!(s2.stream_draft_kinds(), [MdKind::Code]);
    }

    #[test]
    fn wrapped_markdown_line_height_and_copy_use_display_text() {
        let mut app = App::new();
        push_answer(&mut app, &["## aaaa bbbb cccc dddd eeee ffff", "x **bold word** y `code`"]);
        let _ = screen_rows(&mut app, 20, 20);
        let w = app.viewport_width;
        let s = &app.sessions[0];
        assert_eq!(s.row_cache[1], crate::app::wrap_rows("aaaa bbbb cccc dddd eeee ffff", w));
        assert!(s.row_cache[1] > 1);
        let (ax, ay, _, _) = app.text_area.expect("text area");
        assert!(app.sel_begin(ax, ay));
        app.sel_extend(ax + 12, ay + app.sessions[0].total_rows as u16 - 1);
        let copied = app.take_selected_text().expect("selection");
        assert!(copied.contains("aaaa bbbb cccc dddd eeee ffff"), "{copied:?}");
        assert!(copied.contains("x bold word y code"), "{copied:?}");
        assert!(!copied.contains("##") && !copied.contains("**") && !copied.contains('`'));
    }

    #[test]
    fn search_matches_display_text() {
        let mut app = App::new();
        push_answer(&mut app, &["a **bold** move"]);
        app.search_input = "a bold".to_string();
        app.run_search();
        assert_eq!(app.matches, vec![1]);
        app.search_input = "**".to_string();
        app.run_search();
        assert!(app.matches.is_empty());
    }

    #[test]
    fn thought_marker_is_muted_with_bold_word() {
        let th = Theme::get(ThemeKind::GrokNight);
        let d = display_line("◆ Thought for 6s", MdKind::Plain, th);
        assert_eq!(d.text, "◆ Thought for 6s");
        assert_eq!(d.spans[1].content, "Thought");
        assert!(d.spans[1].style.add_modifier.contains(Modifier::BOLD));
        assert!(d.spans.iter().all(|s| s.style.fg == Some(th.muted)));
        let d = display_line("Worked for 5.2s", MdKind::Plain, th);
        assert_eq!(d.spans[0].style.fg, Some(th.muted));
    }

    #[test]
    fn display_line_seam_prompt_and_plain() {
        let th = Theme::get(ThemeKind::GrokNight);
        let p = display_line("> hello", MdKind::Plain, th);
        assert_eq!((p.text.as_str(), p.indent), ("hello", 2));
        assert_eq!(p.fill, Some(th.user_bg));
        let plain = display_line("hello \x1b[2Jworld", MdKind::Plain, th);
        assert_eq!(plain.text, "hello world");
        assert_eq!(plain.indent, 0);
        assert_eq!(
            slice_spans(&plain.spans, 6, 11).iter().map(|s| s.content.as_ref()).collect::<String>(),
            "world"
        );
        assert_eq!(line_rows("> abcdefgh", MdKind::Plain, 6), 2, "prompt wraps at width - indent");
    }
}
