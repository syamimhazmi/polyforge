//! Grok backend: `grok agent stdio` over newline JSON-RPC (ACP dialect).
//! One shared host, one ACP session per tab: session/new → session/prompt
//! → agent_message_chunk lines → prompt-result bell. Permission requests
//! arrive as server→client REQUESTS (session/request_permission); the
//! y/n/a/q modal answers them, exactly like the codex path.
//!
//! Wire notes (probed live + grok-build sources, NOT all live-confirmed):
//! - `initialize` takes `{protocolVersion, clientCapabilities, clientInfo}`
//!   and answers with protocolVersion 1. There is NO `initialized`
//!   notification in ACP; `notifications/initialized` belongs to MCP.
//! - `session/prompt` BLOCKS until the turn ends (result carries
//!   stopReason), so submits are spawned; completion re-enters as a
//!   synthetic `grok/prompt_completed` notification (see [`grok_submit`]).
//! - `session/resume` takes `{sessionId, cwd}`; `session/close` takes
//!   `{sessionId}` (ACP-confirmed, capability-gated; best-effort tab close).
//! - Permission answers follow ACP: selected+optionId, or cancelled.
//!   Option ids come from the request; unknown kinds fail closed.

use serde_json::Value;
use tokio::sync::mpsc::Sender;

use crate::app::{App, ApprovalChoice, DecisionKind, PendingApproval, PendingDiff};
use crate::msp::{self, Host, RpcError, ServerMsg};

/// Bring up the Grok backend: spawn `grok agent stdio` + ACP handshake.
/// Credential-free (initialize needs no auth); session creation happens
/// per tab via the outbox. Returns host + completion-tx + degradation.
/// The completion-tx feeds spawned session/prompt calls back into the
/// main loop (the pump owns the original sender).
pub async fn grok_bringup(
    bin: &str,
    client_version: &str,
    extra_env: Vec<(String, String)>,
    events_tx: Sender<ServerMsg>,
) -> Result<(Host, Sender<ServerMsg>, Option<String>), String> {
    let host = Host::spawn(bin, &["agent", "stdio"], &extra_env, events_tx.clone())
        .await
        .map_err(|e| format!("could not spawn `{bin} agent stdio`: {e}"))?;
    host.call(
        "initialize",
        serde_json::json!({
            "protocolVersion": 1,
            "clientCapabilities": {},
            "clientInfo": {"name": "polyforge", "version": client_version},
        }),
    )
    .await
    .map_err(|e| friendly_handshake_error(&e))?;
    // ACP has no initialized notification; notifications/initialized is MCP.
    Ok((host, events_tx, None))
}

fn friendly_handshake_error(e: &RpcError) -> String {
    let m = e.message.to_lowercase();
    if m.contains("login") || m.contains("auth") || m.contains("credential") {
        return format!("grok: not logged in — run `grok` and sign in ({e})");
    }
    format!("grok handshake failed: {e}")
}

/// Open one ACP session (also used when the picker respawns a tab).
/// Returns the session id plus an optional note (model override ignored).
/// A model override goes through session/set_config_option best-effort:
/// a failed set keeps the server default and says so (non-fatal).
pub async fn grok_new_session(
    host: &Host,
    model: Option<String>,
    workspace: String,
) -> Result<(String, Option<String>), String> {
    let res = host
        .call(
            "session/new",
            serde_json::json!({"cwd": workspace, "mcpServers": []}),
        )
        .await
        .map_err(|e| friendly_session_error(&e))?;
    let id = res
        .get("sessionId")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "session/new: no sessionId in result".to_string())?;
    let mut note = None;
    if let Some(m) = model {
        if let Err(e) = host
            .call(
                "session/set_config_option",
                serde_json::json!({
                    "sessionId": id,
                    "configId": "model",
                    "value": m,
                }),
            )
            .await
        {
            note = Some(format!("grok: model override ignored ({e})"));
        }
    }
    Ok((id, note))
}

fn friendly_session_error(e: &RpcError) -> String {
    let m = e.message.to_lowercase();
    if m.contains("login") || m.contains("auth") || m.contains("credential") {
        return format!("grok: not logged in — run `grok` and sign in ({e})");
    }
    if m.contains("permission denied") || m.contains("os error 1") {
        return format!("grok: sandbox denied session setup ({e})");
    }
    format!("grok session/new failed: {e}")
}

/// Re-attach a session from a previous run (/sessions resume). History
/// stays in our transcript store; resume just re-opens the live tail.
pub async fn grok_resume_session(
    host: &Host,
    session_id: &str,
    workspace: String,
) -> Result<String, String> {
    let res = host
        .call(
            "session/resume",
            serde_json::json!({"sessionId": session_id, "cwd": workspace}),
        )
        .await
        .map_err(|e| format!("grok session/resume failed: {e}"))?;
    Ok(res
        .get("sessionId")
        .and_then(|v| v.as_str())
        .unwrap_or(session_id)
        .to_string())
}

/// ACP session/close uses {sessionId} and is capability-gated.
/// Best-effort: failure abandons the id like the muse/codex path.
pub async fn grok_close_session(host: &Host, session_id: &str) {
    let _ = host
        .call(
            "session/close",
            serde_json::json!({"sessionId": session_id}),
        )
        .await;
}

/// Submit a prompt WITHOUT blocking the drain loop: `session/prompt`
/// answers only at turn end, so the call runs spawned and completion
/// re-enters through `done_tx` as `grok/prompt_completed`.
pub fn grok_submit(
    host: std::sync::Arc<Host>,
    done_tx: Sender<ServerMsg>,
    session_id: String,
    prompt: String,
) {
    tokio::spawn(async move {
        let params = serde_json::json!({
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": prompt}],
        });
        match host.call("session/prompt", params).await {
            Ok(res) => {
                let _ = done_tx
                    .send(ServerMsg::Notif {
                        method: "grok/prompt_completed".to_string(),
                        params: serde_json::json!({
                            "sessionId": session_id,
                            "stopReason": res.get("stopReason").and_then(|v| v.as_str()).unwrap_or("end_turn"),
                        }),
                    })
                    .await;
            }
            Err(e) => {
                let _ = done_tx
                    .send(ServerMsg::Notif {
                        method: "grok/prompt_completed".to_string(),
                        params: serde_json::json!({
                            "sessionId": session_id,
                            "error": e.to_string(),
                        }),
                    })
                    .await;
            }
        }
    });
}

/// Answer a grok permission request. `payload` is the ACP outcome:
/// `{"outcome": {"outcome": "selected", "optionId": …}}` or cancelled.
pub async fn grok_respond(host: &Host, id: Value, payload: Value) -> Result<(), RpcError> {
    host.respond(id, payload).await
}

/// Route one grok notification into App state.
/// Returns true when the bell should ring.
pub fn apply_grok_notif(app: &mut App, tab: usize, method: &str, params: &Value) -> bool {
    if tab >= app.sessions.len() {
        return false;
    }
    match method {
        "session/update" => {
            // Canonical: params.update.{sessionUpdate,…}; a flat variant
            // (params.sessionUpdate) exists in the wild — accept both.
            let update = params.get("update").unwrap_or(params);
            apply_session_update(app, tab, update)
        }
        "grok/prompt_completed" => {
            flush_grok_drafts(app, tab);
            if let Some(e) = params.get("error").and_then(|v| v.as_str()) {
                let s = &mut app.sessions[tab];
                s.busy = false;
                s.push_line(format!("grok: prompt failed: {}", truncate(e, 300)));
                if tab == app.active {
                    app.stick_to_bottom();
                }
                return false;
            }
            let reason = params
                .get("stopReason")
                .and_then(|v| v.as_str())
                .unwrap_or("end_turn");
            let s = &mut app.sessions[tab];
            if s.pending_diff.is_none() {
                s.busy = false;
                if reason == "end_turn" {
                    s.push_line("grok: done ✓".to_string());
                } else {
                    s.push_line(format!("grok: done ({reason}) ✓"));
                }
            }
            if tab == app.active {
                app.stick_to_bottom();
            }
            true
        }
        "x.ai/session_notification" => {
            let update = params.get("update").unwrap_or(params);
            if update
                .get("sessionUpdate")
                .and_then(|v| v.as_str())
                == Some("interaction_resolved")
            {
                let resolved = update
                    .get("tool_call_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                // S2-F3: only retire a card the user already answered
                // (its decision sits queued in the outbox). An
                // unanswered card must not silently vanish: queue an
                // explicit cancel so the wire carries a decision, then
                // close with a transcript note.
                let unanswered: Option<Value> = {
                    let s = &mut app.sessions[tab];
                    s.push_line("grok: permission resolved".to_string());
                    match s.pending_approval.as_ref() {
                        Some(a)
                            if a.approval_id.ends_with(resolved) && !resolved.is_empty() =>
                        {
                            let decided = app
                                .outbox
                                .decides
                                .iter()
                                .any(|d| d.requirement_id == a.requirement_id);
                            (!decided).then(|| a.requirement_id.clone())
                        }
                        _ => None,
                    }
                };
                if let Some(req_id) = unanswered {
                    queue_grok_cancelled(app, tab, req_id);
                    let s = &mut app.sessions[tab];
                    s.pending_approval.take();
                    let _ = s.clear_diff();
                    s.push_line(
                        "grok: permission resolved without a decision — sent cancel".to_string(),
                    );
                } else if !resolved.is_empty() {
                    let s = &mut app.sessions[tab];
                    if let Some(a) = s.pending_approval.as_ref() {
                        if a.approval_id.ends_with(resolved) {
                            s.pending_approval.take();
                            let _ = s.clear_diff();
                        }
                    }
                }
                if tab == app.active {
                    app.stick_to_bottom();
                }
            }
            false
        }
        _ => false, // _x.ai/* firehose (models/settings/announcements): silent.
    }
}

fn apply_session_update(app: &mut App, tab: usize, update: &Value) -> bool {
    let kind = update
        .get("sessionUpdate")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    match kind {
        "agent_message_chunk" => {
            let text = match update.get("content") {
                Some(Value::String(t)) => t.clone(),
                Some(c) => c
                    .get("text")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| msp::extract_text(update).unwrap_or_default()),
                None => msp::extract_text(update).unwrap_or_default(),
            };
            // Commit any live thought above the answer before tokens land.
            flush_thought_draft(app, tab);
            push_text(app, tab, &text);
            false
        }
        "agent_thought_chunk" => {
            // One live coalesced line (Session.draft_thought). Per-token
            // push_line flooded the transcript when the model streamed
            // word-by-word.
            let text = update
                .get("content")
                .and_then(|c| c.get("text"))
                .and_then(|v| v.as_str())
                .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
                .unwrap_or_default();
            if !text.is_empty() {
                append_thought_draft(app, tab, &text);
                if tab == app.active {
                    app.stick_to_bottom();
                }
            }
            false
        }
        "tool_call" => {
            flush_grok_drafts(app, tab);
            let title = update
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("tool");
            let k = update
                .get("kind")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let s = &mut app.sessions[tab];
            if k.is_empty() {
                s.push_line(format!("grok: ⚙ {title}"));
            } else {
                s.push_line(format!("grok: ⚙ {title} [{k}]"));
            }
            s.busy = true;
            if tab == app.active {
                app.stick_to_bottom();
            }
            false
        }
        "tool_call_update" => {
            flush_grok_drafts(app, tab);
            let status = update
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let name = update
                .get("title")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| {
                    update
                        .get("toolCallId")
                        .and_then(|v| v.as_str())
                        .map(|id| format!("…{}", tail6(id)))
                        .unwrap_or_else(|| "tool".to_string())
                });
            let mark = match status {
                "completed" => "✓",
                "failed" => "✗",
                _ => "~",
            };
            if !status.is_empty() {
                let s = &mut app.sessions[tab];
                s.push_line(format!("grok: {mark} {name} [{status}]"));
                if tab == app.active {
                    app.stick_to_bottom();
                }
            }
            false
        }
        "plan" => {
            let entries = update
                .get("entries")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            let s = &mut app.sessions[tab];
            if entries > 0 {
                s.push_line(format!("grok: plan ({entries} steps)"));
            } else {
                s.push_line("grok: plan updated".to_string());
            }
            if tab == app.active {
                app.stick_to_bottom();
            }
            false
        }
        _ => false,
    }
}

/// Cap on the open answer draft (force-commit if a turn never sends `\n`).
const ANSWER_DRAFT_CAP: usize = 64 * 1024;
/// Display + commit cap for the live thought line (matches prior truncate).
const THOUGHT_DRAFT_CAP: usize = 160;

/// Append answer deltas into `draft_answer`; peel complete lines on `\n`.
fn push_text(app: &mut App, tab: usize, t: &str) {
    if t.is_empty() {
        return;
    }
    let s = &mut app.sessions[tab];
    s.draft_answer.push_str(t);
    while let Some(idx) = s.draft_answer.find('\n') {
        let line: String = s.draft_answer.drain(..=idx).collect();
        let line = line.trim_end_matches('\n');
        if !line.is_empty() {
            s.push_line(line.to_string());
        }
    }
    if s.draft_answer.len() > ANSWER_DRAFT_CAP {
        let overflow = std::mem::take(&mut s.draft_answer);
        s.push_line(overflow);
    }
    if tab == app.active {
        app.stick_to_bottom();
    }
}

fn append_thought_draft(app: &mut App, tab: usize, piece: &str) {
    let s = &mut app.sessions[tab];
    match s.draft_thought.as_mut() {
        Some(body) => {
            if !body.is_empty() {
                body.push(' ');
            }
            body.push_str(piece);
            if body.len() > THOUGHT_DRAFT_CAP {
                *body = truncate(body, THOUGHT_DRAFT_CAP);
            }
        }
        None => {
            s.draft_thought = Some(truncate(piece, THOUGHT_DRAFT_CAP));
        }
    }
}

fn flush_thought_draft(app: &mut App, tab: usize) {
    let s = &mut app.sessions[tab];
    if let Some(body) = s.draft_thought.take() {
        if !body.is_empty() {
            s.push_line(format!("grok: ∴ {}", truncate(&body, THOUGHT_DRAFT_CAP)));
        }
    }
}

fn flush_answer_draft(app: &mut App, tab: usize) {
    let s = &mut app.sessions[tab];
    let rest = std::mem::take(&mut s.draft_answer);
    if !rest.is_empty() {
        s.push_line(rest);
    }
}

/// Commit live thought + open answer before chrome / turn-end lines.
fn flush_grok_drafts(app: &mut App, tab: usize) {
    flush_thought_draft(app, tab);
    flush_answer_draft(app, tab);
}

/// Render a grok permission request as a diff card + live approval handle.
/// `req_id` is the server's request id for [`grok_respond`]. Question-style
/// requests (ask_user_question / exit_plan_mode / mcp/elicit, gateway
/// `_`-wrapped or not) are NOT answerable by y/n/a/q: like the codex
/// permissions path they show a transcript line and respond cancelled.
pub fn apply_grok_permission(
    app: &mut App,
    tab: usize,
    method: &str,
    req_id: Value,
    params: &Value,
) -> bool {
    if tab >= app.sessions.len() {
        return false;
    }
    let base = method.trim_start_matches('_');
    if base != "session/request_permission" {
        let s = &mut app.sessions[tab];
        s.push_line(format!(
            "grok: cancelled — {base} is a question, not an approval"
        ));
        queue_grok_cancelled(app, tab, req_id);
        app.flash = format!("grok: {base} cancelled (see transcript)");
        return false;
    }
    let tool = params.get("toolCall").unwrap_or(&Value::Null);
    let tool_id = tool
        .get("toolCallId")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let title = tool
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("tool call");
    let kind = tool
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let input = tool
        .get("input")
        .map(|v| truncate(&v.to_string(), 1200))
        .unwrap_or_default();
    let body = if kind.is_empty() {
        format!("{title}\n{input}")
    } else {
        format!("{title} [{kind}]\n{input}")
    };
    let choices: Vec<ApprovalChoice> = params
        .get("options")
        .and_then(|v| v.as_array())
        .map(|opts| {
            opts.iter()
                .filter_map(|o| {
                    Some(ApprovalChoice {
                        choice_id: o.get("optionId")?.as_str()?.to_string(),
                        decision: o.get("optionId")?.as_str()?.to_string(),
                        scope: o
                            .get("kind")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        label: o
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("option")
                            .to_string(),
                        accepts_feedback: false,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let s = &mut app.sessions[tab];
    s.stage_diff(PendingDiff {
        file: title.to_string(),
        body,
    });
    // The toolCallId rides in approval_id so interaction_resolved can
    // retire a card the agent already moved past.
    s.pending_approval = Some(PendingApproval {
        approval_id: format!("session/request_permission:{tool_id}"),
        requirement_id: req_id,
        choices,
    });
    app.flash = format!("grok {title} wants approval (y/n/a/q)");
    true
}

/// y/n/a/q → ACP outcomes per the request's own options. Returns the
/// server request id plus the outcome payload. `q` ("later") maps to
/// `cancelled` — the turn may continue or stop at the agent's discretion,
/// which the transcript line says. Never approves on the user's behalf.
pub fn map_grok_decision(
    approval: &PendingApproval,
    kind: DecisionKind,
) -> Option<(Value, Value)> {
    use DecisionKind::*;
    if matches!(kind, Later) {
        return Some((
            approval.requirement_id.clone(),
            serde_json::json!({"outcome": {"outcome": "cancelled"}}),
        ));
    }
    let pick = |kind: &str| approval.choices.iter().find(|c| c.scope == kind);
    let opt = match kind {
        Approve => pick("allow_once"),
        ApproveAll => pick("allow_always"),
        Reject => pick("reject_once").or_else(|| pick("reject_always")),
        Later => unreachable!(),
    }?;
    Some((
        approval.requirement_id.clone(),
        serde_json::json!({"outcome": {"outcome": "selected", "optionId": opt.decision}}),
    ))
}

/// Queue a host-level reply; the tab is only diagnostic context.
pub fn queue_grok_cancelled(app: &mut App, tab: usize, request_id: Value) {
    app.outbox.decides.push(crate::app::OutboxDecide {
        tab,
        backend: crate::app::BackendKind::Grok,
        requirement_id: request_id,
        choice_id: serde_json::json!({"outcome": {"outcome": "cancelled"}}).to_string(),
        ..Default::default()
    });
}

/// Last 6 chars of an id for compact display (short ids unchanged).
fn tail6(id: &str) -> String {
    if id.len() <= 6 {
        return id.to_string();
    }
    id.chars().rev().take(6).collect::<String>().chars().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, BackendKind};

    fn grok_tab() -> App {
        let mut app = App::new();
        app.active_mut().backend = BackendKind::Grok;
        app
    }

    fn chunk(text: &str) -> Value {
        serde_json::json!({
            "sessionId": "sess-1",
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": text},
            },
        })
    }

    #[test]
    fn chunk_lines_land_in_transcript() {
        let mut app = grok_tab();
        assert!(!apply_grok_notif(&mut app, 0, "session/update", &chunk("hello\nworld")));
        assert!(app.active().lines.iter().any(|l| l == "hello"));
        // Trailing segment without `\n` stays in the open answer draft.
        assert_eq!(app.active().draft_answer, "world");
        assert!(!app.active().lines.iter().any(|l| l == "world"));
        // Flat variant (update fields at params top level) also routes.
        let flat = serde_json::json!({
            "sessionId": "sess-1",
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": "flat"},
        });
        assert!(!apply_grok_notif(&mut app, 0, "session/update", &flat));
        assert_eq!(app.active().draft_answer, "worldflat");
        // Turn end commits the open draft.
        assert!(apply_grok_notif(
            &mut app,
            0,
            "grok/prompt_completed",
            &serde_json::json!({"sessionId": "sess-1", "stopReason": "end_turn"}),
        ));
        assert!(app.active().lines.iter().any(|l| l == "worldflat"));
        assert!(app.active().draft_answer.is_empty());
    }

    #[test]
    fn thought_tool_and_plan_render_one_line_each() {
        let mut app = grok_tab();
        let thought = serde_json::json!({
            "sessionId": "s", "update": {
                "sessionUpdate": "agent_thought_chunk",
                "content": {"type": "text", "text": "hmm  let  me   think"},
            },
        });
        assert!(!apply_grok_notif(&mut app, 0, "session/update", &thought));
        // Live draft only — not committed until flush (tool / turn end).
        assert_eq!(
            app.active().draft_thought.as_deref(),
            Some("hmm let me think")
        );
        assert!(!app.active().lines.iter().any(|l| l.starts_with("grok: ∴")));
        let tool = serde_json::json!({
            "sessionId": "s", "update": {
                "sessionUpdate": "tool_call",
                "toolCallId": "tc-1", "title": "read foo.rs", "kind": "read",
            },
        });
        assert!(!apply_grok_notif(&mut app, 0, "session/update", &tool));
        assert!(app.active().draft_thought.is_none());
        assert!(app.active().lines.iter().any(|l| l == "grok: ∴ hmm let me think"));
        assert!(app.active().lines.iter().any(|l| l == "grok: ⚙ read foo.rs [read]"));
        assert!(app.active().busy);
        let upd = serde_json::json!({
            "sessionId": "s", "update": {
                "sessionUpdate": "tool_call_update",
                "toolCallId": "tc-1", "status": "completed",
            },
        });
        assert!(!apply_grok_notif(&mut app, 0, "session/update", &upd));
        assert!(app.active().lines.iter().any(|l| l.starts_with("grok: ✓")));
        // Unknown updates and the _x.ai firehose stay silent.
        let before = app.active().lines.len();
        assert!(!apply_grok_notif(&mut app, 0, "_x.ai/models/update", &serde_json::json!({})));
        assert!(!apply_grok_notif(
            &mut app, 0, "session/update",
            &serde_json::json!({"sessionId": "s", "update": {"sessionUpdate": "frobnicator"}}),
        ));
        assert_eq!(app.active().lines.len(), before);
    }

    #[test]
    fn thought_chunks_coalesce_one_line() {
        let mut app = grok_tab();
        for word in ["The", "user", "said", "hello"] {
            let thought = serde_json::json!({
                "sessionId": "s", "update": {
                    "sessionUpdate": "agent_thought_chunk",
                    "content": {"type": "text", "text": word},
                },
            });
            assert!(!apply_grok_notif(&mut app, 0, "session/update", &thought));
        }
        assert_eq!(
            app.active().draft_thought.as_deref(),
            Some("The user said hello")
        );
        assert_eq!(
            app.active().stream_draft_lines(),
            vec!["grok: ∴ The user said hello".to_string()]
        );
        assert!(!app.active().lines.iter().any(|l| l.starts_with("grok: ∴")));
        assert!(apply_grok_notif(
            &mut app,
            0,
            "grok/prompt_completed",
            &serde_json::json!({"sessionId": "s", "stopReason": "end_turn"}),
        ));
        let thought_lines: Vec<_> = app
            .active()
            .lines
            .iter()
            .filter(|l| l.starts_with("grok: ∴"))
            .collect();
        assert_eq!(thought_lines.len(), 1);
        assert_eq!(thought_lines[0], "grok: ∴ The user said hello");
        assert!(app.active().draft_thought.is_none());
    }

    #[test]
    fn answer_chunks_coalesce_until_newline() {
        let mut app = grok_tab();
        for part in ["Hel", "lo ", "world\n", "Next"] {
            assert!(!apply_grok_notif(&mut app, 0, "session/update", &chunk(part)));
        }
        assert!(app.active().lines.iter().any(|l| l == "Hello world"));
        assert_eq!(app.active().draft_answer, "Next");
        assert!(!app.active().lines.iter().any(|l| l == "Next"));
        assert!(apply_grok_notif(
            &mut app,
            0,
            "grok/prompt_completed",
            &serde_json::json!({"sessionId": "sess-1", "stopReason": "end_turn"}),
        ));
        assert!(app.active().lines.iter().any(|l| l == "Next"));
        assert!(app.active().draft_answer.is_empty());
        // Done line lands after the flushed answer.
        let lines = &app.active().lines;
        let next_i = lines.iter().position(|l| l == "Next").unwrap();
        let done_i = lines.iter().position(|l| l == "grok: done ✓").unwrap();
        assert!(next_i < done_i);
    }

    #[test]
    fn prompt_completed_rings_with_stop_reason() {
        let mut app = grok_tab();
        app.active_mut().busy = true;
        let done = serde_json::json!({"sessionId": "sess-1", "stopReason": "end_turn"});
        assert!(apply_grok_notif(&mut app, 0, "grok/prompt_completed", &done));
        assert!(!app.active().busy);
        assert!(app.active().lines.iter().any(|l| l == "grok: done ✓"));
        // Non-default reasons are named, not hidden.
        app.active_mut().busy = true;
        let capped = serde_json::json!({"sessionId": "sess-1", "stopReason": "max_tokens"});
        assert!(apply_grok_notif(&mut app, 0, "grok/prompt_completed", &capped));
        assert!(app.active().lines.iter().any(|l| l == "grok: done (max_tokens) ✓"));
        // Transport errors clear busy WITHOUT a bell.
        app.active_mut().busy = true;
        let err = serde_json::json!({"sessionId": "sess-1", "error": "boom"});
        assert!(!apply_grok_notif(&mut app, 0, "grok/prompt_completed", &err));
        assert!(!app.active().busy);
        assert!(app.active().lines.iter().any(|l| l.contains("prompt failed")));
    }

    fn permission_req() -> Value {
        serde_json::json!({
            "sessionId": "sess-1",
            "toolCall": {"toolCallId": "tc-9", "title": "edit a.rs", "kind": "edit",
                         "input": {"path": "a.rs"}},
            "options": [
                {"optionId": "allow-once", "name": "Allow once", "kind": "allow_once"},
                {"optionId": "allow-always-x", "name": "Always", "kind": "allow_always"},
                {"optionId": "opt-reject-once", "name": "Reject", "kind": "reject_once"},
            ],
        })
    }

    #[test]
    fn permission_mapping_fails_closed_and_ignores_order_and_labels() {
        let mut app = grok_tab();
        let mut req = permission_req();
        req["options"].as_array_mut().unwrap().reverse();
        req["options"].as_array_mut().unwrap().insert(0, serde_json::json!({
            "optionId": "deny-all", "kind": "reject_always", "name": "Allow"
        }));
        apply_grok_permission(&mut app, 0, "session/request_permission", serde_json::json!(11), &req);
        let mut approval = app.active().pending_approval.clone().unwrap();
        for (kind, expected) in [(DecisionKind::Approve, "allow-once"),
            (DecisionKind::ApproveAll, "allow-always-x"), (DecisionKind::Reject, "opt-reject-once")] {
            assert_eq!(map_grok_decision(&approval, kind).unwrap().1["outcome"]["optionId"], expected);
        }
        approval.choices.retain(|c| c.scope != "reject_once");
        assert_eq!(map_grok_decision(&approval, DecisionKind::Reject).unwrap().1["outcome"]["optionId"], "deny-all");
        approval.choices.retain(|c| c.scope == "allow_once");
        approval.choices[0].label = "Reject deny reject_once".into();
        assert!(map_grok_decision(&approval, DecisionKind::Reject).is_none());
        assert!(map_grok_decision(&approval, DecisionKind::ApproveAll).is_none());
        approval.choices[0].scope = "custom_allow_once".into();
        approval.choices[0].label = "Allow once".into();
        for kind in [DecisionKind::Approve, DecisionKind::ApproveAll, DecisionKind::Reject] {
            assert!(map_grok_decision(&approval, kind).is_none());
        }
        assert_eq!(map_grok_decision(&approval, DecisionKind::Later).unwrap().1["outcome"]["outcome"], "cancelled");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn model_override_sends_string_and_preserves_failure_note() {
        let (tx, mut events) = tokio::sync::mpsc::channel(4);
        let host = Host::spawn("/bin/sh", &["-c", r#"
            read -r request
            printf '%s\n' '{"id":1,"result":{"sessionId":"sess-1"}}'
            read -r request
            printf '{"method":"observed","params":%s}\n' "$request"
            printf '%s\n' '{"id":2,"error":{"code":-32602,"message":"unsupported model"}}'
        "#], &[], tx).await.unwrap();
        let (id, note) = tokio::time::timeout(std::time::Duration::from_secs(3),
            grok_new_session(&host, Some("model-x".into()), "/tmp".into()))
            .await.unwrap().unwrap();
        assert_eq!(id, "sess-1");
        assert!(note.unwrap().contains("model override ignored"));
        match events.recv().await.unwrap() {
            ServerMsg::Notif { params, .. } => {
                assert_eq!(params["method"], "session/set_config_option");
                assert_eq!(params["params"], serde_json::json!({"sessionId":"sess-1", "configId":"model", "value":"model-x"}));
            }
            other => panic!("unexpected: {other:?}"),
        }
        host.shutdown().await;
    }

    #[test]
    fn permission_request_builds_card_and_maps() {
        let mut app = grok_tab();
        assert!(apply_grok_permission(&mut app, 0, "session/request_permission", serde_json::json!(11), &permission_req()));
        let s = app.active();
        assert!(s.pending_diff.is_some());
        assert_eq!(s.pending_diff.as_ref().unwrap().file, "edit a.rs");
        let a = s.pending_approval.clone().expect("approval");
        assert_eq!(a.choices.len(), 3);
        let (_, p) = map_grok_decision(&a, DecisionKind::Approve).unwrap();
        assert_eq!(p, serde_json::json!({"outcome": {"outcome": "selected", "optionId": "allow-once"}}));
        let (_, p) = map_grok_decision(&a, DecisionKind::ApproveAll).unwrap();
        assert_eq!(p["outcome"]["optionId"], serde_json::json!("allow-always-x"));
        let (_, p) = map_grok_decision(&a, DecisionKind::Reject).unwrap();
        assert_eq!(p["outcome"]["optionId"], serde_json::json!("opt-reject-once"));
        // Later maps to cancelled, never to an allow option.
        let (req, p) = map_grok_decision(&a, DecisionKind::Later).unwrap();
        assert_eq!(req, serde_json::json!(11));
        assert_eq!(p, serde_json::json!({"outcome": {"outcome": "cancelled"}}));
    }

    #[test]
    fn question_requests_respond_cancelled() {
        let mut app = grok_tab();
        assert!(!apply_grok_permission(
            &mut app, 0, "x.ai/ask_user_question", serde_json::json!(3), &serde_json::json!({}),
        ));
        assert!(app.active().pending_approval.is_none());
        assert_eq!(app.outbox.decides.len(), 1);
        assert_eq!(app.outbox.decides[0].requirement_id, serde_json::json!(3));
        assert!(app.outbox.decides[0].choice_id.contains("cancelled"));
        assert!(app.active().lines.iter().any(|l| l.contains("question, not an approval")));
    }

    /// S2-F3: an UNANSWERED card must not silently vanish on
    /// interaction_resolved — an explicit cancel goes on the wire, then
    /// the card retires with a transcript note.
    #[test]
    fn interaction_resolved_without_decision_sends_cancel() {
        let mut app = grok_tab();
        assert!(apply_grok_permission(&mut app, 0, "session/request_permission", serde_json::json!(11), &permission_req()));
        assert!(app.active().pending_approval.is_some());
        assert!(app.outbox.decides.is_empty());
        let resolved = serde_json::json!({
            "sessionId": "sess-1",
            "update": {"sessionUpdate": "interaction_resolved", "tool_call_id": "tc-9"},
        });
        assert!(!apply_grok_notif(&mut app, 0, "x.ai/session_notification", &resolved));
        assert!(app.active().pending_approval.is_none());
        assert!(app.active().pending_diff.is_none());
        assert_eq!(app.outbox.decides.len(), 1);
        assert_eq!(app.outbox.decides[0].requirement_id, serde_json::json!(11));
        assert!(app.outbox.decides[0].choice_id.contains("cancelled"));
        assert!(app.active().lines.iter().any(|l| l.contains("permission resolved")));
        assert!(app.active().lines.iter().any(|l| l.contains("sent cancel")));
    }

    /// S2-F3 control: a card the user already answered (its decision is
    /// queued) still retires quietly — no duplicate cancel.
    #[test]
    fn interaction_resolved_with_decision_retires_quietly() {
        let mut app = grok_tab();
        assert!(apply_grok_permission(&mut app, 0, "session/request_permission", serde_json::json!(11), &permission_req()));
        queue_grok_cancelled(&mut app, 0, serde_json::json!(11));
        let resolved = serde_json::json!({
            "sessionId": "sess-1",
            "update": {"sessionUpdate": "interaction_resolved", "tool_call_id": "tc-9"},
        });
        assert!(!apply_grok_notif(&mut app, 0, "x.ai/session_notification", &resolved));
        assert!(app.active().pending_approval.is_none());
        assert!(app.active().pending_diff.is_none());
        assert_eq!(app.outbox.decides.len(), 1, "answered card must not queue a second cancel");
    }

    /// Live bringup against a real `grok agent stdio`: initialize needs no
    /// auth and no session (session/new is quota/sandbox-gated, so it stays
    /// out of this test). Skips loudly without the binary.
    #[tokio::test]
    async fn grok_bringup_no_auth() {
        let bin = std::env::var("GROK_BIN").unwrap_or_else(|_| "grok".to_string());
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        match grok_bringup(&bin, "test", vec![], tx).await {
            Ok((host, _, _)) => {
                host.shutdown().await;
            }
            Err(e) if e.contains("could not spawn") => {
                eprintln!("SKIP grok_bringup_no_auth: {e}");
            }
            Err(e) => panic!("bringup failed for the wrong reason: {e}"),
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = 0;
    for (i, c) in s.char_indices() {
        if i + c.len_utf8() > max {
            break;
        }
        end = i + c.len_utf8();
    }
    format!("{}…[truncated {} chars]", &s[..end], s.len() - end)
}
