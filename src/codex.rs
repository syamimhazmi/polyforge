//! M3 Codex backend: `codex app-server` over stdio (newline JSON-RPC).
//! Per-tab threads: thread/start → turn/start → AgentMessageDelta lines,
//! turn/completed bell. Approvals arrive as server→client REQUESTS
//! (applyPatchApproval / execCommandApproval / fileChange…); the y/n/a/q
//! modal answers them, exactly like the muse path.

use serde_json::Value;

use crate::app::{App, ApprovalChoice, DecisionKind, PendingApproval, PendingDiff};
use crate::msp::{self, Host, RpcError, ServerMsg};

/// Bring up the Codex backend: spawn app-server and handshake. Threads
/// are opened per tab afterwards via [`codex_start_thread`].
pub async fn codex_bringup(
    bin: &str,
    extra_env: Vec<(String, String)>,
    events_tx: tokio::sync::mpsc::Sender<ServerMsg>,
) -> Result<Host, String> {
    let host = Host::spawn(bin, &["app-server"], &extra_env, events_tx)
        .await
        .map_err(|e| format!("could not spawn `{bin} app-server`: {e}"))?;
    host.handshake("polyforge", env!("CARGO_PKG_VERSION"))
        .await
        .map_err(|e| format!("codex handshake failed: {e}"))?;
    // NOTE: codex has no `initialized` acknowledgement in the probed build;
    // calls proceed once `initialize` resolves.
    Ok(host)
}

/// Open one codex thread (also used when the picker respawns a tab).
pub async fn codex_start_thread(
    host: &Host,
    model: Option<String>,
    workspace: String,
) -> Result<String, String> {
    // UNCONFIRMED: exact enum string vs muse's camelCase `onRequest`.
    // `"never"` killed the modal path; `"on-request"` matches interactive gating.
    let mut params = serde_json::json!({
        "cwd": workspace,
        "approvalPolicy": "on-request",
        "sandbox": "read-only",
    });
    if let Some(m) = model {
        params["model"] = Value::String(m);
    }
    let res = host
        .call("thread/start", params)
        .await
        .map_err(|e| friendly_start_error(&e))?;
    res.get("thread")
        .and_then(|t| t.get("id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "thread/start: no thread id in result".to_string())
}

fn friendly_start_error(e: &RpcError) -> String {
    let m = e.message.to_lowercase();
    if m.contains("login") || m.contains("auth") || m.contains("credential") {
        return format!("codex: not logged in — run `codex login` ({e})");
    }
    format!("codex thread/start failed: {e}")
}

/// Re-attach a thread from a previous run (Q10 resume). History stays in
/// our transcript store; resume just re-opens the live tail.
pub async fn codex_resume_thread(host: &Host, thread_id: &str) -> Result<String, String> {
    let res = host
        .call("thread/resume", serde_json::json!({"threadId": thread_id}))
        .await
        .map_err(|e| format!("codex thread/resume failed: {e}"))?;
    let resumed = res
        .get("thread")
        .and_then(|t| t.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or(thread_id);
    Ok(resumed.to_string())
}

/// Start a turn; returns `result.turn.id` (needed to interrupt it).
pub async fn codex_submit(
    host: &Host,
    thread_id: &str,
    prompt: &str,
) -> Result<Option<String>, RpcError> {
    host.call(
        "turn/start",
        serde_json::json!({
            "threadId": thread_id,
            "input": [{"type": "text", "text": prompt}],
        }),
    )
    .await
    .map(|res| msp::turn_id_of(&res))
}

/// Interrupt a running turn (app-server v2; `turnId` is required). The
/// turn ends via its normal `turn/completed`.
pub async fn codex_interrupt(host: &Host, thread_id: &str, turn_id: &str) -> Result<(), RpcError> {
    host.call(
        "turn/interrupt",
        serde_json::json!({"threadId": thread_id, "turnId": turn_id}),
    )
    .await
    .map(|_| ())
}

/// Answer a codex approval request. `payload` must match the request kind's
/// response shape (e.g. {"decision": "approved"}).
pub async fn codex_respond(host: &Host, id: Value, payload: Value) -> Result<(), RpcError> {
    host.respond(id, payload).await
}

/// Route one codex notification into App state.
/// Returns true when the bell should ring.
pub fn apply_codex_notif(app: &mut App, tab: usize, method: &str, params: &Value) -> bool {
    if tab >= app.sessions.len() {
        return false;
    }
    // Lowercased once for the segment-tail arms below. Exact live method
    // names remain UNCONFIRMED (fileChange/ vs item/ vs itemGuardian/).
    let lower = method.to_lowercase();
    match method {
        "agentMessageDelta" => {
            let s = &mut app.sessions[tab];
            s.flush_thought();
            s.set_phase(crate::activity::Phase::Responding);
            if let Some(t) = params.get("delta").and_then(|v| v.as_str()) {
                push_text(app, tab, t);
            } else if let Some(t) = msp::extract_text(params) {
                push_text(app, tab, &t);
            }
            false
        }
        "agentMessage" | "item/completed" | "thread/realtime/item/completed" => {
            // Full items: skip user echoes, keep assistant text.
            let kind = params
                .get("item")
                .and_then(|i| i.get("type"))
                .and_then(|k| k.as_str())
                .unwrap_or("");
            if kind.contains("user") {
                return false;
            }
            if let Some(t) = msp::extract_text(params) {
                let s = &mut app.sessions[tab];
                if kind.contains("reasoning") {
                    s.set_phase(crate::activity::Phase::Thinking);
                    s.push_thought(&format!("{t}\n"));
                } else {
                    s.flush_thought();
                    s.set_phase(crate::activity::Phase::Responding);
                    push_text(app, tab, &t);
                }
            }
            false
        }
        "turn/started" => {
            let s = &mut app.sessions[tab];
            if s.busy
                && let Some(id) = msp::turn_id_of(params)
            {
                s.turn_id = Some(id);
            }
            false
        }
        "thread/tokenUsage/updated" => {
            let n = params
                .pointer("/tokenUsage/last/totalTokens")
                .and_then(crate::app::take_u64);
            app.sessions[tab].note_tokens(n);
            false
        }
        "turn/completed" => {
            let s = &mut app.sessions[tab];
            s.flush_thought();
            if s.pending_diff.is_none() {
                s.busy = false;
            }
            if tab == app.active {
                app.stick_to_bottom();
            }
            true
        }
        "turn/failed" | "thread/realtime/error" => {
            let s = &mut app.sessions[tab];
            s.flush_thought();
            s.push_line("codex: turn failed (see flash)".to_string());
            app.flash = format!("codex: {}", truncate(&params.to_string(), 300));
            false
        }
        "error" => {
            // Reconnect/weather notices: visible, never fatal by themselves.
            app.flash = format!("codex: {}", truncate(&params.to_string(), 300));
            false
        }
        "warning" => {
            app.flash = format!("codex: {}", truncate(&params.to_string(), 300));
            false
        }
        "thread/status/changed" => {
            let active = params
                .get("status")
                .and_then(|s| s.get("type"))
                .and_then(|v| v.as_str())
                .map(|t| t == "active")
                .unwrap_or(false);
            let s = &mut app.sessions[tab];
            if !active && s.pending_diff.is_none() {
                s.busy = false;
            } else if active && !s.sealed {
                s.busy = true;
            }
            false
        }
        _ if method_segment_tail(&lower, "patchupdated") => {
            // fileChange/patchUpdated: the agent wrote files mid-turn.
            // {changes: [{path, kind: {type: add|delete|update}, diff}]}.
            // One line per file — the unified diff itself rides the
            // approval card when the turn gates, so streaming stays
            // readable. Segment-matched (`/` before tail); exact live
            // names remain UNCONFIRMED.
            // S7-F2: these writes are ALREADY APPLIED server-side — the
            // y/n modal never gated them. Every line says so; never
            // claim y/n covers all writes.
            let s = &mut app.sessions[tab];
            let changes = params
                .get("changes")
                .and_then(|c| c.as_array())
                .cloned()
                .unwrap_or_default();
            if changes.is_empty() {
                s.push_line(
                    "codex: files updated (already applied — not gated by y/n)".to_string(),
                );
            }
            for ch in &changes {
                let path = ch.get("path").and_then(|p| p.as_str()).unwrap_or("?");
                let kind = ch
                    .get("kind")
                    .and_then(|k| k.get("type"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("update");
                let mark = match kind {
                    "add" => "+",
                    "delete" => "-",
                    _ => "~",
                };
                s.push_line(format!("codex: {mark} {path} (already applied)"));
            }
            if tab == app.active {
                app.stick_to_bottom();
            }
            false
        }
        _ if method_segment_tail(&lower, "outputdelta") => {
            // fileChange/outputDelta streams file bytes mid-write; the
            // completed change lands via patchUpdated above. Deliberately
            // silent — byte noise the transcript must not keep.
            false
        }
        _ if method_segment_tail(&lower, "approvalreviewstarted")
            || lower.ends_with("approvalreview/started") =>
        {
            // Guardian auto-review opened on a tool call (explains why a
            // gated-looking step never raised the modal).
            let s = &mut app.sessions[tab];
            s.push_line(format!(
                "codex: reviewing {}",
                summarize_guardian_action(params.get("action").unwrap_or(&Value::Null))
            ));
            if tab == app.active {
                app.stick_to_bottom();
            }
            false
        }
        _ if method_segment_tail(&lower, "approvalreviewcompleted")
            || lower.ends_with("approvalreview/completed") =>
        {
            let review = params.get("review").unwrap_or(&Value::Null);
            let status = review
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("done");
            let mut line = format!(
                "codex: guardian {status} — {}",
                summarize_guardian_action(params.get("action").unwrap_or(&Value::Null))
            );
            if let Some(r) = review.get("rationale").and_then(|v| v.as_str()) {
                if !r.is_empty() {
                    line.push_str(&format!(" ({})", truncate(r, 200)));
                }
            }
            // S7-F2: an approved guardian review applies the side effect
            // without ever raising the y/n modal — say so on the line.
            if status == "approved" {
                line.push_str(" [already applied — no y/n modal]");
            }
            let s = &mut app.sessions[tab];
            s.push_line(line);
            if tab == app.active {
                app.stick_to_bottom();
            }
            false
        }
        _ => false,
    }
}

/// One-line summary of a guardian-reviewed action. Checks the known
/// variant payloads (command / program+argv / tool-ish keys) and falls
/// back to the variant `type`, so future variants still render.
fn summarize_guardian_action(action: &Value) -> String {
    if let Some(c) = action.get("command").and_then(|v| v.as_str()) {
        return format!("`{}`", truncate(c, 120));
    }
    if let Some(p) = action.get("program").and_then(|v| v.as_str()) {
        let mut s = p.to_string();
        if let Some(args) = action.get("argv").and_then(|v| v.as_array()) {
            for a in args.iter().take(4) {
                if let Some(a) = a.as_str() {
                    s.push(' ');
                    s.push_str(&truncate(a, 40));
                }
            }
        }
        return format!("`{}`", truncate(&s, 120));
    }
    for key in ["tool", "name", "path", "host", "url"] {
        if let Some(v) = action.get(key).and_then(|v| v.as_str()) {
            return format!("{key} {v}");
        }
    }
    action
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("action")
        .to_string()
}

fn push_text(app: &mut App, tab: usize, t: &str) {
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

/// Render a codex approval request as a diff card + live approval handle.
/// `req_id` is the server's request id for [`codex_respond`].
pub fn apply_codex_approval(
    app: &mut App,
    tab: usize,
    method: &str,
    req_id: Value,
    params: &Value,
) -> bool {
    if tab >= app.sessions.len() {
        return false;
    }
    let (tool, body) = match method {
        "applyPatchApproval" => {
            let files = params
                .get("fileChanges")
                .map(|v| truncate(&v.to_string(), 2000))
                .unwrap_or_default();
            let reason = params.get("reason").and_then(|v| v.as_str()).unwrap_or("");
            ("applyPatch".to_string(), format!("{reason}\n{files}"))
        }
        "execCommandApproval" => {
            let cmd = params
                .get("command")
                .map(|v| truncate(&v.to_string(), 1200))
                .unwrap_or_default();
            let reason = params.get("reason").and_then(|v| v.as_str()).unwrap_or("");
            ("exec".to_string(), format!("{reason}\n$ {cmd}"))
        }
        _ => (method.to_string(), truncate(&params.to_string(), 2000)),
    };
    let s = &mut app.sessions[tab];
    s.stage_diff(PendingDiff {
        file: tool.clone(),
        body,
    });
    // Codex approvals answer with {"decision": <word>} under the request id.
    // Choices below carry the response decision in `choice_id`; the request
    // id rides in `requirement_id` (repurposed string slot).
    s.pending_approval = Some(PendingApproval {
        approval_id: method.to_string(),
        requirement_id: req_id,
        choices: codex_choices(method),
    });
    app.flash = format!("codex {tool} wants approval (y/n/a/q)");
    true
}

/// y/n/a/q → response decisions per approval kind. Returns the server
/// request id plus the bare decision word. `q` ("later") maps to a denial
/// where one exists — the model may re-ask — and to local-close (None)
/// otherwise. Never approves on the user's behalf. (The deferred-vs-denied
/// distinction lives in the transcript line, not the wire.)
pub fn map_codex_decision(
    approval: &PendingApproval,
    kind: DecisionKind,
) -> Option<(Value, String)> {
    use DecisionKind::*;
    let want: &[&str] = match (approval.approval_id.as_str(), kind) {
        ("applyPatchApproval", Approve) | ("execCommandApproval", Approve) => &["approved"],
        // Session-scoped only — never fall back to once-`approved`.
        ("applyPatchApproval", ApproveAll) | ("execCommandApproval", ApproveAll) => {
            &["approved_for_session"]
        }
        ("applyPatchApproval", Reject)
        | ("execCommandApproval", Reject)
        | ("applyPatchApproval", Later)
        | ("execCommandApproval", Later) => &["denied"],
        (_, Approve) => &["accept", "approved", "allow"],
        (_, ApproveAll) => &["acceptForSession", "approved_for_session"],
        (_, Reject) | (_, Later) => &["deny", "denied"],
    };
    // Priority order is the `want` list's: first wanted decision wins.
    let hit = want
        .iter()
        .find_map(|w| approval.choices.iter().find(|c| &c.decision == w))?;
    Some((approval.requirement_id.clone(), hit.decision.clone()))
}

fn codex_choices(method: &str) -> Vec<ApprovalChoice> {
    // Only the patch/exec ReviewDecision vocabulary is proven against the
    // schema. Every other request kind (fileChange variants, permissions,
    // user-input) gets NO choices, so the modal can only be closed locally,
    // sending nothing — never a fabricated answer. M4 maps fileChange live.
    let decisions: &[(&str, &str)] = match method {
        "applyPatchApproval" | "execCommandApproval" => &[
            ("approved", "Allow"),
            ("approved_for_session", "Allow for session"),
            ("denied", "Deny"),
        ],
        _ => &[],
    };
    decisions
        .iter()
        .map(|(d, l)| ApprovalChoice {
            choice_id: d.to_string(),
            decision: d.to_string(),
            scope: "once".to_string(),
            label: l.to_string(),
            accepts_feedback: false,
        })
        .collect()
}

/// True when `lower` is `tail` or ends with `/{tail}` (segment boundary).
/// Rejects glued false positives like `foopatchupdated`.
fn method_segment_tail(lower: &str, tail: &str) -> bool {
    match lower.strip_suffix(tail) {
        Some("") => true,
        Some(rest) => rest.ends_with('/'),
        None => false,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasoning_item_is_thought_not_answer_text() {
        let mut app = App::new();
        app.active_mut().busy = true;
        let reasoning = serde_json::json!({"item": {"type": "reasoning", "text": "weigh options"}});
        apply_codex_notif(&mut app, 0, "item/completed", &reasoning);
        assert_eq!(app.active().thought.trim(), "weigh options");
        assert!(app.active().lines.iter().all(|l| !l.contains("weigh")));
        assert_eq!(app.active().phase, Some(crate::activity::Phase::Thinking));
        let msg = serde_json::json!({"item": {"type": "agentMessage", "text": "answer"}});
        apply_codex_notif(&mut app, 0, "item/completed", &msg);
        let lines = &app.active().lines;
        assert!(lines.iter().any(|l| l.starts_with("◆ Thought for ")));
        assert!(lines.iter().any(|l| l == "answer"));
        assert!(app.active().thought.is_empty());
    }

    fn patch_approval() -> PendingApproval {
        PendingApproval {
            approval_id: "applyPatchApproval".into(),
            requirement_id: serde_json::json!(7),
            choices: codex_choices("applyPatchApproval"),
        }
    }

    #[test]
    fn patch_decisions_map() {
        let a = patch_approval();
        let (_, d) = map_codex_decision(&a, DecisionKind::Approve).unwrap();
        assert_eq!(d, "approved");
        let (_, d) = map_codex_decision(&a, DecisionKind::ApproveAll).unwrap();
        assert_eq!(d, "approved_for_session");
        let (_, d) = map_codex_decision(&a, DecisionKind::Reject).unwrap();
        assert_eq!(d, "denied");
        // Later/q maps to wire `denied` when choices exist (not a true defer).
        let (_, d) = map_codex_decision(&a, DecisionKind::Later).unwrap();
        assert_eq!(d, "denied");
    }

    #[test]
    fn approve_all_without_session_choice_is_none() {
        let mut a = patch_approval();
        a.choices.retain(|c| c.decision != "approved_for_session");
        assert!(map_codex_decision(&a, DecisionKind::ApproveAll).is_none());
        assert!(a.choices.iter().any(|c| c.decision == "approved"));
    }

    #[test]
    fn filechange_vocabulary_maps_when_offered() {
        // The FileChangeApprovalDecision words from the schema: if a live
        // request ever offers them, the mapping below is exact.
        let mut a = patch_approval();
        a.approval_id = "fileChangeApproval".into();
        a.choices = ["accept", "acceptForSession", "deny"]
            .iter()
            .map(|d| ApprovalChoice {
                choice_id: d.to_string(),
                decision: d.to_string(),
                scope: "once".to_string(),
                label: d.to_string(),
                accepts_feedback: false,
            })
            .collect();
        let (_, d) = map_codex_decision(&a, DecisionKind::Approve).unwrap();
        assert_eq!(d, "accept");
        let (_, d) = map_codex_decision(&a, DecisionKind::ApproveAll).unwrap();
        assert_eq!(d, "acceptForSession");
        let (_, d) = map_codex_decision(&a, DecisionKind::Reject).unwrap();
        assert_eq!(d, "deny");
    }

    #[test]
    fn unmapped_request_offers_no_choices() {
        // Unknown kinds (permissions, user-input, fileChange variants)
        // parse to zero choices, so decide_ui can only local-close.
        assert!(codex_choices("permissions").is_empty());
        assert!(codex_choices("fileChangeApproval").is_empty());
        assert_eq!(codex_choices("applyPatchApproval").len(), 3);
    }

    fn tail(app: &App, n: usize) -> Vec<String> {
        let s = &app.sessions[0];
        s.lines[s.lines.len().saturating_sub(n)..].to_vec()
    }

    #[test]
    fn patch_updated_lists_each_file() {
        let mut app = App::new();
        let params = serde_json::json!({
            "threadId": "t", "turnId": "u", "itemId": "i",
            "changes": [
                {"path": "src/a.rs", "kind": {"type": "update"}, "diff": "@@ ..."},
                {"path": "new/b.rs", "kind": {"type": "add"}, "diff": ""},
                {"path": "old/c.rs", "kind": {"type": "delete"}, "diff": ""},
            ]
        });
        assert!(!apply_codex_notif(
            &mut app,
            0,
            "fileChange/patchUpdated",
            &params
        ));
        // S7-F2: every mid-turn write line carries the already-applied note.
        assert_eq!(
            tail(&app, 3),
            vec![
                "codex: ~ src/a.rs (already applied)",
                "codex: + new/b.rs (already applied)",
                "codex: - old/c.rs (already applied)",
            ]
        );
    }

    #[test]
    fn patch_updated_matches_any_prefix_and_empty_changes() {
        let mut app = App::new();
        // Alternate prefix still routes (exact wire prefix unconfirmed live).
        let params = serde_json::json!({"changes": [
            {"path": "x.rs", "kind": {"type": "update"}, "diff": ""}
        ]});
        apply_codex_notif(&mut app, 0, "item/patchUpdated", &params);
        assert_eq!(tail(&app, 1), vec!["codex: ~ x.rs (already applied)"]);
        // Empty change list degrades to a single line, never silence.
        let empty = serde_json::json!({"changes": []});
        apply_codex_notif(&mut app, 0, "fileChange/patchUpdated", &empty);
        assert_eq!(
            tail(&app, 1),
            vec!["codex: files updated (already applied — not gated by y/n)"]
        );
        // Glued false positive must not match.
        let before = app.sessions[0].lines.len();
        assert!(!apply_codex_notif(&mut app, 0, "foopatchUpdated", &params));
        assert_eq!(app.sessions[0].lines.len(), before);
    }

    #[test]
    fn output_delta_stays_silent() {
        let mut app = App::new();
        let before = app.sessions[0].lines.len();
        let params = serde_json::json!({"delta": "bytes…", "itemId": "i"});
        assert!(!apply_codex_notif(
            &mut app,
            0,
            "fileChange/outputDelta",
            &params
        ));
        assert_eq!(app.sessions[0].lines.len(), before);
    }

    #[test]
    fn guardian_reviews_render_one_liners() {
        let mut app = App::new();
        let started = serde_json::json!({
            "action": {"type": "command", "command": "cargo test", "cwd": "/tmp", "source": "agent"},
        });
        apply_codex_notif(&mut app, 0, "itemGuardian/approvalReviewStarted", &started);
        assert_eq!(tail(&app, 1), vec!["codex: reviewing `cargo test`"]);
        let done = serde_json::json!({
            "action": {"type": "command", "command": "cargo test", "cwd": "/tmp", "source": "agent"},
            "review": {"status": "approved", "rationale": "read-only test run"},
        });
        apply_codex_notif(&mut app, 0, "item/autoApprovalReview/completed", &done);
        // S7-F2: approved guardian reviews bypass the modal — noted inline.
        assert_eq!(
            tail(&app, 1),
            vec![
                "codex: guardian approved — `cargo test` (read-only test run) [already applied — no y/n modal]"
            ]
        );
    }

    #[test]
    fn truncate_stays_on_char_boundary() {
        let out = truncate("éééé", 3);
        assert!(out.starts_with('é'));
        assert!(out.contains("truncated"));
        let _ = truncate("你好", 3);
    }

    #[test]
    fn unmapped_kind_sends_nothing() {
        let mut a = patch_approval();
        a.approval_id = "permissions".into();
        a.choices = vec![];
        assert!(map_codex_decision(&a, DecisionKind::Approve).is_none());
    }

    /// Live bringup against a real `codex app-server` with an isolated,
    /// credential-free HOME: handshake + thread opens must succeed (auth
    /// only matters at turn time). Skips loudly without the binary.
    #[tokio::test]
    async fn codex_bringup_no_auth() {
        let bin = std::env::var("CODEX_BIN").unwrap_or_else(|_| "codex".to_string());
        let (tx, _rx) = tokio::sync::mpsc::channel(64);
        let home = std::env::temp_dir().join(format!("pf-codex-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&home);
        let res = codex_bringup(
            &bin,
            vec![(
                "CODEX_HOME".to_string(),
                home.to_string_lossy().into_owned(),
            )],
            tx,
        )
        .await;
        match res {
            Ok(host) => {
                let id = codex_start_thread(&host, Some("gpt-5.6".to_string()), "/tmp".to_string())
                    .await;
                assert!(id.is_ok(), "thread should open without auth");
                host.shutdown().await;
            }
            Err(e) if e.contains("could not spawn") => {
                eprintln!("SKIP codex_bringup_no_auth: {e}");
            }
            Err(e) => panic!("bringup failed for the wrong reason: {e}"),
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    fn codex_app() -> App {
        let mut app = App::new();
        app.active_mut().backend = crate::app::BackendKind::Codex;
        app.active_mut().remote_id = Some("th".into());
        app
    }

    #[test]
    fn token_usage_uses_last_turn_total_only() {
        let mut app = codex_app();
        let p = serde_json::json!({"tokenUsage": {
            "last": {"totalTokens": 2500},
            "total": {"totalTokens": 99000},
        }});
        apply_codex_notif(&mut app, 0, "thread/tokenUsage/updated", &p);
        assert_eq!(app.active().tokens, Some(2500));
        // No `last`: the cumulative total is not a context size.
        let mut app = codex_app();
        let p = serde_json::json!({"tokenUsage": {"total": {"totalTokens": 99000}}});
        apply_codex_notif(&mut app, 0, "thread/tokenUsage/updated", &p);
        assert_eq!(app.active().tokens, None);
    }

    #[test]
    fn turn_started_captures_turn_id_so_stop_can_name_it() {
        let mut app = codex_app();
        app.active_mut().busy = true;
        apply_codex_notif(&mut app, 0, "turn/started", &serde_json::json!({"turn": {"id": "tu-3"}}));
        assert_eq!(app.active().turn_id.as_deref(), Some("tu-3"));
        app.request_stop();
        assert_eq!(app.outbox.stops[0].turn_id.as_deref(), Some("tu-3"));
    }

    #[test]
    fn sealed_tab_ignores_late_active_status() {
        let mut app = codex_app();
        app.active_mut().busy = true;
        app.active_mut().turn_id = Some("tu".into());
        app.request_stop();
        app.request_stop();
        let active = serde_json::json!({"status": {"type": "active"}});
        apply_codex_notif(&mut app, 0, "thread/status/changed", &active);
        assert!(!app.active().busy);
    }
}
