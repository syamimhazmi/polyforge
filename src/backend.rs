//! Backend lifecycle: the live host handles ([`Backends`]) and the off-loop
//! bring-up, remote session open and grey-out paths that fill them.

use crate::agy::{AGY_NO_MODAL_WARNING, AgyFrame, AgyHandle, push_agy_open_notice, spawn_agy};
use crate::app::{App, BackendKind};
use crate::claude::{ClaudeChildren, spawn_claude};
use crate::codex::{codex_bringup, codex_resume_thread, codex_start_thread};
use crate::config::Config;
use crate::grok::{grok_bringup, grok_new_session, grok_resume_session};
use crate::msp;
use crate::msp::ServerMsg;
use crate::provider::{muse_bringup, muse_resume_session, muse_start_session};
use crate::server_msg::claude_id_open_elsewhere;
use std::time::Duration;
use tokio::sync::mpsc;

pub(crate) struct LiveBackend {
    // Shared: grok's blocking session/prompt runs spawned per submit and
    // holds a clone until the turn ends (deref coercion keeps &Host call
    // sites unchanged).
    pub(crate) host: std::sync::Arc<msp::Host>,
    pub(crate) rx: mpsc::Receiver<ServerMsg>,
}

#[derive(Default)]
pub(crate) struct Backends {
    pub(crate) muse: Option<LiveBackend>,
    pub(crate) codex: Option<LiveBackend>,
    pub(crate) grok: Option<LiveBackend>,
    /// Completion sender for spawned grok prompts (session/prompt answers
    /// only at turn end, so submits can't await it in the drain loop).
    pub(crate) grok_tx: Option<mpsc::Sender<ServerMsg>>,
    /// One agy child per tab (each holds its own conversation).
    pub(crate) agy: Vec<Option<AgyHandle>>,
    /// One claude child per tab, keyed by session id.
    pub(crate) claude: ClaudeChildren,
    /// Shared event sender for claude children (set at boot).
    pub(crate) claude_tx: Option<mpsc::Sender<ServerMsg>>,
    /// Next spawn id. Default 0; each `spawn_claude` takes the next value.
    pub(crate) claude_generation: u64,
    /// In-flight host/session bring-ups, run off the event loop. Dropping
    /// the set aborts them, which drops (and so kills) any half-started host.
    pub(crate) bringups: tokio::task::JoinSet<BringDone>,
    /// Hosts with a bring-up in flight (one per kind).
    pub(crate) starting: Vec<BackendKind>,
    /// Last `Session::connecting` token handed out.
    pub(crate) next_token: u64,
}

impl Backends {
    pub(crate) fn get(&self, kind: BackendKind) -> Option<&LiveBackend> {
        match kind {
            BackendKind::Muse => self.muse.as_ref(),
            BackendKind::Codex => self.codex.as_ref(),
            BackendKind::Grok => self.grok.as_ref(),
            BackendKind::Mock | BackendKind::Agy | BackendKind::Claude => None,
        }
    }
}

/// Mark every tab of `kind` degraded with the exact fix (spec Q9 grey-out).
pub(crate) fn grey_out(app: &mut App, kind: BackendKind, reason: &str) {
    let tag = kind.label();
    for s in &mut app.sessions {
        if s.backend == kind {
            s.connecting = None;
            s.tab_degraded = Some(reason.to_string());
            s.push_line(format!("{tag}: {reason}"));
            s.drop_held_prompt();
        }
    }
    app.flash = format!("{tag}: {reason}");
}

/// Upper bound on one backend bring-up (spawn + handshake, or opening a
/// tab's session). Bring-ups run in tasks, so a server that never answers
/// only greys out its tabs; the UI stays live.
pub(crate) const BRINGUP_TIMEOUT: Duration = Duration::from_secs(30);

/// Tab status while its backend comes up off the event loop.
const CONNECTING: &str = "connecting…";

pub(crate) fn no_reply() -> String {
    format!(
        "no reply in {}s — press P to retry",
        BRINGUP_TIMEOUT.as_secs()
    )
}

/// A started host plus its event channel (and grok's completion sender).
pub(crate) struct HostUp {
    host: msp::Host,
    rx: mpsc::Receiver<ServerMsg>,
    grok_tx: Option<mpsc::Sender<ServerMsg>>,
}

/// A session/thread opened on a live host, with the transcript notes the
/// open produced (resume fallback, model override). `id: None` = a fresh
/// muse tab whose session starts on the first submit.
pub(crate) struct Opened {
    id: Option<String>,
    resumed: bool,
    notes: Vec<String>,
}

/// Result of a bring-up task, applied by the event loop.
pub(crate) enum BringDone {
    Host {
        kind: BackendKind,
        res: Result<HostUp, String>,
    },
    Session {
        token: u64,
        prev_id: Option<String>,
        res: Result<Opened, String>,
    },
}

/// Start the serve/app-server host for `kind`. Runs in a task: dropping
/// the future (timeout, abort, quit) drops the half-started host, which
/// kills its child.
async fn bring_host(kind: BackendKind, cfg: &Config) -> Result<HostUp, String> {
    let (tx, rx) = mpsc::channel(256);
    match kind {
        BackendKind::Muse => {
            let host = muse_bringup(&cfg.muse_bin(), vec![], tx).await?;
            Ok(HostUp {
                host,
                rx,
                grok_tx: None,
            })
        }
        BackendKind::Codex => {
            let host = codex_bringup(&cfg.codex_bin(), vec![], tx).await?;
            Ok(HostUp {
                host,
                rx,
                grok_tx: None,
            })
        }
        BackendKind::Grok => {
            let (host, prompt_tx) =
                grok_bringup(&cfg.grok_bin(), env!("CARGO_PKG_VERSION"), vec![], tx).await?;
            // Retained for spawned session/prompt completions (see drain).
            Ok(HostUp {
                host,
                rx,
                grok_tx: Some(prompt_tx),
            })
        }
        // Agy/claude children are per-tab (see open_session).
        BackendKind::Agy | BackendKind::Claude | BackendKind::Mock => {
            Err("no shared host".to_string())
        }
    }
}

/// Open the tab's session on a live host: resume `resume` if given, falling
/// back to a fresh one.
async fn remote_open(
    host: &msp::Host,
    kind: BackendKind,
    cfg: &Config,
    resume: Option<String>,
) -> Result<Opened, String> {
    let workspace = cfg.workspace_root();
    // A fresh id, plus a model-override note (grok, non-fatal).
    let fresh = async || -> Result<(String, Option<String>), String> {
        match kind {
            BackendKind::Muse => muse_start_session(
                host,
                cfg.muse_provider_id(),
                cfg.muse_cfg.model.clone(),
                workspace.clone(),
            )
            .await
            .map(|id| (id, None)),
            BackendKind::Codex => codex_start_thread(host, cfg.codex_model(), workspace.clone())
                .await
                .map(|id| (id, None)),
            _ => grok_new_session(host, cfg.grok_model(), workspace.clone()).await,
        }
    };
    let Some(old) = resume else {
        // A fresh muse tab starts its session on the first submit, so idle
        // launches leave no empty sessions behind.
        if kind == BackendKind::Muse {
            return Ok(Opened {
                id: None,
                resumed: false,
                notes: vec![],
            });
        }
        let (id, note) = fresh().await?;
        return Ok(Opened {
            id: Some(id),
            resumed: false,
            notes: note.into_iter().collect(),
        });
    };
    let resumed = match kind {
        BackendKind::Muse => muse_resume_session(host, &old).await,
        BackendKind::Codex => codex_resume_thread(host, &old).await,
        _ => grok_resume_session(host, &old, workspace.clone()).await,
    };
    match resumed {
        Ok(id) => Ok(Opened {
            id: Some(id),
            resumed: true,
            notes: vec![],
        }),
        Err(rerr) => {
            let (id, note) = fresh().await?;
            let mut notes = vec![format!(
                "{}: resume failed ({rerr}) — started fresh",
                kind.label()
            )];
            notes.extend(note);
            Ok(Opened {
                id: Some(id),
                resumed: false,
                notes,
            })
        }
    }
}

/// Open the tab's session in a task, if its host is up.
fn spawn_open(
    backends: &mut Backends,
    kind: BackendKind,
    token: u64,
    resume: Option<String>,
    cfg: &Config,
) {
    let Some(ctx) = backends.get(kind) else {
        return;
    };
    let (host, cfg) = (ctx.host.clone(), cfg.clone());
    backends.bringups.spawn(async move {
        let res = tokio::time::timeout(
            BRINGUP_TIMEOUT,
            remote_open(&host, kind, &cfg, resume.clone()),
        )
        .await
        .unwrap_or_else(|_| Err(no_reply()));
        BringDone::Session {
            token,
            prev_id: resume,
            res,
        }
    });
}

/// Start a muse/codex/grok tab's bring-up without blocking the event loop:
/// the tab shows "connecting…" until [`finish_bringup`] lands the result.
/// The host starts first (once per kind) if it isn't up yet.
fn begin_remote_open(
    backends: &mut Backends,
    app: &mut App,
    tab: usize,
    cfg: &Config,
    resume: Option<String>,
) {
    let kind = app.sessions[tab].backend;
    backends.next_token += 1;
    let token = backends.next_token;
    let tag = kind.label();
    let s = &mut app.sessions[tab];
    s.connecting = Some((token, resume.clone()));
    s.tab_degraded = Some(CONNECTING.to_string());
    app.flash = format!("{tag}: {CONNECTING}");
    if backends.get(kind).is_some() {
        spawn_open(backends, kind, token, resume, cfg);
    } else if !backends.starting.contains(&kind) {
        backends.starting.push(kind);
        let cfg = cfg.clone();
        backends.bringups.spawn(async move {
            let res = tokio::time::timeout(BRINGUP_TIMEOUT, bring_host(kind, &cfg))
                .await
                .unwrap_or_else(|_| Err(no_reply()));
            BringDone::Host { kind, res }
        });
    }
}

/// Apply a finished bring-up task on the event loop.
pub(crate) fn finish_bringup(
    app: &mut App,
    backends: &mut Backends,
    cfg: &Config,
    done: BringDone,
) {
    match done {
        BringDone::Host { kind, res } => {
            backends.starting.retain(|k| *k != kind);
            let up = match res {
                Ok(up) => up,
                Err(e) => return grey_out(app, kind, &e),
            };
            let live = LiveBackend {
                host: std::sync::Arc::new(up.host),
                rx: up.rx,
            };
            match kind {
                BackendKind::Muse => backends.muse = Some(live),
                BackendKind::Codex => backends.codex = Some(live),
                _ => {
                    backends.grok = Some(live);
                    backends.grok_tx = up.grok_tx;
                }
            }
            // Every connecting tab of this kind was waiting on the host.
            let waiting: Vec<_> = app
                .sessions
                .iter()
                .filter(|s| s.backend == kind)
                .filter_map(|s| s.connecting.clone())
                .collect();
            for (token, resume) in waiting {
                spawn_open(backends, kind, token, resume, cfg);
            }
        }
        BringDone::Session {
            token,
            prev_id,
            res,
        } => {
            // The tab may have been closed or re-opened meanwhile.
            let Some(tab) = app
                .sessions
                .iter()
                .position(|s| s.connecting.as_ref().is_some_and(|c| c.0 == token))
            else {
                return;
            };
            let tag = app.sessions[tab].backend.label();
            let s = &mut app.sessions[tab];
            s.connecting = None;
            s.tab_degraded = None;
            match res {
                Ok(o) => {
                    for n in o.notes {
                        s.push_line(n);
                    }
                    match o.id {
                        None => s.session_deferred = true,
                        Some(id) => {
                            if prev_id.as_deref() != Some(id.as_str()) {
                                s.tokens = None;
                            }
                            s.remote_id = Some(id);
                            if o.resumed {
                                s.push_line(format!("{tag}: resumed previous session"));
                            }
                            app.save_tab_meta(tab);
                        }
                    }
                    // The drain (same loop pass) sends what was typed early.
                    let s = &mut app.sessions[tab];
                    if let Some(prompt) = s.held_prompt.take() {
                        let backend = s.backend;
                        app.outbox.submits.push(crate::app::OutboxSubmit {
                            tab,
                            backend,
                            prompt,
                        });
                    }
                }
                Err(reason) => {
                    s.tab_degraded = Some(reason.clone());
                    s.push_line(format!("{tag}: {reason}"));
                    s.drop_held_prompt();
                    app.flash = format!("{tag}: {reason}");
                }
            }
        }
    }
}

/// Spawn (or replace) the agy child for one tab. The conversation id
/// arrives later via the init event.
async fn ensure_agy_tab(
    backends: &mut Backends,
    tab: usize,
    cfg: &Config,
    events_tx: &mpsc::Sender<AgyFrame>,
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

/// [`open_session`] under [`BRINGUP_TIMEOUT`]; a hung server greys out
/// just this tab.
pub(crate) async fn open_tab_session(
    backends: &mut Backends,
    app: &mut App,
    tab: usize,
    cfg: &Config,
    agy_tx: &mpsc::Sender<AgyFrame>,
) {
    let opened = tokio::time::timeout(
        BRINGUP_TIMEOUT,
        open_session(backends, app, tab, cfg, agy_tx),
    )
    .await;
    if opened.is_err() {
        let tag = app.sessions[tab].backend.label();
        let reason = no_reply();
        let s = &mut app.sessions[tab];
        s.tab_degraded = Some(reason.clone());
        s.push_line(format!("{tag}: {reason}"));
        app.flash = format!("{tag}: {reason}");
    }
}

/// Start a fresh muse session with the configured provider and model.
pub(crate) async fn muse_start_fresh(host: &msp::Host, cfg: &Config) -> Result<String, String> {
    muse_start_session(
        host,
        cfg.muse_provider_id(),
        cfg.muse_cfg.model.clone(),
        cfg.workspace_root(),
    )
    .await
}

/// Open (or re-open) the remote session for one tab. Mock tabs need nothing.
/// A stored remote id resumes the previous session (Q10); a failed resume
/// falls back to a fresh session and says so. Failures grey out just this
/// tab with the exact fix.
async fn open_session(
    backends: &mut Backends,
    app: &mut App,
    tab: usize,
    cfg: &Config,
    agy_tx: &mpsc::Sender<AgyFrame>,
) {
    let (backend, workspace) = (app.sessions[tab].backend, cfg.workspace_root());
    // Resume candidate from the store; cleared so a stale id never lingers.
    let resume = app.sessions[tab].remote_id.clone();
    let prev_id = resume.clone();
    let s = &mut app.sessions[tab];
    s.remote_id = None;
    s.session_deferred = false;
    s.reset_turn_state();
    // Context size belongs to the session: only a resume of the same id
    // keeps it.
    if resume.is_none() {
        s.tokens = None;
    }
    s.tab_degraded = None;
    s.connecting = None;
    // A respawn (P, picker) may switch backend: never forward early text.
    s.drop_held_prompt();
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
        if prev_id.as_deref() != Some(id.as_str()) {
            s.tokens = None;
        }
        s.remote_id = Some(id);
        if resumed {
            s.push_line(format!("{tag}: resumed previous session"));
        }
        app.save_tab_meta(tab);
    };
    // Any agy child on this tab belongs to the old session: kill it, also
    // when the tab switched to another backend (it would run until close).
    if let Some(old) = backends.agy.get_mut(tab).and_then(Option::take) {
        old.shutdown().await;
    }
    match backend {
        BackendKind::Mock => app.save_tab_meta(tab),
        // Handshake and session RPCs run off the event loop; the result
        // lands via `finish_bringup`.
        BackendKind::Muse | BackendKind::Codex | BackendKind::Grok => {
            begin_remote_open(backends, app, tab, cfg, resume);
        }
        BackendKind::Agy => {
            // A fresh child per switch (the old one is killed above).
            // A stored conversation id is passed through for continuation.
            // Keep the resume id; `agy/init` confirms (or replaces) it.
            if let Some(ref id) = resume {
                app.sessions[tab].remote_id = Some(id.clone());
            }
            let had_resume = resume.is_some();
            match ensure_agy_tab(backends, tab, cfg, agy_tx, resume).await {
                Ok(()) => {
                    // S7-F1: the child is live, so the no-modal warning
                    // fires here — fresh or resumed, every open.
                    push_agy_open_notice(&mut app.sessions[tab], had_resume);
                    app.flash = AGY_NO_MODAL_WARNING.to_string();
                }
                Err(e) => {
                    app.sessions[tab].remote_id = None;
                    fail(app, &e);
                }
            }
        }
        BackendKind::Claude => {
            // Resume keeps the stored id; fresh picks one up front so
            // every frame routes by session_id from the first line.
            // Another tab already holding this resume id must keep its
            // child: removing the map entry would kill that tab.
            let resumed = resume.is_some();
            if let Some(ref resume_id) = resume
                && claude_id_open_elsewhere(app, tab, resume_id)
            {
                fail(app, "session is open in another tab — /tab close it first");
                return;
            }
            let id = resume.unwrap_or_else(msp::uuid7);
            if let Some(old) = backends.claude.remove(&id) {
                old.shutdown().await;
            }
            let Some(tx) = backends.claude_tx.clone() else {
                fail(app, "claude event channel not ready");
                return;
            };
            backends.claude_generation = backends.claude_generation.saturating_add(1);
            let generation = backends.claude_generation;
            match spawn_claude(
                &cfg.claude_bin(),
                &cfg.claude_args(),
                &workspace,
                &id,
                resumed,
                generation,
                tx,
            )
            .await
            {
                Ok(h) => {
                    backends.claude.insert(id.clone(), h);
                    opened(app, id, resumed);
                }
                Err(e) => fail(app, &format!("could not spawn `claude`: {e}")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app;
    use crate::drain::drain_outbox;

    /// A muse tab still connecting at boot: a prompt typed meanwhile is
    /// held, then queued for the drain once the session opens.
    #[test]
    fn prompt_typed_while_connecting_sends_on_open() {
        let (mut app, mut backends) = (App::new(), Backends::default());
        let s = app.active_mut();
        s.backend = BackendKind::Muse;
        s.connecting = Some((7, None));
        s.input = "hi".to_string();
        app.submit();
        assert!(app.outbox.submits.is_empty(), "held, not sent yet");
        assert!(app.active().busy);
        let done = BringDone::Session {
            token: 7,
            prev_id: None,
            res: Ok(Opened {
                id: None,
                resumed: false,
                notes: vec![],
            }),
        };
        finish_bringup(&mut app, &mut backends, &Config::default(), done);
        assert_eq!(app.outbox.submits.len(), 1);
        assert_eq!(app.outbox.submits[0].prompt, "hi");
        assert!(app.active().session_deferred && app.active().held_prompt.is_none());
    }

    /// A failed bring-up drops the held prompt and frees the tab.
    #[test]
    fn failed_open_drops_held_prompt() {
        let (mut app, mut backends) = (App::new(), Backends::default());
        let s = app.active_mut();
        s.backend = BackendKind::Codex;
        s.connecting = Some((3, None));
        s.input = "hi".to_string();
        app.submit();
        let done = BringDone::Session {
            token: 3,
            prev_id: None,
            res: Err("boom".to_string()),
        };
        finish_bringup(&mut app, &mut backends, &Config::default(), done);
        let s = app.active();
        assert!(app.outbox.submits.is_empty() && !s.busy && s.held_prompt.is_none());
        assert!(s.lines.iter().any(|l| l.contains("prompt not sent")));
    }

    /// A tab leaving agy kills its old child on the next open instead of
    /// leaving it running until the tab returns to agy or closes.
    #[cfg(unix)]
    #[tokio::test]
    async fn switching_away_from_agy_kills_the_old_child() {
        let (tx, _rx) = mpsc::channel(16);
        let sleep = ["-c".to_string(), "sleep 30".to_string()];
        let child = crate::agy::spawn_agy("/bin/sh", &sleep, "/tmp", tx.clone())
            .await
            .unwrap();
        let pid = child.pid().unwrap().to_string();
        let mut backends = Backends {
            agy: vec![Some(child)],
            ..Default::default()
        };
        let mut app = App::new();
        app.active_mut().backend = BackendKind::Mock;
        open_tab_session(&mut backends, &mut app, 0, &Config::default(), &tx).await;
        assert!(backends.agy[0].is_none());
        let alive = std::process::Command::new("kill")
            .args(["-0", &pid])
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(!alive, "old agy child {pid} still running");
    }

    /// Bring-up runs in a task: drain returns at once with the tab
    /// "connecting…", and aborting it (quit) kills the half-started host.
    #[cfg(unix)]
    #[tokio::test]
    async fn bringup_runs_off_loop_and_abort_kills_host() {
        let dir = std::env::temp_dir().join(format!("polyforge-bringup-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (bin, pidfile) = (dir.join("muse"), dir.join("pid"));
        std::fs::write(
            &bin,
            format!(
                "#!/bin/sh\necho $$ > {}\nexec sleep 600\n",
                pidfile.display()
            ),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut cfg = Config::default();
        cfg.muse_cfg.bin = Some(bin.display().to_string());
        let mut app = App::new();
        app.active_mut().backend = BackendKind::Muse;
        app.outbox.respawns.push(app::OutboxRespawn {
            tab: 0,
            backend: BackendKind::Muse,
        });
        let mut backends = Backends::default();
        let (agy_tx, _agy_rx) = mpsc::channel(1);
        tokio::time::timeout(
            Duration::from_secs(1),
            drain_outbox(&mut app, &mut backends, &cfg, &agy_tx),
        )
        .await
        .expect("drain blocked on bring-up");
        assert_eq!(backends.starting, vec![BackendKind::Muse]);
        assert!(app.active().connecting.is_some());
        assert_eq!(app.active().tab_degraded.as_deref(), Some(CONNECTING));
        // The fake server is running and never answers the handshake.
        let pid = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Ok(t) = std::fs::read_to_string(&pidfile)
                    && let Ok(pid) = t.trim().parse::<u32>()
                {
                    break pid;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("fake server never started");
        backends.bringups.shutdown().await;
        let gone = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let out = std::process::Command::new("ps")
                    .args(["-o", "stat=", "-p", &pid.to_string()])
                    .output()
                    .unwrap();
                let stat = String::from_utf8_lossy(&out.stdout);
                // Gone, or a zombie awaiting reap.
                if stat.trim().is_empty() || stat.trim().starts_with('Z') {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        let _ = std::fs::remove_dir_all(&dir);
        assert!(gone.is_ok(), "host child leaked after abort");
    }

    #[test]
    fn finish_bringup_applies_matching_token_only() {
        let mut app = App::new();
        app.active_mut().backend = BackendKind::Muse;
        app.active_mut().connecting = Some((7, None));
        app.active_mut().tab_degraded = Some(CONNECTING.into());
        let mut backends = Backends::default();
        let cfg = Config::default();
        let opened = |id: &str| Opened {
            id: Some(id.into()),
            resumed: false,
            notes: vec![],
        };
        // A superseded bring-up changes nothing.
        let stale = BringDone::Session {
            token: 6,
            prev_id: None,
            res: Ok(opened("old")),
        };
        finish_bringup(&mut app, &mut backends, &cfg, stale);
        assert!(app.active().remote_id.is_none());
        assert!(app.active().connecting.is_some());
        let done = BringDone::Session {
            token: 7,
            prev_id: None,
            res: Ok(opened("sid")),
        };
        finish_bringup(&mut app, &mut backends, &cfg, done);
        let s = app.active();
        assert_eq!(s.remote_id.as_deref(), Some("sid"));
        assert!(s.connecting.is_none() && s.tab_degraded.is_none());
        // A failed bring-up greys out the tab with the reason.
        app.active_mut().connecting = Some((8, None));
        let failed = BringDone::Session {
            token: 8,
            prev_id: None,
            res: Err(no_reply()),
        };
        finish_bringup(&mut app, &mut backends, &cfg, failed);
        assert_eq!(
            app.active().tab_degraded.as_deref(),
            Some(no_reply().as_str())
        );
        // A fresh muse tab defers its session to the first submit.
        app.active_mut().connecting = Some((9, None));
        let deferred = BringDone::Session {
            token: 9,
            prev_id: None,
            res: Ok(Opened {
                id: None,
                resumed: false,
                notes: vec![],
            }),
        };
        finish_bringup(&mut app, &mut backends, &cfg, deferred);
        let s = app.active();
        assert!(s.session_deferred && s.connecting.is_none() && s.tab_degraded.is_none());
    }
}
