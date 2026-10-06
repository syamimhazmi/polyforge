//! M2 MSP client: newline-delimited JSON-RPC 2.0 over `muse serve` stdio.
//! Shapes follow the offline `muse schema` export (stable surface v1).
//! Unknown fields are ignored so additive server evolution can't break us.

use std::collections::{HashMap, VecDeque};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;
use tokio::io::{AsyncWriteExt, BufReader};
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
    Notif {
        method: String,
        params: Value,
    },
    /// Server→client request (e.g. Codex approvals): must be answered with
    /// [`Host::respond`]; the id space is the server's own.
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    /// Transport/parse failure or an id-less error frame.
    Transport(String),
}

/// Recent stderr lines of a server, newest last. Bounded: see [`STDERR_TAIL_LINES`].
type StderrTail = Arc<Mutex<VecDeque<String>>>;

/// Keep only this many stderr lines (each cut to [`STDERR_LINE_CHARS`]) so a
/// chatty server cannot grow our memory; enough to explain a crash.
const STDERR_TAIL_LINES: usize = 20;
const STDERR_LINE_CHARS: usize = 200;

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, RpcError>>>>>;

/// S1-F1: cap one NDJSON line from an agent child (1 MiB). A hostile child
/// sending an unbounded line must not grow our buffer without limit.
/// Oversize lines are dropped with a transport error.
pub const MAX_NDJSON_LINE_BYTES: usize = 1_048_576;

/// Bounded line-read outcome. Oversize tails are discarded so the next
/// read resynchronises on the following line.
#[derive(Debug, PartialEq, Eq)]
pub enum CappedLine {
    Line(String),
    /// Line exceeded the cap; `bytes` is cap + 1 (what we buffered).
    Oversize {
        bytes: usize,
    },
    /// Line was not valid UTF-8.
    InvalidUtf8 {
        bytes: usize,
    },
    Eof,
}

/// Read one `\n`-terminated line while never buffering more than
/// [`MAX_NDJSON_LINE_BYTES`] + 1 bytes, even before the first newline.
pub async fn read_capped_line<R>(
    reader: &mut R,
    scratch: &mut Vec<u8>,
) -> std::io::Result<CappedLine>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    use tokio::io::AsyncBufReadExt;
    scratch.clear();
    let mut total = 0usize;
    loop {
        let remaining = MAX_NDJSON_LINE_BYTES + 1 - total;
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            if total == 0 {
                return Ok(CappedLine::Eof);
            }
            return to_line(scratch);
        }
        // Copy at most up to and including the first newline, and never
        // more than the remaining budget.
        let mut take = remaining;
        if let Some(pos) = chunk.iter().position(|&b| b == b'\n') {
            take = take.min(pos + 1);
        }
        take = take.min(chunk.len());
        scratch.extend_from_slice(&chunk[..take]);
        reader.consume(take);
        total += take;
        if total > MAX_NDJSON_LINE_BYTES {
            // S1-GAP-B: if the cap+1 scratch already ends on `\n`, the
            // oversize line is fully consumed — do not discard further
            // (that would eat the next line).
            if !scratch.ends_with(b"\n") {
                discard_until_newline(reader).await?;
            }
            return Ok(CappedLine::Oversize { bytes: total });
        }
        if scratch.ends_with(b"\n") {
            return to_line(scratch);
        }
    }
}

fn to_line(scratch: &[u8]) -> std::io::Result<CappedLine> {
    match String::from_utf8(scratch.to_vec()) {
        Ok(s) => Ok(CappedLine::Line(s)),
        Err(_) => Ok(CappedLine::InvalidUtf8 {
            bytes: scratch.len(),
        }),
    }
}

/// Swallow the rest of an oversize line without allocating. Uses only
/// `fill_buf` + `consume` so a multi-MiB tail cannot grow a Vec.
async fn discard_until_newline<R>(reader: &mut R) -> std::io::Result<()>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    use tokio::io::AsyncBufReadExt;
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            return Ok(());
        }
        if let Some(pos) = chunk.iter().position(|&b| b == b'\n') {
            reader.consume(pos + 1);
            return Ok(());
        }
        let n = chunk.len();
        reader.consume(n);
    }
}

pub struct Host {
    next_id: AtomicU64,
    writer: Arc<tokio::sync::Mutex<ChildStdin>>,
    pending: Pending,
    /// Set by the pump once the server's stdout is gone (exit or crash).
    closed: Arc<AtomicBool>,
    stderr_tail: StderrTail,
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
            .stderr(Stdio::piped())
            // No Host may outlive its owner with a live child: the final
            // drop (after in-flight spawned calls release their Arcs)
            // kills the server instead of leaking it headless.
            .kill_on_drop(true)
            .spawn()?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let pump_pending = pending.clone();
        let closed = Arc::new(AtomicBool::new(false));
        let pump_closed = closed.clone();
        let stderr_tail: StderrTail = Arc::new(Mutex::new(VecDeque::new()));
        // Always drain stderr, or a chatty server blocks on a full pipe.
        // Ends on EOF with the process, so it never keeps it alive.
        let drain_tail = stderr_tail.clone();
        let drain = tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut scratch = Vec::new();
            // Oversize / non-UTF-8 lines are skipped; the tail is best effort.
            while let Ok(l) = read_capped_line(&mut reader, &mut scratch).await {
                match l {
                    CappedLine::Line(line) => {
                        let line = line.trim();
                        if line.is_empty() {
                            continue;
                        }
                        let mut tail = drain_tail.lock().unwrap();
                        if tail.len() == STDERR_TAIL_LINES {
                            tail.pop_front();
                        }
                        tail.push_back(line.chars().take(STDERR_LINE_CHARS).collect());
                    }
                    CappedLine::Eof => break,
                    _ => {}
                }
            }
        });
        let pump_tail = stderr_tail.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            let mut scratch = Vec::new();
            loop {
                match read_capped_line(&mut reader, &mut scratch).await {
                    Ok(CappedLine::Line(line)) => {
                        route_line(&line, &pump_pending, &events_tx).await
                    }
                    Ok(CappedLine::Oversize { bytes }) => {
                        let _ = events_tx
                            .send(ServerMsg::Transport(format!(
                                "serve stdout: line of {bytes} bytes exceeds \
                                 {MAX_NDJSON_LINE_BYTES}-byte cap, dropped"
                            )))
                            .await;
                    }
                    Ok(CappedLine::InvalidUtf8 { bytes }) => {
                        let _ = events_tx
                            .send(ServerMsg::Transport(format!(
                                "serve stdout: line of {bytes} bytes is not UTF-8, dropped"
                            )))
                            .await;
                    }
                    Ok(CappedLine::Eof) => {
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
            // The server is gone: no reply can arrive, so fail every
            // waiting call instead of leaving it to hang forever. Give the
            // stderr drain a moment to land its last lines first (a
            // grandchild holding the pipe open must not stall us).
            let _ = tokio::time::timeout(std::time::Duration::from_millis(300), drain).await;
            pump_closed.store(true, Ordering::SeqCst);
            fail_pending(&pump_pending, &pump_tail);
        });
        Ok(Self {
            next_id: AtomicU64::new(1),
            writer: Arc::new(tokio::sync::Mutex::new(stdin)),
            pending,
            closed,
            stderr_tail,
            child,
        })
    }

    pub async fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        // Closed before (or while) we registered: the pump's drain may have
        // missed this entry, so fail it here.
        if self.is_closed() {
            self.pending.lock().unwrap().remove(&id);
            return Err(exited(&self.stderr_tail));
        }
        let frame =
            serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        {
            let mut w = self.writer.lock().await;
            if let Err(e) = w.write_all(format!("{frame}\n").as_bytes()).await {
                self.pending.lock().unwrap().remove(&id);
                return Err(RpcError {
                    code: -32000,
                    message: format!("serve write: {e}"),
                });
            }
        } // Release the writer before waiting: server requests need respond().
        rx.await.unwrap_or(Err(RpcError {
            code: -32000,
            message: "serve call dropped".into(),
        }))
    }

    /// True once the server has exited; the host is dead and must be respawned.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Last `n` stderr lines (oldest first): why the server died.
    pub fn stderr_tail(&self, n: usize) -> Vec<String> {
        tail_lines(&self.stderr_tail, n)
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

    /// Notification carrying params (ACP `session/cancel`).
    pub async fn notify_params(&self, method: &str, params: Value) {
        let frame = serde_json::json!({"jsonrpc": "2.0", "method": method, "params": params});
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

fn tail_lines(tail: &StderrTail, n: usize) -> Vec<String> {
    let tail = tail.lock().unwrap();
    tail.iter()
        .skip(tail.len().saturating_sub(n))
        .cloned()
        .collect()
}

/// "server exited", plus the last stderr lines when it said anything.
fn exited(tail: &StderrTail) -> RpcError {
    let why = tail_lines(tail, 3).join(" | ");
    RpcError {
        code: -32000,
        message: if why.is_empty() {
            "server exited".into()
        } else {
            format!("server exited: {why}")
        },
    }
}

fn fail_pending(pending: &Pending, tail: &StderrTail) {
    for (_, tx) in pending.lock().unwrap().drain() {
        let _ = tx.send(Err(exited(tail)));
    }
}

async fn route_line(line: &str, pending: &Pending, events_tx: &mpsc::Sender<ServerMsg>) {
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

/// Turn id from a `turn/start` result or `turn/started` params: `turnId`
/// (muse) or `turn.id` (codex). Optional everywhere, so absence is fine.
pub fn turn_id_of(v: &Value) -> Option<String> {
    v.get("turnId")
        .or_else(|| v.get("turn").and_then(|t| t.get("id")))
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// RFC 9562 UUIDv7 (required for MSP commandId idempotency handles).
pub fn uuid7() -> String {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let mut buf = [0u8; 16];
    // OS-seeded RandomState hashed with a process-wide counter: no extra
    // crate, works on every platform, distinct even within one millisecond.
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    use std::hash::{BuildHasher, Hasher};
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    for (i, chunk) in buf.chunks_mut(8).enumerate() {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(n);
        h.write_u64(i as u64);
        chunk.copy_from_slice(&h.finish().to_be_bytes());
    }
    buf[0..6].copy_from_slice(&ms.to_be_bytes()[2..8]);
    buf[6] = (buf[6] & 0x0f) | 0x70; // version 7
    buf[8] = (buf[8] & 0x3f) | 0x80; // variant 10
    let h: String = buf.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    )
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

    #[cfg(unix)]
    #[tokio::test]
    async fn pending_call_allows_permission_response() {
        let (tx, mut events) = mpsc::channel(4);
        // A local peer waits for our permission response before ending the call.
        let host = Host::spawn(
            "/bin/sh",
            &[
                "-c",
                r#"
            read -r prompt
            printf '%s\n' '{"id":"permission","method":"session/request_permission"}'
            read -r response
            printf '{"method":"observed","params":%s}\n' "$response"
            printf '%s\n' '{"id":1,"result":{"stopReason":"end_turn"}}'
        "#,
            ],
            &[],
            tx,
        )
        .await
        .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            let call = host.call("session/prompt", Value::Null);
            let respond = async {
                assert!(matches!(
                    events.recv().await,
                    Some(ServerMsg::Request { .. })
                ));
                host.respond(
                    serde_json::json!("permission"),
                    serde_json::json!({"outcome":{"outcome":"cancelled"}}),
                )
                .await
                .unwrap();
                match events.recv().await.unwrap() {
                    ServerMsg::Notif { params, .. } => {
                        assert_eq!(params["id"], "permission");
                        assert_eq!(params["result"]["outcome"]["outcome"], "cancelled");
                    }
                    other => panic!("unexpected: {other:?}"),
                }
            };
            let (result, ()) = tokio::join!(call, respond);
            assert_eq!(result.unwrap()["stopReason"], "end_turn");
        })
        .await
        .expect("call held writer while awaiting response");
        host.shutdown().await;
    }

    /// A server that exits without replying must fail the call, not hang
    /// it (this froze startup before the first frame).
    #[cfg(unix)]
    #[tokio::test]
    async fn call_fails_when_server_exits_without_reply() {
        let (tx, _events) = mpsc::channel(4);
        let host = Host::spawn("/bin/sh", &["-c", "read -r line; exit 1"], &[], tx)
            .await
            .unwrap();
        let res = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            host.call("initialize", Value::Null),
        )
        .await
        .expect("call hung after the server exited");
        assert_eq!(res.unwrap_err().message, "server exited");
        assert!(host.is_closed());
        // Calls after the exit fail straight away too.
        assert!(host.call("session/start", Value::Null).await.is_err());
    }

    /// What the server wrote to stderr before dying reaches the error.
    #[cfg(unix)]
    #[tokio::test]
    async fn exit_error_carries_stderr_tail() {
        let (tx, _events) = mpsc::channel(4);
        let host = Host::spawn(
            "/bin/sh",
            &[
                "-c",
                "read -r line; echo 'noise' >&2; echo 'fatal: bad token' >&2; exit 1",
            ],
            &[],
            tx,
        )
        .await
        .unwrap();
        let err = host.call("initialize", Value::Null).await.unwrap_err();
        assert_eq!(err.message, "server exited: noise | fatal: bad token");
        assert_eq!(host.stderr_tail(1), ["fatal: bad token"]);
    }

    #[test]
    fn uuid7_parses_and_sorts() {
        let a = uuid7();
        let b = uuid7();
        for id in [&a, &b] {
            assert_eq!(id.len(), 36);
            assert_eq!(&id[14..15], "7", "version nibble: {id}");
            assert!(
                matches!(&id[19..20], "8" | "9" | "a" | "b"),
                "variant: {id}"
            );
        }
        // Same-millisecond ids are unordered by design; only the 48-bit
        // timestamp prefix is non-decreasing.
        let ts = |id: &str| u64::from_str_radix(&id.replace('-', "")[..12], 16).unwrap();
        assert!(ts(&a) <= ts(&b), "v7 timestamp went backwards");
    }

    #[test]
    fn uuid7_differs_within_same_millisecond() {
        let ids: std::collections::HashSet<String> = (0..1000).map(|_| uuid7()).collect();
        assert_eq!(ids.len(), 1000);
    }

    #[tokio::test]
    async fn capped_line_passes_normal_framing() {
        let data = b"{\"a\":1}\n\n{\"b\":2}\n";
        let mut reader = BufReader::new(&data[..]);
        let mut scratch = Vec::new();
        assert_eq!(
            read_capped_line(&mut reader, &mut scratch).await.unwrap(),
            CappedLine::Line("{\"a\":1}\n".to_string())
        );
        assert_eq!(
            read_capped_line(&mut reader, &mut scratch).await.unwrap(),
            CappedLine::Line("\n".to_string())
        );
        assert_eq!(
            read_capped_line(&mut reader, &mut scratch).await.unwrap(),
            CappedLine::Line("{\"b\":2}\n".to_string())
        );
        assert_eq!(
            read_capped_line(&mut reader, &mut scratch).await.unwrap(),
            CappedLine::Eof
        );
    }

    #[tokio::test]
    async fn capped_line_drops_oversize_and_resyncs() {
        let mut data = vec![b'x'; MAX_NDJSON_LINE_BYTES + 100];
        data.push(b'\n');
        data.extend_from_slice(b"{\"ok\":true}\n");
        let mut reader = BufReader::new(&data[..]);
        let mut scratch = Vec::new();
        // S1-F1: the hostile line is dropped, never buffered past cap + 1.
        assert_eq!(
            read_capped_line(&mut reader, &mut scratch).await.unwrap(),
            CappedLine::Oversize {
                bytes: MAX_NDJSON_LINE_BYTES + 1
            }
        );
        assert!(scratch.len() <= MAX_NDJSON_LINE_BYTES + 1);
        // Framing resynchronises: the next line still parses.
        assert_eq!(
            read_capped_line(&mut reader, &mut scratch).await.unwrap(),
            CappedLine::Line("{\"ok\":true}\n".to_string())
        );
        assert_eq!(
            read_capped_line(&mut reader, &mut scratch).await.unwrap(),
            CappedLine::Eof
        );
    }

    #[tokio::test]
    async fn capped_line_accepts_eof_terminated_tail() {
        let data = b"{\"tail\":true}";
        let mut reader = BufReader::new(&data[..]);
        let mut scratch = Vec::new();
        assert_eq!(
            read_capped_line(&mut reader, &mut scratch).await.unwrap(),
            CappedLine::Line("{\"tail\":true}".to_string())
        );
        assert_eq!(
            read_capped_line(&mut reader, &mut scratch).await.unwrap(),
            CappedLine::Eof
        );
    }

    #[tokio::test]
    async fn capped_line_flags_non_utf8_without_killing_stream() {
        let mut data = vec![0xff, 0xfe, b'\n'];
        data.extend_from_slice(b"{\"ok\":true}\n");
        let mut reader = BufReader::new(&data[..]);
        let mut scratch = Vec::new();
        assert!(matches!(
            read_capped_line(&mut reader, &mut scratch).await.unwrap(),
            CappedLine::InvalidUtf8 { .. }
        ));
        assert_eq!(
            read_capped_line(&mut reader, &mut scratch).await.unwrap(),
            CappedLine::Line("{\"ok\":true}\n".to_string())
        );
    }

    /// S1-GAP-B: exact cap+1 bytes ending in `\n` must Oversize without
    /// discarding the following line.
    #[tokio::test]
    async fn capped_line_exact_cap_plus_one_newline_preserves_next() {
        let mut data = vec![b'x'; MAX_NDJSON_LINE_BYTES];
        data.push(b'\n');
        data.extend_from_slice(b"{\"ok\":true}\n");
        let mut reader = BufReader::new(&data[..]);
        let mut scratch = Vec::new();
        assert_eq!(
            read_capped_line(&mut reader, &mut scratch).await.unwrap(),
            CappedLine::Oversize {
                bytes: MAX_NDJSON_LINE_BYTES + 1
            }
        );
        assert!(scratch.len() <= MAX_NDJSON_LINE_BYTES + 1);
        assert!(scratch.ends_with(b"\n"));
        assert_eq!(
            read_capped_line(&mut reader, &mut scratch).await.unwrap(),
            CappedLine::Line("{\"ok\":true}\n".to_string())
        );
        assert_eq!(
            read_capped_line(&mut reader, &mut scratch).await.unwrap(),
            CappedLine::Eof
        );
    }

    /// S1-GAP-A: multi-MiB oversize tail with a late `\n` still resyncs;
    /// scratch never exceeds cap+1 (discard allocates no growing Vec).
    #[tokio::test]
    async fn capped_line_multi_mib_tail_resyncs_without_growing_scratch() {
        let mut data = vec![b'y'; MAX_NDJSON_LINE_BYTES + 1];
        // ~4 MiB more without newline, then newline + next frame.
        data.extend(std::iter::repeat(b'z').take(4 * 1024 * 1024));
        data.push(b'\n');
        data.extend_from_slice(b"{\"ok\":true}\n");
        let mut reader = BufReader::new(&data[..]);
        let mut scratch = Vec::new();
        assert_eq!(
            read_capped_line(&mut reader, &mut scratch).await.unwrap(),
            CappedLine::Oversize {
                bytes: MAX_NDJSON_LINE_BYTES + 1
            }
        );
        assert!(scratch.len() <= MAX_NDJSON_LINE_BYTES + 1);
        assert_eq!(
            read_capped_line(&mut reader, &mut scratch).await.unwrap(),
            CappedLine::Line("{\"ok\":true}\n".to_string())
        );
        assert_eq!(
            read_capped_line(&mut reader, &mut scratch).await.unwrap(),
            CappedLine::Eof
        );
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
