//! polyforge M2 — provider-backed TUI shell.
//! Backend `mock` (M1 behavior, quota-free) or `muse` (MSP `serve` host over
//! stdio: initialize → session/start → turn/start, deltas to transcript,
//! approval/requested to the diff modal, turn/completed to the bell).
//! Select via `~/.config/polyforge/config.toml` (`[polyforge] provider`).

mod agy;
mod app;
mod codex;
mod config;
mod mock;
mod msp;
mod provider;
mod store;
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

use agy::{agy_submit, apply_agy_notif, spawn_agy, AgyHandle};
use app::{App, BackendKind, DecisionKind, Mode, OutboxDecide};
use config::Config;
use msp::ServerMsg;
use codex::{
    apply_codex_approval, apply_codex_notif, codex_bringup, codex_respond,
    codex_resume_thread, codex_start_thread, map_codex_decision,
};
use provider::{
    map_decision, muse_bringup, muse_decide, muse_resume_session, muse_start_session,
    muse_submit,
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
    host: msp::Host,
    rx: mpsc::Receiver<ServerMsg>,
}

#[derive(Default)]
struct Backends {
    muse: Option<LiveBackend>,
    codex: Option<LiveBackend>,
    /// One agy child per tab (each holds its own conversation).
    agy: Vec<Option<AgyHandle>>,
}

impl Backends {
    fn get(&self, kind: BackendKind) -> Option<&LiveBackend> {
        match kind {
            BackendKind::Muse => self.muse.as_ref(),
            BackendKind::Codex => self.codex.as_ref(),
            BackendKind::Mock | BackendKind::Agy => None,
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
async fn ensure_host(backends: &mut Backends, kind: BackendKind, cfg: &Config) -> Result<(), String> {
    let present = match kind {
        BackendKind::Muse => backends.muse.is_some(),
        BackendKind::Codex => backends.codex.is_some(),
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
            backends.muse = Some(LiveBackend { host, rx });
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
            backends.codex = Some(LiveBackend { host, rx });
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
    match spawn_agy(&cfg.agy_bin(), &args, &cfg.workspace_root(), events_tx.clone()).await
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
                app.sessions[tab]
                    .push_line(format!("{tag}: continuing previous conversation"));
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
    // Q10 resume: restore each tab's backend + transcript from the store
    // before any bringup, so open_tab_session can re-attach the remote
    // session it records. Storageless falls back to the configured default.
    if let Some(store) = store::Store::open() {
        app.store = Some(store);
        for tab in 0..app.sessions.len() {
            app.restore_tab(tab, def);
        }
    } else {
        for s in &mut app.sessions {
            s.backend = def;
        }
    }

    let mut backends = Backends::default();
    // One shared channel for all agy tab children.
    let (agy_tx, mut agy_rx) = mpsc::channel::<ServerMsg>(256);
    // Bring up every Muse/Codex host actually needed after restore (not just
    // the configured default). Grey-out only the backends whose ensure fails.
    for kind in App::backends_needed(&app.sessions) {
        if matches!(kind, BackendKind::Muse | BackendKind::Codex) {
            if let Err(e) = ensure_host(&mut backends, kind, cfg).await {
                grey_out(app, kind, &e);
            }
        }
    }
    for tab in 0..app.sessions.len() {
        let kind = app.sessions[tab].backend;
        // Host already greyed out: skip so we don't clobber the ensure error.
        if matches!(kind, BackendKind::Muse | BackendKind::Codex)
            && backends.get(kind).is_none()
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
    if let Some(ctx) = backends.muse {
        ctx.host.shutdown().await;
    }
    if let Some(ctx) = backends.codex {
        ctx.host.shutdown().await;
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
                            if app
                                .sessions
                                .get(t)
                                .is_some_and(|s| s.backend == BackendKind::Agy && s.pending_agy_init)
                            {
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
                            .filter(|(_, s)| {
                                s.backend == BackendKind::Agy && s.remote_id.is_none()
                            })
                            .map(|(i, _)| i)
                            .collect();
                        (waiting.len() == 1).then_some(waiting[0])
                    })
                    .unwrap_or(app.active),
                _ => tab_for_session(app, kind, &params).unwrap_or(app.active),
            };
            match kind {
                BackendKind::Muse => provider::apply_notif(app, tab, &method, &params),
                BackendKind::Codex => apply_codex_notif(app, tab, &method, &params),
                BackendKind::Agy => apply_agy_notif(app, tab, &method, &params),
                BackendKind::Mock => false,
            }
        }
        ServerMsg::Request { id, method, params } => match kind {
            BackendKind::Codex => {
                let tab = tab_for_session(app, kind, &params).unwrap_or(app.active);
                apply_codex_approval(app, tab, &method, id, &params)
            }
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
fn tab_for_session(
    app: &App,
    kind: BackendKind,
    params: &serde_json::Value,
) -> Option<usize> {
    let keys: &[&str] = match kind {
        BackendKind::Muse => &["sessionId"],
        BackendKind::Codex => &["threadId", "conversationId"],
        BackendKind::Agy => &["conversation_id"],
        BackendKind::Mock => return None,
    };
    let sid = keys.iter().filter_map(|k| params.get(k)?.as_str()).next()?;
    app.sessions.iter().position(|s| {
        s.backend == kind && s.remote_id.as_deref() == Some(sid)
    })
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
                    s.push_line(format!("{tag}: no session for this tab — press P to respawn"));
                }
            }
            continue;
        }
        let sid = app.sessions[sub.tab].remote_id.clone();
        let host = backends.get(sub.backend).map(|c| &c.host);
        match (host, sid) {
            (Some(host), Some(id)) => {
                let res = match sub.backend {
                    BackendKind::Muse => {
                        muse_submit(host, &id, &sub.prompt).await.map_err(|e| e.to_string())
                    }
                    BackendKind::Codex => {
                        codex::codex_submit(host, &id, &sub.prompt)
                            .await
                            .map_err(|e| e.to_string())
                    }
                    BackendKind::Mock | BackendKind::Agy => Ok(()),
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
                s.push_line(format!("{tag}: no session for this tab — press P to respawn"));
            }
        }
    }
    while let Some(d) = app.outbox.decides.pop() {
        did = true;
        let tag = d.backend.label();
        let sid = app.sessions[d.tab].remote_id.clone();
        let host = backends.get(d.backend).map(|c| &c.host);
        match (host, sid) {
            (Some(host), Some(id)) => {
                let res = match d.backend {
                    BackendKind::Muse => {
                        muse_decide(host, &id, &d).await.map_err(|e| e.to_string())
                    }
                    BackendKind::Codex => codex_respond(
                        host,
                        d.requirement_id.clone(),
                        serde_json::json!({"decision": d.choice_id}),
                    )
                    .await
                    .map_err(|e| e.to_string()),
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
            KeyCode::Char(c) if no_mods && ['1', '2', '3', '4'].contains(&c) => {
                let i = (c as usize - '1' as usize).min(BackendKind::ALL.len() - 1);
                app.respawn_active(BackendKind::ALL[i].0);
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
        app.active_mut().backend = BackendKind::Muse;
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
        app.active_mut().pending_diff = Some(PendingDiff {
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

    /// Picker reaches all four backends.
    #[test]
    fn picker_lists_four_backends() {
        let mut app = App::new();
        handle_key(&mut app, KeyCode::Char('P'), SHIFT);
        handle_key(&mut app, KeyCode::Char('4'), NONE);
        assert_eq!(app.active().backend, BackendKind::Agy);
        assert_eq!(app.outbox.respawns.len(), 1);
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
