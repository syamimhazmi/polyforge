//! Backend to app message handling: receiving frames, per-provider
//! notification/request routing and the dead-backend path.

use crate::agy::{AgyFrame, apply_agy_notif};
use crate::app::{App, BackendKind, DecisionKind, OutboxDecide};
use crate::backend::{Backends, LiveBackend, grey_out};
use crate::claude::{apply_claude_notif, claude_frame_matches};
use crate::codex::{apply_codex_approval, apply_codex_notif};
use crate::grok::{apply_grok_notif, apply_grok_permission};
use crate::input::decide_ui;
use crate::msp::ServerMsg;
use crate::{grok, provider};

/// Await one server frame, or park forever when the backend is down.
pub(crate) async fn backend_msg(slot: &mut Option<LiveBackend>) -> Option<ServerMsg> {
    match slot {
        Some(ctx) => ctx.rx.recv().await,
        None => std::future::pending().await,
    }
}

/// Apply one frame plus any burst queued behind it. Returns bell.
pub(crate) fn backend_frame(
    app: &mut App,
    kind: BackendKind,
    slot: &mut Option<LiveBackend>,
    first: Option<ServerMsg>,
) -> bool {
    let Some(msg) = first else {
        // Channel closed: the server exited. Without this the select arm
        // resolves instantly forever (100% CPU).
        backend_down(app, kind, slot);
        return false;
    };
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
    // Grok keeps a sender alive (grok_tx), so its channel never closes:
    // the host's own flag is what says the server is gone.
    if slot.as_ref().is_some_and(|ctx| ctx.host.is_closed()) {
        backend_down(app, kind, slot);
    }
    bell
}

/// Drop a dead host and grey out its tabs; P respawns (and resumes).
fn backend_down(app: &mut App, kind: BackendKind, slot: &mut Option<LiveBackend>) {
    // Why it died: the server's last stderr lines (empty if it said nothing).
    let why = slot.as_ref().map_or(vec![], |ctx| ctx.host.stderr_tail(3));
    *slot = None;
    for s in app.sessions.iter_mut().filter(|s| s.backend == kind) {
        s.busy = false;
    }
    // The flash/banner gets the last line; the transcripts get all three.
    let last = why.last().map_or(String::new(), |l| format!(": {l}"));
    grey_out(
        app,
        kind,
        &format!("server exited{last} — press P to respawn"),
    );
    for s in app.sessions.iter_mut().filter(|s| s.backend == kind) {
        for l in &why {
            s.push_line(format!("  stderr: {l}"));
        }
    }
}

/// True when some tab other than `tab` already holds this Claude remote id.
pub(crate) fn claude_id_open_elsewhere(app: &App, tab: usize, resume_id: &str) -> bool {
    app.sessions
        .iter()
        .enumerate()
        .any(|(i, s)| i != tab && s.remote_id.as_deref() == Some(resume_id))
}

/// Notif and Request apply only when `generation` is the live child's.
/// Transport has no generation and still applies.
pub(crate) fn claude_frame_current(backends: &Backends, msg: &ServerMsg) -> bool {
    let params = match msg {
        ServerMsg::Transport(_) => return true,
        ServerMsg::Notif { params, .. } | ServerMsg::Request { params, .. } => params,
    };
    let live = params
        .get("session_id")
        .and_then(|v| v.as_str())
        .and_then(|id| backends.claude.get(id).map(|h| h.generation));
    claude_frame_matches(live, params["generation"].as_u64())
}

/// Agy frames route by the sending child's pid: only the agy tab whose
/// live handle owns it is touched. Agy frames carry no session id after
/// `init`, so a replaced, closed or switched-away child matches nothing
/// and is dropped, never attributed to the active tab.
pub(crate) fn handle_agy_msg(app: &mut App, backends: &Backends, frame: AgyFrame) -> bool {
    let ServerMsg::Notif { method, params } = frame.msg else {
        return handle_server_msg(app, BackendKind::Agy, frame.msg);
    };
    let tab = backends
        .agy
        .iter()
        .position(|h| frame.pid.is_some() && h.as_ref().and_then(|h| h.pid()) == frame.pid)
        .filter(|&t| {
            app.sessions
                .get(t)
                .is_some_and(|s| s.backend == BackendKind::Agy)
        });
    tab.is_some_and(|t| apply_agy_notif(app, t, &method, &params))
}

/// A stop is in flight: an approval that arrives now is declined through
/// the normal reject path instead of opening a card nobody is waiting on.
fn decline_if_stopping(app: &mut App, tab: usize) {
    let Some(s) = app.sessions.get(tab) else {
        return;
    };
    if s.stopping.is_none() || s.backend == BackendKind::Mock {
        return;
    }
    let Some(tool) = s.pending_diff.as_ref().map(|d| d.file.clone()) else {
        return;
    };
    // decide_ui answers the ACTIVE tab; point it at `tab` for the call.
    let prev = std::mem::replace(&mut app.active, tab);
    decide_ui(app, DecisionKind::Reject, "rejected");
    app.active = prev;
    app.sessions[tab].push_line(format!("stop: declined {tool} approval"));
}

pub(crate) fn handle_server_msg(app: &mut App, kind: BackendKind, msg: ServerMsg) -> bool {
    let tag = kind.label();
    match msg {
        ServerMsg::Notif { method, params } => {
            // S2-F1: frames naming an unknown (or no) session are
            // dropped, never attributed to the active tab — a wrong-tab
            // approval modal is worse than a dropped transcript line.
            let Some(tab) = tab_for_session(app, kind, &params) else {
                return false;
            };
            let bell = match kind {
                BackendKind::Muse => provider::apply_notif(app, tab, &method, &params),
                BackendKind::Codex => apply_codex_notif(app, tab, &method, &params),
                BackendKind::Grok => apply_grok_notif(app, tab, &method, &params),
                BackendKind::Claude => apply_claude_notif(app, tab, &method, &params),
                // Agy routes by pid in handle_agy_msg; mock has no wire.
                BackendKind::Agy | BackendKind::Mock => false,
            };
            decline_if_stopping(app, tab);
            bell
        }
        ServerMsg::Request { id, method, params } => match kind {
            BackendKind::Codex => {
                match tab_for_session(app, kind, &params) {
                    Some(tab) => {
                        let bell = apply_codex_approval(app, tab, &method, id, &params);
                        decline_if_stopping(app, tab);
                        bell
                    }
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
                Some(tab) => {
                    let bell = apply_grok_permission(app, tab, &method, id, &params);
                    decline_if_stopping(app, tab);
                    bell
                }
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
/// Agy routes by child pid instead ([`handle_agy_msg`]).
fn tab_for_session(app: &App, kind: BackendKind, params: &serde_json::Value) -> Option<usize> {
    let keys: &[&str] = match kind {
        BackendKind::Muse | BackendKind::Grok => &["sessionId"],
        BackendKind::Codex => &["threadId", "conversationId"],
        BackendKind::Claude => &["session_id"],
        BackendKind::Agy | BackendKind::Mock => return None,
    };
    let sid = keys.iter().filter_map(|k| params.get(k)?.as_str()).next()?;
    app.sessions
        .iter()
        .position(|s| s.backend == kind && s.remote_id.as_deref() == Some(sid))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{PendingApproval, PendingDiff};
    use crate::backend::Backends;
    use crate::test_support::busy_tab;
    use crate::test_support::muse_modal_app;

    #[test]
    fn claude_resume_sees_remote_id_open_on_another_tab() {
        let mut app = App::new();
        app.sessions[0].backend = BackendKind::Claude;
        app.sessions[0].remote_id = Some("live-claude".into());
        app.open_tab();
        assert!(!claude_id_open_elsewhere(&app, 0, "live-claude"));
        assert!(claude_id_open_elsewhere(&app, 1, "live-claude"));
        assert!(!claude_id_open_elsewhere(&app, 1, "other"));
    }

    #[test]
    fn claude_frame_current_drops_unmatched_and_keeps_transport() {
        let backends = Backends::default();
        let exit = ServerMsg::Notif {
            method: "claude/exit".into(),
            params: serde_json::json!({"session_id": "sid", "generation": 1}),
        };
        assert!(!claude_frame_current(&backends, &exit));
        assert!(claude_frame_current(
            &backends,
            &ServerMsg::Transport("dropped line".into())
        ));
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

    /// An approval that arrives while a stop is in flight is rejected on
    /// the wire instead of opening a card.
    #[test]
    fn approval_arriving_while_stopping_is_declined() {
        let mut app = busy_tab(BackendKind::Muse);
        app.request_stop();
        handle_server_msg(
            &mut app,
            BackendKind::Muse,
            ServerMsg::Notif {
                method: "approval/requested".into(),
                params: serde_json::json!({
                    "sessionId": "sess-1",
                    "toolName": "bash",
                    "approvalId": "ap-1",
                    "availableChoices": [
                        {"choiceId": "c-allow", "decision": "approved", "scope": "once", "label": "Allow"},
                        {"choiceId": "c-deny", "decision": "denied", "scope": "once", "label": "Deny"},
                    ],
                }),
            },
        );
        let s = app.active();
        assert!(s.pending_diff.is_none() && s.pending_approval.is_none());
        assert_eq!(app.outbox.decides.len(), 1);
        assert_eq!(app.outbox.decides[0].choice_id, "c-deny");
        assert!(s.lines.iter().any(|l| l == "stop: declined bash approval"));
        // Not stopping: the card opens as usual.
        let mut calm = busy_tab(BackendKind::Muse);
        handle_server_msg(
            &mut calm,
            BackendKind::Muse,
            ServerMsg::Notif {
                method: "approval/requested".into(),
                params: serde_json::json!({
                    "sessionId": "sess-1",
                    "toolName": "bash",
                    "availableChoices": [
                        {"choiceId": "c-deny", "decision": "denied", "scope": "once", "label": "Deny"},
                    ],
                }),
            },
        );
        assert!(calm.active().pending_diff.is_some());
        assert!(calm.outbox.decides.is_empty());
    }

    /// The stop-time decline answers the tab the approval names, even
    /// when it isn't active: the active tab's own card stays open.
    #[test]
    fn stop_decline_on_background_tab_leaves_active_card() {
        let mut app = busy_tab(BackendKind::Muse);
        let mut s = crate::app::Session::new("s2");
        s.backend = BackendKind::Muse;
        s.remote_id = Some("sess-2".into());
        s.busy = true;
        app.sessions.push(s);
        app.active = 1;
        app.request_stop();
        app.active = 0;
        assert!(app.sessions[1].stopping.is_some());
        app.active_mut().stage_diff(PendingDiff {
            file: "edit".into(),
            body: "body".into(),
        });
        app.active_mut().pending_approval = Some(PendingApproval {
            approval_id: "ap-0".into(),
            requirement_id: serde_json::Value::Null,
            choices: Vec::new(),
        });
        handle_server_msg(
            &mut app,
            BackendKind::Muse,
            ServerMsg::Notif {
                method: "approval/requested".into(),
                params: serde_json::json!({
                    "sessionId": "sess-2",
                    "toolName": "bash",
                    "approvalId": "ap-1",
                    "availableChoices": [
                        {"choiceId": "c-deny", "decision": "denied", "scope": "once", "label": "Deny"},
                    ],
                }),
            },
        );
        assert_eq!(app.active, 0);
        let bg = &app.sessions[1];
        assert!(bg.pending_diff.is_none() && bg.pending_approval.is_none());
        assert!(bg.lines.iter().any(|l| l == "stop: declined bash approval"));
        assert_eq!(app.outbox.decides.len(), 1);
        assert_eq!(app.outbox.decides[0].tab, 1);
        assert_eq!(app.outbox.decides[0].choice_id, "c-deny");
        let fg = &app.sessions[0];
        assert_eq!(fg.pending_diff.as_ref().unwrap().file, "edit");
        assert_eq!(fg.pending_approval.as_ref().unwrap().approval_id, "ap-0");
    }

    /// Agy frames route by the sending child's pid. Agy output carries no
    /// session id, so before this a background agy tab's frames landed on
    /// whatever tab was active (and a stray init rebound its session id).
    #[cfg(unix)]
    #[tokio::test]
    async fn agy_frames_route_by_child_pid() {
        use crate::agy::spawn_agy;
        use serde_json::{Value, json};
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let sleep = ["-c".to_string(), "sleep 30".to_string()];
        let child = spawn_agy("/bin/sh", &sleep, "/tmp", tx).await.unwrap();
        let pid = child.pid();
        // Tab 0: active claude tab. Tab 1: busy agy tab owning `child`.
        let mut app = busy_tab(BackendKind::Claude);
        let mut s = crate::app::Session::new("s2");
        s.backend = BackendKind::Agy;
        s.busy = true;
        app.sessions.push(s);
        let mut backends = Backends {
            agy: vec![None, Some(child)],
            ..Default::default()
        };
        let frame = |pid, method: &str, params| AgyFrame {
            pid,
            msg: ServerMsg::Notif {
                method: method.into(),
                params,
            },
        };
        let result = |text: &str| json!({"result": {"status": "SUCCESS", "response": text}});
        // Unknown (closed/replaced) or missing pid: dropped, nothing touched.
        for f in [
            frame(Some(4242), "agy/init", json!({"conversation_id": "stray"})),
            frame(Some(4242), "agy/result", result("stray")),
            frame(None, "agy/exit", Value::Null),
        ] {
            assert!(!handle_agy_msg(&mut app, &backends, f));
        }
        assert!(app.sessions.iter().all(|s| s.busy && s.lines.is_empty()));
        assert_eq!(app.sessions[0].remote_id.as_deref(), Some("sess-1"));
        assert!(app.sessions[1].remote_id.is_none());
        // The live child's frames reach its own tab, not the active one.
        let init = frame(pid, "agy/init", json!({"conversation_id": "conv-1"}));
        handle_agy_msg(&mut app, &backends, init);
        assert!(handle_agy_msg(
            &mut app,
            &backends,
            frame(pid, "agy/result", result("hi"))
        ));
        assert_eq!(app.sessions[1].remote_id.as_deref(), Some("conv-1"));
        assert!(!app.sessions[1].busy);
        assert!(app.sessions[1].lines.iter().any(|l| l == "hi"));
        assert!(app.sessions[0].busy && app.sessions[0].lines.is_empty());
        assert_eq!(app.sessions[0].remote_id.as_deref(), Some("sess-1"));
        // Tab switched to another backend while the old child lives on.
        app.sessions[1].backend = BackendKind::Claude;
        app.sessions[1].busy = true;
        assert!(!handle_agy_msg(
            &mut app,
            &backends,
            frame(pid, "agy/exit", Value::Null)
        ));
        assert!(app.sessions[1].busy);
        backends.agy[1].take().unwrap().shutdown().await;
    }
}
