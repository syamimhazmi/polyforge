//! M2 MSP client: newline-delimited JSON-RPC 2.0 over `muse serve` stdio.
//! Shapes follow the offline `muse schema` export (stable surface v1).
//! Unknown fields are ignored so additive server evolution can't break us.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Clone)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "msp error {}: {}", self.code, self.message)
    }
}

impl std::error::Error for RpcError {}

/// Server-originated frames the provider layer must route.
#[derive(Debug)]
pub enum ServerMsg {
    Notif { method: String, params: Value },
    /// Server→client request (e.g. Codex approvals): must be answered with
    /// [`Host::respond`]; the id space is the server's own.
    Request { id: Value, method: String, params: Value },
    /// Transport/parse failure or an id-less error frame.
    Transport(String),
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, RpcError>>>>>;

pub struct Host {
    next_id: AtomicU64,
    writer: Arc<tokio::sync::Mutex<ChildStdin>>,
    pending: Pending,
    child: Child,
}

impl Host {
    /// Spawn `bin serve ...` and pump stdout lines into `events_tx`.
    /// Returns the host plus the notification receiver for the main loop.
    pub async fn spawn(
        bin: &str,
        args: &[&str],
        extra_env: &[(String, String)],
        events_tx: mpsc::Sender<ServerMsg>,
    ) -> std::io::Result<Self> {
        let mut child = Command::new(bin)
            .args(args)
            .envs(extra_env.iter().cloned())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let pump_pending = pending.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => route_line(&line, &pump_pending, &events_tx).await,
                    Ok(None) => {
                        let _ = events_tx
                            .send(ServerMsg::Transport("serve stdout closed".into()))
                            .await;
                        break;
                    }
                    Err(e) => {
                        let _ = events_tx
                            .send(ServerMsg::Transport(format!("serve read: {e}")))
                            .await;
                        break;
                    }
                }
            }
        });
        Ok(Self {
            next_id: AtomicU64::new(1),
            writer: Arc::new(tokio::sync::Mutex::new(stdin)),
            pending,
            child,
        })
    }

    pub async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let frame = serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let mut w = self.writer.lock().await;
        if let Err(e) = w.write_all(format!("{frame}\n").as_bytes()).await {
            self.pending.lock().unwrap().remove(&id);
            return Err(RpcError {
                code: -32000,
                message: format!("serve write: {e}"),
            });
        }
        rx.await.unwrap_or(Err(RpcError {
            code: -32000,
            message: "serve call dropped".into(),
        }))
    }

    /// Answer a server→client request (Codex approvals).
    pub async fn respond(&self, id: Value, result: Value) -> Result<(), RpcError> {
        let frame = serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result});
        let mut w = self.writer.lock().await;
        w.write_all(format!("{frame}\n").as_bytes())
            .await
            .map_err(|e| RpcError {
                code: -32000,
                message: format!("serve write: {e}"),
            })
    }

    pub async fn notify(&self, method: &str) {
        let frame = serde_json::json!({"jsonrpc": "2.0", "method": method});
        let mut w = self.writer.lock().await;
        let _ = w.write_all(format!("{frame}\n").as_bytes()).await;
    }

    /// Full bring-up: initialize + initialized. Returns server info value.
    pub async fn handshake(&self, client: &str, version: &str) -> Result<Value, RpcError> {
        let res = self
            .call(
                "initialize",
                serde_json::json!({"clientInfo": {"name": client, "version": version}}),
            )
            .await?;
        self.notify("initialized").await;
        Ok(res)
    }

    pub async fn shutdown(mut self) {
        let _ = self.child.kill().await;
    }
}

async fn route_line(
    line: &str,
    pending: &Pending,
    events_tx: &mpsc::Sender<ServerMsg>,
) {
    let line = line.trim();
    if line.is_empty() {
        return;
    }
    let v: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return, // stdout banners etc. (serve logs on stderr)
    };
    if let Some(method) = v.get("method").and_then(|m| m.as_str()) {
        let params = v.get("params").cloned().unwrap_or(Value::Null);
        // A method frame WITH an id is a server→client request (Codex
        // approvals); without one it is a plain notification.
        let msg = match v.get("id") {
            Some(id) if !id.is_null() => ServerMsg::Request {
                id: id.clone(),
                method: method.to_string(),
                params,
            },
            _ => ServerMsg::Notif {
                method: method.to_string(),
                params,
            },
        };
        let _ = events_tx.send(msg).await;
        return;
    }
    if let Some(id) = v.get("id").and_then(|i| i.as_u64()) {
        let tx = pending.lock().unwrap().remove(&id);
        if let Some(tx) = tx {
            let res = if let Some(err) = v.get("error") {
                Err(RpcError {
                    code: err.get("code").and_then(|c| c.as_i64()).unwrap_or(-32000),
                    message: err
                        .get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("unknown error")
                        .to_string(),
                })
            } else {
                Ok(v.get("result").cloned().unwrap_or(Value::Null))
            };
            let _ = tx.send(res);
        }
        return;
    }
    // id-less error frame: surface it, don't drop it.
    if v.get("error").is_some() {
        let _ = events_tx
            .send(ServerMsg::Transport(format!("serve: {v}")))
            .await;
    }
}

/// RFC 9562 UUIDv7 (required for MSP commandId idempotency handles).
pub fn uuid7() -> String {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let mut buf = [0u8; 16];
    // Fill from OS randomness; fall back to time-mixed counter (still unique
    // per process since ms advances). getrandom crate avoided for one call.
    #[cfg(unix)]
    {
        use std::io::Read;
        let _ = std::fs::File::open("/dev/urandom").map(|mut f| f.read_exact(&mut buf));
    }
    #[cfg(not(unix))]
    {
        let t = std::time::SystemTime::now();
        let h = std::collections::hash_map::DefaultHasher::new();
        let _ = (t, h);
    }
    buf[0..6].copy_from_slice(&ms.to_be_bytes()[2..8]);
    buf[6] = (buf[6] & 0x0f) | 0x70; // version 7
    buf[8] = (buf[8] & 0x3f) | 0x80; // variant 10
    let h: String = buf.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

/// Best-effort text out of item/delta-style notification params.
/// M2 heuristic: first string under a `text`/`delta` key, depth-first.
pub fn extract_text(params: &Value) -> Option<String> {
    fn walk(v: &Value) -> Option<String> {
        match v {
            Value::String(_) => None,
            Value::Array(a) => a.iter().find_map(walk),
            Value::Object(m) => {
                for key in ["text", "delta"] {
                    if let Some(Value::String(s)) = m.get(key) {
                        if !s.is_empty() {
                            return Some(s.clone());
                        }
                    }
                }
                m.values().find_map(walk)
            }
            _ => None,
        }
    }
    walk(params)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid7_parses_and_sorts() {
        let a = uuid7();
        let b = uuid7();
        for id in [&a, &b] {
            assert_eq!(id.len(), 36);
            assert_eq!(&id[14..15], "7", "version nibble: {id}");
            assert!(matches!(&id[19..20], "8" | "9" | "a" | "b"), "variant: {id}");
        }
        // Same-millisecond ids are unordered by design; only the 48-bit
        // timestamp prefix is non-decreasing.
        let ts = |id: &str| u64::from_str_radix(&id.replace('-', "")[..12], 16).unwrap();
        assert!(ts(&a) <= ts(&b), "v7 timestamp went backwards");
    }

    #[test]
    fn extract_text_prefers_text_keys() {
        let p: Value = serde_json::json!({
            "sessionId": "x",
            "item": {"kind": "assistant", "text": "hello there"},
            "noise": 1
        });
        assert_eq!(extract_text(&p).as_deref(), Some("hello there"));
        let q: Value = serde_json::json!({"kind": "thing", "n": 2});
        assert_eq!(extract_text(&q), None);
    }
}
