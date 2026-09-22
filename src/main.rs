//! polyforge M2 — provider-backed TUI shell.
//! Backend `mock` (M1 behavior, quota-free) or `muse` (MSP `serve` host over
//! stdio: initialize → session/start → turn/start, deltas to transcript,
//! approval/requested to the diff modal, turn/completed to the bell).
//! Select via `~/.config/polyforge/config.toml` (`[polyforge] provider`).

mod agy;
mod app;
mod clipboard;
mod codex;
mod config;
mod grok;
mod mock;
mod msp;
mod provider;
mod store;
mod typesafe;
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

use agy::{AgyHandle, agy_submit, apply_agy_notif, spawn_agy};
use app::{App, BackendKind, DecisionKind, Mode, OutboxDecide};
use codex::{
    apply_codex_approval, apply_codex_notif, codex_bringup, codex_respond, codex_resume_thread,
    codex_start_thread, map_codex_decision,
};
use config::Config;
use grok::{
    apply_grok_notif, apply_grok_permission, grok_bringup, grok_close_session, grok_new_session,
    grok_respond, grok_resume_session, grok_submit, map_grok_decision,
};
use msp::ServerMsg;
use provider::{
    map_decision, muse_bringup, muse_decide, muse_resume_session, muse_start_session, muse_submit,
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

struct LiveBackend {
    // Shared: grok's blocking session/prompt runs spawned per submit and
    // holds a clone until the turn ends (deref coercion keeps &Host call
    // sites unchanged).
    host: std::sync::Arc<msp::Host>,
    rx: mpsc::Receiver<ServerMsg>,
}

#[derive(Default)]
struct Backends {
    muse: Option<LiveBackend>,
    codex: Option<LiveBackend>,
    grok: Option<LiveBackend>,
    /// Completion sender for spawned grok prompts (session/prompt answers
    /// only at turn end, so submits can't await it in the drain loop).
    grok_tx: Option<mpsc::Sender<ServerMsg>>,
    /// One agy child per tab (each holds its own conversation).
    agy: Vec<Option<AgyHandle>>,
}

impl Backends {
    fn get(&self, kind: BackendKind) -> Option<&LiveBackend> {
        match kind {
            BackendKind::Muse => self.muse.as_ref(),
            BackendKind::Codex => self.codex.as_ref(),
            BackendKind::Grok => self.grok.as_ref(),
            BackendKind::Mock | BackendKind::Agy => None,
        }
    }
}

/// TypeSafe approval-risk result (tab + generation for staleness).
enum RiskMsg {
    Ready {
        tab: usize,
        token: u64,
        judgment: typesafe::ApprovalJudgment,
    },
    Failed {
        tab: usize,
        token: u64,
        err: String,
    },
}

/// Spawn one System One call per open DIFF that lacks a judgment yet.
fn spawn_pending_risk_scores(
    app: &mut App,
    client: &Option<typesafe::Client>,
    tx: &mpsc::Sender<RiskMsg>,
) {
    let Some(client) = client else {
        return;
    };
    for (tab, s) in app.sessions.iter_mut().enumerate() {
        let Some(diff) = s.pending_diff.as_ref() else {
            continue;
        };
        if s.approval_risk.is_some() {
            continue;
        }
        if s.risk_spawned_gen == Some(s.risk_gen) {
            continue;
        }
        let token = s.risk_gen;
        s.risk_spawned_gen = Some(token);
        let tool = diff.file.clone();
        let body = diff.body.clone();
        let client = client.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            let msg = match client.judge_approval(&tool, &body).await {
                Ok(judgment) => RiskMsg::Ready {
                    tab,
                    token,
                    judgment,
                },
                Err(err) => RiskMsg::Failed { tab, token, err },
            };
            let _ = tx.send(msg).await;
        });
    }
}

fn apply_risk_msg(app: &mut App, msg: RiskMsg) {
    match msg {
        RiskMsg::Ready {
            tab,
            token,
            judgment,
        } => {
            let Some(s) = app.sessions.get_mut(tab) else {
                return;
            };
            if s.pending_diff.is_none() || s.risk_gen != token {
                return;
            }
            let line = judgment.summary_line();
            s.approval_risk = Some(judgment);
            if tab == app.active {
                app.flash = line;
            }
        }
        RiskMsg::Failed { tab, token, err } => {
            let Some(s) = app.sessions.get_mut(tab) else {
                return;
            };
            if s.pending_diff.is_none() || s.risk_gen != token {
                return;
            }
            // Leave approval_risk None; allow a later retry if token bumps.
            s.risk_spawned_gen = None;
            if tab == app.active {
                app.flash = format!("risk score failed: {err}");
            }
        }
    }
}

/// Mark every tab of `kind` degraded with the exact fix (spec Q9 grey-out).
fn grey_out(app: &mut App, kind: BackendKind, reason: &str) {
    let tag = kind.label();
    for s in &mut app.sessions {
        if s.backend == kind {
            s.tab_degraded = Some(reason.to_string());
            s.push_line(format!("{tag}: {reason}"));
        }
    }
    app.flash = format!("{tag}: {reason}");
}

/// Bring up the serve/app-server host for `kind` if not running.
async fn ensure_host(
    backends: &mut Backends,
    kind: BackendKind,
    cfg: &Config,
) -> Result<(), String> {
    let present = match kind {
        BackendKind::Muse => backends.muse.is_some(),
        BackendKind::Codex => backends.codex.is_some(),
        BackendKind::Grok => backends.grok.is_some(),
        // Agy children are per-tab (see open_tab_session); nothing shared.
        BackendKind::Agy | BackendKind::Mock => true,
    };
    if present {
        return Ok(());
    }
    let (tx, rx) = mpsc::channel(256);
    match kind {
        BackendKind::Muse => {
            let (host, ids, degraded) = muse_bringup(
                &cfg.muse_bin(),
                0,
                cfg.muse_provider_id(),
                cfg.muse_cfg.model.clone(),
                cfg.workspace_root(),
                vec![],
                tx,
            )
            .await?;
            let _ = (ids, degraded);
            backends.muse = Some(LiveBackend {
                host: std::sync::Arc::new(host),
                rx,
            });
            Ok(())
        }
        BackendKind::Codex => {
            let (host, ids, degraded) = codex_bringup(
                &cfg.codex_bin(),
                0,
                cfg.codex_model(),
                cfg.workspace_root(),
                vec![],
                tx,
            )
            .await?;
            let _ = (ids, degraded);
            backends.codex = Some(LiveBackend {
                host: std::sync::Arc::new(host),
                rx,
            });
            Ok(())
        }
        BackendKind::Grok => {
            let (host, prompt_tx, degraded) =
                grok_bringup(&cfg.grok_bin(), env!("CARGO_PKG_VERSION"), vec![], tx).await?;
            let _ = degraded;
            backends.grok = Some(LiveBackend {
                host: std::sync::Arc::new(host),
                rx,
            });
            // Retained for spawned session/prompt completions (see drain).
            backends.grok_tx = Some(prompt_tx);
            Ok(())
        }
        // Agy is unreachable here (the respawn drain routes it to
        // open_tab_session first); keep the total match honest.
        BackendKind::Agy | BackendKind::Mock => Ok(()),
    }
}

/// Spawn (or replace) the agy child for one tab. The conversation id
/// arrives later via the init event.
async fn ensure_agy_tab(
    backends: &mut Backends,
    tab: usize,
    cfg: &Config,
    events_tx: &mpsc::Sender<ServerMsg>,
    resume: Option<String>,
) -> Result<(), String> {
    if backends.agy.len() <= tab {
        backends.agy.resize_with(tab + 1, || None);
    }
    if backends.agy[tab].is_some() {
        return Ok(());
    }
    let mut args = vec![
        "--input-format".to_string(),
        "stream-json".to_string(),
        "--output-format".to_string(),
        "stream-json".to_string(),
    ];
    if let Some(m) = cfg.agy_cfg.model.clone() {
        args.push("--model".to_string());
        args.push(m);
    }
    if let Some(a) = cfg.agy_cfg.agent.clone() {
        args.push("--agent".to_string());
        args.push(a);
    }
    if let Some(id) = resume {
        args.push("--conversation".to_string());
        args.push(id);
    }
    match spawn_agy(
        &cfg.agy_bin(),
        &args,
        &cfg.workspace_root(),
        events_tx.clone(),
    )
    .await
    {
        Ok(h) => {
            backends.agy[tab] = Some(h);
            Ok(())
        }
        Err(e) => Err(format!("could not spawn `agy`: {e}")),
    }
}

/// Open (or re-open) the remote session for one tab. Mock tabs need nothing.
/// A stored remote id resumes the previous session (Q10); a failed resume
/// falls back to a fresh session and says so. Failures grey out just this
/// tab with the exact fix.
async fn open_tab_session(
    backends: &mut Backends,
    app: &mut App,
    tab: usize,
    cfg: &Config,
    agy_tx: &mpsc::Sender<ServerMsg>,
) {
    let (backend, workspace) = (app.sessions[tab].backend, cfg.workspace_root());
    // Resume candidate from the store; cleared so a stale id never lingers.
    let resume = app.sessions[tab].remote_id.clone();
    let s = &mut app.sessions[tab];
    s.remote_id = None;
    s.tab_degraded = None;
    let tag = backend.label();
    let fail = |app: &mut App, reason: &str| {
        let s = &mut app.sessions[tab];
        s.tab_degraded = Some(reason.to_string());
        s.push_line(format!("{tag}: {reason}"));
        app.flash = format!("{tag}: {reason}");
    };
    // Record the outcome for the next boot.
    let opened = |app: &mut App, id: String, resumed: bool| {
        let s = &mut app.sessions[tab];
        s.remote_id = Some(id);
        if resumed {
            s.push_line(format!("{tag}: resumed previous session"));
        }
        app.save_tab_meta(tab);
    };
    match backend {
        BackendKind::Mock => app.save_tab_meta(tab),
        BackendKind::Muse => {
            let Some(ctx) = backends.get(backend) else {
                fail(app, "host not running — press P to respawn");
                return;
            };
            let fresh = || {
                muse_start_session(
                    &ctx.host,
                    cfg.muse_provider_id(),
                    cfg.muse_cfg.model.clone(),
                    workspace.clone(),
                )
            };
            match resume {
                Some(old) => match muse_resume_session(&ctx.host, &old).await {
                    Ok(id) => opened(app, id, true),
                    Err(rerr) => match fresh().await {
                        Ok(id) => {
                            let s = &mut app.sessions[tab];
                            s.push_line(format!("{tag}: resume failed ({rerr}) — started fresh"));
                            opened(app, id, false);
                        }
                        Err(e) => fail(app, &e),
                    },
                },
                None => match fresh().await {
                    Ok(id) => opened(app, id, false),
                    Err(e) => fail(app, &e),
                },
            }
        }
        BackendKind::Codex => {
            let Some(ctx) = backends.get(backend) else {
                fail(app, "host not running — press P to respawn");
                return;
            };
            match resume {
                Some(old) => match codex_resume_thread(&ctx.host, &old).await {
                    Ok(id) => opened(app, id, true),
                    Err(rerr) => {
                        match codex_start_thread(&ctx.host, cfg.codex_model(), workspace).await {
                            Ok(id) => {
                                let s = &mut app.sessions[tab];
                                s.push_line(format!(
                                    "{tag}: resume failed ({rerr}) — started fresh"
                                ));
                                opened(app, id, false);
                            }
                            Err(e) => fail(app, &e),
                        }
                    }
                },
                None => match codex_start_thread(&ctx.host, cfg.codex_model(), workspace).await {
                    Ok(id) => opened(app, id, false),
                    Err(e) => fail(app, &e),
                },
            }
        }
        BackendKind::Grok => {
            let Some(ctx) = backends.get(backend) else {
                fail(app, "host not running — press P to respawn");
                return;
            };
            // A model-override note rides alongside a fresh id (non-fatal).
            let fresh_note = |app: &mut App, note: Option<String>| {
                if let Some(n) = note {
                    app.sessions[tab].push_line(n);
                }
            };
            match resume {
                Some(old) => match grok_resume_session(&ctx.host, &old, workspace.clone()).await {
                    Ok(id) => opened(app, id, true),
                    Err(rerr) => {
                        match grok_new_session(&ctx.host, cfg.grok_model(), workspace.clone()).await
                        {
                            Ok((id, note)) => {
                                let s = &mut app.sessions[tab];
                                s.push_line(format!(
                                    "{tag}: resume failed ({rerr}) — started fresh"
                                ));
                                fresh_note(app, note);
                                opened(app, id, false);
                            }
                            Err(e) => fail(app, &e),
                        }
                    }
                },
                None => {
                    match grok_new_session(&ctx.host, cfg.grok_model(), workspace.clone()).await {
                        Ok((id, note)) => {
                            fresh_note(app, note);
                            opened(app, id, false);
                        }
                        Err(e) => fail(app, &e),
                    }
                }
            }
        }
        BackendKind::Agy => {
            // A fresh child per switch: kill the old conversation first.
            // A stored conversation id is passed through for continuation.
            if backends.agy.len() > tab {
                if let Some(old) = backends.agy[tab].take() {
                    old.shutdown().await;
                }
            }
            app.sessions[tab].pending_agy_init = false;
            // Keep expected resume id so init can match by conversation_id.
            if let Some(ref id) = resume {
                app.sessions[tab].remote_id = Some(id.clone());
                app.sessions[tab].push_line(format!("{tag}: continuing previous conversation"));
            }
            let had_resume = resume.is_some();
            match ensure_agy_tab(backends, tab, cfg, agy_tx, resume).await {
                Ok(()) => {
                    app.sessions[tab].pending_agy_init = true;
                    // Resume tabs match by conversation_id; only fresh
                    // spawns claim a FIFO slot.
                    if !had_resume {
                        app.agy_init_fifo.push_back(tab);
                    }
                }
                Err(e) => {
                    app.sessions[tab].remote_id = None;
                    fail(app, &e);
                }
            }
        }
    }
}

async fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    cfg: &Config,
) -> io::Result<()> {
    let def = cfg.default_backend();
    // Keymap is remembered in the config file (`/vim` toggles + saves).
    app.vim = cfg.polyforge.vim;
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

    let mut backends = Backends::default();
    // TypeSafe: optional. Missing key leaves approvals unscored.
    let typesafe = typesafe::Client::from_env();
    let (risk_tx, mut risk_rx) = mpsc::channel::<RiskMsg>(32);
    // One shared channel for all agy tab children.
    let (agy_tx, mut agy_rx) = mpsc::channel::<ServerMsg>(256);
    // Bring up every shared host actually needed (not just the configured
    // default). Grey-out only the backends whose ensure fails.
    for kind in App::backends_needed(&app.sessions) {
        if matches!(
            kind,
            BackendKind::Muse | BackendKind::Codex | BackendKind::Grok
        ) {
            if let Err(e) = ensure_host(&mut backends, kind, cfg).await {
                grey_out(app, kind, &e);
            }
        }
    }
    for tab in 0..app.sessions.len() {
        let kind = app.sessions[tab].backend;
        // Host already greyed out: skip so we don't clobber the ensure error.
        if matches!(
            kind,
            BackendKind::Muse | BackendKind::Codex | BackendKind::Grok
        ) && backends.get(kind).is_none()
        {
            continue;
        }
        open_tab_session(&mut backends, app, tab, cfg, &agy_tx).await;
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
                        MouseEventKind::Down(_) => {
                            if app.mouse && app.sel_begin(m.column, m.row) {
                                dirty = true;
                            }
                        }
                        MouseEventKind::Drag(_) => {
                            if app.mouse && app.sel.is_some() {
                                app.sel_extend(m.column, m.row);
                                dirty = true;
                            }
                        }
                        MouseEventKind::Up(_) => {
                            if app.mouse && app.sel.is_some() {
                                // take_selected_text always dehighlights
                                // (copy path and empty click alike).
                                if let Some(text) = app.take_selected_text() {
                                    app.flash = copy_to_clipboard(&text);
                                }
                                dirty = true;
                            }
                        }
                        _ => {}
                    },
                    _ => {}
                }
                apply_mouse_capture(app)?;
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
                    let mut bell = handle_server_msg(app, BackendKind::Agy, msg);
                    while let Ok(m) = agy_rx.try_recv() {
                        bell |= handle_server_msg(app, BackendKind::Agy, m);
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
                // Mock streaming only; an idle TUI paints nothing.
                if app.sessions.iter().any(|s| s.backend == BackendKind::Mock && s.busy)
                {
                    if let Some(tab) = app.tick() {
                        ring_bell();
                        app.flash = format!("{tab} done ✓");
                    }
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
    // In-flight spawned grok prompts hold the last Arcs: try_unwrap then
    // misses, and the final drop kills the child (kill_on_drop) after
    // stdin EOF already asked it to exit. No headless leak either way.
    if let Some(ctx) = backends.muse {
        if let Ok(host) = std::sync::Arc::try_unwrap(ctx.host) {
            host.shutdown().await;
        }
    }
    if let Some(ctx) = backends.codex {
        if let Ok(host) = std::sync::Arc::try_unwrap(ctx.host) {
            host.shutdown().await;
        }
    }
    if let Some(ctx) = backends.grok {
        if let Ok(host) = std::sync::Arc::try_unwrap(ctx.host) {
            host.shutdown().await;
        }
    }
    for slot in backends.agy.iter_mut() {
        if let Some(h) = slot.take() {
            h.shutdown().await;
        }
    }
    Ok(())
}

/// Await one server frame, or park forever when the backend is down.
async fn backend_msg(slot: &mut Option<LiveBackend>) -> Option<ServerMsg> {
    match slot {
        Some(ctx) => ctx.rx.recv().await,
        None => std::future::pending().await,
    }
}

/// Apply one frame plus any burst queued behind it. Returns bell.
fn backend_frame(
    app: &mut App,
    kind: BackendKind,
    slot: &mut Option<LiveBackend>,
    first: Option<ServerMsg>,
) -> bool {
    let Some(msg) = first else { return false };
    let mut bell = false;
    let mut next = Some(msg);
    // Drain the whole burst: one frame per batch, not per delta.
    loop {
        let m = match next.take() {
            Some(m) => m,
            None => match slot.as_mut().and_then(|ctx| ctx.rx.try_recv().ok()) {
                Some(m) => m,
                None => break,
            },
        };
        bell |= handle_server_msg(app, kind, m);
    }
    bell
}

fn handle_server_msg(app: &mut App, kind: BackendKind, msg: ServerMsg) -> bool {
    let tag = kind.label();
    match msg {
        ServerMsg::Notif { method, params } => {
            let tab = match (kind, method.as_str()) {
                // Prefer conversation_id match (resume), else spawn-order
                // FIFO, else the sole waiting agy tab.
                (BackendKind::Agy, "agy/init") => tab_for_session(app, kind, &params)
                    .or_else(|| {
                        while let Some(t) = app.agy_init_fifo.pop_front() {
                            if app.sessions.get(t).is_some_and(|s| {
                                s.backend == BackendKind::Agy && s.pending_agy_init
                            }) {
                                return Some(t);
                            }
                        }
                        None
                    })
                    .or_else(|| {
                        let waiting: Vec<usize> = app
                            .sessions
                            .iter()
                            .enumerate()
                            .filter(|(_, s)| s.backend == BackendKind::Agy && s.remote_id.is_none())
                            .map(|(i, _)| i)
                            .collect();
                        (waiting.len() == 1).then_some(waiting[0])
                    })
                    .unwrap_or(app.active),
                (BackendKind::Grok, _) => match tab_for_session(app, kind, &params) {
                    Some(tab) => tab,
                    None => return false,
                },
                // S2-F1: muse/codex frames naming an unknown (or no)
                // session are dropped, never attributed to the active
                // tab — a wrong-tab approval modal is worse than a
                // dropped transcript line.
                (BackendKind::Muse | BackendKind::Codex, _) => {
                    match tab_for_session(app, kind, &params) {
                        Some(tab) => tab,
                        None => return false,
                    }
                }
                _ => tab_for_session(app, kind, &params).unwrap_or(app.active),
            };
            match kind {
                BackendKind::Muse => provider::apply_notif(app, tab, &method, &params),
                BackendKind::Codex => apply_codex_notif(app, tab, &method, &params),
                BackendKind::Grok => apply_grok_notif(app, tab, &method, &params),
                BackendKind::Agy => apply_agy_notif(app, tab, &method, &params),
                BackendKind::Mock => false,
            }
        }
        ServerMsg::Request { id, method, params } => match kind {
            BackendKind::Codex => {
                match tab_for_session(app, kind, &params) {
                    Some(tab) => apply_codex_approval(app, tab, &method, id, &params),
                    // S2-F1: an orphan approval must never bind its
                    // modal to the active tab. Fail closed: queue a
                    // host-level deny (no session needed to send it)
                    // and say so on the active tab, if any survives.
                    None => {
                        app.outbox.decides.push(OutboxDecide {
                            tab: app.active,
                            backend: kind,
                            approval_id: method.clone(),
                            requirement_id: id.clone(),
                            choice_id: "denied".to_string(),
                            feedback: None,
                        });
                        let line =
                            format!("codex: denied orphan request {method} (unknown session)");
                        if let Some(s) = app.sessions.get_mut(app.active) {
                            s.push_line(line.clone());
                        }
                        app.flash = line;
                        false
                    }
                }
            }
            BackendKind::Grok => match tab_for_session(app, kind, &params) {
                Some(tab) => apply_grok_permission(app, tab, &method, id, &params),
                None => {
                    grok::queue_grok_cancelled(app, app.active, id);
                    app.active_mut()
                        .push_line(format!("grok: cancelled orphan request {method}"));
                    app.flash = format!("grok: cancelled orphan request {method}");
                    false
                }
            },
            _ => {
                app.flash = format!("{tag}: unexpected server request {method}");
                false
            }
        },
        ServerMsg::Transport(e) => {
            app.flash = format!("{tag}: {e}");
            false
        }
    }
}

/// Route a frame to the tab whose remote session it names (muse:
/// `sessionId`; codex: `threadId`, falling back to `conversationId`).
fn tab_for_session(app: &App, kind: BackendKind, params: &serde_json::Value) -> Option<usize> {
    let keys: &[&str] = match kind {
        BackendKind::Muse | BackendKind::Grok => &["sessionId"],
        BackendKind::Codex => &["threadId", "conversationId"],
        BackendKind::Agy => &["conversation_id"],
        BackendKind::Mock => return None,
    };
    let sid = keys.iter().filter_map(|k| params.get(k)?.as_str()).next()?;
    app.sessions
        .iter()
        .position(|s| s.backend == kind && s.remote_id.as_deref() == Some(sid))
}

/// Wheel = 3 lines, Shift+wheel = page (M1 spec, unchanged).
fn wheel_step(m: &crossterm::event::MouseEvent, app: &App) -> i32 {
    if m.modifiers.contains(KeyModifiers::SHIFT) {
        app.viewport_height as i32
    } else {
        3
    }
}

/// Perform queued provider work: respawns, turn submits, decisions.
/// Returns true when anything was sent or recorded (caller repaints).
async fn drain_outbox(
    app: &mut App,
    backends: &mut Backends,
    cfg: &Config,
    agy_tx: &mpsc::Sender<ServerMsg>,
) -> bool {
    let mut did = false;
    while let Some(r) = app.outbox.respawns.pop() {
        did = true;
        if r.backend == BackendKind::Agy {
            open_tab_session(backends, app, r.tab, cfg, agy_tx).await;
            continue;
        }
        if r.backend != BackendKind::Mock {
            if let Err(e) = ensure_host(backends, r.backend, cfg).await {
                grey_out(app, r.backend, &e);
                continue;
            }
        }
        open_tab_session(backends, app, r.tab, cfg, agy_tx).await;
    }
    while let Some(sub) = app.outbox.submits.pop() {
        did = true;
        let tag = sub.backend.label();
        // Grok prompts answer only at turn end: spawn, completion
        // re-enters as grok/prompt_completed (never await in the drain).
        if sub.backend == BackendKind::Grok {
            let sid = app.sessions[sub.tab].remote_id.clone();
            match (backends.get(sub.backend), backends.grok_tx.clone(), sid) {
                (Some(ctx), Some(tx), Some(id)) => {
                    grok_submit(ctx.host.clone(), tx, id, sub.prompt);
                }
                _ => {
                    let s = &mut app.sessions[sub.tab];
                    s.busy = false;
                    s.push_line(format!(
                        "{tag}: no session for this tab — press P to respawn"
                    ));
                }
            }
            continue;
        }
        if sub.backend == BackendKind::Agy {
            let handle = backends.agy.get(sub.tab).and_then(|o| o.as_ref());
            match handle {
                Some(h) => {
                    if let Err(e) = agy_submit(h, sub.prompt).await {
                        let s = &mut app.sessions[sub.tab];
                        s.busy = false;
                        s.push_line(format!("{tag}: submit failed: {e}"));
                    }
                }
                None => {
                    let s = &mut app.sessions[sub.tab];
                    s.busy = false;
                    s.push_line(format!(
                        "{tag}: no session for this tab — press P to respawn"
                    ));
                }
            }
            continue;
        }
        let sid = app.sessions[sub.tab].remote_id.clone();
        let host = backends.get(sub.backend).map(|c| &c.host);
        match (host, sid) {
            (Some(host), Some(id)) => {
                let res = match sub.backend {
                    BackendKind::Muse => muse_submit(host, &id, &sub.prompt)
                        .await
                        .map_err(|e| e.to_string()),
                    BackendKind::Codex => codex::codex_submit(host, &id, &sub.prompt)
                        .await
                        .map_err(|e| e.to_string()),
                    // Grok exits via the spawned early-continue above.
                    BackendKind::Mock | BackendKind::Agy | BackendKind::Grok => Ok(()),
                };
                if let Err(e) = res {
                    let s = &mut app.sessions[sub.tab];
                    s.busy = false;
                    s.push_line(format!("{tag}: turn/start failed: {e}"));
                }
            }
            _ => {
                let s = &mut app.sessions[sub.tab];
                s.busy = false;
                s.push_line(format!(
                    "{tag}: no session for this tab — press P to respawn"
                ));
            }
        }
    }
    while let Some(d) = app.outbox.decides.pop() {
        did = true;
        let tag = d.backend.label();
        // Grok replies are host-level JSON-RPC responses, including orphan
        // requests. They must not depend on a surviving tab/session.
        if d.backend == BackendKind::Grok {
            if let Some(host) = backends.get(d.backend).map(|c| &c.host) {
                let payload = serde_json::from_str(&d.choice_id)
                    .unwrap_or(serde_json::json!({"outcome": {"outcome": "cancelled"}}));
                if let Err(e) = grok_respond(host, d.requirement_id, payload).await {
                    app.flash = format!("grok: decide failed: {e}");
                }
            } else {
                app.flash = "grok: no host for response".into();
            }
            continue;
        }
        // S2-F1: codex answers are host-level JSON-RPC responses keyed
        // by request id (the session id is unused). Like grok, they
        // must send even when no tab/session survives — orphan
        // approvals are denied fail-closed at route time.
        if d.backend == BackendKind::Codex {
            if let Some(host) = backends.get(d.backend).map(|c| &c.host) {
                if let Err(e) = codex_respond(
                    host,
                    d.requirement_id.clone(),
                    serde_json::json!({"decision": d.choice_id}),
                )
                .await
                {
                    app.flash = format!("codex: decide failed: {e}");
                }
            } else {
                app.flash = "codex: no host for response".into();
            }
            continue;
        }
        let sid = app.sessions[d.tab].remote_id.clone();
        let host = backends.get(d.backend).map(|c| &c.host);
        match (host, sid) {
            (Some(host), Some(id)) => {
                let res = match d.backend {
                    BackendKind::Muse => {
                        muse_decide(host, &id, &d).await.map_err(|e| e.to_string())
                    }
                    BackendKind::Codex | BackendKind::Grok => unreachable!("handled above"),
                    BackendKind::Mock | BackendKind::Agy => Ok(()),
                };
                if let Err(e) = res {
                    let s = &mut app.sessions[d.tab];
                    s.push_line(format!("{tag}: decide failed: {e} (card may reappear)"));
                    app.flash = format!("{tag}: decide failed: {e}");
                }
            }
            _ => {
                app.flash = format!("{tag}: no session for this tab");
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

/// Copy entry point: grok-parity legs (native + tmux + OSC 52, the last
/// capped at `clipboard::MAX_OSC52_RAW_BYTES`) with a backup file unless
/// `POLYFORGE_CLIPBOARD_NO_BACKUP` is set. Returns the status flash naming
/// where the text landed.
fn copy_to_clipboard(text: &str) -> String {
    clipboard::copy_text_or_file(text).toast_message()
}

fn clear_pending_g(app: &mut App) {
    app.pending_g = false;
}

/// Queue a UI-side approval decision: transcript lines now, wire decide via
/// the outbox (live backends) or the immediate mock close-out.
fn decide_ui(app: &mut App, kind: DecisionKind, label: &str) {
    let tab = app.active;
    let backend = app.sessions[tab].backend;
    match backend {
        BackendKind::Mock => {
            if app.decide_diff(label) {
                ring_bell();
            }
        }
        BackendKind::Muse => {
            let approval = app.active().pending_approval.clone();
            match approval {
                Some(a) => match map_decision(&a, kind) {
                    Some((choice_id, feedback)) => {
                        app.outbox.decides.push(OutboxDecide {
                            tab,
                            backend,
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
                        // S2-F2: a negative decision must always deny on
                        // the wire. With no deny choice, closing the card
                        // would fake a denial the server never received
                        // (it may treat silence as consent), so keep the
                        // card open and say so loudly. Positive decisions
                        // still close locally: nothing is approved
                        // server-side, so closing is fail-closed.
                        if matches!(kind, DecisionKind::Reject | DecisionKind::Later) {
                            app.flash = format!(
                                "muse: cannot send `{label}` — server offered no deny path (card kept open; nothing denied)"
                            );
                            ring_bell();
                        } else {
                            app.muse_approved(label);
                            app.flash = format!(
                                "muse: closed locally — server offered no way to send `{label}`"
                            );
                            ring_bell();
                        }
                    }
                },
                None => {
                    if app.muse_approved(label) {
                        ring_bell();
                    }
                }
            }
        }
        BackendKind::Codex => {
            let approval = app.active().pending_approval.clone();
            match approval {
                Some(a) => match map_codex_decision(&a, kind) {
                    Some((req_id, decision)) => {
                        app.outbox.decides.push(OutboxDecide {
                            tab,
                            backend,
                            approval_id: a.approval_id,
                            requirement_id: req_id,
                            choice_id: decision,
                            feedback: None,
                        });
                        if app.approved("codex", label) {
                            ring_bell();
                        }
                    }
                    None => {
                        app.approved("codex", label);
                        app.flash = format!(
                            "codex: closed locally — server offered no way to send `{label}`"
                        );
                        ring_bell();
                    }
                },
                None => {
                    if app.approved("codex", label) {
                        ring_bell();
                    }
                }
            }
        }
        BackendKind::Grok => {
            let approval = app.active().pending_approval.clone();
            match approval {
                Some(a) => match map_grok_decision(&a, kind) {
                    Some((req_id, payload)) => {
                        app.outbox.decides.push(OutboxDecide {
                            tab,
                            backend,
                            approval_id: a.approval_id,
                            requirement_id: req_id,
                            choice_id: payload.to_string(),
                            feedback: None,
                        });
                        if app.approved("grok", label) {
                            ring_bell();
                        }
                    }
                    None => {
                        grok::queue_grok_cancelled(app, tab, a.requirement_id);
                        app.approved("grok", "cancelled");
                        app.flash = format!(
                            "grok: cancelled — server offered no matching option for `{label}`"
                        );
                        ring_bell();
                    }
                },
                None => {
                    if app.approved("grok", label) {
                        ring_bell();
                    }
                }
            }
        }
        BackendKind::Agy => {
            // Agy has no interactive approvals (vendor policy decides); the
            // modal can never appear, so any key here just ensures closure.
            app.flash =
                "agy: approvals aren't interactive — vendor policy decides (see transcript)"
                    .to_string();
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
            KeyCode::Char('j') if no_mods && app.vim => {
                clear_pending_g(app);
                app.scroll_lines(1);
            }
            KeyCode::Char('k') if no_mods && app.vim => {
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
            KeyCode::Char('g') if no_mods && app.vim => {
                app.scroll_top();
                clear_pending_g(app);
            }
            KeyCode::Char('G') if no_mods && app.vim => {
                app.scroll_bottom();
                clear_pending_g(app);
            }
            // Universal (both keymaps): full-size keys for the same moves.
            KeyCode::PageDown if no_mods => {
                clear_pending_g(app);
                let h = app.half_page();
                app.scroll_lines(h);
            }
            KeyCode::PageUp if no_mods => {
                clear_pending_g(app);
                let h = app.half_page();
                app.scroll_lines(-h);
            }
            KeyCode::Home if no_mods => {
                clear_pending_g(app);
                app.scroll_top();
            }
            KeyCode::End if no_mods => {
                clear_pending_g(app);
                app.scroll_bottom();
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
            KeyCode::Char('i') | KeyCode::Char('a') if no_mods && app.vim => {
                clear_pending_g(app);
                app.mode = Mode::Insert;
            }
            // Normal keymap: Enter types (vim users keep i/a).
            KeyCode::Enter if !app.vim => {
                clear_pending_g(app);
                app.mode = Mode::Insert;
            }
            // Space types in BOTH keymaps (dx shortcut: no Enter needed).
            KeyCode::Char(' ') => {
                clear_pending_g(app);
                app.mode = Mode::Insert;
            }
            KeyCode::Char('m') if no_mods => {
                clear_pending_g(app);
                app.mouse = !app.mouse;
                app.flash = format!("mouse {}", if app.mouse { "on" } else { "off" });
            }
            KeyCode::Char('P') if no_mods => {
                clear_pending_g(app);
                // Preselect the tab's current backend in the picker.
                let cur = app.sessions[app.active].backend;
                app.picker_sel = BackendKind::ALL
                    .iter()
                    .position(|(b, _)| *b == cur)
                    .unwrap_or(0);
                app.mode = Mode::Picker;
            }
            KeyCode::Char('R') if no_mods => {
                clear_pending_g(app);
                // Same-backend fresh respawn (the restore banner's promise).
                let cur = app.sessions[app.active].backend;
                app.respawn_active(cur);
            }
            KeyCode::Char(c)
                if no_mods && c >= '1' && (c as usize - '1' as usize) < app.sessions.len() =>
            {
                clear_pending_g(app);
                app.active = c as usize - '1' as usize;
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
            KeyCode::Esc => {
                app.mode = Mode::Normal;
                app.cmd_sel = 0;
            }
            // Ctrl-[ sends the same bytes as Esc on most terminals; belt & braces.
            KeyCode::Char('[') if ctrl => {
                app.mode = Mode::Normal;
                app.cmd_sel = 0;
            }
            KeyCode::Enter => app.submit(),
            // Slash-command suggestions (input starts with `/`):
            // Up/Down moves the highlight, Tab accepts it.
            KeyCode::Tab if no_mods => {
                app.accept_slash_completion();
            }
            KeyCode::Up if !app.slash_matches().is_empty() => {
                app.cycle_cmd_sel(-1);
            }
            KeyCode::Down if !app.slash_matches().is_empty() => {
                app.cycle_cmd_sel(1);
            }
            KeyCode::Backspace => {
                let s = app.active_mut();
                if s.cursor > 0 {
                    s.cursor -= 1;
                    let bi = byte_index(&s.input, s.cursor);
                    s.input.remove(bi);
                }
                app.cmd_sel = 0;
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
                app.cmd_sel = 0;
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
        Mode::Picker => match code {
            KeyCode::Esc => app.mode = Mode::Normal,
            KeyCode::Char('[') if ctrl => app.mode = Mode::Normal,
            KeyCode::Char('j') | KeyCode::Down if no_mods => {
                app.picker_sel = (app.picker_sel + 1) % BackendKind::ALL.len();
            }
            KeyCode::Char('k') | KeyCode::Up if no_mods => {
                app.picker_sel =
                    (app.picker_sel + BackendKind::ALL.len() - 1) % BackendKind::ALL.len();
            }
            KeyCode::Enter => {
                let (backend, _) = BackendKind::ALL[app.picker_sel];
                app.respawn_active(backend);
            }
            KeyCode::Char(c) if no_mods && ['1', '2', '3', '4', '5'].contains(&c) => {
                let i = (c as usize - '1' as usize).min(BackendKind::ALL.len() - 1);
                app.respawn_active(BackendKind::ALL[i].0);
            }
            _ => {}
        },
        Mode::Sessions => match code {
            KeyCode::Esc => app.mode = Mode::Normal,
            KeyCode::Char('[') if ctrl => app.mode = Mode::Normal,
            KeyCode::Char('j') | KeyCode::Down if no_mods => {
                if !app.sess_list.is_empty() {
                    app.sess_sel = (app.sess_sel + 1) % app.sess_list.len();
                }
            }
            KeyCode::Char('k') | KeyCode::Up if no_mods => {
                if !app.sess_list.is_empty() {
                    app.sess_sel = (app.sess_sel + app.sess_list.len() - 1) % app.sess_list.len();
                }
            }
            KeyCode::Enter => app.choose_session(app.sess_sel),
            KeyCode::Char('d') if no_mods => app.delete_selected_session(),
            KeyCode::Char(c) if no_mods && c >= '1' && c <= '9' => {
                let i = c as usize - '1' as usize;
                if i < app.sess_list.len() {
                    app.choose_session(i);
                }
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

    #[cfg(unix)]
    #[tokio::test]
    async fn grok_cancelled_requests_reach_host_without_remote_session() {
        let (tx, rx) = mpsc::channel(4);
        let host = msp::Host::spawn(
            "/bin/sh",
            &[
                "-c",
                r#"
            while read -r response; do
                printf '{"method":"observed","params":%s}\n' "$response"
            done
        "#,
            ],
            &[],
            tx,
        )
        .await
        .unwrap();
        let mut backends = Backends {
            grok: Some(LiveBackend {
                host: std::sync::Arc::new(host),
                rx,
            }),
            ..Default::default()
        };
        let mut app = App::new();
        app.active_mut().backend = BackendKind::Grok;
        app.active_mut().remote_id = Some("known".into());
        let (agy_tx, _agy_rx) = mpsc::channel(1);
        for (id, method, sid) in [
            (1, "x.ai/ask_user_question", "known"),
            (2, "session/request_permission", "orphan"),
        ] {
            handle_server_msg(
                &mut app,
                BackendKind::Grok,
                ServerMsg::Request {
                    id: serde_json::json!(id),
                    method: method.into(),
                    params: serde_json::json!({"sessionId":sid}),
                },
            );
        }
        assert!(
            app.active()
                .lines
                .iter()
                .any(|l| l.contains("question, not an approval"))
        );
        // A tab can disappear before drain; host-level responses still apply.
        app.sessions.clear();
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            assert!(drain_outbox(&mut app, &mut backends, &Config::default(), &agy_tx).await);
            for expected_id in [2, 1] {
                match backends.grok.as_mut().unwrap().rx.recv().await.unwrap() {
                    ServerMsg::Notif { params, .. } => {
                        assert_eq!(params["id"], expected_id);
                        assert_eq!(params["result"]["outcome"]["outcome"], "cancelled");
                    }
                    other => panic!("unexpected: {other:?}"),
                }
            }
        })
        .await
        .expect("cancelled response did not reach host");
    }

    #[test]
    fn grok_unmatched_frames_cannot_change_active_approval_or_busy_state() {
        let mut app = muse_modal_app();
        app.active_mut().busy = true;
        for sid in [serde_json::json!("orphan"), serde_json::Value::Null] {
            for method in ["session/request_permission", "x.ai/ask_user_question"] {
                handle_server_msg(
                    &mut app,
                    BackendKind::Grok,
                    ServerMsg::Request {
                        id: serde_json::json!(17),
                        method: method.into(),
                        params: serde_json::json!({"sessionId": sid}),
                    },
                );
                let reply = app.outbox.decides.pop().unwrap();
                assert_eq!(reply.backend, BackendKind::Grok);
                assert_eq!(reply.requirement_id, 17);
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(&reply.choice_id).unwrap()["outcome"]
                        ["outcome"],
                    "cancelled"
                );
                assert!(app.flash.contains("orphan"));
            }
            for method in ["grok/prompt_completed", "session/update"] {
                assert!(!handle_server_msg(
                    &mut app,
                    BackendKind::Grok,
                    ServerMsg::Notif {
                        method: method.into(),
                        params: serde_json::json!({"sessionId": sid, "stopReason":"end_turn"}),
                    }
                ));
            }
            assert!(app.active().busy);
            assert_eq!(
                app.active().pending_approval.as_ref().unwrap().approval_id,
                "a1"
            );
            assert!(app.active().pending_diff.is_some());
        }
    }

    #[test]
    fn grok_unmappable_decision_queues_cancelled_and_closes_card() {
        let mut app = muse_modal_app();
        app.active_mut().backend = BackendKind::Grok;
        app.active_mut()
            .pending_approval
            .as_mut()
            .unwrap()
            .requirement_id = serde_json::json!(42);
        decide_ui(&mut app, DecisionKind::ApproveAll, "approved-all");
        assert!(app.active().pending_approval.is_none());
        assert!(app.active().pending_diff.is_none());
        assert!(app.flash.contains("no matching option"));
        assert_eq!(app.outbox.decides.len(), 1);
        assert_eq!(app.outbox.decides[0].requirement_id, 42);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&app.outbox.decides[0].choice_id).unwrap()["outcome"]
                ["outcome"],
            "cancelled"
        );
    }

    const NONE: KeyModifiers = KeyModifiers::empty();
    const SHIFT: KeyModifiers = KeyModifiers::SHIFT;

    fn muse_modal_app() -> App {
        let mut app = App::new();
        app.active_mut().backend = BackendKind::Muse;
        app.active_mut().stage_diff(PendingDiff {
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

    /// S2-F2: with no deny choice offered, q/Esc must NOT close the
    /// card — a silent close fakes a denial the server never received.
    /// The card stays open with a loud flash; y still escapes via the
    /// offered approve choice, so the user is never trapped.
    #[test]
    fn q_keeps_modal_open_without_deny_choice() {
        let mut app = muse_modal_app();
        handle_key(&mut app, KeyCode::Char('q'), NONE);
        assert!(
            app.active().pending_diff.is_some(),
            "q silently closed a deny-less card"
        );
        assert!(app.outbox.decides.is_empty(), "q must not send a decision");
        assert!(
            app.flash.contains("no deny path"),
            "flash must name the missing deny path"
        );
        let mut app = muse_modal_app();
        handle_key(&mut app, KeyCode::Esc, NONE);
        assert!(
            app.active().pending_diff.is_some(),
            "Esc silently closed a deny-less card"
        );
        assert!(
            app.outbox.decides.is_empty(),
            "Esc must not send a decision"
        );
        // The offered approve path still closes (fail-closed: y queues
        // the allow choice; n/q never fabricate a denial).
        let mut app = muse_modal_app();
        handle_key(&mut app, KeyCode::Char('n'), NONE);
        assert!(
            app.active().pending_diff.is_some(),
            "n silently closed a deny-less card"
        );
        assert!(app.outbox.decides.is_empty(), "n must not send a decision");
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

    /// ApproveAll with only once-approved offered: local-close, no wire send.
    #[test]
    fn approve_all_without_session_choice_closes_locally() {
        let mut app = muse_modal_app();
        handle_key(&mut app, KeyCode::Char('a'), NONE);
        assert!(app.active().pending_diff.is_none());
        assert!(app.outbox.decides.is_empty());
        assert!(!app.flash.is_empty());
    }

    /// Codex q/Later queues wire `denied` when choices exist.
    #[test]
    fn codex_later_queues_denied() {
        let mut app = App::new();
        app.active_mut().backend = BackendKind::Codex;
        app.active_mut().stage_diff(PendingDiff {
            file: "applyPatch".into(),
            body: "body".into(),
        });
        app.active_mut().pending_approval = Some(PendingApproval {
            approval_id: "applyPatchApproval".into(),
            requirement_id: serde_json::json!(7),
            choices: vec![
                ApprovalChoice {
                    choice_id: "approved".into(),
                    decision: "approved".into(),
                    scope: "once".into(),
                    label: "Allow".into(),
                    accepts_feedback: false,
                },
                ApprovalChoice {
                    choice_id: "approved_for_session".into(),
                    decision: "approved_for_session".into(),
                    scope: "once".into(),
                    label: "Allow for session".into(),
                    accepts_feedback: false,
                },
                ApprovalChoice {
                    choice_id: "denied".into(),
                    decision: "denied".into(),
                    scope: "once".into(),
                    label: "Deny".into(),
                    accepts_feedback: false,
                },
            ],
        });
        handle_key(&mut app, KeyCode::Char('q'), NONE);
        assert!(app.active().pending_diff.is_none());
        assert_eq!(app.outbox.decides.len(), 1);
        assert_eq!(app.outbox.decides[0].choice_id, "denied");
    }

    /// S2-F1: muse/codex notifs naming an unknown (or no) session are
    /// dropped, never attributed to the active tab. The active tab's
    /// modal, transcript, and busy state stay untouched; a frame naming
    /// the live session still routes.
    #[test]
    fn orphan_notifs_cannot_touch_active_tab() {
        for backend in [BackendKind::Muse, BackendKind::Codex] {
            let mut app = App::new();
            app.active_mut().backend = backend;
            app.active_mut().remote_id = Some("sess-1".into());
            app.active_mut().busy = true;
            app.active_mut().stage_diff(PendingDiff {
                file: "tool".into(),
                body: "body".into(),
            });
            app.active_mut().pending_approval = Some(PendingApproval {
                approval_id: "a1".into(),
                requirement_id: serde_json::Value::Null,
                choices: vec![],
            });
            let before = app.active().lines.len();
            for params in [
                serde_json::json!({"sessionId": "orphan", "threadId": "orphan"}),
                serde_json::json!({}),
            ] {
                assert!(!handle_server_msg(
                    &mut app,
                    backend,
                    ServerMsg::Notif {
                        method: "item/delta".into(),
                        params: params.clone(),
                    }
                ));
                assert!(!handle_server_msg(
                    &mut app,
                    backend,
                    ServerMsg::Notif {
                        method: "approval/requested".into(),
                        params: params.clone(),
                    }
                ));
            }
            assert!(app.active().busy);
            assert_eq!(
                app.active().pending_approval.as_ref().unwrap().approval_id,
                "a1"
            );
            assert!(app.active().pending_diff.is_some());
            assert_eq!(app.active().lines.len(), before);
            assert!(app.outbox.decides.is_empty());
        }
        // Control: a frame naming the live session still routes (muse).
        let mut app = App::new();
        app.active_mut().backend = BackendKind::Muse;
        app.active_mut().remote_id = Some("sess-1".into());
        assert!(handle_server_msg(
            &mut app,
            BackendKind::Muse,
            ServerMsg::Notif {
                method: "approval/requested".into(),
                params: serde_json::json!({
                    "sessionId": "sess-1",
                    "toolName": "write",
                    "availableChoices": [
                        {"choiceId": "c-deny", "decision": "denied", "scope": "once",
                         "label": "Deny", "acceptsFeedback": false},
                    ],
                }),
            }
        ));
        assert!(app.active().pending_diff.is_some());
    }

    /// S2-F1: an orphan codex approval request is denied fail-closed on
    /// the wire, never staged as a modal on the active tab. A request
    /// naming the live session still stages its modal.
    #[test]
    fn orphan_codex_request_denied_never_staged() {
        let mut app = App::new();
        app.active_mut().backend = BackendKind::Codex;
        app.active_mut().remote_id = Some("thread-1".into());
        for params in [
            serde_json::json!({"threadId": "nope"}),
            serde_json::json!({}),
        ] {
            assert!(!handle_server_msg(
                &mut app,
                BackendKind::Codex,
                ServerMsg::Request {
                    id: serde_json::json!(8),
                    method: "applyPatchApproval".into(),
                    params: params.clone(),
                }
            ));
        }
        assert!(app.active().pending_diff.is_none());
        assert!(app.active().pending_approval.is_none());
        assert_eq!(app.outbox.decides.len(), 2);
        for d in &app.outbox.decides {
            assert_eq!(d.backend, BackendKind::Codex);
            assert_eq!(d.requirement_id, 8);
            assert_eq!(d.choice_id, "denied");
        }
        assert!(app.flash.contains("orphan"));
        // Control: the live session's own request still stages a modal.
        assert!(handle_server_msg(
            &mut app,
            BackendKind::Codex,
            ServerMsg::Request {
                id: serde_json::json!(9),
                method: "applyPatchApproval".into(),
                params: serde_json::json!({"threadId": "thread-1"}),
            }
        ));
        assert!(app.active().pending_diff.is_some());
        assert!(app.active().pending_approval.is_some());
    }

    /// S2-F1: the queued orphan deny is a host-level response — it must
    /// reach the host even when the tab/session is gone by drain time.
    #[cfg(unix)]
    #[tokio::test]
    async fn codex_orphan_deny_reaches_host_without_session() {
        let (tx, rx) = mpsc::channel(4);
        let host = msp::Host::spawn(
            "/bin/sh",
            &[
                "-c",
                r#"
            while read -r response; do
                printf '{"method":"observed","params":%s}\n' "$response"
            done
        "#,
            ],
            &[],
            tx,
        )
        .await
        .unwrap();
        let mut backends = Backends {
            codex: Some(LiveBackend {
                host: std::sync::Arc::new(host),
                rx,
            }),
            ..Default::default()
        };
        let mut app = App::new();
        app.active_mut().backend = BackendKind::Codex;
        app.active_mut().remote_id = Some("thread-1".into());
        let (agy_tx, _agy_rx) = mpsc::channel(1);
        assert!(!handle_server_msg(
            &mut app,
            BackendKind::Codex,
            ServerMsg::Request {
                id: serde_json::json!(8),
                method: "applyPatchApproval".into(),
                params: serde_json::json!({"threadId": "nope"}),
            }
        ));
        assert!(app.active().pending_diff.is_none());
        // The tab can disappear before the drain; the deny still applies.
        app.sessions.clear();
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            assert!(drain_outbox(&mut app, &mut backends, &Config::default(), &agy_tx).await);
            match backends.codex.as_mut().unwrap().rx.recv().await.unwrap() {
                ServerMsg::Notif { params, .. } => {
                    assert_eq!(params["id"], 8);
                    assert_eq!(params["result"]["decision"], "denied");
                }
                other => panic!("unexpected: {other:?}"),
            }
        })
        .await
        .expect("denied response did not reach host");
    }

    /// P opens the picker; j/k move; Enter respawns the tab fresh.
    #[test]
    fn picker_respawns_tab_fresh() {
        let mut app = App::new();
        handle_key(&mut app, KeyCode::Char('P'), SHIFT);
        assert_eq!(app.mode, Mode::Picker);
        handle_key(&mut app, KeyCode::Char('j'), NONE);
        handle_key(&mut app, KeyCode::Char('j'), NONE);
        assert_eq!(app.picker_sel, 2); // mock -> muse -> codex
        handle_key(&mut app, KeyCode::Enter, NONE);
        assert_eq!(app.mode, Mode::Normal);
        let s = app.active();
        assert_eq!(s.backend, BackendKind::Codex);
        assert!(s.remote_id.is_none());
        assert_eq!(s.lines.len(), 1); // marker only: history never carries over
        assert!(s.lines[0].contains("fresh"));
        assert_eq!(app.outbox.respawns.len(), 1);
        assert_eq!(app.outbox.respawns[0].backend, BackendKind::Codex);
    }

    /// R respawns the active tab fresh under its current backend.
    #[test]
    fn r_respawns_active_tab_same_backend() {
        let mut app = App::new();
        app.active_mut().backend = BackendKind::Codex;
        app.active_mut().remote_id = Some("thread-1".into());
        app.active_mut().push_line("old history".to_string());
        handle_key(&mut app, KeyCode::Char('R'), SHIFT);
        assert_eq!(app.mode, Mode::Normal);
        let s = app.active();
        assert_eq!(s.backend, BackendKind::Codex);
        assert!(s.remote_id.is_none());
        assert_eq!(s.lines.len(), 1); // marker only: history never carries over
        assert!(s.lines[0].contains("fresh"));
        assert_eq!(app.outbox.respawns.len(), 1);
        assert_eq!(app.outbox.respawns[0].backend, BackendKind::Codex);
    }

    /// Picker Esc cancels without touching the tab.
    #[test]
    fn picker_esc_cancels() {
        let mut app = App::new();
        handle_key(&mut app, KeyCode::Char('P'), SHIFT);
        handle_key(&mut app, KeyCode::Esc, NONE);
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.active().backend, BackendKind::Mock);
        assert!(app.outbox.respawns.is_empty());
    }

    /// Agy submit queues unconditionally (no session id needed up front;
    /// the drain writes into the tab child's stdin).
    #[test]
    fn agy_submit_queues_without_session_id() {
        let mut app = App::new();
        app.active_mut().backend = BackendKind::Agy;
        app.active_mut().input = "hi".to_string();
        app.submit();
        assert_eq!(app.outbox.submits.len(), 1);
        assert_eq!(app.outbox.submits[0].backend, BackendKind::Agy);
        assert!(app.active().busy);
    }

    /// Sessions chooser: j/k move, out-of-range digits are a no-op,
    /// Esc cancels without touching the tab.
    #[test]
    fn sessions_keys_navigate_and_cancel() {
        fn stored(id: &str) -> crate::store::StoredSession {
            crate::store::StoredSession {
                id: id.to_string(),
                backend: "mock".to_string(),
                remote_id: None,
                created_at: 1000,
                updated_at: 1000,
                title: String::new(),
                preview: "preview".to_string(),
            }
        }
        let mut app = App::new();
        app.sess_list = vec![stored("a"), stored("b")];
        app.mode = Mode::Sessions;
        handle_key(&mut app, KeyCode::Char('j'), NONE);
        assert_eq!(app.sess_sel, 1);
        handle_key(&mut app, KeyCode::Char('k'), NONE);
        assert_eq!(app.sess_sel, 0);
        handle_key(&mut app, KeyCode::Char('9'), NONE);
        assert_eq!(app.mode, Mode::Sessions, "out-of-range digit is a no-op");
        assert!(app.outbox.respawns.is_empty());
        handle_key(&mut app, KeyCode::Esc, NONE);
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.outbox.respawns.is_empty());
    }

    /// Digit tabs follow the live tab count (boot = 1 tab).
    #[test]
    fn digit_tabs_are_dynamic() {
        let mut app = App::new();
        handle_key(&mut app, KeyCode::Char('2'), NONE);
        assert_eq!(app.active, 0, "no second tab yet: digit ignored");
        app.open_tab();
        handle_key(&mut app, KeyCode::Char('2'), NONE);
        assert_eq!(app.active, 1);
        handle_key(&mut app, KeyCode::Char('1'), NONE);
        assert_eq!(app.active, 0);
    }

    /// Picker reaches all five backends.
    #[test]
    fn picker_lists_five_backends() {
        let mut app = App::new();
        handle_key(&mut app, KeyCode::Char('P'), SHIFT);
        handle_key(&mut app, KeyCode::Char('4'), NONE);
        assert_eq!(app.active().backend, BackendKind::Agy);
        assert_eq!(app.outbox.respawns.len(), 1);
        handle_key(&mut app, KeyCode::Char('P'), SHIFT);
        handle_key(&mut app, KeyCode::Char('5'), NONE);
        assert_eq!(app.active().backend, BackendKind::Grok);
        // Same tab re-picked: the stale Agy respawn is dropped, one Grok
        // respawn queued.
        assert_eq!(app.outbox.respawns.len(), 1);
        assert_eq!(app.outbox.respawns[0].backend, BackendKind::Grok);
    }

    /// Submit on a grok tab queues a backend-tagged submit (the drain
    /// spawns session/prompt); submit on a dead tab reports, never hangs.
    #[test]
    fn submit_routes_grok_tab() {
        let mut app = App::new();
        app.active_mut().backend = BackendKind::Grok;
        app.active_mut().remote_id = Some("acp-sess-1".into());
        app.active_mut().input = "hi".to_string();
        app.submit();
        assert_eq!(app.outbox.submits.len(), 1);
        assert_eq!(app.outbox.submits[0].backend, BackendKind::Grok);
        assert!(app.active().busy);

        let mut dead = App::new();
        dead.active_mut().backend = BackendKind::Grok;
        dead.active_mut().input = "hi".to_string();
        dead.submit();
        assert!(dead.outbox.submits.is_empty());
        assert!(!dead.active().busy);
        assert!(dead.active().lines.iter().any(|l| l.contains("grok")));
    }

    /// Submit on a codex tab queues a backend-tagged submit (async drain
    /// sends turn/start); submit on a dead tab reports instead of hanging.
    #[test]
    fn submit_routes_per_tab_backend() {
        let mut app = App::new();
        app.active_mut().backend = BackendKind::Codex;
        app.active_mut().remote_id = Some("thread-1".into());
        app.active_mut().input = "hi".to_string();
        app.submit();
        assert_eq!(app.outbox.submits.len(), 1);
        assert_eq!(app.outbox.submits[0].backend, BackendKind::Codex);
        assert!(app.active().busy);

        let mut dead = App::new();
        dead.active_mut().backend = BackendKind::Codex;
        dead.active_mut().input = "hi".to_string();
        dead.submit();
        assert!(dead.outbox.submits.is_empty());
        assert!(!dead.active().busy);
        assert!(dead.active().lines.iter().any(|l| l.contains("codex")));
    }

    /// Scrollable app: production tabs open empty, so tests seed filler.
    fn scroll_app(vim: bool) -> App {
        let mut app = App::new();
        crate::mock::seed(&mut app.sessions[0]);
        app.vim = vim;
        app.viewport_height = 20;
        app.stick_to_bottom();
        app
    }

    /// Crossterm delivers `G` as Char('G')+SHIFT; it must jump to bottom.
    #[test]
    fn shift_g_goes_to_bottom() {
        let mut app = scroll_app(true);
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
        let mut app = scroll_app(true);
        let max = app.active().lines.len().saturating_sub(20);
        handle_key(&mut app, KeyCode::Char('k'), NONE);
        assert_eq!(app.active().scroll, max - 1);
        handle_key(&mut app, KeyCode::Char('n'), NONE); // no search: flash, no panic
        assert!(!app.flash.is_empty());
        handle_key(&mut app, KeyCode::Char('q'), NONE);
        assert!(app.should_quit);
    }

    /// Default keymap is NOT vim: j/k/g/G/i/a do nothing, Enter types.
    #[test]
    fn normal_keymap_ignores_vim_keys() {
        let mut app = App::new();
        assert!(!app.vim);
        app.viewport_height = 20;
        app.stick_to_bottom();
        let scroll = app.active().scroll;
        for c in ['j', 'k', 'g', 'G', 'i', 'a'] {
            handle_key(&mut app, KeyCode::Char(c), NONE);
        }
        assert_eq!(app.active().scroll, scroll);
        assert_eq!(app.mode, Mode::Normal);
        handle_key(&mut app, KeyCode::Enter, NONE);
        assert_eq!(app.mode, Mode::Insert, "Enter types in the normal keymap");
    }

    /// Space enters insert in BOTH keymaps (dx shortcut).
    #[test]
    fn space_types_in_both_keymaps() {
        for vim in [false, true] {
            let mut app = App::new();
            app.vim = vim;
            handle_key(&mut app, KeyCode::Char(' '), NONE);
            assert_eq!(app.mode, Mode::Insert, "vim={vim}: Space must type");
        }
    }

    /// Vim keymap: i/a type, Enter does NOT enter insert.
    #[test]
    fn vim_keymap_types_with_i_a_only() {
        let mut app = App::new();
        app.vim = true;
        handle_key(&mut app, KeyCode::Enter, NONE);
        assert_eq!(app.mode, Mode::Normal, "Enter must not type in vim mode");
        handle_key(&mut app, KeyCode::Char('i'), NONE);
        assert_eq!(app.mode, Mode::Insert);
    }

    /// A command typed in Normal mode lands in Search; a miss that looks
    /// like a command must redirect to Insert mode instead of silence.
    #[test]
    fn failed_search_matching_a_command_redirects_to_insert() {
        let mut app = App::new();
        app.mode = Mode::Search;
        app.search_input = "tab close".to_string();
        app.run_search();
        assert!(app.flash.contains("INSERT mode"), "flash: {}", app.flash);
        assert!(app.flash.contains("/tab close"), "flash: {}", app.flash);
        // Ordinary misses keep the plain message (no false redirect).
        app.search_input = "zzz-no-such-line".to_string();
        app.run_search();
        assert_eq!(app.flash, "no match: zzz-no-such-line");
    }

    /// Insert mode: typing `/` narrows suggestions, Up/Down moves the
    /// highlight, Tab accepts it, Enter still sends.
    #[test]
    fn slash_suggestions_complete_via_tab() {
        let mut app = App::new();
        app.mode = Mode::Insert;
        handle_key(&mut app, KeyCode::Char('/'), NONE);
        handle_key(&mut app, KeyCode::Char('t'), NONE);
        assert_eq!(app.active().input, "/t");
        assert_eq!(app.slash_matches().len(), 2);
        handle_key(&mut app, KeyCode::Down, NONE);
        assert_eq!(app.cmd_sel, 1);
        handle_key(&mut app, KeyCode::Up, NONE);
        assert_eq!(app.cmd_sel, 0);
        handle_key(&mut app, KeyCode::Tab, NONE);
        assert_eq!(app.active().input, "/tab new");
        assert_eq!(app.mode, Mode::Insert, "Tab completes, it does not send");
        // Plain text: Up/Down/Tab leave the input alone.
        app.active_mut().input = "hi".to_string();
        app.active_mut().cursor = 2;
        handle_key(&mut app, KeyCode::Up, NONE);
        handle_key(&mut app, KeyCode::Down, NONE);
        handle_key(&mut app, KeyCode::Tab, NONE);
        assert_eq!(app.active().input, "hi");
    }

    /// Home/End/PageUp/PageDown scroll in BOTH keymaps.
    #[test]
    fn fullsize_nav_keys_are_universal() {
        for vim in [false, true] {
            let mut app = scroll_app(vim);
            let max = app.active().lines.len().saturating_sub(20);
            assert!(max > 0, "precondition: scrollable seed");
            handle_key(&mut app, KeyCode::Home, NONE);
            assert_eq!(app.active().scroll, 0);
            handle_key(&mut app, KeyCode::End, NONE);
            assert_eq!(app.active().scroll, max);
            handle_key(&mut app, KeyCode::PageUp, NONE);
            assert!(app.active().scroll < max);
            handle_key(&mut app, KeyCode::PageDown, NONE);
            assert_eq!(app.active().scroll, max);
        }
    }

    #[test]
    fn risk_ready_applies_only_for_matching_token() {
        let mut app = muse_modal_app();
        let token = app.active().risk_gen;
        let j = typesafe::ApprovalJudgment::compose(2.0, 1.0, 0.97, 0.96);
        apply_risk_msg(
            &mut app,
            RiskMsg::Ready {
                tab: 0,
                token,
                judgment: j.clone(),
            },
        );
        assert_eq!(app.active().approval_risk.as_ref(), Some(&j));
        assert!(app.flash.contains("HIGH"));

        // Stale token after clear: ignored.
        let _ = app.active_mut().clear_diff();
        apply_risk_msg(
            &mut app,
            RiskMsg::Ready {
                tab: 0,
                token,
                judgment: j,
            },
        );
        assert!(app.active().approval_risk.is_none());
    }

    #[test]
    fn risk_failed_clears_spawned_marker() {
        let mut app = muse_modal_app();
        let token = app.active().risk_gen;
        app.active_mut().risk_spawned_gen = Some(token);
        apply_risk_msg(
            &mut app,
            RiskMsg::Failed {
                tab: 0,
                token,
                err: "boom".into(),
            },
        );
        assert!(app.active().approval_risk.is_none());
        assert!(app.active().risk_spawned_gen.is_none());
        assert!(app.flash.contains("boom"));
    }
}
