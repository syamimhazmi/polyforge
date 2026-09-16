//! Q10 session store: per-tab JSONL transcripts plus a meta file under
//! `$XDG_DATA_HOME/polyforge/sessions` (fallback `~/.local/share/...`).
//! Every pushed line appends through a buffered sink, so a crash loses at
//! most one frame's lines; boot truncates runaway files back to MAX_LINES
//! and re-attaches the remote session recorded in the meta file.
//! Storage failures degrade to storageless (None), never crash the TUI.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::app::{BackendKind, MAX_LINES};

/// Files longer than MAX_LINES + SLOP are rewritten with the tail on boot.
const COMPACT_SLOP: usize = 4096;

pub struct Store {
    dir: PathBuf,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct SessionMeta {
    pub backend: String,
    pub remote_id: Option<String>,
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

    fn transcript_path(&self, tab: usize) -> PathBuf {
        self.dir.join(format!("tab{tab}.jsonl"))
    }

    fn meta_path(&self, tab: usize) -> PathBuf {
        self.dir.join(format!("tab{tab}.meta.json"))
    }

    /// Load the transcript tail (at most MAX_LINES, oldest first). Garbage
    /// lines load as-is so one corrupt write never eats the history.
    /// Oversize files are compacted to the tail as a side effect.
    pub fn load_transcript(&self, tab: usize) -> Vec<String> {
        let path = self.transcript_path(tab);
        let file = match File::open(&path) {
            Ok(f) => f,
            Err(_) => return Vec::new(),
        };
        let raw: Vec<String> = BufReader::new(file)
            .lines()
            .filter_map(|l| l.ok())
            .collect();
        let mut lines: Vec<String> = raw.iter().map(|l| decode_line(l)).collect();
        if lines.len() > MAX_LINES {
            let tail = &lines[lines.len() - MAX_LINES..];
            if lines.len() > MAX_LINES + COMPACT_SLOP {
                let _ = self.rewrite(tab, tail);
            }
            lines = tail.to_vec();
        }
        lines
    }

    fn rewrite(&self, tab: usize, tail: &[String]) -> std::io::Result<()> {
        let mut out = String::new();
        for l in tail {
            out.push_str(&serde_json::to_string(l).unwrap_or_default());
            out.push('\n');
        }
        fs::write(self.transcript_path(tab), out)
    }

    /// Append sink for a tab (created on demand). Attached AFTER replay so
    /// the restore never duplicates history.
    pub fn open_sink(&self, tab: usize) -> Option<BufWriter<File>> {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.transcript_path(tab))
            .ok()
            .map(BufWriter::new)
    }

    /// Load the meta file. Unknown backends are rejected (fresh default).
    pub fn load_meta(&self, tab: usize) -> Option<SessionMeta> {
        let raw = fs::read_to_string(self.meta_path(tab)).ok()?;
        let meta: SessionMeta = serde_json::from_str(&raw).ok()?;
        BackendKind::parse(&meta.backend)?;
        Some(meta)
    }

    pub fn save_meta(&self, tab: usize, meta: &SessionMeta) {
        if let Ok(raw) = serde_json::to_string(meta) {
            let _ = fs::write(self.meta_path(tab), raw);
        }
    }

    /// Forget a tab (respawn-fresh). The caller drops the sink first.
    pub fn reset(&self, tab: usize) {
        let _ = fs::remove_file(self.transcript_path(tab));
        let _ = fs::remove_file(self.meta_path(tab));
    }
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
        (Store::open_in(dir.clone()).expect("open_in"), TempGuard(dir))
    }

    struct TempGuard(PathBuf);
    impl Drop for TempGuard {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
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
        let mut sink = store.open_sink(0).expect("sink");
        for l in &lines {
            append_line(&mut sink, l);
        }
        sink.flush().expect("flush");
        drop(sink);
        assert_eq!(store.load_transcript(0), lines);
    }

    #[test]
    fn corrupt_lines_load_raw() {
        let (store, _g) = tmp_store();
        fs::write(store.transcript_path(1), "{not json\n\"ok\"\n").expect("write");
        assert_eq!(
            store.load_transcript(1),
            vec!["{not json".to_string(), "ok".to_string()]
        );
    }

    #[test]
    fn oversize_file_compacts_to_tail() {
        let (store, _g) = tmp_store();
        let mut sink = store.open_sink(2).expect("sink");
        for i in 0..(MAX_LINES + COMPACT_SLOP + 100) {
            append_line(&mut sink, &format!("line {i:06}"));
        }
        sink.flush().expect("flush");
        drop(sink);
        let loaded = store.load_transcript(2);
        assert_eq!(loaded.len(), MAX_LINES);
        assert_eq!(loaded[0], format!("line {:06}", COMPACT_SLOP + 100));
        // Rewrite happened: the file itself shrank to MAX_LINES.
        let raw = fs::read_to_string(store.transcript_path(2)).expect("read");
        assert_eq!(raw.lines().count(), MAX_LINES);
    }

    #[test]
    fn meta_round_trip_and_reset() {
        let (store, _g) = tmp_store();
        assert!(store.load_meta(0).is_none());
        let meta = SessionMeta {
            backend: "muse".to_string(),
            remote_id: Some("sess-1".to_string()),
        };
        store.save_meta(0, &meta);
        assert_eq!(store.load_meta(0), Some(meta));
        // Unknown backends are rejected.
        store.save_meta(
            0,
            &SessionMeta {
                backend: "wat".to_string(),
                remote_id: None,
            },
        );
        assert!(store.load_meta(0).is_none());
        store.reset(0);
        assert!(store.load_transcript(0).is_empty());
        assert!(!store.transcript_path(0).exists());
    }
}
