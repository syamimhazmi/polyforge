//! Outbox draining: performs the provider work the synchronous `App` queued
//! (respawns, submits, stops, decisions).

use crate::agy::{AgyFrame, agy_submit};
use crate::app::{App, BackendKind};
use crate::backend::{BRINGUP_TIMEOUT, Backends, muse_start_fresh, no_reply, open_tab_session};
use crate::claude::{claude_interrupt, claude_send, claude_submit};
use crate::codex;
use crate::codex::{codex_interrupt, codex_respond};
use crate::config::Config;
use crate::grok::{grok_cancel, grok_respond, grok_submit};
use crate::provider::{muse_decide, muse_interrupt, muse_submit};
use tokio::sync::mpsc;

/// Perform queued provider work: respawns, turn submits, decisions.
/// Returns true when anything was sent or recorded (caller repaints).
pub(crate) async fn drain_outbox(
    app: &mut App,
    backends: &mut Backends,
    cfg: &Config,
    agy_tx: &mpsc::Sender<AgyFrame>,
) -> bool {
    let mut did = false;
    // Kill replaced/closed claude children before respawns: a re-attach
    // of the same session id must not be killed after it spawns.
    for id in std::mem::take(&mut app.pending_claude_kill) {
        did = true;
        if let Some(h) = backends.claude.remove(&id) {
            h.shutdown().await;
        }
    }
    while let Some(r) = app.outbox.respawns.pop() {
        did = true;
        // A tab closed or switched since the request has nothing to open.
        if app
            .sessions
            .get(r.tab)
            .is_none_or(|s| s.backend != r.backend)
        {
            continue;
        }
        open_tab_session(backends, app, r.tab, cfg, agy_tx).await;
    }
    // Stops go out before submits: a stop must not queue behind new work.
    // Snapshots (ids taken at press time); nothing here awaits a reply.
    for st in std::mem::take(&mut app.outbox.stops) {
        did = true;
        let tag = st.backend.label();
        let unsent = format!("{tag}: stop not sent — press again to force");
        match st.backend {
            BackendKind::Mock => {}
            BackendKind::Muse | BackendKind::Codex | BackendKind::Grok => {
                let host = backends.get(st.backend).map(|c| c.host.clone());
                let (Some(host), Some(id)) = (host, st.remote_id) else {
                    app.flash = unsent;
                    continue;
                };
                match st.backend {
                    BackendKind::Muse => {
                        tokio::spawn(async move {
                            let _ = muse_interrupt(&host, &id, st.turn_id.as_deref()).await;
                        });
                    }
                    BackendKind::Codex => {
                        let Some(turn) = st.turn_id else {
                            app.flash = unsent;
                            continue;
                        };
                        tokio::spawn(async move {
                            let _ = codex_interrupt(&host, &id, &turn).await;
                        });
                    }
                    _ => {
                        tokio::spawn(async move { grok_cancel(&host, &id).await });
                    }
                }
            }
            BackendKind::Claude => {
                let handle = st.remote_id.as_ref().and_then(|id| backends.claude.get(id));
                if handle.map(claude_interrupt).is_none_or(|r| r.is_err()) {
                    app.flash = unsent;
                }
            }
            BackendKind::Agy => {
                let handle = backends.agy.get(st.tab).and_then(|o| o.as_ref());
                if !handle.is_some_and(|h| h.interrupt()) {
                    app.flash = unsent;
                }
            }
        }
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
        if sub.backend == BackendKind::Claude {
            let handle = app.sessions[sub.tab]
                .remote_id
                .as_ref()
                .and_then(|id| backends.claude.get(id));
            let err = match handle {
                Some(h) => claude_submit(h, sub.prompt).await.err(),
                None => Some("no session for this tab — press P to respawn".to_string()),
            };
            if let Some(e) = err {
                let s = &mut app.sessions[sub.tab];
                s.busy = false;
                s.push_line(format!("{tag}: {e}"));
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
        if app.sessions[sub.tab].session_deferred {
            let started = match backends.get(sub.backend) {
                Some(ctx) => {
                    tokio::time::timeout(BRINGUP_TIMEOUT, muse_start_fresh(&ctx.host, cfg))
                        .await
                        .unwrap_or_else(|_| Err(no_reply()))
                }
                None => Err("host not running — press P to respawn".to_string()),
            };
            let s = &mut app.sessions[sub.tab];
            s.session_deferred = false;
            match started {
                Ok(id) => {
                    s.remote_id = Some(id);
                    app.save_tab_meta(sub.tab);
                }
                Err(e) => {
                    s.busy = false;
                    s.tab_degraded = Some(e.clone());
                    s.push_line(format!("{tag}: {e}"));
                    app.flash = format!("{tag}: {e}");
                    continue;
                }
            }
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
                    BackendKind::Mock
                    | BackendKind::Agy
                    | BackendKind::Grok
                    | BackendKind::Claude => Ok(None),
                };
                match res {
                    Ok(turn_id) => app.sessions[sub.tab].turn_id = turn_id,
                    Err(e) => {
                        let s = &mut app.sessions[sub.tab];
                        s.busy = false;
                        s.push_line(format!("{tag}: turn/start failed: {e}"));
                    }
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
        // Claude answers go to the child named by the queued session id.
        if d.backend == BackendKind::Claude {
            let frame = serde_json::from_str(&d.choice_id).unwrap_or_default();
            let err = match backends.claude.get(&d.approval_id) {
                Some(h) => claude_send(h, frame).await.err(),
                None => Some("claude session ended".to_string()),
            };
            if let Some(e) = err {
                app.flash = format!("claude: decide failed: {e}");
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
                    BackendKind::Codex | BackendKind::Grok | BackendKind::Claude => {
                        unreachable!("handled above")
                    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app;
    use crate::backend::{Backends, LiveBackend};
    use crate::msp;
    use crate::msp::ServerMsg;
    use crate::server_msg::handle_server_msg;
    use crate::test_support::busy_tab;
    use std::time::Duration;

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

    /// Host that echoes every frame it receives back as a notification.
    #[cfg(unix)]
    async fn echo_backend() -> LiveBackend {
        let (tx, rx) = mpsc::channel(8);
        let host = msp::Host::spawn(
            "/bin/sh",
            &[
                "-c",
                r#"while read -r l; do printf '{"method":"observed","params":%s}\n' "$l"; done"#,
            ],
            &[],
            tx,
        )
        .await
        .unwrap();
        LiveBackend {
            host: std::sync::Arc::new(host),
            rx,
        }
    }

    /// Stops reach the right wire method per backend from the snapshot ids
    /// (codex: turn/interrupt with thread + turn; grok: session/cancel).
    #[cfg(unix)]
    #[tokio::test]
    async fn drain_sends_stop_frames_from_snapshot() {
        let (agy_tx, _agy_rx) = mpsc::channel(1);
        let mut backends = Backends {
            codex: Some(echo_backend().await),
            grok: Some(echo_backend().await),
            muse: Some(echo_backend().await),
            ..Default::default()
        };
        let mut app = App::new();
        app.outbox.stops.push(app::OutboxStop {
            tab: 0,
            backend: BackendKind::Codex,
            remote_id: Some("th-1".into()),
            turn_id: Some("tu-1".into()),
        });
        app.outbox.stops.push(app::OutboxStop {
            tab: 0,
            backend: BackendKind::Grok,
            remote_id: Some("acp-1".into()),
            turn_id: None,
        });
        app.outbox.stops.push(app::OutboxStop {
            tab: 0,
            backend: BackendKind::Muse,
            remote_id: Some("mu-1".into()),
            turn_id: None,
        });
        // Codex without a turn id cannot be interrupted: says so.
        app.outbox.stops.push(app::OutboxStop {
            tab: 0,
            backend: BackendKind::Codex,
            remote_id: Some("th-1".into()),
            turn_id: None,
        });
        let cfg = Config::default();
        tokio::time::timeout(Duration::from_secs(3), async {
            assert!(drain_outbox(&mut app, &mut backends, &cfg, &agy_tx).await);
            assert!(app.outbox.stops.is_empty());
            assert!(app.flash.contains("stop not sent"), "{}", app.flash);
            let next = async |slot: &mut Option<LiveBackend>| match slot
                .as_mut()
                .unwrap()
                .rx
                .recv()
                .await
                .unwrap()
            {
                ServerMsg::Notif { params, .. } => params,
                other => panic!("unexpected {other:?}"),
            };
            let p = next(&mut backends.codex).await;
            assert_eq!(p["method"], "turn/interrupt");
            assert_eq!(p["params"]["threadId"], "th-1");
            assert_eq!(p["params"]["turnId"], "tu-1");
            let p = next(&mut backends.grok).await;
            assert_eq!(p["method"], "session/cancel");
            assert_eq!(p["params"]["sessionId"], "acp-1");
            assert!(p.get("id").is_none(), "cancel is a notification");
            let p = next(&mut backends.muse).await;
            assert_eq!(p["method"], "turn/interrupt");
            assert_eq!(p["params"]["sessionId"], "mu-1");
            assert!(p["params"].get("turnId").is_none());
        })
        .await
        .expect("drain timed out");
    }

    /// The awaited turn/start result supplies the codex turn id.
    #[cfg(unix)]
    #[tokio::test]
    async fn codex_submit_stores_turn_id_from_result() {
        let (tx, rx) = mpsc::channel(4);
        let host = msp::Host::spawn(
            "/bin/sh",
            &[
                "-c",
                r#"read -r l; printf '{"jsonrpc":"2.0","id":1,"result":{"turn":{"id":"turn-7"}}}\n'; while read -r l; do :; done"#,
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
        let mut app = busy_tab(BackendKind::Codex);
        app.outbox.submits.push(app::OutboxSubmit {
            tab: 0,
            backend: BackendKind::Codex,
            prompt: "hi".into(),
        });
        let (agy_tx, _agy_rx) = mpsc::channel(1);
        tokio::time::timeout(Duration::from_secs(3), async {
            drain_outbox(&mut app, &mut backends, &Config::default(), &agy_tx).await;
        })
        .await
        .expect("drain timed out");
        assert_eq!(app.active().turn_id.as_deref(), Some("turn-7"));
        // Stop now snapshots that id.
        app.request_stop();
        assert_eq!(app.outbox.stops[0].turn_id.as_deref(), Some("turn-7"));
    }

    /// Fake muse host: replies to session/start (id 1), then view/subscribe
    /// (id 2) and turn/start (id 3).
    #[cfg(unix)]
    async fn muse_backend(start_reply: &str) -> LiveBackend {
        let (tx, rx) = mpsc::channel(4);
        let script = format!(
            "read -r l; echo '{start_reply}'; for id in 2 3; do read -r l; echo '{{\"jsonrpc\":\"2.0\",\"id\":'$id',\"result\":{{}}}}'; done; while read -r l; do :; done"
        );
        let host = msp::Host::spawn("/bin/sh", &["-c", &script], &[], tx)
            .await
            .unwrap();
        LiveBackend {
            host: std::sync::Arc::new(host),
            rx,
        }
    }

    /// A deferred muse tab with its first prompt submitted (queued in the outbox).
    fn deferred_muse_app() -> App {
        let mut app = App::new();
        let s = app.active_mut();
        s.backend = BackendKind::Muse;
        s.session_deferred = true;
        s.input = "hello".into();
        app.submit();
        assert_eq!(app.outbox.submits.len(), 1);
        app
    }

    /// The first submit starts the session, records its id, then sends the turn.
    #[cfg(unix)]
    #[tokio::test]
    async fn deferred_muse_session_starts_on_first_submit() {
        let mut backends = Backends {
            muse: Some(
                muse_backend(
                    r#"{"jsonrpc":"2.0","id":1,"result":{"session":{"sessionId":"mu-9"}}}"#,
                )
                .await,
            ),
            ..Default::default()
        };
        let mut app = deferred_muse_app();
        assert!(app.active().remote_id.is_none());
        let (agy_tx, _agy_rx) = mpsc::channel(1);
        tokio::time::timeout(Duration::from_secs(3), async {
            drain_outbox(&mut app, &mut backends, &Config::default(), &agy_tx).await;
        })
        .await
        .expect("drain timed out");
        let s = app.active();
        assert_eq!(s.remote_id.as_deref(), Some("mu-9"));
        assert!(!s.session_deferred);
        assert!(s.tab_degraded.is_none());
        assert!(s.busy);
    }

    /// A failed deferred start greys the tab out with the friendly error.
    #[cfg(unix)]
    #[tokio::test]
    async fn deferred_muse_start_failure_greys_out_tab() {
        let mut backends = Backends {
            muse: Some(
                muse_backend(
                    r#"{"jsonrpc":"2.0","id":1,"error":{"code":-1,"message":"no credentials"}}"#,
                )
                .await,
            ),
            ..Default::default()
        };
        let mut app = deferred_muse_app();
        let (agy_tx, _agy_rx) = mpsc::channel(1);
        tokio::time::timeout(Duration::from_secs(3), async {
            drain_outbox(&mut app, &mut backends, &Config::default(), &agy_tx).await;
        })
        .await
        .expect("drain timed out");
        let s = app.active();
        assert!(s.remote_id.is_none());
        assert!(!s.session_deferred);
        assert!(!s.busy);
        assert!(s.tab_degraded.as_deref().unwrap().contains("muse login"));
    }

    /// A failed deferred start sends nothing more: no `turn/start` reaches
    /// the host and the tab shows the start error only.
    #[cfg(unix)]
    #[tokio::test]
    async fn deferred_muse_start_failure_sends_no_turn() {
        let (tx, rx) = mpsc::channel(4);
        let script = r#"read -r l; echo '{"jsonrpc":"2.0","id":1,"error":{"code":-1,"message":"no credentials"}}'; while read -r l; do printf '{"method":"observed","params":%s}\n' "$l"; done"#;
        let host = msp::Host::spawn("/bin/sh", &["-c", script], &[], tx)
            .await
            .unwrap();
        let mut backends = Backends {
            muse: Some(LiveBackend {
                host: std::sync::Arc::new(host),
                rx,
            }),
            ..Default::default()
        };
        let mut app = deferred_muse_app();
        let before = app.active().lines.len();
        let (agy_tx, _agy_rx) = mpsc::channel(1);
        tokio::time::timeout(Duration::from_secs(3), async {
            drain_outbox(&mut app, &mut backends, &Config::default(), &agy_tx).await;
        })
        .await
        .expect("drain timed out");
        let rx = &mut backends.muse.as_mut().unwrap().rx;
        let sent = tokio::time::timeout(Duration::from_millis(300), rx.recv()).await;
        assert!(
            sent.is_err(),
            "host received a frame after the failed start: {sent:?}"
        );
        assert_eq!(
            app.active().lines.len(),
            before + 1,
            "{:?}",
            app.active().lines
        );
    }
}
