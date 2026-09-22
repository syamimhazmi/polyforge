//! Session store: one JSONL transcript plus a meta file per session id under
//! `$XDG_DATA_HOME/polyforge/sessions` (fallback `~/.local/share/...`).
//! Every boot opens a FRESH session id; previous sessions stay on disk and
//! are listed by `/sessions` (view + continue). Every pushed line appends
//! through a buffered sink, so a crash loses at most one frame's lines;
//! oversize files are compacted back to MAX_LINES on load. Storage failures
//! degrade to storageless (None), never crash the TUI.

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::app::{BackendKind, MAX_LINES};

/// Files longer than MAX_LINES + SLOP are rewritten with the tail on load.
const COMPACT_SLOP: usize = 4096;
/// Most sessions `/sessions` lists (newest first).
pub const LIST_CAP: usize = 50;
/// Preview width for the `/sessions` list.
pub const PREVIEW_LEN: usize = 60;

pub struct Store {
    dir: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionMeta {
    pub backend: String,
    pub remote_id: Option<String>,
    /// Creation time, millis since the Unix epoch.
    pub created_at: u64,
    /// Last activity (bumped on every meta save): the UPDATED column.
    /// Defaults to 0 so metas written before this field sort last.
    #[serde(default)]
    pub updated_at: u64,
    /// First user prompt, truncated (display only).
    pub title: String,
}

/// One stored session for the `/sessions` chooser, newest activity first
/// (mirrors `grok sessions list`: id + created + updated + summary).
#[derive(Debug, Clone)]
pub struct StoredSession {
    pub id: String,
    pub backend: String,
    pub remote_id: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
    pub title: String,
    pub preview: String,
}

/// Millis since the Unix epoch (session ids, ordering, display).
pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Compact `MM-DD HH:MM` (UTC) for the `/sessions` columns.
pub fn fmt_short(millis: u64) -> String {
    let full = fmt_time(millis);
    full.get(5..).unwrap_or(&full).to_string()
}

/// `YYYY-MM-DD HH:MM` (UTC) for the `/sessions` list. No chrono dependency:
/// days-to-civil from the epoch, done by hand.
pub fn fmt_time(millis: u64) -> String {
    let secs = millis / 1000;
    let days = (secs / 86_400) as i64;
    let sod = (secs % 86_400) as i64;
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        y + i64::from(m <= 2),
        m,
        d,
        sod / 3600,
        (sod % 3600) / 60
    )
}

impl Store {
    /// Open the real store (creates the directory). None when the home
    /// directory cannot be determined or created — the TUI runs storageless.
    pub fn open() -> Option<Self> {
        let base = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))?;
        Self::open_in(base.join("polyforge").join("sessions"))
    }

    /// Open a store at an explicit directory (tests).
    pub fn open_in(dir: PathBuf) -> Option<Self> {
        fs::create_dir_all(&dir).ok()?;
        Some(Self { dir })
    }

    /// A fresh id per session (boot, respawn, new tab). Millis + pid
    /// order it; the process-wide sequence makes back-to-back mints
    /// (same millis) unique. Old sessions are never deleted.
    pub fn new_session_id() -> String {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        format!("{}-{}-{seq}", now_millis(), std::process::id())
    }

    fn transcript_path(&self, id: &str) -> PathBuf {
        debug_assert!(assert_safe_id(id), "unsafe store id: {id:?}");
        self.dir.join(format!("sess-{id}.jsonl"))
    }

    fn meta_path(&self, id: &str) -> PathBuf {
        debug_assert!(assert_safe_id(id), "unsafe store id: {id:?}");
        self.dir.join(format!("sess-{id}.meta.json"))
    }

    /// Load the transcript tail (at most MAX_LINES, oldest first). Garbage
    /// lines load as-is so one corrupt write never eats the history.
    /// Oversize files are compacted to the tail as a side effect.
    ///
    /// Single streaming pass: decoded lines are held in a capped deque,
    /// so peak memory stays at one copy of the tail (never the whole
    /// file twice over plus a tail clone).
    pub fn load_transcript(&self, id: &str) -> Vec<String> {
        if !assert_safe_id(id) {
            return Vec::new();
        }
        let path = self.transcript_path(id);
        let file = match File::open(&path) {
            Ok(f) => f,
            Err(_) => return Vec::new(),
        };
        let mut tail: VecDeque<String> = VecDeque::new();
        let mut total = 0usize;
        for line in BufReader::new(file).lines().filter_map(|l| l.ok()) {
            total += 1;
            if tail.len() == MAX_LINES {
                tail.pop_front();
            }
            tail.push_back(decode_line(&line));
        }
        let lines = Vec::from(tail);
        if total > MAX_LINES + COMPACT_SLOP {
            let _ = self.rewrite(id, &lines);
        }
        lines
    }

    /// First transcript line, decoded (the `/sessions` preview). Reads one
    /// line only: listing must stay fast no matter how large transcripts
    /// grow. None when the transcript is missing or empty.
    pub fn first_line(&self, id: &str) -> Option<String> {
        if !assert_safe_id(id) {
            return None;
        }
        let file = File::open(self.transcript_path(id)).ok()?;
        let line = BufReader::new(file).lines().next()?.ok()?;
        Some(decode_line(&line))
    }

    fn rewrite(&self, id: &str, tail: &[String]) -> std::io::Result<()> {
        debug_assert!(assert_safe_id(id), "unsafe store id: {id:?}");
        if !assert_safe_id(id) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "unsafe session id",
            ));
        }
        let file = File::create(self.transcript_path(id))?;
        let mut out = BufWriter::new(file);
        for l in tail {
            let encoded = serde_json::to_string(l).unwrap_or_default();
            writeln!(out, "{encoded}")?;
        }
        out.flush()
    }

    /// Append sink for a session (created on demand). Attached AFTER replay
    /// so viewing a previous session never duplicates history.
    pub fn open_sink(&self, id: &str) -> Option<BufWriter<File>> {
        if !assert_safe_id(id) {
            return None;
        }
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.transcript_path(id))
            .ok()
            .map(BufWriter::new)
    }

    /// Load the meta file. Unknown backends are rejected (fresh default).
    pub fn load_meta(&self, id: &str) -> Option<SessionMeta> {
        if !assert_safe_id(id) {
            return None;
        }
        let raw = fs::read_to_string(self.meta_path(id)).ok()?;
        let meta: SessionMeta = serde_json::from_str(&raw).ok()?;
        BackendKind::parse(&meta.backend)?;
        Some(meta)
    }

    pub fn save_meta(&self, id: &str, meta: &SessionMeta) {
        if !assert_safe_id(id) {
            return;
        }
        if let Ok(raw) = serde_json::to_string(meta) {
            let _ = fs::write(self.meta_path(id), raw);
        }
    }

    /// Previous sessions, newest first (at most LIST_CAP). Sessions whose
    /// meta is missing or unparsable are skipped; a missing transcript
    /// previews as empty rather than dropping the entry.
    pub fn list_sessions(&self) -> Vec<StoredSession> {
        let entries = fs::read_dir(&self.dir)
            .map(|rd| rd.filter_map(|e| e.ok()).collect::<Vec<_>>())
            .unwrap_or_default();
        let mut out = Vec::new();
        for e in entries {
            let name = e.file_name().to_string_lossy().into_owned();
            let Some(id) = name
                .strip_prefix("sess-")
                .and_then(|s| s.strip_suffix(".meta.json"))
            else {
                continue;
            };
            let raw = match fs::read_to_string(e.path()) {
                Ok(r) => r,
                Err(_) => continue,
            };
            let Ok(meta): Result<SessionMeta, _> = serde_json::from_str(&raw) else {
                continue;
            };
            if BackendKind::parse(&meta.backend).is_none() {
                continue;
            }
            let preview = self
                .first_line(id)
                .map(|l| truncate(&l, PREVIEW_LEN))
                .unwrap_or_else(|| "(no lines yet)".to_string());
            out.push(StoredSession {
                id: id.to_string(),
                backend: meta.backend,
                remote_id: meta.remote_id,
                created_at: meta.created_at,
                updated_at: meta.updated_at,
                title: meta.title,
                preview,
            });
        }
        // Most recently active first (grok's UPDATED-first ordering), with
        // zero-timestamp (legacy) entries sunk to the bottom.
        out.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| b.created_at.cmp(&a.created_at))
                .then_with(|| b.id.cmp(&a.id))
        });
        out.truncate(LIST_CAP);
        out
    }

    /// Permanently delete a session's files (`grok sessions delete`).
    /// Returns true when something was removed.
    pub fn delete_session(&self, id: &str) -> bool {
        if !assert_safe_id(id) {
            return false;
        }
        let t = fs::remove_file(self.transcript_path(id)).is_ok();
        let m = fs::remove_file(self.meta_path(id)).is_ok();
        t || m
    }
}

/// Shared session-id gate for every store path helper (S6-F1). Session ids
/// are embedded in filenames (`sess-{id}.jsonl`), so an id containing a
/// separator or dot-dot could redirect a read, write, or delete outside the
/// store dir. Minted ids (`{millis}-{pid}-{seq}`) never contain dots, so any
/// `..` substring is rejected outright. Fail-closed: callers refuse the op.
fn assert_safe_id(id: &str) -> bool {
    !id.is_empty()
        && !id.contains('\0')
        && !id.contains('/')
        && !id.contains('\\')
        && !id.contains("..")
}

fn truncate(s: &str, max: usize) -> String {
    let mut out: String = s.chars().take(max).collect();
    if s.chars().count() > max {
        out.push('…');
    }
    out
}

fn decode_line(raw: &str) -> String {
    serde_json::from_str::<String>(raw).unwrap_or_else(|_| raw.to_string())
}

/// Append one transcript line (JSON-escaped, newline-free) to a tab sink.
/// Failures are ignored: the store is best-effort, the TUI is not.
pub fn append_line(sink: &mut BufWriter<File>, line: &str) {
    let encoded = serde_json::to_string(line).unwrap_or_default();
    let _ = writeln!(sink, "{encoded}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_store() -> (Store, TempGuard) {
        let dir = std::env::temp_dir().join(format!(
            "pf-store-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        let _ = fs::remove_dir_all(&dir);
        (
            Store::open_in(dir.clone()).expect("open_in"),
            TempGuard(dir),
        )
    }

    struct TempGuard(PathBuf);
    impl Drop for TempGuard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn test_meta() -> SessionMeta {
        SessionMeta {
            backend: "muse".to_string(),
            remote_id: Some("sess-1".to_string()),
            created_at: 1_700_000_000_000,
            updated_at: 1_700_000_000_000,
            title: "hello".to_string(),
        }
    }

    #[test]
    fn round_trip_with_tricky_chars() {
        let (store, _g) = tmp_store();
        let lines = vec![
            "hello".to_string(),
            "quote \" and \\ backslash".to_string(),
            "emoji ✓ tab\there".to_string(),
            "".to_string(),
        ];
        let mut sink = store.open_sink("a").expect("sink");
        for l in &lines {
            append_line(&mut sink, l);
        }
        sink.flush().expect("flush");
        drop(sink);
        assert_eq!(store.load_transcript("a"), lines);
    }

    #[test]
    fn corrupt_lines_load_raw() {
        let (store, _g) = tmp_store();
        fs::write(store.transcript_path("b"), "{not json\n\"ok\"\n").expect("write");
        assert_eq!(
            store.load_transcript("b"),
            vec!["{not json".to_string(), "ok".to_string()]
        );
    }

    #[test]
    fn oversize_file_compacts_to_tail() {
        let (store, _g) = tmp_store();
        let mut sink = store.open_sink("c").expect("sink");
        for i in 0..(MAX_LINES + COMPACT_SLOP + 100) {
            append_line(&mut sink, &format!("line {i:06}"));
        }
        sink.flush().expect("flush");
        drop(sink);
        let loaded = store.load_transcript("c");
        assert_eq!(loaded.len(), MAX_LINES);
        assert_eq!(loaded[0], format!("line {:06}", COMPACT_SLOP + 100));
        // Rewrite happened: the file itself shrank to MAX_LINES.
        let raw = fs::read_to_string(store.transcript_path("c")).expect("read");
        assert_eq!(raw.lines().count(), MAX_LINES);
    }

    #[test]
    fn load_transcript_keeps_tail_window_without_rewrite() {
        let (store, _g) = tmp_store();
        let mut sink = store.open_sink("w").expect("sink");
        for i in 0..(MAX_LINES + 100) {
            append_line(&mut sink, &format!("line {i:06}"));
        }
        use std::io::Write;
        sink.flush().expect("flush");
        drop(sink);
        let loaded = store.load_transcript("w");
        assert_eq!(loaded.len(), MAX_LINES);
        assert_eq!(loaded[0], "line 000100");
        assert_eq!(loaded[MAX_LINES - 1], format!("line {:06}", MAX_LINES + 99));
        // Under the rewrite slop: the file itself is untouched.
        let raw = fs::read_to_string(store.transcript_path("w")).expect("read");
        assert_eq!(raw.lines().count(), MAX_LINES + 100);
    }

    #[test]
    fn first_line_reads_head_only() {
        let (store, _g) = tmp_store();
        assert_eq!(store.first_line("missing"), None);
        let mut sink = store.open_sink("h").expect("sink");
        append_line(&mut sink, "> first \"quoted\"");
        append_line(&mut sink, "second");
        use std::io::Write;
        sink.flush().expect("flush");
        drop(sink);
        assert_eq!(store.first_line("h").as_deref(), Some("> first \"quoted\""));
        assert_eq!(store.first_line("missing"), None);
    }

    #[test]
    fn meta_round_trip_rejects_unknown_backend() {
        let (store, _g) = tmp_store();
        assert!(store.load_meta("m").is_none());
        let meta = test_meta();
        store.save_meta("m", &meta);
        assert_eq!(store.load_meta("m"), Some(meta));
        // Unknown backends are rejected.
        store.save_meta(
            "m",
            &SessionMeta {
                backend: "wat".to_string(),
                remote_id: None,
                created_at: 0,
                updated_at: 0,
                title: String::new(),
            },
        );
        assert!(store.load_meta("m").is_none());
    }

    #[test]
    fn list_sessions_most_active_first_with_preview() {
        let (store, _g) = tmp_store();
        // "old" was created later but "new" was active more recently:
        // UPDATED-first ordering (grok's `sessions list` contract).
        for (id, created, updated) in [("old", 2000u64, 2000u64), ("new", 1000u64, 3000u64)] {
            let mut meta = test_meta();
            meta.created_at = created;
            meta.updated_at = updated;
            meta.title = format!("title {id}");
            store.save_meta(id, &meta);
            let mut sink = store.open_sink(id).expect("sink");
            append_line(&mut sink, &format!("> first line of {id}"));
            append_line(&mut sink, "second");
            use std::io::Write;
            sink.flush().expect("flush");
        }
        // Unparsable meta never surfaces; legacy tab files are ignored.
        fs::write(store.dir.join("sess-broken.meta.json"), "{nope").expect("write");
        fs::write(store.dir.join("tab0.meta.json"), "{}").expect("write");
        let listed = store.list_sessions();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, "new");
        assert_eq!(listed[1].id, "old");
        assert_eq!(listed[0].title, "title new");
        assert_eq!(listed[0].preview, "> first line of new");
        assert_eq!(listed[0].remote_id.as_deref(), Some("sess-1"));
    }

    #[test]
    fn delete_session_removes_files_and_rejects_paths() {
        let (store, _g) = tmp_store();
        store.save_meta("gone", &test_meta());
        let mut sink = store.open_sink("gone").expect("sink");
        append_line(&mut sink, "x");
        use std::io::Write;
        sink.flush().expect("flush");
        assert!(store.delete_session("gone"));
        assert!(!store.transcript_path("gone").exists());
        assert!(!store.meta_path("gone").exists());
        assert!(!store.delete_session("gone"), "second delete is a no-op");
        assert!(!store.delete_session("../evil"));
        assert!(!store.delete_session(""));
        assert!(!store.list_sessions().iter().any(|s| s.id == "gone"));
    }

    #[test]
    fn assert_safe_id_allows_minted_ids_and_rejects_traversal() {
        assert!(assert_safe_id(&Store::new_session_id()));
        for good in ["a", "tab", "old", "123-456-7", "sess-x"] {
            assert!(assert_safe_id(good), "good id rejected: {good:?}");
        }
        for bad in [
            "",
            "..",
            "../evil",
            "..\\evil",
            "a/b",
            "/abs",
            "a\\b",
            "C:\\evil",
            "evil\0",
            "\0",
            "a\0b",
            "...",
            "a..b",
        ] {
            assert!(!assert_safe_id(bad), "traversal id accepted: {bad:?}");
        }
    }

    #[test]
    fn traversal_ids_are_rejected_on_all_store_paths() {
        let (store, _g) = tmp_store();
        let bad_ids = [
            "",
            "..",
            "../evil",
            "..\\evil",
            "a/b",
            "a\\b",
            "evil\0",
            "a\0b",
        ];
        for id in bad_ids {
            assert!(store.open_sink(id).is_none(), "sink opened for {id:?}");
            assert!(
                store.load_transcript(id).is_empty(),
                "transcript loaded for {id:?}"
            );
            assert_eq!(store.first_line(id), None, "first line read for {id:?}");
            assert_eq!(store.load_meta(id), None, "meta loaded for {id:?}");
            store.save_meta(id, &test_meta());
            assert_eq!(
                store.load_meta(id),
                None,
                "meta persisted for {id:?}"
            );
            assert!(!store.delete_session(id), "delete ran for {id:?}");
        }
        // Fail-closed means no files or subdirectories were created.
        let entries: Vec<_> = fs::read_dir(&store.dir)
            .expect("read_dir")
            .filter_map(|e| e.ok())
            .collect();
        assert!(entries.is_empty(), "traversal created files: {entries:?}");
        // Legit ids still work.
        let mut sink = store.open_sink("ok-id").expect("sink");
        append_line(&mut sink, "hello");
        use std::io::Write;
        sink.flush().expect("flush");
        drop(sink);
        assert_eq!(store.load_transcript("ok-id"), vec!["hello".to_string()]);
    }

    #[test]
    fn planted_traversal_meta_cannot_launder_through_list() {
        let (store, _g) = tmp_store();
        // A planted file whose id parses back to `..` must not reach the fs:
        // preview falls back and every op on the laundered id refuses.
        fs::write(
            store.dir.join("sess-...meta.json"),
            serde_json::to_string(&test_meta()).expect("meta json"),
        )
        .expect("plant");
        let listed = store.list_sessions();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "..");
        assert_eq!(listed[0].preview, "(no lines yet)");
        assert!(store.load_transcript("..").is_empty());
        assert!(!store.delete_session(".."));
    }

    #[test]
    fn fmt_short_truncates_to_day_time() {
        assert_eq!(fmt_short(1_789_516_800_000), "09-16 00:00");
        assert_eq!(fmt_short(0), "01-01 00:00");
    }

    #[test]
    fn fmt_time_known_dates() {
        // 2026-09-16 00:00:00 UTC.
        assert_eq!(fmt_time(1_789_516_800_000), "2026-09-16 00:00");
        assert_eq!(fmt_time(0), "1970-01-01 00:00");
    }
}
