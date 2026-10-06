//! polyforge M2 — provider-backed TUI shell.
//! Backend `mock` (M1 behavior, quota-free) or `muse` (MSP `serve` host over
//! stdio: initialize → session/start → turn/start, deltas to transcript,
//! approval/requested to the diff modal, turn/completed to the bell).
//! Select via `~/.config/polyforge/config.toml` (`[polyforge] provider`).

mod activity;
mod agy;
mod app;
mod backend;
mod claude;
mod clipboard;
mod codex;
mod config;
mod drain;
mod grok;
mod input;
mod markdown;
mod mock;
mod msp;
mod provider;
mod risk;
mod server_msg;
mod store;
#[cfg(test)]
mod test_support;
mod theme;
mod typesafe;
mod ui;

use std::io;
use std::time::Duration;

use crossterm::{
    event::{self, Event, MouseButton, MouseEventKind},
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};
use tokio::sync::mpsc;

use app::{App, BackendKind, Mode};
use backend::{Backends, finish_bringup};
use config::Config;
use drain::drain_outbox;
use grok::grok_close_session;
use input::{apply_mouse_capture, copy_to_clipboard, handle_key, ring_bell, wheel_step};
use msp::ServerMsg;
use risk::{RiskMsg, apply_risk_msg, spawn_pending_risk_scores};
use server_msg::{
    backend_frame, backend_msg, claude_frame_current, handle_agy_msg, handle_server_msg,
};

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

async fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    cfg: &Config,
) -> io::Result<()> {
    let def = cfg.default_backend();
    // Keymap is remembered in the config file (`/vim` toggles + saves).
    app.vim = cfg.polyforge.vim;
    // Theme likewise (`/theme` switches + saves; typos fall back).
    app.theme = cfg.theme_kind();
    app.workspace = cfg.workspace_root();
    // Fresh boot: exactly one tab with a NEW session id (previous sessions
    // stay on disk for `/sessions`). Storageless falls back to no store.
    if let Some(store) = store::Store::open() {
        app.store = Some(store);
        app.sessions[0].backend = def;
        app.attach_fresh_store(0);
    } else {
        for s in &mut app.sessions {
            s.backend = def;
        }
    }

    // First run: no provider saved yet. Ask before bringing anything up;
    // the choice queues the tab's respawn (drain_outbox starts the host).
    if cfg.polyforge.provider.is_none() {
        app.onboarding = true;
        app.picker_sel = BackendKind::ALL
            .iter()
            .position(|(b, _)| *b == def)
            .unwrap_or(0);
        app.mode = Mode::Picker;
    }

    // Paint and start reading keys before any backend comes up: bring-up
    // can take seconds, and the user should see "connecting…", not a
    // blank screen.
    terminal.draw(|f| ui::render(f, app))?;
    // Crossterm reads block; isolate them on a thread, forward as messages.
    let (key_tx, mut key_rx) = mpsc::channel::<Event>(128);
    std::thread::spawn(move || {
        while let Ok(ev) = event::read() {
            if key_tx.blocking_send(ev).is_err() {
                break;
            }
        }
    });

    let mut backends = Backends::default();
    // TypeSafe: optional. Missing key leaves approvals unscored.
    let typesafe = typesafe::Client::from_env();
    let (risk_tx, mut risk_rx) = mpsc::channel::<RiskMsg>(32);
    // One shared channel for all agy tab children.
    let (agy_tx, mut agy_rx) = mpsc::channel::<agy::AgyFrame>(256);
    // Likewise for claude tab children.
    let (claude_tx, mut claude_rx) = mpsc::channel::<ServerMsg>(256);
    backends.claude_tx = Some(claude_tx);
    // Queue a bring-up for every tab (hosts start once per kind, off the
    // loop, so keys and redraws work meanwhile). Onboarding skips this:
    // nothing is chosen yet.
    if !app.onboarding {
        for tab in 0..app.sessions.len() {
            let backend = app.sessions[tab].backend;
            app.outbox
                .respawns
                .push(app::OutboxRespawn { tab, backend });
        }
        drain_outbox(app, &mut backends, cfg, &agy_tx).await;
    }

    // Render is the most expensive thing this loop does (full-screen redraw
    // through tmux, 20x/sec unconditionally, is what "feels slow"). Draw
    // only when state actually changed, batch bursty server messages into a
    // single frame, and cap the paint rate grok-build-style (16ms min draw
    // interval ≈ 60fps; skipped frames stay dirty and land on a later tick).
    const MIN_DRAW: Duration = Duration::from_millis(16);
    let mut ticker = tokio::time::interval(Duration::from_millis(50));
    let mut dirty = true;
    let mut last_draw: Option<std::time::Instant> = None;
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
                        // Drag-to-select in the transcript; release copies.
                        MouseEventKind::Down(btn) => {
                            // The `[stop]` button wins over selection.
                            if app.mouse
                                && ((btn == MouseButton::Left && app.click_stop(m.column, m.row))
                                    || app.sel_begin(m.column, m.row))
                            {
                                dirty = true;
                            }
                        }
                        MouseEventKind::Drag(_) => {
                            if app.mouse && app.sel.is_some() {
                                app.sel_extend(m.column, m.row);
                                dirty = true;
                            }
                        }
                        MouseEventKind::Up(_) if app.mouse && app.sel.is_some() => {
                            // take_selected_text always dehighlights
                            // (copy path and empty click alike).
                            if let Some(text) = app.take_selected_text() {
                                app.flash = copy_to_clipboard(&text);
                            }
                            dirty = true;
                        }
                        _ => {}
                    },
                    _ => {}
                }
                apply_mouse_capture(app)?;
            }
            Some(done) = backends.bringups.join_next() => {
                // A panicked task drops its result; the tab keeps showing
                // "connecting…" until P retries.
                if let Ok(done) = done {
                    finish_bringup(app, &mut backends, cfg, done);
                }
                dirty = true;
            }
            muse_msg = backend_msg(&mut backends.muse) => {
                let had = muse_msg.is_some();
                if backend_frame(app, BackendKind::Muse, &mut backends.muse, muse_msg) {
                    ring_bell();
                }
                if had {
                    dirty = true;
                }
            }
            codex_msg = backend_msg(&mut backends.codex) => {
                let had = codex_msg.is_some();
                if backend_frame(app, BackendKind::Codex, &mut backends.codex, codex_msg) {
                    ring_bell();
                }
                if had {
                    dirty = true;
                }
            }
            grok_msg = backend_msg(&mut backends.grok) => {
                let had = grok_msg.is_some();
                if backend_frame(app, BackendKind::Grok, &mut backends.grok, grok_msg) {
                    ring_bell();
                }
                if had {
                    dirty = true;
                }
            }
            agy_msg = agy_rx.recv() => {
                if let Some(msg) = agy_msg {
                    // Agy children share one channel; drain the burst.
                    let mut bell = handle_agy_msg(app, &backends, msg);
                    while let Ok(m) = agy_rx.try_recv() {
                        bell |= handle_agy_msg(app, &backends, m);
                    }
                    if bell {
                        ring_bell();
                    }
                    dirty = true;
                }
            }
            claude_msg = claude_rx.recv() => {
                if let Some(msg) = claude_msg {
                    let mut bell = false;
                    if claude_frame_current(&backends, &msg) {
                        bell = handle_server_msg(app, BackendKind::Claude, msg);
                    }
                    while let Ok(m) = claude_rx.try_recv() {
                        if claude_frame_current(&backends, &m) {
                            bell |= handle_server_msg(app, BackendKind::Claude, m);
                        }
                    }
                    if bell {
                        ring_bell();
                    }
                    dirty = true;
                }
            }
            risk_msg = risk_rx.recv() => {
                if let Some(msg) = risk_msg {
                    apply_risk_msg(app, msg);
                    while let Ok(m) = risk_rx.try_recv() {
                        apply_risk_msg(app, m);
                    }
                    dirty = true;
                }
            }
            _ = ticker.tick() => {
                // Mock streaming and the busy animation only; an idle TUI
                // paints nothing.
                if app.sessions.iter().any(|s| s.backend == BackendKind::Mock && s.busy)
                {
                    if let Some(tab) = app.tick() {
                        ring_bell();
                        app.flash = format!("{tab} done ✓");
                    }
                    dirty = true;
                }
                let a = app.active();
                if a.busy && a.pending_diff.is_none() {
                    dirty = true;
                }
            }
        }
        if drain_outbox(app, &mut backends, cfg, &agy_tx).await {
            dirty = true;
        }
        spawn_pending_risk_scores(app, &typesafe, &risk_tx);
        // Kill agy children of `/tab close`d tabs (queued indices refer to
        // the layout at close time: compensate for earlier removals with a
        // strictly-less shift so surviving tabs keep their handles).
        if !app.pending_agy_kill.is_empty() {
            let mut removed: Vec<usize> = Vec::new();
            for tab in app.pending_agy_kill.drain(..) {
                let idx = tab.saturating_sub(removed.iter().filter(|r| **r < tab).count());
                if backends.agy.len() > idx {
                    // Removing the slot keeps handles aligned with tabs;
                    // shutdown kills the child (drop alone would leak it).
                    if let Some(h) = backends.agy.remove(idx) {
                        h.shutdown().await;
                    }
                    removed.push(idx);
                    dirty = true;
                }
            }
        }
        // Server-side close for `/tab close`d grok tabs (best-effort,
        // spawned: a hung server must never stall the event loop).
        if !app.pending_grok_close.is_empty() {
            if let Some(ctx) = backends.get(BackendKind::Grok) {
                let host = ctx.host.clone();
                let sids: Vec<String> = app.pending_grok_close.drain(..).collect();
                tokio::spawn(async move {
                    for sid in sids {
                        grok_close_session(&host, &sid).await;
                    }
                });
                dirty = true;
            } else {
                app.pending_grok_close.clear();
            }
        }
        // Persist transcript lines toward disk (free when buffers are empty).
        app.flush_store();
        for s in &mut app.sessions {
            s.sync_activity();
        }
        let due = last_draw.map(|t| t.elapsed() >= MIN_DRAW).unwrap_or(true);
        if dirty && due {
            terminal.draw(|f| ui::render(f, app))?;
            last_draw = Some(std::time::Instant::now());
            dirty = false;
        }
        if app.should_quit {
            break;
        }
    }
    // Abort in-flight bring-ups first: dropping them kills any half-started
    // host and releases their Arc clones.
    backends.bringups.shutdown().await;
    // In-flight spawned grok prompts hold the last Arcs: try_unwrap then
    // misses, and the final drop kills the child (kill_on_drop) after
    // stdin EOF already asked it to exit. No headless leak either way.
    if let Some(ctx) = backends.muse
        && let Ok(host) = std::sync::Arc::try_unwrap(ctx.host)
    {
        host.shutdown().await;
    }
    if let Some(ctx) = backends.codex
        && let Ok(host) = std::sync::Arc::try_unwrap(ctx.host)
    {
        host.shutdown().await;
    }
    if let Some(ctx) = backends.grok
        && let Ok(host) = std::sync::Arc::try_unwrap(ctx.host)
    {
        host.shutdown().await;
    }
    for slot in backends.agy.iter_mut() {
        if let Some(h) = slot.take() {
            h.shutdown().await;
        }
    }
    for (_, h) in backends.claude.drain() {
        h.shutdown().await;
    }
    Ok(())
}
