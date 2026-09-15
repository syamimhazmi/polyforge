//! M2 provider layer: `Backend::Mock` (M1 behavior, quota-free) or
//! `Backend::Muse` (MSP `serve` host). The App stays synchronous: muse work
//! is queued in [`Outbox`] and performed by the async main loop, while
//! server notifications are applied via [`apply_notif`].

use serde_json::Value;

use crate::app::{App, ApprovalChoice, OutboxDecide, PendingApproval, PendingDiff};
use crate::msp::{self, Host, RpcError, ServerMsg};

/// Bring up the muse backend: spawn serve, handshake, start one session per
/// tab, subscribe to each. Errors degrade (never crash the TUI): the reason
/// is shown on the tabs so the user sees the exact fix (`muse login`).
/// Bring up the muse backend: spawn serve, handshake, start one session per
/// tab. Returns the host, the per-tab session ids (None = failed to start;
/// that tab is greyed out), and the first degradation reason, if any.
pub async fn muse_bringup(
    bin: &str,
    tabs: usize,
    provider_id: Option<String>,
    model: Option<String>,
    workspace: String,
    extra_env: Vec<(String, String)>,
    events_tx: tokio::sync::mpsc::Sender<ServerMsg>,
) -> Result<(Host, Vec<Option<String>>, Option<String>), String> {
    let host = Host::spawn(bin, &["serve"], &extra_env, events_tx)
        .await
        .map_err(|e| format!("could not spawn `{bin} serve`: {e}"))?;
    host.handshake("polyforge", env!("CARGO_PKG_VERSION"))
        .await
        .map_err(|e| format!("msp handshake failed: {e}"))?;
    let mut session_ids = Vec::new();
    let mut degraded = None;
    for _ in 0..tabs {
        match start_session(&host, provider_id.clone(), model.clone(), workspace.clone()).await
        {
            Ok(id) => {
                // Best-effort live subscription; events arrive anyway.
                let _ = host
                    .call("view/subscribe", serde_json::json!({"sessionId": id}))
                    .await;
                session_ids.push(Some(id));
            }
            Err(e) => {
                degraded = Some(e);
                session_ids.push(None);
            }
        }
        if degraded.is_some() {
            break;
        }
    }
    Ok((host, session_ids, degraded))
}

async fn start_session(
    host: &Host,
    provider_id: Option<String>,
    model: Option<String>,
    workspace: String,
) -> Result<String, String> {
    let mut params = serde_json::json!({
        "commandId": msp::uuid7(),
        "workspaceRoot": workspace,
        // Ask, don't assume: every side effect must raise approval/requested
        // so the TUI's y/n/a/q modal (never the server default) decides.
        "approvalMode": "onRequest",
    });
    if let Some(p) = provider_id {
        params["providerId"] = Value::String(p);
    }
    if let Some(m) = model {
        params["modelId"] = Value::String(m);
    }
    let res = host
        .call("session/start", params)
        .await
        .map_err(|e| friendly_start_error(&e))?;
    res.get("session")
        .and_then(|s| s.get("sessionId"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "session/start: no sessionId in result".to_string())
}

fn friendly_start_error(e: &RpcError) -> String {
    let m = e.message.to_lowercase();
    if m.contains("credential") || m.contains("login") || m.contains("auth") {
        return format!("muse: not logged in — run `muse login` ({e})");
    }
    format!("muse session/start failed: {e}")
}

pub async fn muse_submit(
    host: &Host,
    session_id: &str,
    prompt: &str,
) -> Result<(), RpcError> {
    host
        .call(
            "turn/start",
            serde_json::json!({
                "commandId": msp::uuid7(),
                "sessionId": session_id,
                "input": [{"type": "text", "text": prompt}],
            }),
        )
        .await
        .map(|_| ())
}

pub async fn muse_decide(
    host: &Host,
    session_id: &str,
    d: &OutboxDecide,
) -> Result<(), RpcError> {
    let mut params = serde_json::json!({
        "commandId": msp::uuid7(),
        "sessionId": session_id,
        "approvalId": d.approval_id,
        "requirementId": d.requirement_id,
        "choiceId": d.choice_id,
    });
    if let Some(fb) = &d.feedback {
        params["feedback"] = Value::String(fb.clone());
    }
    host.call("approval/decide", params).await.map(|_| ())
}

/// Route one server notification into App state.
/// Returns true when the bell should ring (turn done or approval waiting).
pub fn apply_notif(app: &mut App, tab: usize, method: &str, params: &Value) -> bool {
    if tab >= app.sessions.len() {
        return false;
    }
    match method {
        "item/delta" => {
            if let Some(t) = msp::extract_text(params) {
                let s = &mut app.sessions[tab];
                for line in t.split('\n') {
                    if !line.is_empty() {
                        s.push_line(line.to_string());
                    }
                }
                if tab == app.active {
                    app.stick_to_bottom();
                }
            }
            false
        }
        "item/completed" => {
            // Atomic items (echo provider) carry their full text here.
            if let Some(t) = msp::extract_text(params) {
                let looks_like_prompt = params
                    .get("item")
                    .and_then(|i| i.get("kind"))
                    .and_then(|k| k.as_str())
                    .map(|k| k.contains("user"))
                    .unwrap_or(false);
                if !looks_like_prompt {
                    let s = &mut app.sessions[tab];
                    for line in t.split('\n') {
                        if !line.is_empty() {
                            s.push_line(line.to_string());
                        }
                    }
                }
            }
            false
        }
        "approval/requested" => {
            let p = &params;
            let tool = p
                .get("toolName")
                .and_then(|v| v.as_str())
                .unwrap_or("tool");
            let mut body = format!("tool: {tool}\n");
            if let Some(args) = p.get("rawArgs").and_then(|v| v.as_str()) {
                body.push_str(&truncate(args, 1200));
                body.push('\n');
            }
            if let Some(subj) = p.get("subject") {
                body.push_str(&truncate(
                    &serde_json::to_string_pretty(subj).unwrap_or_else(|_| subj.to_string()),
                    2000,
                ));
            }
            let choices = parse_choices(p);
            if !choices.is_empty() {
                let labels: Vec<&str> =
                    choices.iter().map(|c| c.label.as_str()).collect();
                body.push_str(&format!("\nchoices: {}", labels.join(" · ")));
            }
            let s = &mut app.sessions[tab];
            s.pending_diff = Some(PendingDiff {
                file: tool.to_string(),
                body,
            });
            s.pending_approval = Some(PendingApproval {
                approval_id: p
                    .get("approvalId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                requirement_id: p
                    .get("currentRequirementId")
                    .cloned()
                    .unwrap_or(Value::Null),
                choices: parse_choices(p),
            });
            app.flash = format!("{tool} wants approval (y/n/a/q)");
            true // bell: a decision is waiting
        }
        "turn/completed" | "turn/failed" => {
            let failed = method == "turn/failed";
            let s = &mut app.sessions[tab];
            // A turn that ends while its diff modal is open stays busy until
            // the user decides; otherwise the job is done.
            if s.pending_diff.is_none() {
                s.busy = false;
                s.push_line(if failed {
                    "muse: turn failed".to_string()
                } else {
                    "muse: done ✓".to_string()
                });
            }
            if tab == app.active {
                app.stick_to_bottom();
            }
            true
        }
        "session/statusChanged" => {
            let running = params
                .get("status")
                .and_then(|v| v.as_str())
                .map(|s| s == "running")
                .unwrap_or(false);
            let s = &mut app.sessions[tab];
            if !running && s.pending_diff.is_none() {
                s.busy = false;
            } else if running {
                s.busy = true;
            }
            false
        }
        _ => false,
    }
}

/// Map y/n/a/q onto an available choice. `q` ("later") has no wire
/// dismiss, so it denies with "ask again later" feedback when accepted.
pub fn map_decision(
    approval: &PendingApproval,
    kind: DecisionKind,
) -> Option<(String, Option<String>)> {
    let pick = |dec: &str, scope: Option<&str>| {
        approval.choices.iter().find(|c| {
            c.decision == dec && scope.map(|s| c.scope == s).unwrap_or(true)
        })
    };
    match kind {
        DecisionKind::Approve => pick("approved", Some("once"))
            .or_else(|| pick("approved", None))
            .map(|c| (c.choice_id.clone(), None)),
        DecisionKind::ApproveAll => pick("approvedForSession", None)
            .or_else(|| {
                approval
                    .choices
                    .iter()
                    .find(|c| c.decision == "approved" && c.scope == "session")
            })
            .or_else(|| pick("approved", None))
            .map(|c| (c.choice_id.clone(), None)),
        DecisionKind::Reject => pick("denied", None).map(|c| (c.choice_id.clone(), None)),
        DecisionKind::Later => {
            let deny = pick("denied", None)?;
            let fb = deny
                .accepts_feedback
                .then(|| "User deferred for now; re-ask later if still needed.".to_string());
            Some((deny.choice_id.clone(), fb))
        }
    }
}

fn parse_choices(p: &Value) -> Vec<ApprovalChoice> {
    p.get("availableChoices")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .map(|c| ApprovalChoice {
                    choice_id: str_field(c, "choiceId"),
                    decision: str_field(c, "decision"),
                    scope: str_field(c, "scope"),
                    label: str_field(c, "label"),
                    accepts_feedback: c
                        .get("acceptsFeedback")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string()
}

#[derive(Clone, Copy)]
pub enum DecisionKind {
    Approve,
    ApproveAll,
    Reject,
    Later,
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    format!("{}…[truncated {} chars]", &s[..max], s.len() - max)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::PendingApproval;

    fn approval() -> PendingApproval {
        PendingApproval {
            approval_id: "a1".into(),
            requirement_id: Value::String("r1".into()),
            choices: vec![
                ApprovalChoice {
                    choice_id: "c-once".into(),
                    decision: "approved".into(),
                    scope: "once".into(),
                    label: "Allow once".into(),
                    accepts_feedback: false,
                },
                ApprovalChoice {
                    choice_id: "c-sess".into(),
                    decision: "approvedForSession".into(),
                    scope: "session".into(),
                    label: "Allow session".into(),
                    accepts_feedback: false,
                },
                ApprovalChoice {
                    choice_id: "c-deny".into(),
                    decision: "denied".into(),
                    scope: "once".into(),
                    label: "Deny".into(),
                    accepts_feedback: true,
                },
            ],
        }
    }

    #[test]
    fn decisions_map_to_choices() {
        let a = approval();
        assert_eq!(
            map_decision(&a, DecisionKind::Approve).unwrap().0,
            "c-once"
        );
        assert_eq!(
            map_decision(&a, DecisionKind::ApproveAll).unwrap().0,
            "c-sess"
        );
        assert_eq!(
            map_decision(&a, DecisionKind::Reject).unwrap().0,
            "c-deny"
        );
        let (id, fb) = map_decision(&a, DecisionKind::Later).unwrap();
        assert_eq!(id, "c-deny");
        assert!(fb.unwrap().contains("deferred"));
    }

    /// Live echo roundtrip against a real `muse serve`. Needs the binary
    /// (MUSE_BIN env or PATH) and skips loudly without it.
    #[tokio::test]
    async fn msp_echo_roundtrip() {
        let bin = std::env::var("MUSE_BIN").unwrap_or_else(|_| "muse".to_string());
        let (tx, mut rx) = tokio::sync::mpsc::channel(256);
        // Isolated HOME so the test never touches the user's login or logs.
        let home = std::env::temp_dir().join(format!("pf-test-home-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&home);
        let home_s = home.to_string_lossy().into_owned();
        let host = match Host::spawn(
            &bin,
            &["serve", "--disable-write", "--disable-shell"],
            &[("HOME".to_string(), home_s)],
            tx,
        )
        .await
        {
            Ok(h) => h,
            Err(e) => {
                eprintln!("SKIP msp_echo_roundtrip: cannot spawn `{bin}`: {e}");
                return;
            }
        };
        host.handshake("polyforge_test", "0.0.0").await.expect("handshake");
        host.notify("initialized").await;
        let res = host
            .call(
                "session/start",
                serde_json::json!({"commandId": msp::uuid7(), "providerId": "echo", "workspaceRoot": "/tmp"}),
            )
            .await
            .expect("session/start");
        let sid = res["session"]["sessionId"].as_str().expect("sessionId").to_string();
        let _ = host
            .call("view/subscribe", serde_json::json!({"sessionId": sid}))
            .await;
        host
            .call(
                "turn/start",
                serde_json::json!({"commandId": msp::uuid7(), "sessionId": sid,
                    "input": [{"type": "text", "text": "say probe-ok"}]}),
            )
            .await
            .expect("turn/start");
        let mut saw_text = false;
        let mut done = false;
        let deadline = tokio::time::sleep(std::time::Duration::from_secs(45));
        tokio::pin!(deadline);
        while !(saw_text && done) {
            tokio::select! {
                _ = &mut deadline => panic!("echo roundtrip timed out"),
                msg = rx.recv() => {
                    let msg = msg.expect("host alive");
                    if let ServerMsg::Notif { method, params } = msg {
                        if method == "item/delta" || method == "item/completed" {
                            if let Some(t) = msp::extract_text(&params) {
                                if t.contains("probe-ok") { saw_text = true; }
                            }
                        }
                        if method == "turn/completed" { done = true; }
                    }
                }
            }
        }
        assert!(saw_text && done);
        host.shutdown().await;
        let _ = std::fs::remove_dir_all(&home);
    }
}
