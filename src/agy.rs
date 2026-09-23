//! M4 Antigravity backend: one `agy` child per tab, driven over pipes.
//! Protocol is first-party documented (antigravity.google/docs/cli/headless):
//! spawn `agy --input-format stream-json --output-format stream-json`,
//! write `{"event":"user","message":{"content":...}}` per turn, read
//! `init` → `step_update`* → `result`. Stdout carries the run; diagnostics
//! (incl. permission soft-denials) go to stderr.
//!
//! Vendor constraint (user decision, M4): headless agy has NO interactive
//! approval round-trip — workspace writes auto-allow, Ask-actions soft-deny
//! unless pre-granted in the user's own settings.json (which we never
//! touch). Every tool step is rendered to the transcript (visible), but
//! nothing is gated (ungated). The y/n/a/q modal never fires for agy.

use std::process::Stdio;

use serde_json::Value;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

use crate::app::App;
use crate::msp::{CappedLine, MAX_NDJSON_LINE_BYTES, ServerMsg, read_capped_line};

/// Prompt channel + owned child for one agy tab.
pub struct AgyHandle {
    prompt_tx: mpsc::Sender<String>,
    child: Child,
}

impl AgyHandle {
    pub async fn shutdown(mut self) {
        let _ = self.child.kill().await;
    }
}

/// Spawn the child and its stdout/stderr pumps. Returns before `init`
/// arrives; the conversation id lands via [`apply_agy_notif`].
pub async fn spawn_agy(
    bin: &str,
    args: &[String],
    workspace: &str,
    events_tx: mpsc::Sender<ServerMsg>,
) -> std::io::Result<AgyHandle> {
    let mut child = Command::new(bin)
        .args(args)
        .current_dir(workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");

    let (prompt_tx, mut prompt_rx) = mpsc::channel::<String>(16);
    // Stdin pump: one user event per submitted prompt; closing ends session.
    tokio::spawn(async move {
        let mut stdin = stdin;
        while let Some(prompt) = prompt_rx.recv().await {
            let frame = serde_json::json!({"event": "user", "message": {"content": prompt}});
            if stdin
                .write_all(format!("{frame}\n").as_bytes())
                .await
                .is_err()
            {
                break;
            }
        }
    });
    // Stdout pump: NDJSON run events (S1-F1: line size capped).
    let stdout_tx = events_tx.clone();
    tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);
        let mut scratch = Vec::new();
        loop {
            match read_capped_line(&mut reader, &mut scratch).await {
                Ok(CappedLine::Line(line)) => {
                    let line = line.trim().to_string();
                    if line.is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<Value>(&line) {
                        Ok(v) => {
                            let kind = v.get("event").and_then(|e| e.as_str()).unwrap_or("unknown");
                            let _ = stdout_tx
                                .send(ServerMsg::Notif {
                                    method: format!("agy/{kind}"),
                                    params: v,
                                })
                                .await;
                        }
                        Err(_) => continue, // banners; diagnostics use stderr
                    }
                }
                Ok(CappedLine::Oversize { bytes }) => {
                    let _ = stdout_tx
                        .send(ServerMsg::Transport(format!(
                            "agy stdout: line of {bytes} bytes exceeds \
                             {MAX_NDJSON_LINE_BYTES}-byte cap, dropped"
                        )))
                        .await;
                }
                Ok(CappedLine::InvalidUtf8 { bytes }) => {
                    let _ = stdout_tx
                        .send(ServerMsg::Transport(format!(
                            "agy stdout: line of {bytes} bytes is not UTF-8, dropped"
                        )))
                        .await;
                }
                Ok(CappedLine::Eof) => break,
                Err(_) => break,
            }
        }
    });
    // Stderr pump: permission soft-denials and diagnostics live here.
    // Same S1-F1 cap: stderr is equally child-controlled.
    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut scratch = Vec::new();
        loop {
            match read_capped_line(&mut reader, &mut scratch).await {
                Ok(CappedLine::Line(line)) => {
                    let line = line.trim().to_string();
                    if line.is_empty() {
                        continue;
                    }
                    let _ = events_tx
                        .send(ServerMsg::Notif {
                            method: "agy/stderr".into(),
                            params: Value::String(line),
                        })
                        .await;
                }
                Ok(CappedLine::Oversize { bytes }) => {
                    let _ = events_tx
                        .send(ServerMsg::Transport(format!(
                            "agy stderr: line of {bytes} bytes exceeds \
                             {MAX_NDJSON_LINE_BYTES}-byte cap, dropped"
                        )))
                        .await;
                }
                Ok(CappedLine::InvalidUtf8 { bytes }) => {
                    let _ = events_tx
                        .send(ServerMsg::Transport(format!(
                            "agy stderr: line of {bytes} bytes is not UTF-8, dropped"
                        )))
                        .await;
                }
                Ok(CappedLine::Eof) => break,
                Err(_) => break,
            }
        }
    });

    Ok(AgyHandle { prompt_tx, child })
}

pub async fn agy_submit(handle: &AgyHandle, prompt: String) -> Result<(), String> {
    handle
        .prompt_tx
        .send(prompt)
        .await
        .map_err(|_| "agy session ended".to_string())
}

/// Route one agy frame into App state. Returns true for the bell.
pub fn apply_agy_notif(app: &mut App, tab: usize, method: &str, params: &Value) -> bool {
    if tab >= app.sessions.len() {
        return false;
    }
    match method {
        "agy/init" => {
            let s = &mut app.sessions[tab];
            s.pending_agy_init = false;
            if let Some(id) = params.get("conversation_id").and_then(|v| v.as_str()) {
                s.remote_id = Some(id.to_string());
                let short: String = id.chars().take(8).collect();
                s.push_line(format!("agy: session {short}"));
                app.save_tab_meta(tab);
            }
            false
        }
        "agy/step_update" => {
            let su = params.get("step_update").unwrap_or(params);
            let step_type = su.get("step_type").and_then(|v| v.as_str()).unwrap_or("");
            match step_type {
                "agent_response" => {
                    if let Some(t) = su.get("text_delta").and_then(|v| v.as_str()) {
                        push_text(app, tab, t);
                    }
                    false
                }
                "tool" => {
                    let name = su
                        .get("tool_name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("tool");
                    let info = su.get("tool_info").unwrap_or(&Value::Null);
                    let summary = tool_summary(name, info);
                    app.sessions[tab].push_line(format!("agy {name}: {summary}"));
                    if tab == app.active {
                        app.stick_to_bottom();
                    }
                    false
                }
                _ => false, // user_input echoes, checkpoints: nothing to show
            }
        }
        "agy/result" => {
            let r = params.get("result").unwrap_or(params);
            let status = r.get("status").and_then(|v| v.as_str()).unwrap_or("");
            let s = &mut app.sessions[tab];
            s.busy = false;
            if status == "SUCCESS" {
                if let Some(resp) = r.get("response").and_then(|v| v.as_str()) {
                    for line in resp.split('\n') {
                        if !line.is_empty() {
                            s.push_line(line.to_string());
                        }
                    }
                }
                s.push_line("agy: done ✓".to_string());
            } else {
                s.push_line(format!("agy: turn ended ({status})"));
                app.flash = format!("agy: turn ended ({status})");
            }
            if tab == app.active {
                app.stick_to_bottom();
            }
            true
        }
        "agy/stderr" => {
            // Permission soft-denials surface here: transcript-visible.
            let text = params.as_str().unwrap_or(&params.to_string()).to_string();
            let short = truncate(&text, 300);
            app.sessions[tab].push_line(format!("agy: {short}"));
            app.flash = format!("agy: {short}");
            false
        }
        _ => false,
    }
}

/// S7-F1: hard no-modal warning printed on every agy session open (fresh
/// or resumed). Headless agy has no interactive approval round-trip, so
/// opening the tab trusts the agent with workspace writes.
pub const AGY_NO_MODAL_WARNING: &str = "agy: WARNING — no y/n approval modal on this tab (headless vendor limitation): tools run ungated, workspace writes auto-allow. Opening this session trusts the agent — watch the transcript.";

/// Push the S7-F1 warning onto a freshly opened agy tab. Call only after
/// the child spawned: a failed open has no agent to warn about.
pub fn push_agy_open_notice(s: &mut crate::app::Session, resumed: bool) {
    if resumed {
        s.push_line("agy: continuing previous conversation".to_string());
    }
    s.push_line(AGY_NO_MODAL_WARNING.to_string());
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

/// One-line rendering of a tool step: output on success, error otherwise.
fn tool_summary(name: &str, info: &Value) -> String {
    if let Some(err) = info.get("error") {
        let msg = err
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or(&err.to_string())
            .to_string();
        return truncate(&format!("FAILED {msg}"), 300);
    }
    // Prefer the tool's own output; else echo salient parameters.
    if let Some(out) = info.get("output").and_then(|v| v.as_str()) {
        let first = out.split('\n').next().unwrap_or("").trim();
        if !first.is_empty() {
            return truncate(first, 300);
        }
    }
    if let Some(p) = info.get("parameters") {
        return truncate(&format!("{name} {}", compact_params(p)), 300);
    }
    name.to_string()
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
    use crate::app::{App, BackendKind};

    fn agy_app() -> App {
        let mut app = App::new();
        app.active_mut().backend = BackendKind::Agy;
        app
    }

    /// S7-F1: every agy open warns loudly that no y/n modal exists.
    #[test]
    fn open_notice_warns_no_modal_fresh_and_resumed() {
        use crate::app::Session;
        assert!(AGY_NO_MODAL_WARNING.contains("y/n"));
        assert!(AGY_NO_MODAL_WARNING.contains("modal"));
        let mut fresh = Session::new("agy");
        push_agy_open_notice(&mut fresh, false);
        assert_eq!(fresh.lines, vec![AGY_NO_MODAL_WARNING.to_string()]);
        let mut resumed = Session::new("agy");
        push_agy_open_notice(&mut resumed, true);
        assert_eq!(resumed.lines.len(), 2);
        assert!(
            resumed.lines[0].contains("continuing previous conversation"),
            "resume keeps its context line: {:?}",
            resumed.lines
        );
        assert_eq!(resumed.lines[1], AGY_NO_MODAL_WARNING);
    }

    /// Frames below are shaped exactly like the first-party headless docs.
    #[test]
    fn init_records_conversation() {
        let mut app = agy_app();
        app.active_mut().pending_agy_init = true;
        let p: Value = serde_json::json!({
            "event": "init",
            "conversation_id": "c3b66b04-872b-4fbe-a3a4-058a026ef20a",
            "init": {"cwd": "/tmp", "permission_mode": "request-review"}
        });
        assert!(!apply_agy_notif(&mut app, 0, "agy/init", &p));
        assert_eq!(
            app.active().remote_id.as_deref(),
            Some("c3b66b04-872b-4fbe-a3a4-058a026ef20a")
        );
        assert!(!app.active().pending_agy_init);
        assert!(app.active().lines.iter().any(|l| l.contains("c3b66b04")));
    }

    #[test]
    fn truncate_stays_on_char_boundary() {
        let out = truncate("éééé", 3);
        assert!(out.starts_with('é'));
        assert!(out.contains("truncated"));
        let _ = truncate("你好", 3);
    }

    #[test]
    fn agent_deltas_stream_to_transcript() {
        let mut app = agy_app();
        for t in ["Git rebase ", "rewrites history.\n"] {
            let p: Value = serde_json::json!({
                "event": "step_update",
                "step_update": {"step_type": "agent_response", "text_delta": t, "state": "DONE"}
            });
            assert!(!apply_agy_notif(&mut app, 0, "agy/step_update", &p));
        }
        assert!(
            app.active()
                .lines
                .iter()
                .any(|l| l.contains("rewrites history"))
        );
    }

    #[test]
    fn tool_steps_are_visible_with_outcome() {
        let mut app = agy_app();
        let p: Value = serde_json::json!({
            "event": "step_update",
            "step_update": {"step_type": "tool", "tool_name": "run_command",
                "tool_info": {"name": "run_command",
                    "parameters": {"CommandLine": "echo hi"},
                    "output": "hi\r\n"}}
        });
        apply_agy_notif(&mut app, 0, "agy/step_update", &p);
        assert!(
            app.active()
                .lines
                .iter()
                .any(|l| l.contains("run_command") && l.contains("hi"))
        );
        let fail: Value = serde_json::json!({
            "event": "step_update",
            "step_update": {"step_type": "tool", "tool_name": "write_to_file",
                "tool_info": {"error": {"type": "denied", "message": "needs allow(path)"}}}
        });
        apply_agy_notif(&mut app, 0, "agy/step_update", &fail);
        assert!(
            app.active()
                .lines
                .iter()
                .any(|l| l.contains("FAILED") && l.contains("needs allow"))
        );
    }

    #[test]
    fn result_ends_turn_with_bell() {
        let mut app = agy_app();
        app.active_mut().busy = true;
        let p: Value = serde_json::json!({
            "event": "result",
            "result": {"status": "SUCCESS", "response": "apple\n", "num_turns": 1}
        });
        assert!(apply_agy_notif(&mut app, 0, "agy/result", &p));
        assert!(!app.active().busy);
        assert!(app.active().lines.iter().any(|l| l == "agy: done ✓"));
    }

    #[test]
    fn stderr_notice_is_transcript_visible() {
        let mut app = agy_app();
        let p = Value::String("run_command needs allow(shell) — soft-denied".into());
        assert!(!apply_agy_notif(&mut app, 0, "agy/stderr", &p));
        assert!(!app.flash.is_empty());
        assert!(app.active().lines.iter().any(|l| l.contains("soft-denied")));
    }

    /// Deterministic everywhere: a missing binary is a clean spawn error,
    /// never a hang or panic.
    #[tokio::test]
    async fn spawn_failure_is_clean() {
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let res = spawn_agy("/nonexistent/agy-xyz", &[], "/tmp", tx).await;
        assert!(res.is_err());
    }
}
