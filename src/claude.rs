//! Claude Code backend: one `claude -p` child per tab, driven over pipes.
//! Spawn `claude -p --input-format stream-json --output-format stream-json
//! --verbose`, write `{"type":"user","message":{...}}` per turn, read
//! `system` / `assistant` / `user` (tool results) / `result` frames.
//!
//! The session id is chosen here (`--session-id`, a fresh UUID) or passed
//! back on resume (`--resume`, which keeps the id), so every frame routes
//! by its `session_id` — no init FIFO. Each spawn stamps `generation` so a
//! killed child's late frames miss the replacement. Stderr lines are stamped
//! with the same id and generation by the pump.
//!
//! Approvals: `--permission-prompt-tool stdio` makes claude ask before any
//! tool the user's Claude settings don't pre-allow — a `control_request`
//! (`can_use_tool`) on stdout, answered by a `control_response` on stdin.
//! The y/n/a/q modal answers it, like the codex/grok paths.

use std::collections::HashMap;
use std::process::Stdio;

use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

use crate::app::{App, DecisionKind, OutboxDecide, PendingApproval, PendingDiff};
use crate::msp::{CappedLine, MAX_NDJSON_LINE_BYTES, ServerMsg, read_capped_line};

/// Stdin frame channel + owned child for one claude tab.
pub struct ClaudeHandle {
    frame_tx: mpsc::Sender<Value>,
    child: Child,
    /// Spawn counter copied onto every frame this child emits.
    pub generation: u64,
}

impl ClaudeHandle {
    pub async fn shutdown(mut self) {
        let _ = self.child.kill().await;
    }
}

/// Live claude children, keyed by session id (ids survive tab renumbering).
pub type ClaudeChildren = HashMap<String, ClaudeHandle>;

/// Spawn the child for `session_id` (fresh unless `resume`) and its pumps.
/// `generation` is stamped on every frame so a replaced child cannot
/// address the tab that now owns `session_id`.
pub async fn spawn_claude(
    bin: &str,
    extra_args: &[String],
    workspace: &str,
    session_id: &str,
    resume: bool,
    generation: u64,
    events_tx: mpsc::Sender<ServerMsg>,
) -> std::io::Result<ClaudeHandle> {
    let mut args: Vec<String> = [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--permission-prompt-tool",
        "stdio",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    args.push(if resume { "--resume" } else { "--session-id" }.to_string());
    args.push(session_id.to_string());
    args.extend_from_slice(extra_args);
    let mut child = Command::new(bin)
        .args(&args)
        .current_dir(workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");

    let (frame_tx, mut frame_rx) = mpsc::channel::<Value>(16);
    // Stdin pump: user messages + control responses; closing ends session.
    tokio::spawn(async move {
        let mut stdin = stdin;
        while let Some(frame) = frame_rx.recv().await {
            if stdin
                .write_all(format!("{frame}\n").as_bytes())
                .await
                .is_err()
            {
                break;
            }
        }
    });
    let sid = session_id.to_string();
    tokio::spawn(pump(
        stdout,
        "stdout",
        sid.clone(),
        generation,
        events_tx.clone(),
    ));
    tokio::spawn(pump(stderr, "stderr", sid, generation, events_tx));
    Ok(ClaudeHandle {
        frame_tx,
        child,
        generation,
    })
}

/// True only when both sides name the same live spawn.
/// A missing generation (or a missing live child) does not match.
pub fn claude_frame_matches(live: Option<u64>, frame: Option<u64>) -> bool {
    matches!((live, frame), (Some(live), Some(frame)) if live == frame)
}

/// Line pump (S1-F1: line size capped). Stdout NDJSON becomes
/// `claude/<type>`; stderr text becomes `claude/stderr`. Stdout EOF sends
/// `claude/exit` so a dead child never leaves its tab busy.
async fn pump<R: AsyncRead + Unpin>(
    src: R,
    stream: &'static str,
    sid: String,
    generation: u64,
    tx: mpsc::Sender<ServerMsg>,
) {
    let mut reader = BufReader::new(src);
    let mut scratch = Vec::new();
    loop {
        let msg = match read_capped_line(&mut reader, &mut scratch).await {
            Ok(CappedLine::Line(line)) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if stream == "stderr" {
                    ServerMsg::Notif {
                        method: "claude/stderr".into(),
                        params: serde_json::json!({
                            "session_id": sid,
                            "generation": generation,
                            "text": line
                        }),
                    }
                } else {
                    let Ok(mut v) = serde_json::from_str::<Value>(line) else {
                        continue; // banners; diagnostics use stderr
                    };
                    // Control frames carry no session_id; stamp ours so
                    // they route like every other frame. Generation is
                    // ours, never the child's (a late frame must not
                    // claim the replacement spawn).
                    if let Some(o) = v.as_object_mut() {
                        o.entry("session_id").or_insert_with(|| sid.clone().into());
                        o.insert("generation".to_string(), generation.into());
                    }
                    let kind = v.get("type").and_then(|t| t.as_str()).unwrap_or("unknown");
                    ServerMsg::Notif {
                        method: format!("claude/{kind}"),
                        params: v,
                    }
                }
            }
            Ok(CappedLine::Oversize { bytes }) => ServerMsg::Transport(format!(
                "claude {stream}: line of {bytes} bytes exceeds \
                 {MAX_NDJSON_LINE_BYTES}-byte cap, dropped"
            )),
            Ok(CappedLine::InvalidUtf8 { bytes }) => ServerMsg::Transport(format!(
                "claude {stream}: line of {bytes} bytes is not UTF-8, dropped"
            )),
            Ok(CappedLine::Eof) | Err(_) => break,
        };
        if tx.send(msg).await.is_err() {
            return;
        }
    }
    if stream == "stdout" {
        let _ = tx
            .send(ServerMsg::Notif {
                method: "claude/exit".into(),
                params: serde_json::json!({"session_id": sid, "generation": generation}),
            })
            .await;
    }
}

pub async fn claude_submit(handle: &ClaudeHandle, prompt: String) -> Result<(), String> {
    claude_send(
        handle,
        serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": prompt},
        }),
    )
    .await
}

/// Write one raw frame (e.g. a `control_response`) to the child's stdin.
pub async fn claude_send(handle: &ClaudeHandle, frame: Value) -> Result<(), String> {
    handle
        .frame_tx
        .send(frame)
        .await
        .map_err(|_| "claude session ended".to_string())
}

/// Route one claude frame into App state. Returns true for the bell.
pub fn apply_claude_notif(app: &mut App, tab: usize, method: &str, params: &Value) -> bool {
    if tab >= app.sessions.len() {
        return false;
    }
    match method {
        "claude/assistant" => {
            let blocks = params
                .pointer("/message/content")
                .and_then(|c| c.as_array())
                .cloned()
                .unwrap_or_default();
            for b in &blocks {
                match b.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        let t = b.get("text").and_then(|v| v.as_str()).unwrap_or("");
                        let s = &mut app.sessions[tab];
                        s.flush_thought();
                        s.set_phase(crate::activity::Phase::Responding);
                        push_text(app, tab, t);
                    }
                    Some("tool_use") => {
                        app.sessions[tab].flush_thought();
                        let name = b.get("name").and_then(|v| v.as_str()).unwrap_or("tool");
                        let input = b.get("input").unwrap_or(&Value::Null);
                        let line =
                            truncate(&format!("claude {name}: {}", compact_params(input)), 300);
                        app.sessions[tab].push_line(line);
                    }
                    // Thinking blocks feed the live thinking block.
                    Some("thinking") => {
                        let s = &mut app.sessions[tab];
                        s.set_phase(crate::activity::Phase::Thinking);
                        if let Some(t) = b.get("thinking").and_then(|v| v.as_str()) {
                            s.push_thought(&format!("{t}\n"));
                        }
                    }
                    _ => {}
                }
            }
            if tab == app.active {
                app.stick_to_bottom();
            }
            false
        }
        "claude/user" => {
            // Tool results echo back as user frames; surface failures only.
            let blocks = params
                .pointer("/message/content")
                .and_then(|c| c.as_array())
                .cloned()
                .unwrap_or_default();
            for b in &blocks {
                let failed = b.get("type").and_then(|t| t.as_str()) == Some("tool_result")
                    && b.get("is_error").and_then(|v| v.as_bool()) == Some(true);
                if failed {
                    let msg = tool_result_text(b.get("content").unwrap_or(&Value::Null));
                    let first = msg.split('\n').next().unwrap_or("").trim();
                    app.sessions[tab]
                        .push_line(truncate(&format!("claude: tool FAILED {first}"), 300));
                }
            }
            false
        }
        "claude/result" => {
            let s = &mut app.sessions[tab];
            s.flush_thought();
            s.busy = false;
            if s.pending_approval.take().is_some() {
                s.clear_diff();
            }
            let denials = params
                .get("permission_denials")
                .and_then(|d| d.as_array())
                .cloned()
                .unwrap_or_default();
            for d in &denials {
                let name = d
                    .get("tool_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("tool");
                s.push_line(format!("claude: denied {name}"));
            }
            if params.get("is_error").and_then(|v| v.as_bool()) == Some(true) {
                let sub = params
                    .get("subtype")
                    .and_then(|v| v.as_str())
                    .unwrap_or("error");
                let why = params.get("result").and_then(|v| v.as_str()).unwrap_or("");
                let line = truncate(&format!("claude: turn ended ({sub}) {why}"), 300);
                s.push_line(line.clone());
                app.flash = line;
            } else {
                s.push_line("claude: done ✓".to_string());
            }
            if tab == app.active {
                app.stick_to_bottom();
            }
            true
        }
        "claude/stderr" => {
            let text = params.get("text").and_then(|v| v.as_str()).unwrap_or("");
            let short = truncate(text, 300);
            app.sessions[tab].push_line(format!("claude: {short}"));
            app.flash = format!("claude: {short}");
            false
        }
        "claude/exit" => {
            let s = &mut app.sessions[tab];
            s.flush_thought();
            s.busy = false;
            if s.pending_approval.take().is_some() {
                s.clear_diff();
            }
            s.push_line("claude: session ended — press P to respawn".to_string());
            false
        }
        "claude/control_request" => apply_claude_permission(app, tab, params),
        "claude/control_cancel_request" => {
            // Claude abandoned the ask (e.g. interrupted): retire its card.
            let rid = params.get("request_id").and_then(|v| v.as_str());
            let s = &mut app.sessions[tab];
            if rid.is_some() && s.pending_approval.as_ref().map(|a| a.approval_id.as_str()) == rid {
                s.pending_approval = None;
                s.clear_diff();
                s.push_line("claude: approval withdrawn".to_string());
            }
            false
        }
        _ => false, // system (init/hooks), rate_limit_event, stream noise
    }
}

/// Stage a `can_use_tool` request as a DIFF card + live approval handle.
/// `AskUserQuestion` isn't a y/n decision: deny it with a note so claude
/// asks in plain text instead. Other control subtypes are ignored.
fn apply_claude_permission(app: &mut App, tab: usize, params: &Value) -> bool {
    let req = params.get("request").unwrap_or(&Value::Null);
    if req.get("subtype").and_then(|v| v.as_str()) != Some("can_use_tool") {
        return false;
    }
    let rid = params
        .get("request_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let sid = params
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let tool = req
        .get("tool_name")
        .and_then(|v| v.as_str())
        .unwrap_or("tool");
    let input = req
        .get("input")
        .cloned()
        .unwrap_or(Value::Object(Default::default()));
    if tool == "AskUserQuestion" {
        let frame = deny_frame(
            &rid,
            "polyforge can't show question forms — ask in plain text",
            false,
        );
        queue_claude_frame(app, tab, &sid, frame);
        app.sessions[tab]
            .push_line("claude: question form declined — asked to use plain text".into());
        return false;
    }
    let desc = req
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let pretty = serde_json::to_string_pretty(&input).unwrap_or_default();
    let body = if desc.is_empty() {
        truncate(&pretty, 1200)
    } else {
        format!("{desc}\n{}", truncate(&pretty, 1200))
    };
    let s = &mut app.sessions[tab];
    s.stage_diff(PendingDiff {
        file: tool.to_string(),
        body,
    });
    s.pending_approval = Some(PendingApproval {
        approval_id: rid,
        requirement_id: serde_json::json!({
            "session_id": sid,
            "input": input,
            "suggestions": req.get("permission_suggestions").cloned().unwrap_or(Value::Null),
        }),
        choices: Vec::new(),
    });
    app.flash = format!("claude {tool} wants approval (y/n/a/q)");
    if tab == app.active {
        app.stick_to_bottom();
    }
    true
}

/// y/n/a/q → `control_response` frame. `a` also applies claude's own
/// permission suggestions (the "don't ask again" rules); `q` denies and
/// interrupts the turn so nothing proceeds while you think.
pub fn map_claude_decision(approval: &PendingApproval, kind: DecisionKind) -> Value {
    let rid = &approval.approval_id;
    let req = &approval.requirement_id;
    let input = req.get("input").cloned().unwrap_or(Value::Null);
    match kind {
        DecisionKind::Approve => allow_frame(rid, input, None),
        DecisionKind::ApproveAll => {
            let rules = req
                .get("suggestions")
                .filter(|v| v.as_array().is_some_and(|a| !a.is_empty()))
                .cloned();
            allow_frame(rid, input, rules)
        }
        DecisionKind::Reject => deny_frame(rid, "The user rejected this tool use.", false),
        DecisionKind::Later => deny_frame(rid, "The user deferred; stop and wait.", true),
    }
}

/// Queue a claude stdin frame; `approval_id` carries the session id so the
/// reply reaches the right child even if tabs renumber.
pub fn queue_claude_frame(app: &mut App, tab: usize, sid: &str, frame: Value) {
    app.outbox.decides.push(OutboxDecide {
        tab,
        backend: crate::app::BackendKind::Claude,
        approval_id: sid.to_string(),
        choice_id: frame.to_string(),
        ..Default::default()
    });
}

fn allow_frame(rid: &str, input: Value, rules: Option<Value>) -> Value {
    let mut r = serde_json::json!({"behavior": "allow", "updatedInput": input});
    if let Some(rules) = rules {
        r["updatedPermissions"] = rules;
    }
    control_response(rid, r)
}

fn deny_frame(rid: &str, message: &str, interrupt: bool) -> Value {
    control_response(
        rid,
        serde_json::json!({"behavior": "deny", "message": message, "interrupt": interrupt}),
    )
}

fn control_response(rid: &str, response: Value) -> Value {
    serde_json::json!({
        "type": "control_response",
        "response": {"subtype": "success", "request_id": rid, "response": response},
    })
}

fn tool_result_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        other => other.to_string(),
    }
}

fn push_text(app: &mut App, tab: usize, t: &str) {
    let s = &mut app.sessions[tab];
    for line in t.split('\n') {
        if !line.is_empty() {
            s.push_line(line.to_string());
        }
    }
}

fn compact_params(p: &Value) -> String {
    match p {
        Value::Object(m) => m
            .iter()
            .take(3)
            .map(|(k, v)| {
                let vs = match v {
                    Value::String(s) => truncate(s, 80),
                    _ => truncate(&v.to_string(), 80),
                };
                format!("{k}={vs}")
            })
            .collect::<Vec<_>>()
            .join(" "),
        _ => truncate(&p.to_string(), 120),
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
    use crate::app::{App, BackendKind, DecisionKind};
    use serde_json::json;

    fn claude_app() -> App {
        let mut app = App::new();
        app.active_mut().backend = BackendKind::Claude;
        app.active_mut().remote_id = Some("sid-1".into());
        app
    }

    #[test]
    fn thinking_text_is_live_then_collapses() {
        let mut app = claude_app();
        app.active_mut().busy = true;
        let p = json!({"type": "assistant", "session_id": "sid-1", "message": {"content": [
            {"type": "thinking", "thinking": "first idea\nsecond idea"},
        ]}});
        apply_claude_notif(&mut app, 0, "claude/assistant", &p);
        let draft = app.active().stream_draft_lines();
        assert_eq!(draft.len(), 3);
        assert_eq!(draft[2], "  second idea");
        assert!(app.active().lines.iter().all(|l| !l.contains("idea")));
        let t = json!({"type": "assistant", "session_id": "sid-1", "message": {"content": [
            {"type": "text", "text": "hi"},
        ]}});
        apply_claude_notif(&mut app, 0, "claude/assistant", &t);
        let lines = &app.active().lines;
        assert!(lines.iter().any(|l| l.starts_with("∴ Thought for ")));
        assert!(lines.iter().all(|l| !l.contains("idea")));
    }

    #[test]
    fn assistant_text_sets_responding_phase() {
        let mut app = claude_app();
        app.active_mut().busy = true;
        let p = json!({"type": "assistant", "session_id": "sid-1", "message": {"content": [
            {"type": "text", "text": "hi"},
        ]}});
        apply_claude_notif(&mut app, 0, "claude/assistant", &p);
        assert_eq!(app.active().phase, Some(crate::activity::Phase::Responding));
    }

    #[test]
    fn assistant_text_and_tool_use_render() {
        let mut app = claude_app();
        let p = json!({"type": "assistant", "session_id": "sid-1", "message": {"content": [
            {"type": "text", "text": "hello\nworld"},
            {"type": "tool_use", "name": "Bash", "input": {"command": "ls"}},
            {"type": "thinking", "thinking": "hidden"},
        ]}});
        assert!(!apply_claude_notif(&mut app, 0, "claude/assistant", &p));
        let lines = &app.active().lines;
        assert!(lines.iter().any(|l| l == "hello"));
        assert!(lines.iter().any(|l| l == "world"));
        assert!(lines.iter().any(|l| l == "claude Bash: command=ls"));
        assert!(!lines.iter().any(|l| l.contains("hidden")));
    }

    #[test]
    fn failed_tool_result_is_surfaced() {
        let mut app = claude_app();
        let ok = json!({"message": {"content": [
            {"type": "tool_result", "content": "fine", "is_error": false}]}});
        apply_claude_notif(&mut app, 0, "claude/user", &ok);
        assert!(app.active().lines.is_empty(), "successes stay quiet");
        let bad = json!({"message": {"content": [
            {"type": "tool_result", "content": [{"type": "text", "text": "boom\nmore"}], "is_error": true}]}});
        apply_claude_notif(&mut app, 0, "claude/user", &bad);
        assert_eq!(
            app.active().lines,
            vec!["claude: tool FAILED boom".to_string()]
        );
    }

    #[test]
    fn result_clears_busy_rings_and_lists_denials() {
        let mut app = claude_app();
        app.active_mut().busy = true;
        let p = json!({"type": "result", "subtype": "success", "is_error": false,
            "permission_denials": [{"tool_name": "Write"}]});
        assert!(apply_claude_notif(&mut app, 0, "claude/result", &p));
        assert!(!app.active().busy);
        let lines = &app.active().lines;
        assert!(lines.iter().any(|l| l.starts_with("claude: denied Write")));
        assert!(lines.iter().any(|l| l == "claude: done ✓"));
    }

    #[test]
    fn error_result_flashes() {
        let mut app = claude_app();
        let p = json!({"subtype": "error_max_turns", "is_error": true});
        assert!(apply_claude_notif(&mut app, 0, "claude/result", &p));
        assert!(app.flash.contains("error_max_turns"));
    }

    #[test]
    fn exit_clears_busy() {
        let mut app = claude_app();
        app.active_mut().busy = true;
        apply_claude_notif(&mut app, 0, "claude/exit", &json!({"session_id": "sid-1"}));
        assert!(!app.active().busy);
        assert!(app.active().lines[0].contains("session ended"));
    }

    fn can_use_tool(tool: &str) -> Value {
        json!({"type": "control_request", "request_id": "r-1", "session_id": "sid-1",
            "request": {"subtype": "can_use_tool", "tool_name": tool,
                "input": {"file_path": "/tmp/x", "content": "hi"}, "description": "x",
                "permission_suggestions": [{"type": "setMode", "mode": "acceptEdits", "destination": "session"}]}})
    }

    #[test]
    fn can_use_tool_stages_card_and_maps_decisions() {
        let mut app = claude_app();
        assert!(apply_claude_notif(
            &mut app,
            0,
            "claude/control_request",
            &can_use_tool("Write")
        ));
        let s = app.active();
        assert_eq!(s.pending_diff.as_ref().unwrap().file, "Write");
        let a = s.pending_approval.clone().unwrap();
        assert_eq!(a.approval_id, "r-1");

        let y = map_claude_decision(&a, DecisionKind::Approve);
        assert_eq!(y["type"], "control_response");
        assert_eq!(y["response"]["request_id"], "r-1");
        assert_eq!(y["response"]["response"]["behavior"], "allow");
        assert_eq!(y["response"]["response"]["updatedInput"]["content"], "hi");
        assert!(
            y["response"]["response"]
                .get("updatedPermissions")
                .is_none()
        );

        let all = map_claude_decision(&a, DecisionKind::ApproveAll);
        assert_eq!(
            all["response"]["response"]["updatedPermissions"][0]["mode"],
            "acceptEdits"
        );

        let n = map_claude_decision(&a, DecisionKind::Reject);
        assert_eq!(n["response"]["response"]["behavior"], "deny");
        assert_eq!(n["response"]["response"]["interrupt"], false);
        let q = map_claude_decision(&a, DecisionKind::Later);
        assert_eq!(q["response"]["response"]["interrupt"], true);
    }

    #[test]
    fn ask_user_question_is_denied_without_card() {
        let mut app = claude_app();
        assert!(!apply_claude_notif(
            &mut app,
            0,
            "claude/control_request",
            &can_use_tool("AskUserQuestion")
        ));
        assert!(app.active().pending_diff.is_none());
        assert_eq!(app.outbox.decides.len(), 1);
        assert_eq!(app.outbox.decides[0].approval_id, "sid-1");
        let f: Value = serde_json::from_str(&app.outbox.decides[0].choice_id).unwrap();
        assert_eq!(f["response"]["response"]["behavior"], "deny");
    }

    #[test]
    fn cancel_and_result_retire_card() {
        let mut app = claude_app();
        apply_claude_notif(
            &mut app,
            0,
            "claude/control_request",
            &can_use_tool("Write"),
        );
        let other = json!({"request_id": "r-other"});
        apply_claude_notif(&mut app, 0, "claude/control_cancel_request", &other);
        assert!(
            app.active().pending_diff.is_some(),
            "other ids leave the card"
        );
        apply_claude_notif(
            &mut app,
            0,
            "claude/control_cancel_request",
            &json!({"request_id": "r-1"}),
        );
        assert!(app.active().pending_diff.is_none());
        assert!(app.active().pending_approval.is_none());

        apply_claude_notif(
            &mut app,
            0,
            "claude/control_request",
            &can_use_tool("Write"),
        );
        apply_claude_notif(&mut app, 0, "claude/result", &json!({"is_error": false}));
        assert!(app.active().pending_diff.is_none());
    }

    #[test]
    fn claude_frame_matches_requires_equal_generations() {
        assert!(claude_frame_matches(Some(7), Some(7)));
        assert!(!claude_frame_matches(Some(7), Some(8)));
        assert!(!claude_frame_matches(None, Some(7)));
        assert!(!claude_frame_matches(Some(7), None));
        assert!(!claude_frame_matches(None, None));
    }

    #[tokio::test]
    async fn spawn_failure_is_an_error() {
        let (tx, _rx) = mpsc::channel(1);
        let res = spawn_claude("/nonexistent/claude-xyz", &[], "/tmp", "sid", false, 0, tx).await;
        assert!(res.is_err());
    }

    /// Round trip through a fake `claude`: args carry the session id, the
    /// prompt arrives as a user frame, stdout/stderr come back tagged, and
    /// EOF yields `claude/exit`.
    #[cfg(unix)]
    #[tokio::test]
    async fn fake_child_round_trip() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("pf-claude-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("claude");
        std::fs::write(
            &bin,
            "#!/bin/sh\nread line\necho \"argv: $*\" >&2\n\
             case \"$line\" in *'\"type\":\"user\"'*) ;; *) exit 1;; esac\n\
             echo '{\"type\":\"control_request\",\"request_id\":\"r\"}'\n\
             echo '{\"type\":\"result\",\"session_id\":\"sid-9\",\"is_error\":false}'\n",
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (tx, mut rx) = mpsc::channel(16);
        let h = spawn_claude(bin.to_str().unwrap(), &[], "/tmp", "sid-9", false, 7, tx)
            .await
            .expect("spawn");
        claude_submit(&h, "hi".into()).await.expect("submit");
        let mut methods = Vec::new();
        while let Some(ServerMsg::Notif { method, params }) = rx.recv().await {
            assert_eq!(params["session_id"], "sid-9");
            assert_eq!(params["generation"], 7);
            if method == "claude/stderr" {
                assert!(
                    params["text"]
                        .as_str()
                        .unwrap()
                        .contains("--permission-prompt-tool stdio")
                );
                assert!(
                    params["text"]
                        .as_str()
                        .unwrap()
                        .contains("--session-id sid-9")
                );
            }
            methods.push(method);
            if methods.len() == 4 {
                break;
            }
        }
        methods.sort();
        assert_eq!(
            methods,
            [
                "claude/control_request",
                "claude/exit",
                "claude/result",
                "claude/stderr"
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
