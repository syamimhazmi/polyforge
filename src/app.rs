//! M1 PROTOTYPE state model: tabs + vim-modal input + scrollback + mock jobs.
//! In-memory only. Everything here is throwaway; M2 replaces `mock` with a
//! real provider and adds persistence.

/// In-memory scrollback cap per session (older lines spill nowhere in M1).
pub const MAX_LINES: usize = 50_000;
/// Assumed viewport width until the first render measures the real one.
pub const DEFAULT_WIDTH: usize = 100;

/// Number of display rows `line` occupies at `width` (always ≥ 1).
pub fn wrap_rows(line: &str, width: usize) -> usize {
    wrap_chunks(line, width.max(1)).len().max(1)
}

fn wrap_chunks(line: &str, width: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthChar;
    let mut chunks = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    for ch in line.chars() {
        // Control chars have no width; count them as one column rather
        // than breaking the line mid-escape. Tab expansion stays future work.
        let w = UnicodeWidthChar::width(ch).unwrap_or(1).max(1);
        if cur_w + w > width && !cur.is_empty() {
            chunks.push(std::mem::take(&mut cur));
            cur_w = 0;
        }
        cur.push(ch);
        cur_w += w;
    }
    chunks.push(cur);
    chunks
}
/// Max parallel mock sessions (agreed M1 scope: ~3).
pub const MAX_SESSIONS: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    Search,
    Picker,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Normal => "NORMAL",
            Mode::Insert => "INSERT",
            Mode::Search => "SEARCH",
            Mode::Picker => "PICKER",
        }
    }
}

pub struct PendingDiff {
    pub file: String,
    pub body: String,
}

/// One MSP approval choice (subset of the wire shape we decide on).
#[derive(Debug, Clone, Default)]
pub struct ApprovalChoice {
    pub choice_id: String,
    pub decision: String,
    pub scope: String,
    pub label: String,
    pub accepts_feedback: bool,
}

/// A live approval behind the visible diff card (muse backend only).
#[derive(Debug, Clone, Default)]
pub struct PendingApproval {
    pub approval_id: String,
    pub requirement_id: serde_json::Value,
    pub choices: Vec<ApprovalChoice>,
}

/// y/n/a/q mapped onto a pending approval (muse + codex).
#[derive(Clone, Copy)]
pub enum DecisionKind {
    Approve,
    ApproveAll,
    Reject,
    Later,
}

/// Which backend a tab run uses. One provider per session (spec Q7);
/// the picker switches a tab's backend (fresh remote session, M3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BackendKind {
    #[default]
    Mock,
    Muse,
    Codex,
    Agy,
}

impl BackendKind {
    pub const ALL: [(BackendKind, &'static str); 4] = [
        (BackendKind::Mock, "mock — offline fake (no quota)"),
        (BackendKind::Muse, "muse — Muse Spark via `muse serve`"),
        (BackendKind::Codex, "codex — Codex via `codex app-server`"),
        (BackendKind::Agy, "agy — Antigravity via `agy` (visible, ungated)"),
    ];

    pub fn label(self) -> &'static str {
        match self {
            BackendKind::Mock => "mock",
            BackendKind::Muse => "muse",
            BackendKind::Codex => "codex",
            BackendKind::Agy => "agy",
        }
    }

    /// Parse a stored backend label (unknown strings are rejected so boot
    /// falls back to the configured default).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "mock" => Some(BackendKind::Mock),
            "muse" => Some(BackendKind::Muse),
            "codex" => Some(BackendKind::Codex),
            "agy" => Some(BackendKind::Agy),
            _ => None,
        }
    }
}

/// Async provider work queued by the sync App, performed by the main loop.
/// `requirement_id`/`choice_id` are repurposed per backend: muse uses them
/// as the MSP requirement guard + choice id; codex uses `requirement_id`
/// for the server request id and `choice_id` for the decision word.
#[derive(Debug, Default)]
pub struct OutboxSubmit {
    pub tab: usize,
    pub backend: BackendKind,
    pub prompt: String,
}

#[derive(Debug, Default)]
pub struct OutboxDecide {
    pub tab: usize,
    pub backend: BackendKind,
    pub approval_id: String,
    pub requirement_id: serde_json::Value,
    pub choice_id: String,
    pub feedback: Option<String>,
}

#[derive(Debug, Default)]
pub struct OutboxRespawn {
    pub tab: usize,
    pub backend: BackendKind,
}

#[derive(Debug, Default)]
pub struct Outbox {
    pub submits: Vec<OutboxSubmit>,
    pub decides: Vec<OutboxDecide>,
    pub respawns: Vec<OutboxRespawn>,
}

pub struct Session {
    pub name: String,
    /// Transcript lines, oldest first. Capped at MAX_LINES.
    pub lines: Vec<String>,
    /// Wrapped-row counts parallel to `lines` (grok-build-style height
    /// cache); `total_rows` is their sum. Rebuilt when the viewport width
    /// changes; maintained incrementally on push/drain otherwise.
    pub row_cache: Vec<usize>,
    pub total_rows: usize,
    /// Width the cache was built for; None = stale, rebuild on next render.
    pub cache_width: Option<usize>,
    /// First visible wrapped ROW in the transcript viewport.
    pub scroll: usize,
    pub busy: bool,
    /// Mock output lines still waiting to stream in.
    pub queue: Vec<String>,
    /// Staged diff shown once `queue` drains.
    pub diff_after: Option<PendingDiff>,
    pub pending_diff: Option<PendingDiff>,
    /// Live approval behind the card (muse/codex backends).
    pub pending_approval: Option<PendingApproval>,
    /// This tab's provider (per-tab picker, M3).
    pub backend: BackendKind,
    /// Remote session/thread id for the backend above (None = unavailable).
    pub remote_id: Option<String>,
    /// Grey-out reason when this tab's backend degraded (e.g. no login).
    pub tab_degraded: Option<String>,
    pub input: String,
    /// Char-index cursor inside `input` (insert mode).
    pub cursor: usize,
    /// Append sink for the Q10 transcript store (None = storageless:
    /// tests, or a store that failed to open). Attached AFTER replay so
    /// a restore never duplicates history.
    pub sink: Option<std::io::BufWriter<std::fs::File>>,
    /// True when the sink has unflushed appends (idle loops skip flush).
    pub store_dirty: bool,
    /// Set when an agy child is spawned; cleared when `agy/init` lands.
    pub pending_agy_init: bool,
}

impl Session {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            lines: Vec::new(),
            scroll: 0,
            busy: false,
            queue: Vec::new(),
            diff_after: None,
            pending_diff: None,
            pending_approval: None,
            backend: BackendKind::Mock,
            remote_id: None,
            tab_degraded: None,
            row_cache: Vec::new(),
            total_rows: 0,
            cache_width: None,
            input: String::new(),
            cursor: 0,
            sink: None,
            store_dirty: false,
            pending_agy_init: false,
        }
    }

    pub fn push_line(&mut self, line: String) {
        if let Some(sink) = self.sink.as_mut() {
            crate::store::append_line(sink, &line);
            self.store_dirty = true;
        }
        let w = self.cache_width.unwrap_or(DEFAULT_WIDTH);
        let rows = wrap_rows(&line, w);
        self.lines.push(line);
        self.row_cache.push(rows);
        self.total_rows += rows;
        if self.lines.len() > MAX_LINES {
            let drop = self.lines.len() - MAX_LINES;
            let dropped_rows: usize = self.row_cache[..drop].iter().sum();
            self.lines.drain(..drop);
            self.row_cache.drain(..drop);
            self.total_rows -= dropped_rows;
            self.scroll = self.scroll.saturating_sub(dropped_rows);
        }
    }

    /// Rebuild the height cache when the viewport width changed.
    pub fn ensure_cache(&mut self, width: usize) {
        if self.cache_width == Some(width) {
            return;
        }
        self.row_cache = self.lines.iter().map(|l| wrap_rows(l, width)).collect();
        self.total_rows = self.row_cache.iter().sum();
        self.cache_width = Some(width);
        self.scroll = self.scroll.min(self.total_rows);
    }

    /// First wrapped row of logical line `idx`.
    pub fn line_first_row(&self, idx: usize) -> usize {
        self.row_cache[..idx.min(self.row_cache.len())].iter().sum()
    }

    /// Split one logical line into display chunks of at most `width`
    /// columns (greedy, char-boundary, East-Asian-wide aware).
    pub fn wrap_line(line: &str, width: usize) -> Vec<String> {
        wrap_chunks(line, width.max(1))
    }
}

pub struct App {
    pub sessions: Vec<Session>,
    pub active: usize,
    pub outbox: Outbox,
    pub picker_sel: usize,
    pub mode: Mode,
    pub search_input: String,
    pub search_cursor: usize,
    pub last_query: String,
    pub matches: Vec<usize>,
    pub match_pos: Option<usize>,
    pub mouse: bool,
    pub pending_g: bool,
    pub should_quit: bool,
    pub flash: String,
    pub viewport_height: usize,
    pub viewport_width: usize,
    /// Q10 transcript store (None = storageless; main attaches it on boot).
    pub store: Option<crate::store::Store>,
    /// Spawn-order queue for agy init routing (resume matches by id first).
    pub agy_init_fifo: std::collections::VecDeque<usize>,
}

impl App {
    pub fn new() -> Self {
        let mut app = Self {
            sessions: (1..=MAX_SESSIONS)
                .map(|i| Session::new(&format!("s{i}")))
                .collect(),
            active: 0,
            outbox: Outbox::default(),
            picker_sel: 0,
            mode: Mode::Normal,
            search_input: String::new(),
            search_cursor: 0,
            last_query: String::new(),
            matches: Vec::new(),
            match_pos: None,
            mouse: true,
            pending_g: false,
            should_quit: false,
            flash: String::new(),
            viewport_height: 20,
            viewport_width: DEFAULT_WIDTH,
            store: None,
            agy_init_fifo: std::collections::VecDeque::new(),
        };
        for s in &mut app.sessions {
            crate::mock::seed(s);
        }
        app.stick_to_bottom();
        app
    }

    pub fn active(&self) -> &Session {
        &self.sessions[self.active]
    }

    pub fn active_mut(&mut self) -> &mut Session {
        &mut self.sessions[self.active]
    }

    // -- scrolling (viewport-relative; clamped on read) --

    fn max_scroll(&self) -> usize {
        let s = self.active();
        s.total_rows.saturating_sub(self.viewport_height.max(1))
    }

    fn set_scroll(&mut self, v: usize) {
        self.active_mut().scroll = v.min(self.max_scroll());
    }

    pub fn scroll_lines(&mut self, n: i32) {
        let cur = self.active().scroll as i32;
        self.set_scroll((cur + n).max(0) as usize);
    }

    pub fn half_page(&self) -> i32 {
        (self.viewport_height.max(2) / 2) as i32
    }

    pub fn scroll_top(&mut self) {
        self.set_scroll(0);
    }

    pub fn scroll_bottom(&mut self) {
        self.set_scroll(usize::MAX);
    }

    pub fn stick_to_bottom(&mut self) {
        // Explicit user navigation pins to the live tail.
        let vh = self.viewport_height.max(1);
        let w = self.viewport_width.max(1);
        let s = self.active_mut();
        s.ensure_cache(w);
        let tail = s.total_rows.saturating_sub(vh);
        s.scroll = tail;
    }

    // -- submit / tick (mock streaming; all tabs tick = background agents) --

    pub fn submit(&mut self) {
        let prompt = std::mem::take(&mut self.active_mut().input);
        self.active_mut().cursor = 0;
        if prompt.trim().is_empty() {
            return;
        }
        let tab = self.active;
        let backend = self.sessions[tab].backend;
        let sid = self.sessions[tab].remote_id.clone();
        let degraded = self.sessions[tab].tab_degraded.clone();
        let s = self.active_mut();
        s.push_line(format!("> {prompt}"));
        match backend {
            BackendKind::Mock => crate::mock::start_job(s, &prompt),
            BackendKind::Muse | BackendKind::Codex => match sid {
                Some(_) => {
                    s.busy = true;
                    self.outbox.submits.push(OutboxSubmit { tab, backend, prompt });
                }
                None => {
                    let tag = backend.label();
                    let why = degraded
                        .unwrap_or_else(|| format!("{tag} session unavailable"));
                    s.push_line(format!("{tag}: {why}"));
                }
            },
            // Agy prompts queue into the tab's child stdin and run in turn
            // order; no session id is needed up front (init assigns it).
            BackendKind::Agy => {
                s.busy = true;
                self.outbox.submits.push(OutboxSubmit { tab, backend, prompt });
            }
        }
        self.mode = Mode::Normal;
        self.stick_to_bottom();
    }

    /// Switch the active tab to `backend`: fresh remote session, transcript
    /// reset to a marker (a switch is a new session; history never carries
    /// over). The async bringup runs via the outbox.
    /// Restore one tab from the store: clear mock seed, load backend +
    /// remote id from meta, replay transcript, then attach the append sink
    /// (last so replay never duplicates). Falls back to `default` with no
    /// stored state. No-op when storageless.
    pub fn restore_tab(&mut self, tab: usize, default: BackendKind) {
        if self.store.is_none() || tab >= self.sessions.len() {
            if tab < self.sessions.len() {
                self.sessions[tab].backend = default;
            }
            return;
        }
        // Drop mock::seed (and any prior lines) before replay.
        {
            let s = &mut self.sessions[tab];
            s.lines.clear();
            s.row_cache.clear();
            s.total_rows = 0;
            s.scroll = 0;
        }
        // Disjoint field borrows: the store outlives the session borrow.
        let store = self.store.as_ref().expect("checked");
        if let Some(meta) = store.load_meta(tab) {
            if let Some(b) = BackendKind::parse(&meta.backend) {
                self.sessions[tab].backend = b;
            }
            self.sessions[tab].remote_id = meta.remote_id;
        } else {
            self.sessions[tab].backend = default;
        }
        let lines = store.load_transcript(tab);
        let n = lines.len();
        for line in lines {
            self.sessions[tab].push_line(line);
        }
        self.sessions[tab].sink = store.open_sink(tab);
        let backend = self.sessions[tab].backend;
        if n > 0 {
            // Banner is UI-only: detach sink so it never grows JSONL.
            let sink = self.sessions[tab].sink.take();
            self.sessions[tab].push_line(format!(
                "(restored {n} lines — {} resumes, R respawns fresh)",
                backend.label()
            ));
            self.sessions[tab].sink = sink;
            // Pin the restored tab to its tail (render clamps any excess).
            let tail = self.sessions[tab]
                .total_rows
                .saturating_sub(self.viewport_height.max(1));
            self.sessions[tab].scroll = tail;
        }
    }

    /// Unique backends present across tabs (boot order preserved).
    pub fn backends_needed(sessions: &[Session]) -> Vec<BackendKind> {
        let mut out = Vec::new();
        for s in sessions {
            if !out.contains(&s.backend) {
                out.push(s.backend);
            }
        }
        out
    }

    /// Record a tab's current backend + remote id for the next boot.
    /// No-op when storageless.
    pub fn save_tab_meta(&self, tab: usize) {
        if let (Some(store), Some(s)) = (self.store.as_ref(), self.sessions.get(tab)) {
            store.save_meta(
                tab,
                &crate::store::SessionMeta {
                    backend: s.backend.label().to_string(),
                    remote_id: s.remote_id.clone(),
                },
            );
        }
    }

    /// Forget a tab's stored transcript + meta and reopen a fresh sink.
    /// No-op when storageless.
    pub fn reset_tab_store(&mut self, tab: usize) {
        if self.store.is_none() || tab >= self.sessions.len() {
            return;
        }
        let store = self.store.as_ref().expect("checked");
        self.sessions[tab].sink = None;
        store.reset(tab);
        self.sessions[tab].sink = store.open_sink(tab);
    }

    /// Flush dirty transcript sinks only (idle loops must not File::flush).
    pub fn flush_store(&mut self) {
        use std::io::Write;
        for s in &mut self.sessions {
            if !s.store_dirty {
                continue;
            }
            if let Some(sink) = s.sink.as_mut() {
                let _ = sink.flush();
            }
            s.store_dirty = false;
        }
    }

    pub fn respawn_active(&mut self, backend: BackendKind) {
        let tab = self.active;
        // Drop queued work for this tab; it belongs to the old session.
        self.outbox.submits.retain(|o| o.tab != tab);
        self.outbox.decides.retain(|o| o.tab != tab);
        self.outbox.respawns.retain(|o| o.tab != tab);
        self.agy_init_fifo.retain(|&t| t != tab);
        // Fresh transcript on disk too; the marker below opens the new file.
        self.reset_tab_store(tab);
        {
            let s = self.active_mut();
            s.backend = backend;
            s.remote_id = None;
            s.tab_degraded = None;
            s.busy = false;
            s.pending_agy_init = false;
            s.queue.clear();
            s.diff_after = None;
            s.pending_diff = None;
            s.pending_approval = None;
            s.lines.clear();
            s.row_cache.clear();
            s.total_rows = 0;
            s.scroll = 0;
            s.push_line(format!("--- {} session (fresh) ---", backend.label()));
        }
        self.outbox.respawns.push(OutboxRespawn { tab, backend });
        self.mode = Mode::Normal;
        self.stick_to_bottom();
    }

    /// Close out a provider approval from the UI side. The turn itself may
    /// keep running; completion (and the bell) arrives via turn/completed.
    /// Returns true when a card was actually closed.
    pub fn approved(&mut self, tag: &str, decision: &str) -> bool {
        let s = self.active_mut();
        let Some(diff) = s.pending_diff.take() else {
            return false;
        };
        s.pending_approval.take();
        s.push_line(format!("{tag}: {} {} [{decision}]", muse_mark(decision), diff.file));
        self.stick_to_bottom();
        true
    }

    pub fn muse_approved(&mut self, decision: &str) -> bool {
        self.approved("muse", decision)
    }

    /// Advance all sessions one tick. Returns the name of a tab whose job
    /// just finished (caller rings the bell), if any.
    ///
    /// Streaming only re-pins a tab that was already at the exact bottom
    /// *before* new lines arrived — any manual scroll-up (wheel, j/k,
    /// Ctrl-u) sticks, even mid-stream.
    pub fn tick(&mut self) -> Option<String> {
        let mut done = None;
        let vh = self.viewport_height.max(1);
        for s in &mut self.sessions {
            // Mock streaming only: live backends are driven by server events.
            if s.backend != BackendKind::Mock || !s.busy || s.pending_diff.is_some() {
                continue; // idle, remote-driven, or waiting on y/n/a/q
            }
            let pinned = s.scroll + vh >= s.total_rows;
            let mut pushed = false;
            for _ in 0..2 {
                if let Some(line) = next_queued(s) {
                    s.push_line(line);
                    pushed = true;
                } else {
                    break;
                }
            }
            if pushed && pinned {
                s.scroll = s.total_rows.saturating_sub(vh);
            }
            if s.queue.is_empty() {
                if let Some(diff) = s.diff_after.take() {
                    s.pending_diff = Some(diff);
                } else {
                    s.busy = false;
                    s.push_line("mock: done ✓".to_string());
                    if pinned {
                        s.scroll = s.total_rows.saturating_sub(vh);
                    }
                    done = Some(s.name.clone());
                }
            }
        }
        done
    }

    // -- diff approval (per-file y/n/a/q; per-hunk is v2) --

    /// Returns true when a job fully finished (bell).
    pub fn decide_diff(&mut self, decision: &str) -> bool {
        let s = self.active_mut();
        let Some(diff) = s.pending_diff.take() else {
            return false;
        };
        s.push_line(format!("{} {} [{}]", diff_mark(decision), diff.file, decision));
        s.busy = false;
        s.push_line("mock: done ✓".to_string());
        self.stick_to_bottom();
        true
    }

    // -- search (/ + n/N) --

    pub fn run_search(&mut self) {
        self.last_query = self.search_input.clone();
        self.matches = self
            .active()
            .lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.contains(&self.last_query))
            .map(|(i, _)| i)
            .collect();
        if self.matches.is_empty() {
            self.flash = format!("no match: {}", self.last_query);
            self.match_pos = None;
        } else {
            self.match_pos = Some(0);
            let idx = self.matches[0];
            let w = self.viewport_width.max(1);
            self.active_mut().ensure_cache(w);
            let row = self.active().line_first_row(idx);
            self.set_scroll(row);
            self.flash = format!("1/{}: {}", self.matches.len(), self.last_query);
        }
    }

    pub fn search_step(&mut self, dir: i32) {
        if self.matches.is_empty() {
            self.flash = "no active search — press /".to_string();
            return;
        }
        let n = self.matches.len() as i32;
        let cur = self.match_pos.unwrap_or(0) as i32;
        let next = ((cur + dir).rem_euclid(n)) as usize;
        self.match_pos = Some(next);
        let idx = self.matches[next];
        let w = self.viewport_width.max(1);
        self.active_mut().ensure_cache(w);
        let row = self.active().line_first_row(idx);
        self.set_scroll(row);
        self.flash = format!("{}/{}: {}", next + 1, n, self.last_query);
    }
}

fn next_queued(s: &mut Session) -> Option<String> {
    if s.queue.is_empty() {
        None
    } else {
        Some(s.queue.remove(0))
    }
}

fn diff_mark(decision: &str) -> &'static str {
    match decision {
        "approved" => "mock: approved",
        "rejected" => "mock: rejected",
        "approved-all" => "mock: approved-all",
        _ => "mock: deferred",
    }
}

fn muse_mark(decision: &str) -> &'static str {
    match decision {
        "approved" => "approved",
        "rejected" => "rejected",
        "approved-all" => "approved-all",
        _ => "deferred",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VH: usize = 20; // fixed viewport: 62 seed lines -> max scroll 42

    fn bottom_app() -> App {
        let mut app = App::new();
        app.viewport_height = VH;
        app.viewport_width = 100;
        // Place the viewport explicitly: must not depend on stick_to_bottom().
        let tail = app.active().total_rows.saturating_sub(VH);
        app.active_mut().scroll = tail;
        app
    }

    fn test_store(name: &str) -> (crate::store::Store, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("pf-app-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = crate::store::Store::open_in(dir.clone()).expect("open_in");
        (store, dir)
    }

    #[test]
    fn restore_replays_transcript_and_meta() {
        let (store, _dir) = test_store("restore");
        // Pre-populate as a previous run would have left it.
        let mut sink = store.open_sink(0).expect("sink");
        crate::store::append_line(&mut sink, "old line 1");
        crate::store::append_line(&mut sink, "old \"quoted\" 2");
        use std::io::Write;
        sink.flush().expect("flush");
        drop(sink);
        store.save_meta(
            0,
            &crate::store::SessionMeta {
                backend: "codex".to_string(),
                remote_id: Some("thread-9".to_string()),
            },
        );
        // Mock::seed must be cleared; restored lines start at index 0.
        let mut app = App::new();
        assert!(app.sessions[0].lines.len() > 2, "precondition: seed present");
        app.store = Some(store);
        app.restore_tab(0, BackendKind::Mock);
        let s = &app.sessions[0];
        assert_eq!(s.backend, BackendKind::Codex);
        assert_eq!(s.remote_id.as_deref(), Some("thread-9"));
        assert_eq!(s.lines[0], "old line 1");
        assert_eq!(s.lines[1], "old \"quoted\" 2");
        assert!(s.lines[2].starts_with("(restored 2 lines"));
        assert!(s.sink.is_some());
        // New pushes after restore append to the same file, no duplication.
        // Banner must not have been written through the sink.
        app.sessions[0].push_line("new line".to_string());
        app.flush_store();
        let store = app.store.as_ref().expect("store");
        let replayed = store.load_transcript(0);
        assert_eq!(&replayed[..2], &["old line 1", "old \"quoted\" 2"]);
        assert!(replayed.iter().any(|l| l == "new line"));
        assert!(
            !replayed.iter().any(|l| l.starts_with("(restored")),
            "restore banner must not grow JSONL"
        );
        let _ = std::fs::remove_dir_all(_dir);
    }

    #[test]
    fn backends_needed_unique_preserves_order() {
        let mut sessions = vec![
            Session::new("a"),
            Session::new("b"),
            Session::new("c"),
        ];
        sessions[0].backend = BackendKind::Mock;
        sessions[1].backend = BackendKind::Codex;
        sessions[2].backend = BackendKind::Muse;
        assert_eq!(
            App::backends_needed(&sessions),
            vec![BackendKind::Mock, BackendKind::Codex, BackendKind::Muse]
        );
        sessions[0].backend = BackendKind::Codex;
        sessions[1].backend = BackendKind::Muse;
        sessions[2].backend = BackendKind::Codex;
        assert_eq!(
            App::backends_needed(&sessions),
            vec![BackendKind::Codex, BackendKind::Muse]
        );
    }

    #[test]
    fn flush_store_skips_clean_sinks() {
        let (store, dir) = test_store("flush-dirty");
        let mut app = App::new();
        app.store = Some(store);
        app.sessions[0].lines.clear();
        app.sessions[0].row_cache.clear();
        app.sessions[0].total_rows = 0;
        app.sessions[0].sink = app.store.as_ref().unwrap().open_sink(0);
        app.sessions[0].store_dirty = false;
        app.flush_store(); // idle: no-op
        assert!(!app.sessions[0].store_dirty);
        app.sessions[0].push_line("only".into());
        assert!(app.sessions[0].store_dirty);
        app.flush_store();
        assert!(!app.sessions[0].store_dirty);
        let lines = app.store.as_ref().unwrap().load_transcript(0);
        assert_eq!(lines, vec!["only".to_string()]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn respawn_forgets_stored_transcript() {
        let (store, dir) = test_store("respawn");
        let mut app = App::new();
        app.store = Some(store);
        app.restore_tab(1, BackendKind::Mock);
        app.sessions[1].push_line("scratch".to_string());
        app.flush_store();
        app.active = 1;
        app.respawn_active(BackendKind::Mock);
        app.flush_store();
        let store = app.store.as_ref().expect("store");
        let lines = store.load_transcript(1);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with("--- mock session (fresh) ---"));
        assert!(app.sessions[1].remote_id.is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn wrap_counts_rows() {
        assert_eq!(wrap_rows("", 5), 1);
        assert_eq!(wrap_rows("abc", 5), 1);
        assert_eq!(wrap_rows("abcde", 5), 1); // exact fit
        assert_eq!(wrap_rows("abcdef", 5), 2);
        assert_eq!(wrap_rows("abcdefghij", 5), 2);
        // East-Asian-wide chars occupy two columns.
        assert_eq!(wrap_rows("ああ", 3), 2);
        assert_eq!(wrap_rows("aあ", 4), 1);
        assert_eq!(Session::wrap_line("abcdef", 4), vec!["abcd", "ef"]);
    }

    #[test]
    fn cache_rebuilds_on_width_change() {
        let mut s = Session::new("t");
        s.push_line("0123456789".to_string()); // 10 cols
        assert_eq!(s.total_rows, 1); // counted at DEFAULT_WIDTH
        s.ensure_cache(4);
        assert_eq!(s.total_rows, 3); // 4+4+2
        assert_eq!(s.line_first_row(0), 0);
        assert_eq!(s.line_first_row(1), 3);
        // Idempotent: same width is a no-op.
        s.ensure_cache(4);
        assert_eq!(s.total_rows, 3);
    }

    #[test]
    fn cap_drain_keeps_cache_consistent() {
        let mut s = Session::new("t");
        s.cache_width = Some(5);
        for i in 0..(MAX_LINES + 100) {
            s.push_line(format!("line {i:05}")); // 10 cols -> 2 rows at w=5
        }
        assert_eq!(s.lines.len(), MAX_LINES);
        assert_eq!(s.row_cache.len(), MAX_LINES);
        assert_eq!(s.total_rows, s.row_cache.iter().sum::<usize>());
        assert_eq!(s.total_rows, MAX_LINES * 2);
    }

    fn max_scroll(app: &App) -> usize {
        app.active().lines.len().saturating_sub(VH)
    }

    /// Wheel-up on an idle app must stick — tick() must not yank it back.
    #[test]
    fn idle_wheel_up_sticks() {
        let mut app = bottom_app();
        let max = max_scroll(&app);
        assert_eq!(app.active().scroll, max);
        app.scroll_lines(-3); // one wheel-up notch, as run() does
        app.tick();
        app.tick();
        app.tick();
        assert!(
            app.active().scroll < max,
            "wheel-up got yanked back: scroll={} max={}",
            app.active().scroll,
            max
        );
    }

    /// Same while a mock job streams (background-agent case).
    #[test]
    fn midstream_wheel_up_sticks() {
        let mut app = bottom_app();
        app.active_mut().input = "hi".to_string();
        app.submit();
        assert!(app.active().busy);
        app.scroll_lines(-3);
        for _ in 0..10 {
            app.tick();
        }
        let tail = app.active().lines.len().saturating_sub(VH);
        assert!(
            app.active().scroll < tail,
            "mid-stream wheel-up snapped to tail: scroll={} tail={}",
            app.active().scroll,
            tail
        );
    }

    /// A finished job still reaches its diff card and a decision ends it.
    #[test]
    fn mock_job_reaches_diff_and_decision() {
        let mut app = bottom_app();
        app.active_mut().input = "hi".to_string();
        app.submit();
        for _ in 0..40 {
            app.tick();
        }
        assert!(
            app.active().pending_diff.is_some(),
            "job never produced its diff card"
        );
        assert!(app.decide_diff("approved"));
        assert!(!app.active().busy);
        assert!(app.active().pending_diff.is_none());
    }
}
