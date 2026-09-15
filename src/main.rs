//! polyforge M2 — provider-backed TUI shell.
//! Backend `mock` (M1 behavior, quota-free) or `muse` (MSP `serve` host over
//! stdio: initialize → session/start → turn/start, deltas to transcript,
//! approval/requested to the diff modal, turn/completed to the bell).
//! Select via `~/.config/polyforge/config.toml` (`[polyforge] provider`).

mod app;
mod config;
mod mock;
mod msp;
mod provider;
mod ui;

use std::io::{self, Write};
use std::time::Duration;

use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers, MouseEventKind},
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};
use tokio::sync::mpsc;

use app::{App, BackendKind, Mode, OutboxDecide};
use config::Config;
use msp::ServerMsg;
use provider::{DecisionKind, map_decision, muse_bringup, muse_decide, muse_submit};

#[tokio::main]
async fn main() -> io::Result<()> {
    let cfg = Config::load();
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new();
    let res = run(&mut terminal, &mut app, &cfg).await;

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    res
}

struct MuseCtx {
    host: msp::Host,
    rx: mpsc::Receiver<ServerMsg>,
}

async fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    cfg: &Config,
) -> io::Result<()> {
    let use_muse = cfg.polyforge.provider == "muse";
    app.backend = if use_muse {
        BackendKind::Muse
    } else {
        BackendKind::Mock
    };

    let mut muse: Option<MuseCtx> = None;
    if use_muse {
        let (tx, rx) = mpsc::channel(256);
        let tabs = app.sessions.len();
        match muse_bringup(
            &cfg.muse_bin(),
            tabs,
            cfg.muse_provider_id(),
            cfg.muse_cfg.model.clone(),
            cfg.workspace_root(),
            vec![],
            tx,
        )
        .await
        {
            Ok((host, ids, degraded)) => {
                app.muse_sessions = ids;
                if let Some(d) = degraded.clone() {
                    app.muse_degraded = Some(d.clone());
                    for s in &mut app.sessions {
                        s.push_line(format!("muse: {d}"));
                    }
                    app.flash = d;
                }
                muse = Some(MuseCtx { host, rx });
            }
            Err(e) => {
                app.muse_degraded = Some(e.clone());
                for s in &mut app.sessions {
                    s.push_line(format!("muse: {e}"));
                }
                app.flash = e;
            }
        }
    }

    // Crossterm reads block; isolate them on a thread, forward as messages.
    let (key_tx, mut key_rx) = mpsc::channel::<Event>(128);
    std::thread::spawn(move || {
        loop {
            match event::read() {
                Ok(ev) => {
                    if key_tx.blocking_send(ev).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Render is the most expensive thing this loop does (full-screen redraw
    // through tmux, 20x/sec unconditionally, is what "feels slow"). Draw
    // only when state actually changed, and batch bursty server messages
    // into a single frame.
    let mut ticker = tokio::time::interval(Duration::from_millis(50));
    let mut dirty = true;
    terminal.draw(|f| ui::render(f, app))?;
    loop {
        tokio::select! {
            biased;
            Some(ev) = key_rx.recv() => {
                match ev {
                    Event::Key(k) => {
                        handle_key(app, k.code, k.modifiers);
                        dirty = true;
                    }
                    Event::Mouse(m) => match m.kind {
                        MouseEventKind::ScrollUp => {
                            let step = wheel_step(&m, app);
                            app.scroll_lines(-step);
                            dirty = true;
                        }
                        MouseEventKind::ScrollDown => {
                            let step = wheel_step(&m, app);
                            app.scroll_lines(step);
                            dirty = true;
                        }
                        _ => {}
                    },
                    _ => {}
                }
                apply_mouse_capture(app)?;
            }
            msg = async {
                match &mut muse {
                    Some(ctx) => ctx.rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                if let Some(msg) = msg {
                    let mut bell = false;
                    let mut first = Some(msg);
                    // Drain the whole burst: one frame per batch, not per delta.
                    loop {
                        let next = first.take().or_else(|| {
                            muse.as_mut().and_then(|ctx| ctx.rx.try_recv().ok())
                        });
                        let Some(m) = next else { break };
                        match m {
                            ServerMsg::Notif { method, params } => {
                                let tab = tab_for_session(app, &params).unwrap_or(app.active);
                                bell |= provider::apply_notif(app, tab, &method, &params);
                            }
                            ServerMsg::Transport(e) => {
                                app.flash = format!("muse: {e}");
                            }
                        }
                    }
                    if bell {
                        ring_bell();
                    }
                    dirty = true;
                }
            }
            _ = ticker.tick() => {
                // Mock streaming only; an idle TUI paints nothing.
                if app.backend == BackendKind::Mock
                    && app.sessions.iter().any(|s| s.busy)
                {
                    if let Some(tab) = app.tick() {
                        ring_bell();
                        app.flash = format!("{tab} done ✓");
                    }
                    dirty = true;
                }
            }
        }
        if drain_outbox(app, muse.as_ref()).await {
            dirty = true;
        }
        if dirty {
            terminal.draw(|f| ui::render(f, app))?;
            dirty = false;
        }
        if app.should_quit {
            break;
        }
    }
    if let Some(ctx) = muse {
        ctx.host.shutdown().await;
    }
    Ok(())
}

/// Route a notification to the tab whose muse session it names.
fn tab_for_session(app: &App, params: &serde_json::Value) -> Option<usize> {
    let sid = params.get("sessionId")?.as_str()?;
    app.muse_sessions
        .iter()
        .position(|o| o.as_deref() == Some(sid))
}

/// Wheel = 3 lines, Shift+wheel = page (M1 spec, unchanged).
fn wheel_step(m: &crossterm::event::MouseEvent, app: &App) -> i32 {
    if m.modifiers.contains(KeyModifiers::SHIFT) {
        app.viewport_height as i32
    } else {
        3
    }
}

/// Perform queued muse work: turn submits and approval decisions.
/// Returns true when anything was sent or recorded (caller repaints).
async fn drain_outbox(app: &mut App, muse: Option<&MuseCtx>) -> bool {
    let Some(ctx) = muse else { return false };
    let mut did = false;
    while let Some(sub) = app.outbox.submits.pop() {
        did = true;
        let sid = app.muse_sessions.get(sub.tab).and_then(|o| o.clone());
        match sid {
            Some(id) => {
                if let Err(e) = muse_submit(&ctx.host, &id, &sub.prompt).await {
                    let s = &mut app.sessions[sub.tab];
                    s.busy = false;
                    s.push_line(format!("muse: turn/start failed: {e}"));
                }
            }
            None => {
                let s = &mut app.sessions[sub.tab];
                s.busy = false;
                s.push_line("muse: no session for this tab".to_string());
            }
        }
    }
    while let Some(d) = app.outbox.decides.pop() {
        did = true;
        let sid = app.muse_sessions.get(d.tab).and_then(|o| o.clone());
        match sid {
            Some(id) => {
                if let Err(e) = muse_decide(&ctx.host, &id, &d).await {
                    let s = &mut app.sessions[d.tab];
                    s.push_line(format!("muse: decide failed: {e} (card may reappear)"));
                    app.flash = format!("muse: decide failed: {e}");
                }
            }
            None => {
                app.flash = "muse: no session for this tab".to_string();
            }
        }
    }
    did
}

fn apply_mouse_capture(app: &App) -> io::Result<()> {
    if app.mouse {
        execute!(io::stdout(), EnableMouseCapture)?;
    } else {
        execute!(io::stdout(), DisableMouseCapture)?;
    }
    Ok(())
}

fn ring_bell() {
    print!("\x07");
    let _ = io::stdout().flush();
}

fn clear_pending_g(app: &mut App) {
    app.pending_g = false;
}

/// Queue a UI-side approval decision: transcript lines now, wire decide via
/// the outbox (muse) or the immediate mock close-out.
fn decide_ui(app: &mut App, kind: DecisionKind, label: &str) {
    match app.backend {
        BackendKind::Mock => {
            if app.decide_diff(label) {
                ring_bell();
            }
        }
        BackendKind::Muse => {
            let tab = app.active;
            let approval = app.active().pending_approval.clone();
            match approval {
                Some(a) => match map_decision(&a, kind) {
                    Some((choice_id, feedback)) => {
                        app.outbox.decides.push(OutboxDecide {
                            tab,
                            approval_id: a.approval_id,
                            requirement_id: a.requirement_id,
                            choice_id,
                            feedback,
                        });
                        if app.muse_approved(label) {
                            ring_bell();
                        }
                    }
                    None => {
                        // Never trap the user: close the card locally and say
                        // so. The approval stays pending server-side (it may
                        // be re-issued); nothing is auto-approved or denied.
                        app.muse_approved(label);
                        app.flash = format!(
                            "muse: closed locally — server offered no way to send `{label}`"
                        );
                        ring_bell();
                    }
                },
                None => {
                    if app.muse_approved(label) {
                        ring_bell();
                    }
                }
            }
        }
    }
}

fn handle_key(app: &mut App, code: KeyCode, mods: KeyModifiers) {
    let ctrl = mods.contains(KeyModifiers::CONTROL);
    // Crossterm reports uppercase letters with SHIFT held (e.g. `G` arrives
    // as Char('G')+SHIFT). A lone SHIFT must not break single-key bindings.
    let no_mods = mods.is_empty() || mods == KeyModifiers::SHIFT;

    // Diff modal steals y/n/a/q on the active tab.
    if app.active().pending_diff.is_some() {
        if let KeyCode::Char(c) = code {
            if no_mods {
                match c {
                    'y' => {
                        decide_ui(app, DecisionKind::Approve, "approved");
                        return;
                    }
                    'n' => {
                        decide_ui(app, DecisionKind::Reject, "rejected");
                        return;
                    }
                    'a' => {
                        decide_ui(app, DecisionKind::ApproveAll, "approved-all");
                        return;
                    }
                    'q' => {
                        decide_ui(app, DecisionKind::Later, "deferred");
                        return;
                    }
                    _ => {}
                }
            }
        }
        if code == KeyCode::Esc {
            decide_ui(app, DecisionKind::Later, "deferred");
            return;
        }
    }

    match app.mode {
        Mode::Normal => match code {
            KeyCode::Char('c') if ctrl => app.should_quit = true,
            KeyCode::Char('q') if no_mods => app.should_quit = true,
            KeyCode::Char('j') if no_mods => {
                clear_pending_g(app);
                app.scroll_lines(1);
            }
            KeyCode::Char('k') if no_mods => {
                clear_pending_g(app);
                app.scroll_lines(-1);
            }
            KeyCode::Down if no_mods => {
                clear_pending_g(app);
                app.scroll_lines(1);
            }
            KeyCode::Up if no_mods => {
                clear_pending_g(app);
                app.scroll_lines(-1);
            }
            KeyCode::Char('u') if ctrl => {
                clear_pending_g(app);
                let h = app.half_page();
                app.scroll_lines(-h);
            }
            KeyCode::Char('d') if ctrl => {
                clear_pending_g(app);
                let h = app.half_page();
                app.scroll_lines(h);
            }
            // Prototype deviation (documented): single `g` = top, `G` = bottom.
            KeyCode::Char('g') if no_mods => {
                app.scroll_top();
                clear_pending_g(app);
            }
            KeyCode::Char('G') if no_mods => {
                app.scroll_bottom();
                clear_pending_g(app);
            }
            KeyCode::Char('/') if no_mods => {
                clear_pending_g(app);
                app.mode = Mode::Search;
                app.search_input.clear();
                app.search_cursor = 0;
            }
            KeyCode::Char('n') if no_mods => {
                clear_pending_g(app);
                app.search_step(1);
            }
            KeyCode::Char('N') if no_mods => {
                clear_pending_g(app);
                app.search_step(-1);
            }
            KeyCode::Char('i') | KeyCode::Char('a') if no_mods => {
                clear_pending_g(app);
                app.mode = Mode::Insert;
            }
            KeyCode::Char('m') if no_mods => {
                clear_pending_g(app);
                app.mouse = !app.mouse;
                app.flash = format!("mouse {}", if app.mouse { "on" } else { "off" });
            }
            KeyCode::Char(c) if no_mods && ['1', '2', '3'].contains(&c) => {
                clear_pending_g(app);
                app.active = (c as usize - '1' as usize).min(app.sessions.len() - 1);
                app.stick_to_bottom();
            }
            KeyCode::Tab if no_mods => {
                clear_pending_g(app);
                app.active = (app.active + 1) % app.sessions.len();
                app.stick_to_bottom();
            }
            _ => clear_pending_g(app),
        },
        Mode::Insert => match code {
            KeyCode::Esc => app.mode = Mode::Normal,
            // Ctrl-[ sends the same bytes as Esc on most terminals; belt & braces.
            KeyCode::Char('[') if ctrl => app.mode = Mode::Normal,
            KeyCode::Enter => app.submit(),
            KeyCode::Backspace => {
                let s = app.active_mut();
                if s.cursor > 0 {
                    s.cursor -= 1;
                    let bi = byte_index(&s.input, s.cursor);
                    s.input.remove(bi);
                }
            }
            KeyCode::Left => {
                let s = app.active_mut();
                s.cursor = s.cursor.saturating_sub(1);
            }
            KeyCode::Right => {
                let s = app.active_mut();
                let max = s.input.chars().count();
                s.cursor = (s.cursor + 1).min(max);
            }
            KeyCode::Char(c) if !ctrl => {
                let s = app.active_mut();
                let bi = byte_index(&s.input, s.cursor);
                s.input.insert(bi, c);
                s.cursor += 1;
            }
            _ => {}
        },
        Mode::Search => match code {
            KeyCode::Esc => {
                app.mode = Mode::Normal;
                app.flash.clear();
            }
            KeyCode::Char('[') if ctrl => {
                app.mode = Mode::Normal;
                app.flash.clear();
            }
            KeyCode::Enter => {
                app.mode = Mode::Normal;
                app.run_search();
            }
            KeyCode::Backspace => {
                if app.search_cursor > 0 {
                    app.search_cursor -= 1;
                    let bi = byte_index(&app.search_input, app.search_cursor);
                    app.search_input.remove(bi);
                }
            }
            KeyCode::Char(c) if !ctrl => {
                let bi = byte_index(&app.search_input, app.search_cursor);
                app.search_input.insert(bi, c);
                app.search_cursor += 1;
            }
            _ => {}
        },
    }
}

fn byte_index(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .map(|(b, _)| b)
        .nth(char_idx)
        .unwrap_or(s.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{ApprovalChoice, PendingApproval, PendingDiff};
    use crossterm::event::{KeyCode, KeyModifiers};

    const NONE: KeyModifiers = KeyModifiers::empty();
    const SHIFT: KeyModifiers = KeyModifiers::SHIFT;

    fn muse_modal_app() -> App {
        let mut app = App::new();
        app.backend = BackendKind::Muse;
        app.active_mut().pending_diff = Some(PendingDiff {
            file: "tool".into(),
            body: "body".into(),
        });
        app.active_mut().pending_approval = Some(PendingApproval {
            approval_id: "a1".into(),
            requirement_id: serde_json::Value::Null,
            choices: vec![ApprovalChoice {
                choice_id: "c-allow".into(),
                decision: "approved".into(),
                scope: "once".into(),
                label: "Allow".into(),
                accepts_feedback: false,
            }],
        });
        app
    }

    /// The reported bug: with no deny choice offered, q/Esc must still
    /// close the card (locally, sending nothing) — never trap the user.
    #[test]
    fn q_closes_modal_without_deny_choice() {
        let mut app = muse_modal_app();
        handle_key(&mut app, KeyCode::Char('q'), NONE);
        assert!(app.active().pending_diff.is_none(), "modal stuck open on q");
        assert!(app.outbox.decides.is_empty(), "q must not send a decision");
        assert!(!app.flash.is_empty());
        let mut app = muse_modal_app();
        handle_key(&mut app, KeyCode::Esc, NONE);
        assert!(app.active().pending_diff.is_none(), "modal stuck open on Esc");
    }

    /// The happy path in muse mode: y maps to the allow choice and queues
    /// exactly one wire decision.
    #[test]
    fn y_queues_allow_decision_in_muse_mode() {
        let mut app = muse_modal_app();
        handle_key(&mut app, KeyCode::Char('y'), NONE);
        assert!(app.active().pending_diff.is_none());
        assert_eq!(app.outbox.decides.len(), 1);
        assert_eq!(app.outbox.decides[0].choice_id, "c-allow");
    }

    /// Crossterm delivers `G` as Char('G')+SHIFT; it must jump to bottom.
    #[test]
    fn shift_g_goes_to_bottom() {
        let mut app = App::new();
        app.viewport_height = 20;
        app.stick_to_bottom();
        let max = app.active().lines.len().saturating_sub(20);
        handle_key(&mut app, KeyCode::Char('g'), NONE);
        assert_eq!(app.active().scroll, 0);
        handle_key(&mut app, KeyCode::Char('G'), SHIFT);
        assert_eq!(
            app.active().scroll,
            max,
            "Shift+G ignored: scroll={} max={}",
            app.active().scroll,
            max
        );
    }

    /// Plain keys keep working alongside the Shift tolerance.
    #[test]
    fn plain_keys_unaffected() {
        let mut app = App::new();
        app.viewport_height = 20;
        app.stick_to_bottom();
        let max = app.active().lines.len().saturating_sub(20);
        handle_key(&mut app, KeyCode::Char('k'), NONE);
        assert_eq!(app.active().scroll, max - 1);
        handle_key(&mut app, KeyCode::Char('n'), NONE); // no search: flash, no panic
        assert!(!app.flash.is_empty());
        handle_key(&mut app, KeyCode::Char('q'), NONE);
        assert!(app.should_quit);
    }
}
