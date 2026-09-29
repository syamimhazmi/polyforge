//! M1 PROTOTYPE state model: tabs + vim-modal input + scrollback + mock jobs.
//! In-memory only. Everything here is throwaway; M2 replaces `mock` with a
//! real provider and adds persistence.

use crate::markdown::MdKind;

/// In-memory scrollback cap per session (older lines spill nowhere in M1).
pub const MAX_LINES: usize = 50_000;
/// Assumed viewport width until the first render measures the real one.
pub const DEFAULT_WIDTH: usize = 100;
/// Live thought buffer cap (bytes; the tail is kept).
const THOUGHT_CAP: usize = 4096;
/// Thought lines shown in the live block, and the per-line display cap.
const THOUGHT_LINES: usize = 3;
const THOUGHT_LINE_CAP: usize = 240;

/// Number of display rows `line` occupies at `width` (always ≥ 1).
/// ASCII lines (the common case) skip the per-char width walk: every
/// byte is one column, so the count is plain division over the byte
/// length. Must agree with `wrap_chunks` (greedy packs `width` bytes).
pub fn wrap_rows(line: &str, width: usize) -> usize {
    let w = width.max(1);
    if line.is_ascii() {
        return ((line.len() + w - 1) / w).max(1);
    }
    wrap_chunks(line, w).len().max(1)
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
/// Max open tabs (agreed M1 scope: ~3). Boot opens exactly one.
pub const MAX_SESSIONS: usize = 3;

/// Mouse drag selection in the transcript: line index + char index
/// endpoints (anchor = drag start, focus = current end).
#[derive(Debug, Clone, Copy, Default)]
pub struct Selection {
    pub anchor_line: usize,
    pub anchor_char: usize,
    pub focus_line: usize,
    pub focus_char: usize,
}

impl Selection {
    /// Normalized ((top_line, top_char), (bottom_line, bottom_char)).
    pub fn normalized(self) -> ((usize, usize), (usize, usize)) {
        let (a_line, a_char) = (self.anchor_line, self.anchor_char);
        let (f_line, f_char) = (self.focus_line, self.focus_char);
        if (a_line, a_char) <= (f_line, f_char) {
            ((a_line, a_char), (f_line, f_char))
        } else {
            ((f_line, f_char), (a_line, a_char))
        }
    }

    /// Char span selected on `line`, or None when the line is untouched.
    /// `line_len` clamps endpoints past the end (drag past EOL).
    pub fn span_on_line(self, line: usize, line_len: usize) -> Option<(usize, usize)> {
        let ((a_line, a_char), (f_line, f_char)) = self.normalized();
        if line < a_line || line > f_line {
            return None;
        }
        let s = if line == a_line { a_char } else { 0 }.min(line_len);
        let e = if line == f_line { f_char } else { line_len }.min(line_len);
        if s >= e {
            return None;
        }
        Some((s, e))
    }

    pub fn is_empty(self) -> bool {
        (self.anchor_line, self.anchor_char) == (self.focus_line, self.focus_char)
    }
}

/// Display-column of the first `chars` chars of `s` (wide-char aware).
/// Test-only for now: the render path splits pre-wrapped chunks by chars.
#[cfg(test)]
pub fn char_to_col(s: &str, chars: usize) -> usize {
    use unicode_width::UnicodeWidthChar;
    s.chars()
        .take(chars)
        .map(|c| UnicodeWidthChar::width(c).unwrap_or(1).max(1))
        .sum()
}

/// Char index containing display column `col` (click position → char).
/// Columns past the end clamp to the string length.
pub fn col_to_char(s: &str, col: usize) -> usize {
    use unicode_width::UnicodeWidthChar;
    let mut acc = 0usize;
    for (i, c) in s.chars().enumerate() {
        let w = UnicodeWidthChar::width(c).unwrap_or(1).max(1);
        if acc + w > col {
            return i;
        }
        acc += w;
    }
    s.chars().count()
}
/// First user prompt kept as the `/sessions` title (chars).
pub const TITLE_LEN: usize = 48;

/// Slash commands available in Insert mode. The first element is the
/// display template; `accept_slash_completion` inserts `insert` instead
/// (e.g. `/sessions` completes to `/sessions ` so a query can follow).
pub const SLASH_COMMANDS: [(&str, &str, &str); 7] = [
    (
        "/sessions [query]",
        "/sessions ",
        "browse previous sessions",
    ),
    ("/new", "/new", "fresh session in this tab"),
    ("/tab new", "/tab new", "open a tab (max 3)"),
    ("/tab close", "/tab close", "close this tab"),
    ("/theme [name]", "/theme ", "switch theme (bare = next)"),
    ("/vim", "/vim", "toggle vim keymap"),
    ("/help", "/help", "command + key summary"),
];

/// Strip terminal-injection bytes (S3-F1): ESC-led sequences (CSI, OSC,
/// DCS/SOS/PM/APC, charset shifts, single `ESC X`), C1 singletons, C0
/// controls, and DEL are removed. Sequence parameters go with their
/// introducer, so no `[31m` remnant or hyperlink URL survives. Tab and
/// newline are kept; everything else passes through. Idempotent.
pub fn sanitize_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\t' | '\n' => out.push(c),
            '\x1b' => consume_esc(&mut it),
            '\u{9b}' => consume_csi(&mut it),
            '\u{9d}' | '\u{90}' | '\u{98}' | '\u{9e}' | '\u{9f}' => consume_until_st(&mut it),
            _ if c.is_control() => {}
            _ => out.push(c),
        }
    }
    out
}

/// Consume an ESC-led sequence: the introducer is already eaten.
fn consume_esc(it: &mut std::iter::Peekable<std::str::Chars>) {
    match it.next() {
        None => {}
        Some('[') => consume_csi(it),
        Some(']' | 'P' | 'X' | '^' | '_') => consume_until_st(it),
        Some('(' | ')' | '#' | '%' | '$') => {
            it.next();
        }
        Some(_) => {}
    }
}

/// Consume a CSI body through its final byte (`@`–`~`); unterminated
/// input fails closed by eating to the end.
fn consume_csi(it: &mut std::iter::Peekable<std::str::Chars>) {
    for c in it.by_ref() {
        if ('\x40'..='\x7e').contains(&c) {
            break;
        }
    }
}

/// Consume an OSC/DCS-style body through ST (BEL, `ESC \`, or U+009C).
fn consume_until_st(it: &mut std::iter::Peekable<std::str::Chars>) {
    while let Some(c) = it.next() {
        match c {
            '\x07' | '\u{9c}' => break,
            '\x1b' => {
                if it.next() == Some('\\') {
                    break;
                }
            }
            _ => {}
        }
    }
}

/// Short display id for the `/sessions` list (store ids are ASCII
/// `millis-pid-seq`, so byte slicing is safe).
pub fn short_id(id: &str) -> String {
    if id.len() <= 8 {
        id.to_string()
    } else {
        id[id.len() - 8..].to_string()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    Search,
    Picker,
    Sessions,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Normal => "NORMAL",
            Mode::Insert => "INSERT",
            Mode::Search => "SEARCH",
            Mode::Picker => "PICKER",
            Mode::Sessions => "SESSIONS",
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
    Grok,
    Claude,
}

impl BackendKind {
    pub const ALL: [(BackendKind, &'static str); 6] = [
        (BackendKind::Mock, "mock — offline fake (no quota)"),
        (BackendKind::Muse, "muse — Muse Spark via `muse serve`"),
        (BackendKind::Codex, "codex — Codex via `codex app-server`"),
        (
            BackendKind::Agy,
            "agy — Antigravity via `agy` (visible, ungated)",
        ),
        (
            BackendKind::Grok,
            "grok — Grok via `grok agent stdio` (ACP)",
        ),
        (
            BackendKind::Claude,
            "claude — Claude Code via `claude -p` (stream-json)",
        ),
    ];

    pub fn label(self) -> &'static str {
        match self {
            BackendKind::Mock => "mock",
            BackendKind::Muse => "muse",
            BackendKind::Codex => "codex",
            BackendKind::Agy => "agy",
            BackendKind::Grok => "grok",
            BackendKind::Claude => "claude",
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
            "grok" => Some(BackendKind::Grok),
            "claude" => Some(BackendKind::Claude),
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

/// Stop request, snapshotted at press time so a later tab close / respawn
/// cannot retarget it (the drain uses these ids, not the live session).
#[derive(Debug, Default)]
pub struct OutboxStop {
    pub tab: usize,
    pub backend: BackendKind,
    pub remote_id: Option<String>,
    pub turn_id: Option<String>,
}

#[derive(Debug, Default)]
pub struct Outbox {
    pub submits: Vec<OutboxSubmit>,
    pub decides: Vec<OutboxDecide>,
    pub respawns: Vec<OutboxRespawn>,
    pub stops: Vec<OutboxStop>,
}

/// Transcript line when a stop lands (turn ended after a stop request).
pub const STOPPED_LINE: &str = "■ stopped";
/// Second stop press: the backend never ended the turn, so give up locally.
pub const FORCED_STOP_LINE: &str = "■ stopped (forced; late output may still arrive)";

/// Non-negative integer from a JSON number or an integer-valued numeric
/// string; fractions, negatives and everything else are None.
pub fn take_u64(v: &serde_json::Value) -> Option<u64> {
    match v {
        serde_json::Value::Number(n) => n.as_u64(),
        serde_json::Value::String(s) => s.trim().parse::<u64>().ok(),
        _ => None,
    }
}

pub struct Session {
    pub name: String,
    /// Transcript lines, oldest first. Capped at MAX_LINES.
    pub lines: Vec<String>,
    /// Markdown kind per line, parallel to `lines` (carries the open-fence
    /// state across lines). Missing entries read as `Plain`.
    pub kinds: Vec<MdKind>,
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
    /// TypeSafe judgment for the open DIFF card (None = none yet / no key).
    pub approval_risk: Option<crate::typesafe::ApprovalJudgment>,
    /// Generation token: bumped on every stage/clear so late answers drop.
    pub risk_gen: u64,
    /// Last `risk_gen` a TypeSafe request was spawned for (dedupe).
    pub risk_spawned_gen: Option<u64>,
    /// This tab's provider (per-tab picker, M3).
    pub backend: BackendKind,
    /// Remote session/thread id for the backend above (None = unavailable).
    pub remote_id: Option<String>,
    /// Grey-out reason when this tab's backend degraded (e.g. no login).
    pub tab_degraded: Option<String>,
    pub input: String,
    /// Char-index cursor inside `input` (insert mode).
    pub cursor: usize,
    /// Store id this tab persists under (`sess-{id}` files). A tab gets a
    /// FRESH id on boot, respawn, and new-tab; previous ids stay on disk
    /// for `/sessions`. None = storageless (tests, failed store open).
    pub store_id: Option<String>,
    /// Creation time of the tab's stored session (millis, display only).
    pub created_at: u64,
    /// Last activity of the tab's stored session (the UPDATED column).
    pub updated_at: u64,
    /// First user prompt, truncated (the `/sessions` title).
    pub title: String,
    /// Append sink for the transcript store (None = storageless: tests,
    /// or a store that failed to open). Attached AFTER replay so viewing
    /// a previous session never duplicates history.
    pub sink: Option<std::io::BufWriter<std::fs::File>>,
    /// True when the sink has unflushed appends (idle loops skip flush).
    pub store_dirty: bool,
    /// Set when an agy child is spawned; cleared when `agy/init` lands.
    pub pending_agy_init: bool,
    /// Live thought text (any provider). Bounded to its tail; collapsed to
    /// one `◆ Thought for Ns` line on flush, never persisted verbatim.
    pub thought: String,
    /// True once the head of `thought` was dropped to stay bounded.
    pub thought_cut: bool,
    /// When the first thought chunk of the current thought arrived.
    pub thought_since: Option<std::time::Instant>,
    /// (total rows incl. drafts, viewport rows) of the last drawn frame;
    /// lets the renderer keep a bottom-pinned view pinned when the live
    /// block or the turn-status row changes height. None until drawn.
    pub last_geom: Option<(usize, usize)>,
    /// Open Grok answer line. Chunks append here; `\n` peels committed
    /// lines via `push_line`. Remainder flushes at turn/tool boundaries.
    pub draft_answer: String,
    /// Current activity phase; Some only while busy.
    pub phase: Option<crate::activity::Phase>,
    /// Start of the current phase (status-bar timer).
    pub phase_since: Option<std::time::Instant>,
    /// Start of the turn (rail wave clock).
    pub turn_since: Option<std::time::Instant>,
    /// `lines.len()` when the turn was detected; lines at or past this
    /// index belong to the live turn.
    pub turn_first_line: usize,
    /// Set when the user asked to stop the running turn; cleared when the
    /// turn ends. Drives the `Stopping…` label and the second-press force.
    pub stopping: Option<std::time::Instant>,
    /// Set by a forced stop: status-driven re-busy events are ignored until
    /// the next user submit (the backend may still be emitting).
    pub sealed: bool,
    /// Backend id of the running turn (codex/muse) when known.
    pub turn_id: Option<String>,
    /// Latest context occupancy the backend reported (not a per-turn sum).
    pub tokens: Option<u64>,
}

impl Session {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            lines: Vec::new(),
            kinds: Vec::new(),
            scroll: 0,
            busy: false,
            queue: Vec::new(),
            diff_after: None,
            pending_diff: None,
            pending_approval: None,
            approval_risk: None,
            risk_gen: 0,
            risk_spawned_gen: None,
            backend: BackendKind::Mock,
            remote_id: None,
            tab_degraded: None,
            store_id: None,
            created_at: 0,
            updated_at: 0,
            title: String::new(),
            row_cache: Vec::new(),
            total_rows: 0,
            cache_width: None,
            input: String::new(),
            cursor: 0,
            sink: None,
            store_dirty: false,
            pending_agy_init: false,
            thought: String::new(),
            thought_cut: false,
            thought_since: None,
            last_geom: None,
            draft_answer: String::new(),
            phase: None,
            phase_since: None,
            turn_since: None,
            turn_first_line: 0,
            stopping: None,
            sealed: false,
            turn_id: None,
            tokens: None,
        }
    }

    /// Forget per-turn stop state (tab reset / session replace) so a stale
    /// stop can never emit a line on the fresh transcript.
    pub fn reset_turn_state(&mut self) {
        self.stopping = None;
        self.sealed = false;
        self.turn_id = None;
    }

    /// Record a backend token report. Zero/absent never clobbers a good value.
    pub fn note_tokens(&mut self, n: Option<u64>) {
        if let Some(n) = n.filter(|&n| n > 0) {
            self.tokens = Some(n);
        }
    }

    /// Start or end the activity phase to match `busy` (`busy` is flipped
    /// in many places; the main loop calls this once per iteration).
    pub fn sync_activity(&mut self) {
        if self.busy && self.phase.is_none() {
            let now = std::time::Instant::now();
            self.phase = Some(crate::activity::Phase::Thinking);
            self.phase_since = Some(now);
            self.turn_since = Some(now);
            self.turn_first_line = self.lines.len();
        } else if !self.busy && self.phase.is_some() {
            // Catch-all for turn ends a provider did not flush itself.
            self.flush_thought();
            let worked = self.turn_since.map(|t| t.elapsed());
            self.phase = None;
            self.phase_since = None;
            self.turn_since = None;
            // A stopped turn (asked or forced) gets `■ stopped`, not this.
            if let Some(d) = worked.filter(|_| self.stopping.is_none() && !self.sealed) {
                self.push_line(format!("Worked for {}", crate::activity::format_secs(d)));
            }
        }
        if !self.busy {
            self.turn_id = None;
            // A turn that ended after a stop request is a completed stop.
            if self.stopping.take().is_some() {
                self.push_line(STOPPED_LINE.to_string());
            }
        }
    }

    /// Switch the activity phase. Ignored while idle (late events).
    pub fn set_phase(&mut self, p: crate::activity::Phase) {
        if !self.busy {
            return;
        }
        self.sync_activity();
        if self.phase != Some(p) {
            self.phase = Some(p);
            self.phase_since = Some(std::time::Instant::now());
        }
    }

    /// Drop in-flight stream drafts (tab reset / session replace).
    pub fn clear_stream_drafts(&mut self) {
        self.thought.clear();
        self.thought_cut = false;
        self.thought_since = None;
        self.draft_answer.clear();
    }

    /// Append streamed thought text (word fragments, appended verbatim).
    /// Ignored while idle (late events). Keeps only the last ~4 KB.
    pub fn push_thought(&mut self, text: &str) {
        let text = sanitize_text(text);
        if !self.busy || text.is_empty() {
            return;
        }
        self.set_phase(crate::activity::Phase::Thinking);
        self.thought_since.get_or_insert_with(std::time::Instant::now);
        self.thought.push_str(&text);
        if self.thought.len() > THOUGHT_CAP {
            let mut cut = self.thought.len() - THOUGHT_CAP;
            while !self.thought.is_char_boundary(cut) {
                cut += 1;
            }
            self.thought.drain(..cut);
            self.thought_cut = true;
        }
    }

    /// End the current thought: commit ONE `◆ Thought for Ns` line when any
    /// thought text arrived, nothing otherwise.
    pub fn flush_thought(&mut self) {
        let since = self.thought_since.take();
        let had = !self.thought.trim().is_empty();
        self.thought.clear();
        self.thought_cut = false;
        if had {
            let d = since.map_or(std::time::Duration::ZERO, |t| t.elapsed());
            self.push_line(format!("◆ Thought for {}", crate::activity::format_elapsed(d)));
        }
    }

    /// Live thinking block (virtual, never persisted): `◆ Thinking…` header, then
    /// `…` if older text was cut, then the last 3 thought lines. Empty
    /// unless the tab is busy, thinking and not waiting on a diff.
    pub fn thought_block(&self) -> Vec<String> {
        use crate::activity::Phase;
        if !self.busy || self.phase != Some(Phase::Thinking) || self.pending_diff.is_some() {
            return Vec::new();
        }
        let mut out = vec![format!("◆ {}", Phase::Thinking.label())];
        let lines: Vec<String> = self
            .thought
            .split('\n')
            .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|l| !l.is_empty())
            .collect();
        if lines.len() > THOUGHT_LINES || (self.thought_cut && !lines.is_empty()) {
            out.push("  …".to_string());
        }
        for l in &lines[lines.len().saturating_sub(THOUGHT_LINES)..] {
            let n = l.chars().count();
            if n > THOUGHT_LINE_CAP {
                let tail: String = l.chars().skip(n - THOUGHT_LINE_CAP).collect();
                out.push(format!("  …{tail}"));
            } else {
                out.push(format!("  {l}"));
            }
        }
        out
    }

    /// Virtual trailing lines: live thinking block + open answer (render).
    pub fn stream_draft_lines(&self) -> Vec<String> {
        let mut out = self.thought_block();
        if !self.draft_answer.is_empty() {
            out.push(self.draft_answer.clone());
        }
        out
    }

    /// Markdown kinds of [`stream_draft_lines`]: the thought block is
    /// plain, the open answer continues the committed fence state.
    pub fn stream_draft_kinds(&self) -> Vec<MdKind> {
        let mut out = vec![MdKind::Plain; self.thought_block().len()];
        if !self.draft_answer.is_empty() {
            out.push(crate::ui::md_kind(&self.draft_answer, self.last_kind()));
        }
        out
    }

    /// Wrapped-row count of [`stream_draft_lines`] at `width`.
    pub fn draft_display_rows(&self, width: usize) -> usize {
        self.stream_draft_lines()
            .iter()
            .zip(self.stream_draft_kinds())
            .map(|(l, k)| crate::ui::line_rows(l, k, width.max(1)))
            .sum()
    }

    /// Markdown kind of committed line `i` (`Plain` when untracked).
    pub fn kind_at(&self, i: usize) -> MdKind {
        self.kinds.get(i).copied().unwrap_or(MdKind::Plain)
    }

    fn last_kind(&self) -> Option<MdKind> {
        self.lines.len().checked_sub(1).map(|i| self.kind_at(i))
    }

    pub fn push_line(&mut self, line: String) {
        let line = sanitize_text(&line);
        if let Some(sink) = self.sink.as_mut() {
            crate::store::append_line(sink, &line);
            self.store_dirty = true;
        }
        let w = self.cache_width.unwrap_or(DEFAULT_WIDTH);
        let kind = crate::ui::md_kind(&line, self.last_kind());
        let rows = crate::ui::line_rows(&line, kind, w);
        self.kinds.resize(self.lines.len(), MdKind::Plain);
        self.kinds.push(kind);
        self.lines.push(line);
        self.row_cache.push(rows);
        self.total_rows += rows;
        if self.lines.len() > MAX_LINES {
            let drop = self.lines.len() - MAX_LINES;
            let dropped_rows: usize = self.row_cache[..drop].iter().sum();
            self.lines.drain(..drop);
            self.kinds.drain(..drop);
            self.row_cache.drain(..drop);
            self.total_rows -= dropped_rows;
            self.scroll = self.scroll.saturating_sub(dropped_rows);
        }
    }

    /// Replace the whole transcript at once (session switch): the wrap
    /// cache is built in the same pass at `width`, so replay never pays
    /// a per-line push plus a second full rebuild. Old allocations are
    /// dropped instead of retained, keeping a large viewed session from
    /// pinning memory after switching away. Over-cap heads are dropped,
    /// mirroring `push_line`.
    pub fn replace_lines(&mut self, mut lines: Vec<String>, width: usize) {
        for l in lines.iter_mut() {
            let clean = sanitize_text(l);
            *l = clean;
        }
        let w = width.max(1);
        if lines.len() > MAX_LINES {
            lines.drain(..lines.len() - MAX_LINES);
        }
        let mut row_cache = Vec::with_capacity(lines.len());
        let mut kinds: Vec<MdKind> = Vec::with_capacity(lines.len());
        let mut total_rows = 0usize;
        for l in &lines {
            let kind = crate::ui::md_kind(l, kinds.last().copied());
            let rows = crate::ui::line_rows(l, kind, w);
            kinds.push(kind);
            row_cache.push(rows);
            total_rows += rows;
        }
        self.lines = lines;
        self.kinds = kinds;
        self.row_cache = row_cache;
        self.total_rows = total_rows;
        self.cache_width = Some(w);
        self.scroll = 0;
        self.last_geom = None;
        self.clear_stream_drafts();
    }

    /// Rebuild the height cache when the viewport width changed.
    pub fn ensure_cache(&mut self, width: usize) {
        if self.cache_width == Some(width) {
            return;
        }
        self.row_cache = self
            .lines
            .iter()
            .enumerate()
            .map(|(i, l)| crate::ui::line_rows(l, self.kind_at(i), width))
            .collect();
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

    /// Open the DIFF card and invalidate any in-flight risk judgment.
    pub fn stage_diff(&mut self, diff: PendingDiff) {
        // S7-F3: replacing an undecided card is the intra-client TOCTOU —
        // the user may have read the old card and be about to press `y`.
        // Re-prompt loudly so they review the new content instead.
        // (Render→keypress itself is race-free: the event loop is
        // single-threaded, so the pressed key always answers the last
        // rendered card. Server-side content changes past the request
        // stay outside any client gate and are documented as such.)
        let replaced = self.pending_diff.is_some();
        self.pending_diff = Some(PendingDiff {
            file: sanitize_text(&diff.file),
            body: sanitize_text(&diff.body),
        });
        self.approval_risk = None;
        self.risk_gen = self.risk_gen.wrapping_add(1);
        self.risk_spawned_gen = None;
        if replaced {
            self.push_line(
                "approval card updated before your decision — review again before y (content changed)"
                    .to_string(),
            );
        }
    }

    /// Close the DIFF card; bump gen so late TypeSafe answers are ignored.
    pub fn clear_diff(&mut self) -> Option<PendingDiff> {
        self.approval_risk = None;
        self.risk_gen = self.risk_gen.wrapping_add(1);
        self.risk_spawned_gen = None;
        self.pending_diff.take()
    }
}

pub struct App {
    pub sessions: Vec<Session>,
    pub active: usize,
    pub outbox: Outbox,
    pub picker_sel: usize,
    pub mode: Mode,
    /// Selected index into the current slash-command suggestions
    /// (Insert mode, input starts with `/`). Always clamped by the
    /// completion helpers; reset to 0 on every input edit.
    pub cmd_sel: usize,
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
    /// Transcript store (None = storageless; main attaches it on boot).
    pub store: Option<crate::store::Store>,
    /// Spawn-order queue for agy init routing (resume matches by id first).
    pub agy_init_fifo: std::collections::VecDeque<usize>,
    /// `/sessions` chooser state (newest first).
    pub sess_list: Vec<crate::store::StoredSession>,
    pub sess_sel: usize,
    /// Vim keymap (j/k/g/G/i/a). Default off; `/vim` toggles + persists.
    /// Picker/sessions modals keep j/k either way (list navigation).
    pub vim: bool,
    /// Active UI palette. Set from `[ui] theme` at boot; `/theme`
    /// switches it live + persists. ui.rs resolves it per frame.
    pub theme: crate::theme::ThemeKind,
    /// Workspace root shown on the empty-tab welcome dashboard (set from
    /// config at boot; empty hides the row).
    pub workspace: String,
    /// First run (no `[polyforge] provider` yet): boot opens the picker
    /// and starts nothing until a provider is chosen. Esc quits instead
    /// of cancelling — there is no backend to fall back to.
    pub onboarding: bool,
    /// Closed agy tab indices awaiting child kill (main loop drains this;
    /// handles live there, not in App, because shutdown is async).
    pub pending_agy_kill: Vec<usize>,
    /// Closed grok ACP session ids awaiting server-side close (best-effort;
    /// ids, not indices, so later closes can't shift them).
    pub pending_grok_close: Vec<String>,
    /// Claude session ids whose child must be killed (tab closed, switched
    /// or re-attached). Ids, not indices, like grok.
    pub pending_claude_kill: Vec<String>,
    /// Inner transcript rect (x, y, w, h) from the last render, for mouse
    /// cell → text mapping. None before the first frame.
    pub text_area: Option<(u16, u16, u16, u16)>,
    /// `[stop]` cells of the last frame (None when the row was not drawn).
    pub stop_hit: Option<ratatui::layout::Rect>,
    /// Active mouse drag selection (highlight + copy source).
    pub sel: Option<Selection>,
}

impl App {
    pub fn new() -> Self {
        let mut app = Self {
            // Boot opens exactly one tab; `/tab new` grows to MAX_SESSIONS.
            sessions: vec![Session::new("s1")],
            active: 0,
            outbox: Outbox::default(),
            picker_sel: 0,
            mode: Mode::Normal,
            cmd_sel: 0,
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
            sess_list: Vec::new(),
            sess_sel: 0,
            pending_agy_kill: Vec::new(),
            pending_grok_close: Vec::new(),
            pending_claude_kill: Vec::new(),
            vim: false,
            theme: crate::theme::ThemeKind::default(),
            workspace: String::new(),
            onboarding: false,
            text_area: None,
            stop_hit: None,
            sel: None,
        };
        // Fresh tabs open empty: no placeholder filler. Tests that need a
        // scrollable transcript call mock::seed explicitly.
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
        let w = self.viewport_width.max(1);
        let total = s.total_rows + s.draft_display_rows(w);
        total.saturating_sub(self.viewport_height.max(1))
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
        let total = s.total_rows + s.draft_display_rows(w);
        let tail = total.saturating_sub(vh);
        s.scroll = tail;
    }

    // -- submit / tick (mock streaming; all tabs tick = background agents) --

    pub fn submit(&mut self) {
        let prompt = std::mem::take(&mut self.active_mut().input);
        self.active_mut().cursor = 0;
        self.cmd_sel = 0;
        if prompt.trim().is_empty() {
            return;
        }
        // Slash commands never reach a backend. A command may set its
        // own mode (e.g. `/sessions`); only fall back to Normal.
        if prompt.trim_start().starts_with('/') {
            self.run_command(prompt.trim());
            if self.mode == Mode::Insert {
                self.mode = Mode::Normal;
            }
            self.stick_to_bottom();
            return;
        }
        // First prompt names the stored session (the `/sessions` title).
        {
            let s = self.active_mut();
            if s.title.is_empty() {
                s.title = prompt.chars().take(TITLE_LEN).collect();
            }
        }
        self.save_tab_meta(self.active);
        let tab = self.active;
        let backend = self.sessions[tab].backend;
        let sid = self.sessions[tab].remote_id.clone();
        let degraded = self.sessions[tab].tab_degraded.clone();
        let s = self.active_mut();
        s.sealed = false;
        s.turn_id = None;
        s.push_line(format!("> {prompt}"));
        match backend {
            BackendKind::Mock => crate::mock::start_job(s, &prompt),
            BackendKind::Muse | BackendKind::Codex | BackendKind::Grok | BackendKind::Claude => {
                match sid {
                    Some(_) => {
                        s.busy = true;
                        self.outbox.submits.push(OutboxSubmit {
                            tab,
                            backend,
                            prompt,
                        });
                    }
                    None => {
                        let tag = backend.label();
                        let why = degraded.unwrap_or_else(|| format!("{tag} session unavailable"));
                        s.push_line(format!("{tag}: {why}"));
                    }
                }
            }
            // Agy prompts queue into the tab's child stdin and run in turn
            // order; no session id is needed up front (init assigns it).
            BackendKind::Agy => {
                s.busy = true;
                self.outbox.submits.push(OutboxSubmit {
                    tab,
                    backend,
                    prompt,
                });
            }
        }
        self.mode = Mode::Normal;
        self.stick_to_bottom();
    }

    /// Stop the active tab's running turn. First press asks the backend to
    /// stop (queued in the outbox); a second press while it is still
    /// stopping gives up locally and seals the tab against late re-busy.
    pub fn request_stop(&mut self) {
        let tab = self.active;
        let s = &mut self.sessions[tab];
        // A card must be answered first (Esc defers it instead).
        if !s.busy || s.pending_diff.is_some() {
            return;
        }
        let now = std::time::Instant::now();
        if s.backend == BackendKind::Mock {
            s.busy = false;
            s.queue.clear();
            s.diff_after = None;
            s.stopping = Some(now);
            self.flash = "stopping…".to_string();
            return;
        }
        if s.stopping.is_some() {
            s.busy = false;
            s.stopping = None;
            s.sealed = true;
            s.turn_id = None;
            s.push_line(FORCED_STOP_LINE.to_string());
            self.flash = FORCED_STOP_LINE.to_string();
            self.stick_to_bottom();
            return;
        }
        // Codex interrupts by turn id; without one there is nothing to name.
        if s.backend == BackendKind::Codex && s.turn_id.is_none() {
            self.flash = "codex: turn not started yet — try again".to_string();
            return;
        }
        s.stopping = Some(now);
        self.outbox.stops.push(OutboxStop {
            tab,
            backend: s.backend,
            remote_id: s.remote_id.clone(),
            turn_id: s.turn_id.clone(),
        });
        self.flash = "stopping…".to_string();
    }

    /// Left click on the `[stop]` cells of the last frame. True when the
    /// click was on the button and the tab could act on it.
    pub fn click_stop(&mut self, col: u16, row: u16) -> bool {
        let Some(r) = self.stop_hit else {
            return false;
        };
        let s = self.active();
        if !r.contains(ratatui::layout::Position::new(col, row))
            || !s.busy
            || s.pending_diff.is_some()
        {
            return false;
        }
        self.request_stop();
        true
    }

    /// Indices into `SLASH_COMMANDS` matching the active tab's input as
    /// a prefix. Empty unless the input starts with `/`; typing args
    /// past a complete command (e.g. `/sessions foo`) hides the list.
    pub fn slash_matches(&self) -> Vec<usize> {
        let input = self.active().input.as_str();
        if !input.starts_with('/') {
            return Vec::new();
        }
        SLASH_COMMANDS
            .iter()
            .enumerate()
            .filter(|(_, (template, _, _))| template.starts_with(input))
            .map(|(i, _)| i)
            .collect()
    }

    /// Move the suggestion highlight, wrapping around the current list.
    /// `delta` is +1 (next) or -1 (previous). No-op when the list is empty.
    pub fn cycle_cmd_sel(&mut self, delta: i32) {
        let n = self.slash_matches().len();
        if n == 0 {
            self.cmd_sel = 0;
            return;
        }
        self.cmd_sel = (self.cmd_sel as i32 + delta).rem_euclid(n as i32) as usize;
    }

    /// Replace the input with the highlighted suggestion, cursor to the
    /// end. Returns false when no suggestion is showing.
    pub fn accept_slash_completion(&mut self) -> bool {
        let matches = self.slash_matches();
        let Some(&i) = matches.get(self.cmd_sel.min(matches.len().saturating_sub(1))) else {
            return false;
        };
        let (_, insert, _) = SLASH_COMMANDS[i];
        let s = self.active_mut();
        s.input = insert.to_string();
        s.cursor = s.input.chars().count();
        self.cmd_sel = 0;
        true
    }

    /// Slash commands (typed in Insert mode, never sent to a backend):
    /// `/sessions [query]`, `/new`, `/tab new`, `/tab close`, `/theme [name]`,
    /// `/vim`, `/help`.
    pub fn run_command(&mut self, cmd: &str) {
        let mut parts = cmd.split_whitespace();
        match parts.next().unwrap_or("") {
            "/sessions" => {
                let query: Vec<&str> = parts.collect();
                self.open_session_chooser(query.join(" "));
            }
            // Fresh session in the ACTIVE tab (command form of R; `/tab
            // new` puts the fresh session in a new tab instead).
            "/new" => {
                let cur = self.sessions[self.active].backend;
                self.respawn_active(cur);
            }
            "/tab" => match parts.next().unwrap_or("") {
                "new" => self.open_tab(),
                "close" => {
                    if let Err(msg) = self.close_active_tab() {
                        self.flash = msg;
                    }
                }
                _ => self.flash = "usage: /tab new · /tab close".to_string(),
            },
            "/vim" => self.toggle_vim(),
            "/theme" => {
                let arg = parts.next().map(|s| s.to_string());
                self.run_theme_command(arg);
            }
            "/help" => self.show_help(),
            _ => {
                self.flash =
                    "unknown command — /sessions · /new · /tab new · /tab close · /theme · /vim · /help"
                        .to_string()
            }
        }
    }

    /// Flip the vim keymap and persist it to the config file so the
    /// choice sticks across runs. A failed save keeps the in-memory
    /// value and says so (never blocks typing).
    pub fn toggle_vim(&mut self) {
        self.vim = !self.vim;
        let mut cfg = crate::config::Config::load();
        cfg.polyforge.vim = self.vim;
        match cfg.save() {
            Ok(()) => {
                self.flash = format!(
                    "vim mode {} (saved to {})",
                    if self.vim { "on" } else { "off" },
                    crate::config::Config::path().display()
                );
            }
            Err(e) => {
                self.flash = format!(
                    "vim mode {} (NOT saved: {e})",
                    if self.vim { "on" } else { "off" }
                );
            }
        }
    }

    /// Switch the UI theme: `/theme <name>` applies it, bare `/theme`
    /// cycles to the next palette. Persists to `[ui] theme` like `/vim`
    /// so the choice sticks across runs; a failed save keeps the
    /// in-memory theme and says so.
    pub fn run_theme_command(&mut self, arg: Option<String>) {
        let kind = match arg.as_deref() {
            None => self.theme.cycle(),
            Some(name) => match name.parse::<crate::theme::ThemeKind>() {
                Ok(k) => k,
                Err(e) => {
                    self.flash = e;
                    return;
                }
            },
        };
        self.theme = kind;
        let mut cfg = crate::config::Config::load();
        cfg.ui.theme = kind.name().to_string();
        match cfg.save() {
            Ok(()) => {
                self.flash = format!(
                    "theme {} (saved to {})",
                    kind.name(),
                    crate::config::Config::path().display()
                );
            }
            Err(e) => {
                self.flash = format!("theme {} (NOT saved: {e})", kind.name());
            }
        }
    }

    /// Fill the `/sessions` chooser, most active first (grok's `sessions
    /// list` ordering). An optional query filters title + preview + id
    /// (grok's `sessions search`). Empty results flash instead of opening
    /// a dead modal.
    pub fn open_session_chooser(&mut self, query: String) {
        let Some(store) = self.store.as_ref() else {
            self.flash = "no session store (storageless)".to_string();
            return;
        };
        let q = query.trim().to_lowercase();
        self.sess_list = store
            .list_sessions(&self.workspace)
            .into_iter()
            .filter(|s| {
                q.is_empty()
                    || s.title.to_lowercase().contains(&q)
                    || s.preview.to_lowercase().contains(&q)
                    || s.id.to_lowercase().contains(&q)
            })
            .collect();
        // Group by provider: the active tab's provider first, then picker
        // order. Stable sort keeps most-active-first within each group.
        let cur = self.active().backend;
        self.sess_list.sort_by_key(|s| {
            let b = BackendKind::parse(&s.backend).unwrap_or_default();
            let pos = BackendKind::ALL.iter().position(|(k, _)| *k == b);
            (b != cur, pos)
        });
        if self.sess_list.is_empty() {
            self.flash = if q.is_empty() {
                "no previous sessions in this workspace".to_string()
            } else {
                format!("no sessions match: {query}")
            };
            return;
        }
        self.sess_sel = 0;
        self.mode = Mode::Sessions;
    }

    /// Delete the chooser-selected stored session (`grok sessions delete`).
    /// Refuses sessions open in a live tab (close the tab first): deleting
    /// under a running tab would resurrect the files on the next flush.
    pub fn delete_selected_session(&mut self) {
        let Some(pick) = self.sess_list.get(self.sess_sel).cloned() else {
            return;
        };
        if let Some(tab) = self
            .sessions
            .iter()
            .position(|s| s.store_id.as_deref() == Some(pick.id.as_str()))
        {
            self.flash = format!(
                "session is open in {} — /tab close it first",
                self.sessions[tab].name
            );
            return;
        }
        let Some(store) = self.store.as_ref() else {
            self.flash = "no session store (storageless)".to_string();
            return;
        };
        if store.delete_session(&pick.id) {
            self.flash = format!("deleted session {}", short_id(&pick.id));
            self.sess_list.remove(self.sess_sel);
            self.sess_sel = self.sess_sel.min(self.sess_list.len().saturating_sub(1));
            if self.sess_list.is_empty() {
                self.mode = Mode::Normal;
            }
        } else {
            self.flash = format!("deleted nothing ({})", short_id(&pick.id));
        }
    }

    /// Load the chosen stored session into the ACTIVE tab (view + continue):
    /// replay its transcript, re-attach its remote id, and queue a respawn
    /// so the drain re-attaches the live session (resume, else fresh).
    pub fn choose_session(&mut self, idx: usize) {
        let Some(pick) = self.sess_list.get(idx).cloned() else {
            return;
        };
        // Same-tab re-attach stays allowed. Another tab already showing
        // this store id must keep its child (resume would kill it).
        if let Some(open) =
            self.sessions.iter().enumerate().position(|(i, s)| {
                i != self.active && s.store_id.as_deref() == Some(pick.id.as_str())
            })
        {
            self.flash = format!(
                "session is open in {} — /tab close it first",
                self.sessions[open].name
            );
            return;
        }
        let tab = self.active;
        // Drop queued work for this tab; it belongs to the old session.
        self.outbox.submits.retain(|o| o.tab != tab);
        self.outbox.decides.retain(|o| o.tab != tab);
        self.outbox.respawns.retain(|o| o.tab != tab);
        self.outbox.stops.retain(|o| o.tab != tab);
        self.agy_init_fifo.retain(|&t| t != tab);
        self.queue_claude_kill(tab);
        let backend = BackendKind::parse(&pick.backend).unwrap_or_default();
        let store = self.store.as_ref().expect("chooser needs a store");
        // Re-read meta at choose time: the listing may predate another
        // run's writes (newer remote id / title / updated win).
        let (remote_id, title, updated_at) = match store.load_meta(&pick.id) {
            Some(meta) => (meta.remote_id, meta.title, meta.updated_at),
            None => (pick.remote_id.clone(), pick.title.clone(), pick.updated_at),
        };
        {
            let s = self.active_mut();
            s.backend = backend;
            if s.remote_id != remote_id {
                s.tokens = None;
            }
            s.remote_id = remote_id;
            s.tab_degraded = None;
            s.busy = false;
            s.reset_turn_state();
            s.pending_agy_init = false;
            s.queue.clear();
            s.diff_after = None;
            let _ = s.clear_diff();
            s.pending_approval = None;
            s.store_id = Some(pick.id.clone());
            s.created_at = pick.created_at;
            s.updated_at = updated_at;
            s.title = title;
            s.sink = None;
        }
        let store = self.store.as_ref().expect("chooser needs a store");
        let lines = store.load_transcript(&pick.id);
        let n = lines.len();
        // Bulk replay at the live viewport width: one wrap pass, and the
        // previous transcript's allocations are freed (not retained).
        let width = self.viewport_width.max(1);
        self.sessions[tab].replace_lines(lines, width);
        self.sessions[tab].sink = store.open_sink(&pick.id);
        // Banner is UI-only: detach sink so it never grows JSONL.
        let sink = self.sessions[tab].sink.take();
        self.sessions[tab].push_line(format!(
            "(viewing {n} lines from {} — {} continues, R starts fresh)",
            crate::store::fmt_time(pick.created_at),
            backend.label()
        ));
        self.sessions[tab].sink = sink;
        self.outbox.respawns.push(OutboxRespawn { tab, backend });
        self.mode = Mode::Normal;
        self.stick_to_bottom();
    }

    /// Open a new tab (up to MAX_SESSIONS) with a fresh session id and
    /// queue its bringup. The new tab becomes active.
    pub fn open_tab(&mut self) {
        if self.sessions.len() >= MAX_SESSIONS {
            self.flash = format!("already {} tabs (max)", MAX_SESSIONS);
            return;
        }
        let n = self.sessions.len() + 1;
        let mut s = Session::new(&format!("s{n}"));
        s.backend = self.sessions[self.active].backend;
        self.sessions.push(s);
        self.active = self.sessions.len() - 1;
        self.attach_fresh_store(self.active);
        let backend = self.sessions[self.active].backend;
        {
            let s = self.active_mut();
            s.push_line(format!("--- {} session (fresh) ---", backend.label()));
        }
        self.save_tab_meta(self.active);
        self.outbox.respawns.push(OutboxRespawn {
            tab: self.active,
            backend,
        });
        self.stick_to_bottom();
    }

    /// Close the active tab and kill its session: queued work is dropped,
    /// the agy child (if any) is queued for kill by the main loop, and the
    /// muse/codex remote id is abandoned (no vendor kill API — the stored
    /// transcript stays for `/sessions`). Refuses the last tab.
    pub fn close_active_tab(&mut self) -> Result<(), String> {
        if self.sessions.len() <= 1 {
            return Err("can't close the last tab — R starts it fresh".to_string());
        }
        let tab = self.active;
        let backend = self.sessions[tab].backend;
        // Drop queued work for this tab; it belongs to the dead session.
        self.outbox.submits.retain(|o| o.tab != tab);
        self.outbox.decides.retain(|o| o.tab != tab);
        self.outbox.respawns.retain(|o| o.tab != tab);
        self.outbox.stops.retain(|o| o.tab != tab);
        self.agy_init_fifo.retain(|&t| t != tab);
        if backend == BackendKind::Agy {
            self.pending_agy_kill.push(tab);
        }
        if backend == BackendKind::Grok {
            if let Some(id) = self.sessions[tab].remote_id.clone() {
                self.pending_grok_close.push(id);
            }
        }
        self.queue_claude_kill(tab);
        // Flush before dropping the sink so the transcript keeps its tail.
        if let Some(sink) = self.sessions[tab].sink.as_mut() {
            use std::io::Write;
            let _ = sink.flush();
        }
        self.sessions.remove(tab);
        // Renumber everything above the gap (tabs, outbox, agy routing).
        for s in self.outbox.submits.iter_mut() {
            if s.tab > tab {
                s.tab -= 1;
            }
        }
        for d in self.outbox.decides.iter_mut() {
            if d.tab > tab {
                d.tab -= 1;
            }
        }
        for r in self.outbox.respawns.iter_mut() {
            if r.tab > tab {
                r.tab -= 1;
            }
        }
        for st in self.outbox.stops.iter_mut() {
            if st.tab > tab {
                st.tab -= 1;
            }
        }
        for t in self.agy_init_fifo.iter_mut() {
            if *t > tab {
                *t -= 1;
            }
        }
        // pending_agy_kill is deliberately NOT renumbered: entries refer to
        // the layout at close time and the main loop compensates removals.
        for (i, s) in self.sessions.iter_mut().enumerate() {
            s.name = format!("s{}", i + 1);
        }
        self.active = tab.min(self.sessions.len() - 1);
        self.flash = format!("closed tab ({} session killed)", backend.label());
        self.stick_to_bottom();
        Ok(())
    }

    /// UI-only command help (never persisted: detached from the sink).
    fn show_help(&mut self) {
        let sink = self.active_mut().sink.take();
        let move_keys = if self.vim {
            "j/k line · g/G top/bottom · Space/i/a type"
        } else {
            "arrows/HOME/END/PgUp/PgDn · Space/Enter types · /vim for vim keys"
        };
        for l in [
            "commands (Insert mode, Enter sends):".to_string(),
            "  /sessions [query] — browse previous sessions, Enter views + continues, d deletes"
                .to_string(),
            "  /new — fresh session in this tab (same as R)".to_string(),
            "  /tab new — open a tab (max 3), same backend as current".to_string(),
            "  /tab close — close this tab, killing its session".to_string(),
            "  /theme [name] — switch theme, bare cycles (saved to config)".to_string(),
            "  /vim — toggle vim keymap (saved to config)".to_string(),
            format!("keys (Normal mode): {move_keys} · P provider · R fresh · Esc stop turn · q quit"),
        ] {
            self.active_mut().push_line(l);
        }
        self.active_mut().sink = sink;
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

    /// Record a tab's stored session (backend + remote id + title) for
    /// `/sessions`, bumping UPDATED to now. No-op when storageless or the
    /// tab has no store id.
    pub fn save_tab_meta(&mut self, tab: usize) {
        let now = crate::store::now_millis();
        if let Some(s) = self.sessions.get_mut(tab) {
            s.updated_at = now;
        }
        if let (Some(store), Some(s)) = (self.store.as_ref(), self.sessions.get(tab)) {
            if let Some(id) = s.store_id.as_deref() {
                store.save_meta(
                    id,
                    &crate::store::SessionMeta {
                        backend: s.backend.label().to_string(),
                        remote_id: s.remote_id.clone(),
                        created_at: s.created_at,
                        updated_at: s.updated_at,
                        title: s.title.clone(),
                        workspace: self.workspace.clone(),
                    },
                );
            }
        }
    }

    /// Give a tab a FRESH store id + sink + meta (boot, respawn, new tab).
    /// The previous session's files stay on disk for `/sessions`.
    /// No-op when storageless.
    pub fn attach_fresh_store(&mut self, tab: usize) {
        if self.store.is_none() || tab >= self.sessions.len() {
            return;
        }
        let id = crate::store::Store::new_session_id();
        let created_at = crate::store::now_millis();
        {
            let s = &mut self.sessions[tab];
            s.sink = None;
            s.store_id = Some(id.clone());
            s.created_at = created_at;
            s.updated_at = created_at;
            s.title = String::new();
        }
        let store = self.store.as_ref().expect("checked");
        self.sessions[tab].sink = store.open_sink(&id);
        self.save_tab_meta(tab);
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

    /// First-run provider choice: bring the still-empty tab up on it
    /// (the welcome dashboard stays, no fresh-session marker) and save it
    /// to `[polyforge] provider` so later runs skip the picker. A failed
    /// save keeps the choice for this run and says so.
    pub fn choose_first_backend(&mut self, backend: BackendKind) {
        self.onboarding = false;
        self.mode = Mode::Normal;
        let tab = self.active;
        self.sessions[tab].backend = backend;
        self.save_tab_meta(tab);
        self.outbox.respawns.push(OutboxRespawn { tab, backend });
        let mut cfg = crate::config::Config::load();
        cfg.polyforge.provider = Some(backend.label().to_string());
        self.flash = match cfg.save() {
            Ok(()) => format!(
                "provider {} (saved to {}; P switches)",
                backend.label(),
                crate::config::Config::path().display()
            ),
            Err(e) => format!("provider {} (NOT saved: {e})", backend.label()),
        };
    }

    /// Queue the tab's claude child (if any) for kill: its session is
    /// being closed or replaced.
    fn queue_claude_kill(&mut self, tab: usize) {
        let s = &self.sessions[tab];
        if s.backend == BackendKind::Claude {
            if let Some(id) = s.remote_id.clone() {
                self.pending_claude_kill.push(id);
            }
        }
    }

    pub fn respawn_active(&mut self, backend: BackendKind) {
        let tab = self.active;
        // Drop queued work for this tab; it belongs to the old session.
        self.outbox.submits.retain(|o| o.tab != tab);
        self.outbox.decides.retain(|o| o.tab != tab);
        self.outbox.respawns.retain(|o| o.tab != tab);
        self.outbox.stops.retain(|o| o.tab != tab);
        self.agy_init_fifo.retain(|&t| t != tab);
        self.queue_claude_kill(tab);
        // Fresh session id on disk too (old files stay for `/sessions`).
        self.attach_fresh_store(tab);
        {
            let s = self.active_mut();
            s.backend = backend;
            s.remote_id = None;
            s.tokens = None;
            s.tab_degraded = None;
            s.busy = false;
            s.reset_turn_state();
            s.pending_agy_init = false;
            s.queue.clear();
            s.diff_after = None;
            let _ = s.clear_diff();
            s.pending_approval = None;
            s.lines.clear();
            s.kinds.clear();
            s.row_cache.clear();
            s.total_rows = 0;
            s.scroll = 0;
            s.clear_stream_drafts();
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
        let Some(diff) = s.clear_diff() else {
            return false;
        };
        s.pending_approval.take();
        s.push_line(format!(
            "{tag}: {} {} [{decision}]",
            muse_mark(decision),
            diff.file
        ));
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
            if pushed {
                s.set_phase(crate::activity::Phase::Responding);
            }
            if pushed && pinned {
                s.scroll = s.total_rows.saturating_sub(vh);
            }
            if s.queue.is_empty() {
                if let Some(diff) = s.diff_after.take() {
                    s.stage_diff(diff);
                } else {
                    s.busy = false;
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
        let Some(diff) = s.clear_diff() else {
            return false;
        };
        s.push_line(format!(
            "{} {} [{}]",
            diff_mark(decision),
            diff.file,
            decision
        ));
        s.busy = false;
        self.stick_to_bottom();
        true
    }

    // -- mouse drag selection (highlight + copy) --

    /// Start a drag at terminal cell (col, row). Outside the transcript
    /// viewport (or with mouse capture off) this is a no-op returning false.
    pub fn sel_begin(&mut self, col: u16, row: u16) -> bool {
        match self.cell_to_text(col, row) {
            Some((line, ch)) => {
                self.sel = Some(Selection {
                    anchor_line: line,
                    anchor_char: ch,
                    focus_line: line,
                    focus_char: ch,
                });
                true
            }
            None => false,
        }
    }

    /// Extend the active drag to terminal cell (col, row). Clamps to the
    /// viewport: drags outside keep the last in-bounds focus.
    pub fn sel_extend(&mut self, col: u16, row: u16) {
        if let Some((line, ch)) = self.cell_to_text(col, row) {
            if let Some(sel) = self.sel.as_mut() {
                sel.focus_line = line;
                sel.focus_char = ch;
            }
        }
    }

    /// Map a terminal cell to (transcript line, char index), honoring the
    /// last render's origin, the scroll offset, and wrapped rows.
    pub fn cell_to_text(&self, col: u16, row: u16) -> Option<(usize, usize)> {
        let (ax, ay, _w, h) = self.text_area?;
        let c = col.checked_sub(ax)? as usize;
        let vr = row.checked_sub(ay)? as usize;
        if vr >= h as usize {
            return None;
        }
        let s = self.active();
        let target = s.scroll + vr;
        // Walk the height cache to the visible row (same walk as render).
        let mut li = 0usize;
        let mut consumed = 0usize;
        while li < s.lines.len() && consumed + s.row_cache.get(li).copied().unwrap_or(1) <= target {
            consumed += s.row_cache.get(li).copied().unwrap_or(1);
            li += 1;
        }
        let line = s.lines.get(li)?;
        let row_in_line = target - consumed;
        // Chunk offset: char count of the chunks above this one. The render
        // path wraps the DISPLAY text at viewport_width (less the row indent
        // of prompt lines), so the mapping must use the same.
        let d = crate::ui::display_line(line, s.kind_at(li), crate::theme::Theme::groknight());
        let width = self.viewport_width.max(1).saturating_sub(d.indent).max(1);
        let chunks = Session::wrap_line(&d.text, width);
        let chunk = chunks.get(row_in_line)?;
        let mut coff = 0usize;
        for prev in chunks.iter().take(row_in_line) {
            coff += prev.chars().count();
        }
        Some((li, coff + col_to_char(chunk, c.saturating_sub(d.indent))))
    }

    /// The selected text (lines joined with `\n`), or None when empty.
    pub fn selected_text(&self) -> Option<String> {
        let sel = self.sel?;
        if sel.is_empty() {
            return None;
        }
        let s = self.active();
        let ((a_line, _), (f_line, _)) = sel.normalized();
        let mut out = Vec::new();
        for (li, line) in s.lines.iter().enumerate() {
            if li < a_line || li > f_line {
                continue;
            }
            // Selection indexes the display text (what render shows).
            let text =
                crate::ui::display_line(line, s.kind_at(li), crate::theme::Theme::groknight()).text;
            let len = text.chars().count();
            if let Some((cs, ce)) = sel.span_on_line(li, len) {
                out.push(text.chars().skip(cs).take(ce - cs).collect::<String>());
            }
        }
        if out.is_empty() {
            return None;
        }
        Some(sanitize_text(&out.join("\n")))
    }

    /// Selection text for auto-copy; always clears the highlight.
    pub fn take_selected_text(&mut self) -> Option<String> {
        let text = self.selected_text();
        self.sel = None;
        text
    }

    // -- search (/ + n/N) --

    /// True when a failed search looks like a slash command typed in the
    /// wrong mode (`/tab close` in Normal lands here as `tab close`).
    fn looks_like_command(query: &str) -> bool {
        let q = query.trim().to_lowercase();
        q == "sessions" || q == "vim" || q == "help" || q == "tab" || q.starts_with("tab ")
    }

    pub fn run_search(&mut self) {
        self.last_query = self.search_input.clone();
        // Match on the display text so jumps agree with what is drawn.
        let th = crate::theme::Theme::groknight();
        let s = self.active();
        self.matches = s
            .lines
            .iter()
            .enumerate()
            .filter(|(i, l)| {
                crate::ui::display_line(l, s.kind_at(*i), th)
                    .text
                    .contains(&self.last_query)
            })
            .map(|(i, _)| i)
            .collect();
        if self.matches.is_empty() {
            self.flash = if Self::looks_like_command(&self.last_query) {
                format!(
                    "no match — commands run from INSERT mode: Enter, then /{}",
                    self.last_query.trim()
                )
            } else {
                format!("no match: {}", self.last_query)
            };
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

    /// Serializes the tests that mutate `XDG_CONFIG_HOME` (`/vim` and
    /// `/theme` persistence): parallel mutation would redirect each
    /// other's save/read mid-test.
    static CONFIG_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock_config_env() -> std::sync::MutexGuard<'static, ()> {
        CONFIG_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    const VH: usize = 20; // fixed viewport

    fn thinking_session() -> Session {
        let mut s = Session::new("t");
        s.push_line("> hi".into());
        s.busy = true;
        s.sync_activity();
        s
    }

    #[test]
    fn live_block_header_shows_while_thinking_for_any_provider() {
        let mut s = thinking_session();
        let d = s.stream_draft_lines();
        assert_eq!(d.len(), 1);
        assert!(d[0].ends_with(" Thinking…"));
        // Hidden once answering, on a pending diff, or when idle.
        s.set_phase(crate::activity::Phase::Responding);
        assert!(s.stream_draft_lines().is_empty());
        s.set_phase(crate::activity::Phase::Thinking);
        s.stage_diff(PendingDiff {
            file: "a".into(),
            body: "b".into(),
        });
        assert!(s.stream_draft_lines().is_empty());
        let _ = s.clear_diff();
        s.busy = false;
        assert!(s.stream_draft_lines().is_empty());
    }

    #[test]
    fn live_block_body_is_last_three_lines_with_ellipsis() {
        let mut s = thinking_session();
        s.push_thought("one\ntwo\nthree");
        let d = s.stream_draft_lines();
        assert_eq!(d[1..], ["  one", "  two", "  three"]);
        s.push_thought("\nfour\n");
        let d = s.stream_draft_lines();
        assert_eq!(d[1..], ["  …", "  two", "  three", "  four"]);
    }

    #[test]
    fn thought_buffer_is_bounded_and_char_safe() {
        let mut s = thinking_session();
        for _ in 0..3000 {
            s.push_thought("héllo wörld ");
        }
        assert!(s.thought.len() <= THOUGHT_CAP);
        assert!(s.thought_cut);
        assert!(s.stream_draft_lines()[1].starts_with("  …"));
    }

    #[test]
    fn flush_thought_commits_only_when_text_existed() {
        let mut s = thinking_session();
        s.flush_thought();
        assert_eq!(s.lines, ["> hi"]);
        s.push_thought("   ");
        s.flush_thought();
        assert_eq!(s.lines, ["> hi"]);
        s.push_thought("hmm");
        s.flush_thought();
        assert_eq!(s.lines, ["> hi", "◆ Thought for 0s"]);
        assert!(s.thought.is_empty() && s.thought_since.is_none());
    }

    #[test]
    fn thought_ignored_while_idle_and_flushed_at_turn_end() {
        let mut s = Session::new("t");
        s.push_thought("late");
        assert!(s.thought.is_empty());
        let mut s = thinking_session();
        s.push_thought("hmm");
        s.busy = false;
        s.sync_activity();
        assert!(s.lines.iter().any(|l| l.starts_with("◆ Thought for ")));
        assert!(s.stream_draft_lines().is_empty());
    }

    #[test]
    fn sync_activity_starts_and_clears_phase() {
        use crate::activity::Phase;
        let mut s = Session::new("t");
        s.push_line("> hi".into());
        s.sync_activity();
        assert_eq!(s.phase, None);
        s.busy = true;
        s.sync_activity();
        assert_eq!(s.phase, Some(Phase::Thinking));
        assert_eq!(s.turn_first_line, 1);
        assert!(s.phase_since.is_some() && s.turn_since.is_some());
        s.busy = false;
        s.sync_activity();
        assert!(s.phase.is_none() && s.phase_since.is_none() && s.turn_since.is_none());
    }

    #[test]
    fn set_phase_keeps_turn_clock_and_ignores_idle() {
        use crate::activity::Phase;
        let mut s = Session::new("t");
        s.set_phase(Phase::Responding);
        assert_eq!(s.phase, None);
        s.busy = true;
        s.sync_activity();
        let turn = s.turn_since;
        std::thread::sleep(std::time::Duration::from_millis(5));
        s.set_phase(Phase::Responding);
        assert_eq!(s.phase, Some(Phase::Responding));
        assert_eq!(s.turn_since, turn);
        assert_ne!(s.phase_since, turn);
        let since = s.phase_since;
        std::thread::sleep(std::time::Duration::from_millis(5));
        s.set_phase(Phase::Responding);
        assert_eq!(s.phase_since, since);
    }

    fn bottom_app() -> App {
        let mut app = App::new();
        // Production tabs open empty; scroll tests seed their own filler.
        crate::mock::seed(&mut app.sessions[0]);
        app.viewport_height = VH;
        app.viewport_width = 100;
        app.active_mut().ensure_cache(100);
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
    fn boot_opens_exactly_one_tab() {
        let app = App::new();
        assert_eq!(app.sessions.len(), 1);
        assert_eq!(app.sessions[0].name, "s1");
        assert_eq!(app.active, 0);
    }

    /// Fresh tabs open with an empty transcript (no seed/hint filler).
    #[test]
    fn fresh_tabs_open_empty() {
        let mut app = App::new();
        assert!(app.sessions[0].lines.is_empty());
        app.open_tab();
        assert!(app.sessions[1].lines.iter().all(|l| l.contains("fresh")));
    }

    #[test]
    fn choose_session_replays_transcript_and_meta() {
        let (store, _dir) = test_store("choose");
        // Pre-populate as a previous run would have left it.
        let id = crate::store::Store::new_session_id();
        let mut sink = store.open_sink(&id).expect("sink");
        crate::store::append_line(&mut sink, "old line 1");
        crate::store::append_line(&mut sink, "old \"quoted\" 2");
        use std::io::Write;
        sink.flush().expect("flush");
        drop(sink);
        store.save_meta(
            &id,
            &crate::store::SessionMeta {
                backend: "codex".to_string(),
                remote_id: Some("thread-9".to_string()),
                created_at: 1000,
                updated_at: 2000,
                title: "old title".to_string(),
                workspace: String::new(),
            },
        );
        // Seed must be cleared; replayed lines start at index 0.
        let mut app = App::new();
        crate::mock::seed(&mut app.sessions[0]);
        assert!(
            app.sessions[0].lines.len() > 2,
            "precondition: seed present"
        );
        app.store = Some(store);
        app.open_session_chooser(String::new());
        assert_eq!(app.mode, Mode::Sessions);
        assert_eq!(app.sess_list.len(), 1);
        app.choose_session(0);
        assert_eq!(app.mode, Mode::Normal);
        let s = &app.sessions[0];
        assert_eq!(s.backend, BackendKind::Codex);
        assert_eq!(s.remote_id.as_deref(), Some("thread-9"));
        assert_eq!(s.title, "old title");
        assert_eq!(s.lines[0], "old line 1");
        assert_eq!(s.lines[1], "old \"quoted\" 2");
        assert!(s.lines[2].starts_with("(viewing 2 lines"));
        assert!(s.sink.is_some());
        // Re-attach is queued so the drain resumes the remote session.
        assert_eq!(app.outbox.respawns.len(), 1);
        // New pushes after viewing append to the same file, no duplication.
        // Banner must not have been written through the sink.
        app.sessions[0].push_line("new line".to_string());
        app.flush_store();
        let store = app.store.as_ref().expect("store");
        let replayed = store.load_transcript(&id);
        assert_eq!(&replayed[..2], &["old line 1", "old \"quoted\" 2"]);
        assert!(replayed.iter().any(|l| l == "new line"));
        assert!(
            !replayed.iter().any(|l| l.starts_with("(viewing")),
            "view banner must not grow JSONL"
        );
        let _ = std::fs::remove_dir_all(_dir);
    }

    #[test]
    fn choose_session_refuses_store_id_open_on_another_tab() {
        let (store, dir) = test_store("choose-open-tab");
        let id = crate::store::Store::new_session_id();
        store.save_meta(
            &id,
            &crate::store::SessionMeta {
                backend: "claude".to_string(),
                remote_id: Some("live-claude".to_string()),
                created_at: 1000,
                updated_at: 2000,
                title: "live".to_string(),
                workspace: String::new(),
            },
        );
        let mut app = App::new();
        app.store = Some(store);
        app.sessions[0].backend = BackendKind::Claude;
        app.sessions[0].store_id = Some(id.clone());
        app.sessions[0].remote_id = Some("live-claude".to_string());
        app.open_tab();
        app.outbox.respawns.clear();
        app.pending_claude_kill.clear();
        app.open_session_chooser(String::new());
        let row = app
            .sess_list
            .iter()
            .position(|s| s.id == id)
            .expect("saved row");
        app.choose_session(row);
        assert!(app.flash.contains("open in"));
        assert!(app.pending_claude_kill.is_empty());
        assert!(app.outbox.respawns.is_empty());
        assert_eq!(app.sessions[0].remote_id.as_deref(), Some("live-claude"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn backends_needed_unique_preserves_order() {
        let mut sessions = vec![Session::new("a"), Session::new("b"), Session::new("c")];
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
        app.sessions[0].sink = app.store.as_ref().unwrap().open_sink("tab");
        app.sessions[0].store_dirty = false;
        app.flush_store(); // idle: no-op
        assert!(!app.sessions[0].store_dirty);
        app.sessions[0].push_line("only".into());
        assert!(app.sessions[0].store_dirty);
        app.flush_store();
        assert!(!app.sessions[0].store_dirty);
        let lines = app.store.as_ref().unwrap().load_transcript("tab");
        assert_eq!(lines, vec!["only".to_string()]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn respawn_keeps_history_under_a_new_id() {
        let (store, dir) = test_store("respawn");
        let mut app = App::new();
        app.store = Some(store);
        app.attach_fresh_store(0);
        let old_id = app.sessions[0].store_id.clone().expect("id");
        app.sessions[0].push_line("scratch".to_string());
        app.flush_store();
        app.respawn_active(BackendKind::Mock);
        app.flush_store();
        let new_id = app.sessions[0].store_id.clone().expect("id");
        assert_ne!(old_id, new_id, "respawn must mint a fresh session id");
        let store = app.store.as_ref().expect("store");
        // Old session intact for `/sessions`; new one holds the marker.
        let old = store.load_transcript(&old_id);
        assert!(old.iter().any(|l| l == "scratch"));
        let fresh = store.load_transcript(&new_id);
        assert_eq!(fresh.len(), 1);
        assert!(fresh[0].starts_with("--- mock session (fresh) ---"));
        assert!(app.sessions[0].remote_id.is_none());
        assert_eq!(store.list_sessions("").len(), 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn tab_new_grows_to_max_then_flashes() {
        let mut app = App::new();
        app.open_tab();
        app.open_tab();
        assert_eq!(app.sessions.len(), 3);
        assert_eq!(app.active, 2);
        assert_eq!(app.sessions[2].name, "s3");
        assert_eq!(app.outbox.respawns.len(), 2);
        app.open_tab();
        assert_eq!(app.sessions.len(), 3);
        assert!(app.flash.contains("max"));
    }

    #[test]
    fn tab_close_removes_and_renumbers() {
        let mut app = App::new();
        app.open_tab();
        app.open_tab();
        // Queue work on the last tab; closing the middle must renumber it.
        app.active = 2;
        app.active_mut().input = "hi".to_string();
        app.active_mut().backend = BackendKind::Agy;
        app.submit();
        assert_eq!(app.outbox.submits.len(), 1);
        assert_eq!(app.outbox.submits[0].tab, 2);
        app.active = 1;
        app.active_mut().backend = BackendKind::Agy;
        app.close_active_tab().expect("close");
        assert_eq!(app.pending_agy_kill, vec![1], "agy child kill queued");
        assert_eq!(app.sessions.len(), 2);
        assert_eq!(app.sessions[0].name, "s1");
        assert_eq!(app.sessions[1].name, "s2");
        assert_eq!(app.outbox.submits.len(), 1);
        assert_eq!(app.outbox.submits[0].tab, 1, "queued work follows its tab");
        assert!(app.flash.contains("closed tab"));
    }

    #[test]
    fn tab_close_queues_grok_session_close() {
        let mut app = App::new();
        app.open_tab();
        app.active = 1;
        app.active_mut().backend = BackendKind::Grok;
        app.active_mut().remote_id = Some("acp-sess-9".into());
        app.close_active_tab().expect("close");
        assert_eq!(app.sessions.len(), 1);
        assert_eq!(app.pending_grok_close, vec!["acp-sess-9".to_string()]);
        assert!(app.pending_agy_kill.is_empty());
    }

    #[test]
    fn tab_close_refuses_the_last_tab() {
        let mut app = App::new();
        let err = app.close_active_tab().expect_err("must refuse");
        assert!(err.contains("last tab"));
        assert_eq!(app.sessions.len(), 1);
    }

    #[test]
    fn slash_commands_never_reach_a_backend() {
        let mut app = App::new();
        app.active_mut().backend = BackendKind::Codex;
        app.active_mut().remote_id = Some("thread-1".into());
        app.active_mut().input = "/tab new".to_string();
        app.submit();
        assert_eq!(app.sessions.len(), 2, "/tab new opens a tab");
        assert!(app.outbox.submits.is_empty());
        assert!(
            !app.sessions[0].lines.iter().any(|l| l.contains("/tab new")),
            "command must not echo as a prompt"
        );
        app.active_mut().input = "/nope".to_string();
        app.submit();
        assert!(app.flash.contains("unknown command"));
        assert!(app.outbox.submits.is_empty());
    }

    #[test]
    fn vim_command_toggles_and_persists() {
        // Isolate the real config file (serialized with the /theme
        // persistence test via CONFIG_ENV_LOCK).
        let _env = lock_config_env();
        let dir = std::env::temp_dir().join(format!("pf-vim-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let old = std::env::var_os("XDG_CONFIG_HOME");
        // SAFETY: nothing else in this suite reads XDG_CONFIG_HOME, and
        // the original value is restored before this test returns.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", &dir);
        }
        let mut app = App::new();
        assert!(!app.vim);
        app.toggle_vim();
        assert!(app.vim);
        assert!(app.flash.contains("vim mode on"));
        let raw = std::fs::read_to_string(dir.join("polyforge").join("config.toml"))
            .expect("config saved");
        assert!(raw.contains("vim = true"), "choice persisted: {raw}");
        // Toggle back: the file follows, so a reboot stays in normal keys.
        app.toggle_vim();
        assert!(!app.vim);
        let raw = std::fs::read_to_string(dir.join("polyforge").join("config.toml"))
            .expect("config saved");
        assert!(raw.contains("vim = false"), "choice persisted: {raw}");
        // SAFETY: restores the pre-test environment (see above).
        unsafe {
            match old {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn theme_unknown_name_leaves_theme_and_lists_choices() {
        let mut app = App::new();
        let before = app.theme;
        app.run_theme_command(Some("paper".to_string()));
        assert_eq!(app.theme, before);
        assert!(app.flash.contains("unknown theme"), "flash: {}", app.flash);
        assert!(app.flash.contains("groknight"), "flash: {}", app.flash);
    }

    #[test]
    fn theme_command_cycles_and_persists() {
        // Same XDG isolation pattern as vim_command_toggles_and_persists
        // (serialized via CONFIG_ENV_LOCK).
        let _env = lock_config_env();
        let dir = std::env::temp_dir().join(format!("pf-theme-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let old = std::env::var_os("XDG_CONFIG_HOME");
        // SAFETY: nothing else in this suite reads XDG_CONFIG_HOME, and
        // the original value is restored before this test returns.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", &dir);
        }
        let mut app = App::new();
        assert_eq!(app.theme, crate::theme::ThemeKind::GrokNight);
        // Bare /theme cycles to the next palette and saves it.
        app.run_theme_command(None);
        assert_eq!(app.theme, crate::theme::ThemeKind::TokyoNight);
        assert!(
            app.flash.contains("theme tokyonight"),
            "flash: {}",
            app.flash
        );
        let raw = std::fs::read_to_string(dir.join("polyforge").join("config.toml"))
            .expect("config saved");
        assert!(
            raw.contains("theme = \"tokyonight\""),
            "choice persisted: {raw}"
        );
        // Named /theme applies directly (alias accepted) and saves back.
        app.run_theme_command(Some("dark".to_string()));
        assert_eq!(app.theme, crate::theme::ThemeKind::GrokNight);
        let raw = std::fs::read_to_string(dir.join("polyforge").join("config.toml"))
            .expect("config saved");
        assert!(
            raw.contains("theme = \"groknight\""),
            "choice persisted: {raw}"
        );
        // SAFETY: restores the pre-test environment (see above).
        unsafe {
            match old {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn first_backend_choice_starts_tab_and_persists() {
        // Same XDG isolation pattern as vim_command_toggles_and_persists.
        let _env = lock_config_env();
        let dir = std::env::temp_dir().join(format!("pf-first-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let old = std::env::var_os("XDG_CONFIG_HOME");
        // SAFETY: serialized via CONFIG_ENV_LOCK; restored below.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", &dir);
        }
        let mut app = App::new();
        app.onboarding = true;
        app.mode = Mode::Picker;
        app.choose_first_backend(BackendKind::Grok);
        assert!(!app.onboarding);
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.active().backend, BackendKind::Grok);
        assert!(app.active().lines.is_empty(), "welcome stays: no marker");
        assert_eq!(app.outbox.respawns.len(), 1);
        assert_eq!(app.outbox.respawns[0].backend, BackendKind::Grok);
        assert!(app.flash.contains("provider grok"), "flash: {}", app.flash);
        let cfg = crate::config::Config::load();
        assert_eq!(cfg.polyforge.provider.as_deref(), Some("grok"));
        // SAFETY: restores the pre-test environment (see above).
        unsafe {
            match old {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn new_command_respawns_active_tab_fresh() {
        let mut app = App::new();
        app.active_mut().backend = BackendKind::Codex;
        app.active_mut().remote_id = Some("thread-1".into());
        app.active_mut().push_line("old".to_string());
        app.active_mut().input = "/new".to_string();
        app.submit();
        assert_eq!(app.mode, Mode::Normal);
        let s = app.active();
        assert_eq!(s.backend, BackendKind::Codex, "same backend, new session");
        assert!(s.remote_id.is_none());
        assert_eq!(s.lines.len(), 1);
        assert!(s.lines[0].contains("fresh"));
        assert_eq!(app.outbox.respawns.len(), 1);
        assert!(app.outbox.submits.is_empty(), "never sent to a backend");
    }

    #[test]
    fn sessions_query_filters_like_grok_search() {
        let (store, _dir) = test_store("sessions-query");
        for (id, title) in [("aaa", "fix login bug"), ("bbb", "write docs")] {
            store.save_meta(
                id,
                &crate::store::SessionMeta {
                    backend: "mock".to_string(),
                    remote_id: None,
                    created_at: 1000,
                    updated_at: 1000,
                    title: title.to_string(),
                    workspace: String::new(),
                },
            );
        }
        let mut app = App::new();
        app.store = Some(store);
        app.mode = Mode::Insert;
        app.active_mut().input = "/sessions login".to_string();
        app.submit();
        assert_eq!(app.mode, Mode::Sessions);
        assert_eq!(app.sess_list.len(), 1);
        assert_eq!(app.sess_list[0].id, "aaa");
        // No match flashes instead of opening.
        app.mode = Mode::Insert;
        app.active_mut().input = "/sessions zzz".to_string();
        app.submit();
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.flash.contains("no sessions match"));
        let _ = std::fs::remove_dir_all(_dir);
    }

    /// `/sessions` lists only this workspace's sessions, grouped by
    /// provider with the active tab's provider first.
    #[test]
    fn sessions_scoped_to_workspace_and_grouped_by_provider() {
        let (store, _dir) = test_store("sessions-group");
        for (id, backend, updated, ws) in [
            ("m-old", "mock", 1000u64, "/here"),
            ("g-new", "grok", 4000, "/here"),
            ("m-new", "mock", 3000, "/here"),
            ("c-mid", "codex", 2000, "/here"),
            ("elsewhere", "grok", 9000, "/there"),
            ("legacy", "grok", 9000, ""),
        ] {
            store.save_meta(
                id,
                &crate::store::SessionMeta {
                    backend: backend.to_string(),
                    remote_id: None,
                    created_at: 0,
                    updated_at: updated,
                    title: id.to_string(),
                    workspace: ws.to_string(),
                },
            );
        }
        let mut app = App::new();
        app.store = Some(store);
        app.workspace = "/here".to_string();
        app.active_mut().backend = BackendKind::Grok;
        app.open_session_chooser(String::new());
        let ids: Vec<&str> = app.sess_list.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["g-new", "m-new", "m-old", "c-mid"]);
        let _ = std::fs::remove_dir_all(_dir);
    }

    #[test]
    fn delete_selected_session_removes_and_guards_open() {
        let (store, _dir) = test_store("sessions-delete");
        let id = crate::store::Store::new_session_id();
        store.save_meta(
            &id,
            &crate::store::SessionMeta {
                backend: "mock".to_string(),
                remote_id: None,
                created_at: 1000,
                updated_at: 1000,
                title: "doomed".to_string(),
                workspace: String::new(),
            },
        );
        let mut app = App::new();
        app.store = Some(store);
        // Guard: the session is open in the live tab.
        app.sessions[0].store_id = Some(id.clone());
        app.open_session_chooser(String::new());
        assert_eq!(app.mode, Mode::Sessions);
        app.delete_selected_session();
        assert!(app.flash.contains("open in s1"));
        assert_eq!(app.sess_list.len(), 1, "guarded: files stay");
        // After closing the reference (fresh id), delete succeeds.
        app.sessions[0].store_id = Some("other".to_string());
        app.delete_selected_session();
        assert!(app.flash.contains("deleted session"));
        assert!(app.sess_list.is_empty());
        assert_eq!(app.mode, Mode::Normal, "empty chooser closes");
        let store = app.store.as_ref().expect("store");
        assert!(store.list_sessions("").is_empty());
        let _ = std::fs::remove_dir_all(_dir);
    }

    #[test]
    fn short_id_shows_id_tail() {
        assert_eq!(short_id("abc"), "abc");
        assert_eq!(short_id("1789516800000-1234-5"), "0-1234-5");
    }

    #[test]
    fn submitting_sessions_command_opens_the_chooser() {
        let (store, _dir) = test_store("cmd-sessions");
        let id = crate::store::Store::new_session_id();
        store.save_meta(
            &id,
            &crate::store::SessionMeta {
                backend: "mock".to_string(),
                remote_id: None,
                created_at: 1000,
                updated_at: 1000,
                title: "prior".to_string(),
                workspace: String::new(),
            },
        );
        let mut app = App::new();
        app.store = Some(store);
        app.mode = Mode::Insert;
        app.active_mut().input = "/sessions".to_string();
        app.submit();
        assert_eq!(app.mode, Mode::Sessions, "submit must not clobber the mode");
        assert_eq!(app.sess_list.len(), 1);
        let _ = std::fs::remove_dir_all(_dir);
    }

    #[test]
    fn session_chooser_empty_store_flashes() {
        let (store, _dir) = test_store("chooser-empty");
        let mut app = App::new();
        app.store = Some(store);
        app.open_session_chooser(String::new());
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.flash.contains("no previous sessions"));
        let _ = std::fs::remove_dir_all(_dir);
    }

    #[test]
    fn stage_and_clear_diff_bump_risk_gen() {
        let mut s = Session::new("t");
        assert_eq!(s.risk_gen, 0);
        s.stage_diff(PendingDiff {
            file: "f".into(),
            body: "b".into(),
        });
        assert_eq!(s.risk_gen, 1);
        assert!(s.pending_diff.is_some());
        s.approval_risk = Some(crate::typesafe::ApprovalJudgment::compose(
            0.0, 1.0, 0.0, 0.0,
        ));
        s.risk_spawned_gen = Some(1);
        let taken = s.clear_diff();
        assert_eq!(taken.unwrap().file, "f");
        assert_eq!(s.risk_gen, 2);
        assert!(s.approval_risk.is_none());
        assert!(s.risk_spawned_gen.is_none());
    }

    /// S7-F3: staging a second card over an undecided one re-prompts in
    /// the transcript; a first stage stays silent.
    #[test]
    fn stage_replacing_open_card_reprompts() {
        let mut s = Session::new("t");
        s.stage_diff(PendingDiff {
            file: "old".into(),
            body: "old body".into(),
        });
        assert!(s.lines.is_empty(), "first stage stays silent");
        s.stage_diff(PendingDiff {
            file: "new".into(),
            body: "new body".into(),
        });
        assert_eq!(s.pending_diff.as_ref().unwrap().file, "new");
        assert_eq!(s.risk_gen, 2);
        assert_eq!(s.lines.len(), 1);
        assert!(
            s.lines[0].contains("review again before y"),
            "re-prompt note missing: {:?}",
            s.lines
        );
    }

    #[test]
    fn slash_bare_prefix_lists_every_command() {
        let mut app = App::new();
        app.active_mut().input = "/".to_string();
        let got = app.slash_matches();
        assert_eq!(got.len(), SLASH_COMMANDS.len());
        assert_eq!(got, (0..SLASH_COMMANDS.len()).collect::<Vec<_>>());
    }

    #[test]
    fn slash_prefix_filters_and_tab_pair_matches() {
        let mut app = App::new();
        app.active_mut().input = "/s".to_string();
        let names: Vec<&str> = app
            .slash_matches()
            .iter()
            .map(|&i| SLASH_COMMANDS[i].0)
            .collect();
        assert_eq!(names, vec!["/sessions [query]"]);
        // "/t" narrows to the /tab spellings + /theme.
        app.active_mut().input = "/t".to_string();
        let names: Vec<&str> = app
            .slash_matches()
            .iter()
            .map(|&i| SLASH_COMMANDS[i].0)
            .collect();
        assert_eq!(names, vec!["/tab new", "/tab close", "/theme [name]"]);
        app.active_mut().input = "/tab ".to_string();
        assert_eq!(app.slash_matches().len(), 2);
        app.active_mut().input = "/tab c".to_string();
        let names: Vec<&str> = app
            .slash_matches()
            .iter()
            .map(|&i| SLASH_COMMANDS[i].0)
            .collect();
        assert_eq!(names, vec!["/tab close"]);
    }

    #[test]
    fn slash_matches_hide_without_prefix_or_past_args() {
        let mut app = App::new();
        assert!(app.slash_matches().is_empty(), "empty input shows nothing");
        app.active_mut().input = "hello".to_string();
        assert!(app.slash_matches().is_empty());
        // Args past a complete command hide the list (user is typing a query).
        app.active_mut().input = "/sessions foo".to_string();
        assert!(app.slash_matches().is_empty());
        app.active_mut().input = "/nope".to_string();
        assert!(app.slash_matches().is_empty());
    }

    #[test]
    fn slash_cycle_wraps_and_accept_inserts() {
        let mut app = App::new();
        // "/t" matches /tab new, /tab close, /theme [name].
        app.active_mut().input = "/t".to_string();
        app.cycle_cmd_sel(1);
        assert_eq!(app.cmd_sel, 1);
        app.cycle_cmd_sel(1);
        assert_eq!(app.cmd_sel, 2, "third match (/theme)");
        app.cycle_cmd_sel(1);
        assert_eq!(app.cmd_sel, 0, "wraps past the end");
        app.cycle_cmd_sel(-1);
        assert_eq!(app.cmd_sel, 2, "wraps past the start");
        assert!(app.accept_slash_completion());
        assert_eq!(app.active().input, "/theme ");
        assert_eq!(app.active().cursor, "/theme ".chars().count());
        // Sessions completes with a trailing space for the query.
        app.active_mut().input = "/s".to_string();
        assert!(app.accept_slash_completion());
        assert_eq!(app.active().input, "/sessions ");
        // Nothing showing: accept is a no-op.
        app.active_mut().input = "hi".to_string();
        assert!(!app.accept_slash_completion());
        assert_eq!(app.active().input, "hi");
    }

    /// Drag across lines with scroll + wrapping resolves exact text.
    #[test]
    fn drag_selection_extracts_text() {
        let mut app = App::new();
        // Deterministic transcript: 10-char lines, width 10 = 1 row each.
        app.sessions[0].lines.clear();
        app.sessions[0].row_cache.clear();
        app.sessions[0].total_rows = 0;
        for l in ["aaaabbbbcc", "ddddeeeeFF", "gggghhhhii"] {
            app.sessions[0].push_line(l.to_string());
        }
        app.viewport_width = 10;
        app.viewport_height = 10;
        app.active_mut().ensure_cache(10);
        app.text_area = Some((1, 1, 10, 10));
        app.active_mut().scroll = 0;
        // Drag from line 0 char 4 to line 1 char 4.
        assert!(app.sel_begin(1 + 4, 1 + 0));
        app.sel_extend(1 + 4, 1 + 1);
        assert_eq!(app.selected_text().as_deref(), Some("bbbbcc\ndddd"));
        // Reversed drag normalizes the same way.
        assert!(app.sel_begin(1 + 4, 1 + 1));
        app.sel_extend(1 + 4, 1 + 0);
        assert_eq!(app.selected_text().as_deref(), Some("bbbbcc\ndddd"));
        // Click without drag copies nothing.
        assert!(app.sel_begin(1 + 2, 1 + 0));
        assert_eq!(app.selected_text(), None);
        // Outside the viewport: no selection.
        assert!(!app.sel_begin(0, 0));
        assert!(!app.sel_begin(1, 99));
    }

    /// Auto-copy path must clear the highlight (mouse-up dehighlight).
    #[test]
    fn take_selected_text_clears_selection() {
        let mut app = App::new();
        app.sessions[0].lines.clear();
        app.sessions[0].row_cache.clear();
        app.sessions[0].total_rows = 0;
        app.sessions[0].push_line("copy-me-please".to_string());
        app.viewport_width = 20;
        app.viewport_height = 10;
        app.active_mut().ensure_cache(20);
        app.text_area = Some((0, 0, 20, 10));
        app.active_mut().scroll = 0;
        assert!(app.sel_begin(0, 0));
        app.sel_extend(8, 0);
        assert_eq!(app.selected_text().as_deref(), Some("copy-me-"));
        assert!(app.sel.is_some());
        assert_eq!(app.take_selected_text().as_deref(), Some("copy-me-"));
        assert!(app.sel.is_none());
        assert_eq!(app.take_selected_text(), None);
    }

    /// Scrolled viewport maps cells to the right lines.
    #[test]
    fn drag_selection_honors_scroll() {
        let mut app = App::new();
        app.sessions[0].lines.clear();
        app.sessions[0].row_cache.clear();
        app.sessions[0].total_rows = 0;
        for l in ["line-one", "line-two", "line-3"] {
            app.sessions[0].push_line(l.to_string());
        }
        app.viewport_width = 20;
        app.viewport_height = 20;
        app.active_mut().ensure_cache(20);
        app.text_area = Some((0, 5, 20, 20));
        app.active_mut().scroll = 1; // first visible row is line-two
        assert!(app.sel_begin(0, 5));
        app.sel_extend(8, 5);
        assert_eq!(app.selected_text().as_deref(), Some("line-two"));
    }

    #[test]
    fn replace_lines_builds_cache_at_width_and_caps() {
        let mut s = Session::new("t");
        // 14 ASCII cols at width 7 = 2 rows; wide chars take the slow path.
        s.replace_lines(vec!["0123456789ABCD".to_string(), "あいう".to_string()], 7);
        assert_eq!(s.cache_width, Some(7));
        assert_eq!(s.row_cache, vec![2, Session::wrap_line("あいう", 7).len()]);
        assert_eq!(s.total_rows, s.row_cache.iter().sum::<usize>());
        // Over-cap input drops the head, mirroring push_line.
        let big: Vec<String> = (0..(MAX_LINES + 10)).map(|i| format!("l{i}")).collect();
        s.replace_lines(big, 100);
        assert_eq!(s.lines.len(), MAX_LINES);
        assert_eq!(s.lines[0], "l10");
        assert_eq!(s.row_cache.len(), MAX_LINES);
        assert_eq!(s.total_rows, s.row_cache.iter().sum::<usize>());
    }

    #[test]
    fn choose_session_replays_at_viewport_width() {
        let (store, _dir) = test_store("choose-width");
        let id = crate::store::Store::new_session_id();
        let mut sink = store.open_sink(&id).expect("sink");
        for i in 0..500 {
            crate::store::append_line(&mut sink, &format!("line {i:04} with padding"));
        }
        use std::io::Write;
        sink.flush().expect("flush");
        drop(sink);
        store.save_meta(
            &id,
            &crate::store::SessionMeta {
                backend: "mock".to_string(),
                remote_id: None,
                created_at: 1000,
                updated_at: 2000,
                title: "wide".to_string(),
                workspace: String::new(),
            },
        );
        let mut app = App::new();
        app.store = Some(store);
        app.viewport_width = 7;
        app.viewport_height = 20;
        app.open_session_chooser(String::new());
        app.choose_session(0);
        let s = app.active();
        assert_eq!(s.lines.len(), 501, "500 replayed + the viewing banner");
        assert_eq!(s.cache_width, Some(7), "replay wraps at the live width");
        assert_eq!(s.total_rows, s.row_cache.iter().sum::<usize>());
        assert!(
            s.total_rows > 500,
            "7-wide wraps must exceed one row per line"
        );
        assert_eq!(s.scroll + 20, s.total_rows, "pinned to the bottom");
        assert!(s.lines.last().unwrap().starts_with("(viewing 500 lines"));
        assert_eq!(app.mode, Mode::Normal);
        let _ = std::fs::remove_dir_all(_dir);
    }

    /// Wide chars occupy two columns in both directions.
    #[test]
    fn col_char_mapping_handles_wide_chars() {
        assert_eq!(col_to_char("aあb", 0), 0);
        assert_eq!(col_to_char("aあb", 1), 1);
        assert_eq!(col_to_char("aあb", 2), 1); // second cell of あ
        assert_eq!(col_to_char("aあb", 3), 2);
        assert_eq!(col_to_char("aあb", 99), 3); // past end clamps
        assert_eq!(char_to_col("aあb", 2), 3);
        // Selection slicing stays on char boundaries.
        let mut app = App::new();
        app.sessions[0].lines.clear();
        app.sessions[0].row_cache.clear();
        app.sessions[0].total_rows = 0;
        app.sessions[0].push_line("aあb".to_string());
        app.viewport_width = 20;
        app.viewport_height = 20;
        app.active_mut().ensure_cache(20);
        app.active_mut().scroll = 0;
        app.text_area = Some((0, 0, 20, 20));
        assert!(app.sel_begin(0, 0));
        app.sel_extend(4, 0); // one past b (end-exclusive)
        assert_eq!(app.selected_text().as_deref(), Some("aあb"));
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

    /// S3-F1: ESC/C0 fixture is inert — no ESC survives, CSI parameter
    /// remnants (`[31m`) are gone with their sequence, the hyperlink URL
    /// goes with its OSC, and visible text is kept.
    #[test]
    fn sanitize_strips_esc_c0_sequences() {
        let dirty = "\x1b[31mred\x1b[0m\x00\x07bell\x1b]8;;http://evil\x07link";
        assert_eq!(sanitize_text(dirty), "redbelllink");
    }

    /// S3-F1: tab, newline, and non-ASCII text are not display attacks.
    #[test]
    fn sanitize_keeps_tab_newline_unicode() {
        assert_eq!(sanitize_text("a\tb\ncあ🎉"), "a\tb\ncあ🎉");
    }

    /// S3-F1: C1 singletons (never enumerated in the finding) fail closed
    /// too: CSI `U+009B` consumes its parameters, DEL and lone C1 drop.
    #[test]
    fn sanitize_drops_c1_singletons_and_del() {
        assert_eq!(sanitize_text("a\x7fb\u{80}c\u{9b}31md"), "abcd");
        assert_eq!(sanitize_text("a\u{9d}52;c;xyz\x07b"), "ab");
    }

    /// S3-F1: truncated input fails closed — a bare ESC eats its follower,
    /// an unterminated CSI never leaks its parameters.
    #[test]
    fn sanitize_truncated_sequences_fail_closed() {
        assert_eq!(sanitize_text("a\x1bb"), "a");
        assert_eq!(sanitize_text("a\x1b[31"), "a");
        assert_eq!(sanitize_text("a\x1b"), "a");
    }

    /// S3-F1: `push_line` is the store choke point — the dirty line must
    /// be absent from both memory and the JSONL transcript.
    #[test]
    fn push_line_stores_sanitized() {
        let (store, dir) = test_store("s3-store");
        let id = crate::store::Store::new_session_id();
        let mut app = App::new();
        app.sessions[0].sink = store.open_sink(&id);
        app.sessions[0].push_line("\x1b[2Jwiped\x00".to_string());
        app.flush_store();
        assert_eq!(app.sessions[0].lines, vec!["wiped"]);
        assert_eq!(store.load_transcript(&id), vec!["wiped"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// S3-F1: pre-fix transcripts loaded from disk are cleaned on replay.
    #[test]
    fn replace_lines_sanitizes_legacy_data() {
        let mut s = Session::new("t");
        s.replace_lines(vec!["\x1b[1mclean\x1b[0m".to_string()], 100);
        assert_eq!(s.lines, vec!["clean"]);
    }

    /// S3-F1: the copy layer is defense in depth — even a raw line that
    /// bypassed `push_line` must not survive `selected_text`.
    #[test]
    fn selected_text_is_sanitized() {
        let mut app = App::new();
        let dirty = "\x1b[31mcopy-me\x1b[0m";
        app.sessions[0].lines.push(dirty.to_string());
        let len = dirty.chars().count();
        app.sel = Some(Selection {
            anchor_line: 0,
            anchor_char: 0,
            focus_line: 0,
            focus_char: len,
        });
        assert_eq!(app.selected_text().as_deref(), Some("copy-me"));
    }

    /// S3-F1: the approval card is agent-controlled render surface too.
    #[test]
    fn stage_diff_sanitizes_card() {
        let mut s = Session::new("t");
        s.stage_diff(PendingDiff {
            file: "tool\x1b[2K".to_string(),
            body: "do it\x1b]8;;http://evil\x07now".to_string(),
        });
        let diff = s.pending_diff.as_ref().expect("staged");
        assert_eq!(diff.file, "tool");
        assert_eq!(diff.body, "do itnow");
    }

    fn busy_tab(backend: BackendKind) -> App {
        let mut app = App::new();
        let s = app.active_mut();
        s.backend = backend;
        s.remote_id = Some("r1".into());
        s.busy = true;
        s.sync_activity();
        app
    }

    fn stopped_lines(s: &Session) -> usize {
        s.lines.iter().filter(|l| l.starts_with("■ stopped")).count()
    }

    #[test]
    fn stop_first_press_queues_one_snapshot_second_forces() {
        let mut app = busy_tab(BackendKind::Muse);
        app.active_mut().turn_id = Some("t1".into());
        app.request_stop();
        assert!(app.active().stopping.is_some());
        assert_eq!(app.outbox.stops.len(), 1);
        let st = &app.outbox.stops[0];
        assert_eq!(st.tab, 0);
        assert_eq!(st.backend, BackendKind::Muse);
        assert_eq!(st.remote_id.as_deref(), Some("r1"));
        assert_eq!(st.turn_id.as_deref(), Some("t1"));
        assert!(app.active().busy);
        app.request_stop();
        assert_eq!(app.outbox.stops.len(), 1, "force sends nothing new");
        let s = app.active();
        assert!(!s.busy && s.stopping.is_none() && s.sealed && s.turn_id.is_none());
        assert_eq!(s.lines.last().map(String::as_str), Some(FORCED_STOP_LINE));
        // sync_activity must not add a second stop line.
        app.active_mut().sync_activity();
        assert_eq!(stopped_lines(app.active()), 1);
    }

    #[test]
    fn stop_is_a_noop_when_idle_or_card_open() {
        let mut app = App::new();
        app.request_stop();
        assert!(app.outbox.stops.is_empty() && app.active().stopping.is_none());
        let mut app = busy_tab(BackendKind::Muse);
        app.active_mut().stage_diff(PendingDiff {
            file: "f".into(),
            body: "b".into(),
        });
        app.request_stop();
        assert!(app.outbox.stops.is_empty() && app.active().stopping.is_none());
    }

    #[test]
    fn codex_stop_without_turn_id_does_not_enter_stopping() {
        let mut app = busy_tab(BackendKind::Codex);
        app.request_stop();
        assert!(app.active().stopping.is_none());
        assert!(app.outbox.stops.is_empty());
        assert!(app.flash.contains("turn not started yet"));
    }

    #[test]
    fn mock_stop_ends_immediately_with_one_line() {
        let mut app = App::new();
        app.active_mut().input = "go".into();
        app.submit();
        app.active_mut().sync_activity();
        assert!(app.active().busy);
        app.request_stop();
        assert!(!app.active().busy && app.outbox.stops.is_empty());
        app.active_mut().sync_activity();
        assert_eq!(stopped_lines(app.active()), 1);
        assert!(app.active().stopping.is_none() && app.active().queue.is_empty());
    }

    fn worked_lines(s: &Session) -> usize {
        s.lines.iter().filter(|l| l.starts_with("Worked for ")).count()
    }

    #[test]
    fn turn_end_commits_one_worked_for_line_but_not_after_a_stop() {
        // Natural end: exactly one line, however often sync runs.
        let mut app = busy_tab(BackendKind::Grok);
        app.active_mut().busy = false;
        app.active_mut().sync_activity();
        app.active_mut().sync_activity();
        let s = app.active();
        assert_eq!(worked_lines(s), 1);
        let last = s.lines.last().expect("line");
        assert!(last.starts_with("Worked for 0.") && last.ends_with('s'), "{last}");
        assert!(!s.lines.iter().any(|l| l.contains("done ✓")));
        // A stop request: `■ stopped` only.
        let mut app = busy_tab(BackendKind::Grok);
        app.request_stop();
        app.active_mut().busy = false;
        app.active_mut().sync_activity();
        assert_eq!(worked_lines(app.active()), 0);
        assert_eq!(stopped_lines(app.active()), 1);
        // A forced stop (second press): the forced line, no Worked-for.
        let mut app = busy_tab(BackendKind::Grok);
        app.request_stop();
        app.request_stop();
        app.active_mut().sync_activity();
        assert_eq!(worked_lines(app.active()), 0);
        assert_eq!(app.active().lines.last().map(String::as_str), Some(FORCED_STOP_LINE));
    }

    #[test]
    fn natural_end_while_stopping_pushes_exactly_one_line() {
        let mut app = busy_tab(BackendKind::Grok);
        app.request_stop();
        app.active_mut().busy = false; // backend ended the turn
        app.active_mut().sync_activity();
        app.active_mut().sync_activity();
        let s = app.active();
        assert_eq!(s.lines.last().map(String::as_str), Some(STOPPED_LINE));
        assert_eq!(stopped_lines(s), 1);
        assert!(s.stopping.is_none() && s.turn_id.is_none());
        // A normal end without a stop adds nothing.
        let mut calm = busy_tab(BackendKind::Grok);
        calm.active_mut().busy = false;
        calm.active_mut().sync_activity();
        assert_eq!(stopped_lines(calm.active()), 0);
    }

    #[test]
    fn reset_after_stop_pushes_no_stop_line() {
        let mut app = busy_tab(BackendKind::Grok);
        app.request_stop();
        app.respawn_active(BackendKind::Grok);
        app.active_mut().sync_activity();
        assert_eq!(stopped_lines(app.active()), 0);
        assert!(app.outbox.stops.is_empty(), "stale stop dropped");
        assert!(!app.active().sealed && app.active().tokens.is_none());
        let mut s = Session::new("x");
        s.busy = true;
        s.stopping = Some(std::time::Instant::now());
        s.sealed = true;
        s.turn_id = Some("t".into());
        s.busy = false;
        s.reset_turn_state();
        s.sync_activity();
        assert!(s.lines.is_empty() && !s.sealed && s.turn_id.is_none());
    }

    #[test]
    fn closing_a_tab_drops_and_renumbers_stops() {
        let mut app = App::new();
        app.open_tab();
        app.open_tab();
        let stop = |tab| OutboxStop {
            tab,
            backend: BackendKind::Muse,
            remote_id: None,
            turn_id: None,
        };
        app.outbox.stops = vec![stop(0), stop(1), stop(2)];
        app.active = 1;
        app.close_active_tab().unwrap();
        let tabs: Vec<usize> = app.outbox.stops.iter().map(|o| o.tab).collect();
        assert_eq!(tabs, vec![0, 1]);
    }

    #[test]
    fn submit_clears_seal_and_turn_id() {
        let mut app = busy_tab(BackendKind::Muse);
        app.request_stop();
        app.request_stop();
        assert!(app.active().sealed);
        app.active_mut().turn_id = Some("old".into());
        app.active_mut().input = "next".into();
        app.submit();
        assert!(!app.active().sealed && app.active().turn_id.is_none());
    }

    #[test]
    fn click_stop_needs_the_button_and_an_actionable_tab() {
        let mut app = busy_tab(BackendKind::Muse);
        app.stop_hit = Some(ratatui::layout::Rect::new(10, 5, 6, 1));
        assert!(!app.click_stop(9, 5));
        assert!(!app.click_stop(16, 5));
        assert!(!app.click_stop(10, 6));
        assert!(app.outbox.stops.is_empty());
        assert!(app.click_stop(12, 5));
        assert_eq!(app.outbox.stops.len(), 1);
        let mut idle = App::new();
        idle.stop_hit = Some(ratatui::layout::Rect::new(10, 5, 6, 1));
        assert!(!idle.click_stop(12, 5));
        let mut card = busy_tab(BackendKind::Muse);
        card.active_mut().stage_diff(PendingDiff {
            file: "f".into(),
            body: "b".into(),
        });
        card.stop_hit = Some(ratatui::layout::Rect::new(10, 5, 6, 1));
        assert!(!card.click_stop(12, 5));
    }

    #[test]
    fn take_u64_and_note_tokens() {
        use serde_json::json;
        assert_eq!(take_u64(&json!(12)), Some(12));
        assert_eq!(take_u64(&json!("34")), Some(34));
        assert_eq!(take_u64(&json!(-1)), None);
        assert_eq!(take_u64(&json!(1.5)), None);
        assert_eq!(take_u64(&json!("1.5")), None);
        assert_eq!(take_u64(&json!("x")), None);
        assert_eq!(take_u64(&json!(null)), None);
        let mut s = Session::new("t");
        s.note_tokens(Some(500));
        s.note_tokens(Some(0));
        s.note_tokens(None);
        assert_eq!(s.tokens, Some(500));
        s.note_tokens(Some(700));
        assert_eq!(s.tokens, Some(700));
    }
}
