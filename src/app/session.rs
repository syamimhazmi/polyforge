//! One tab's session state: transcript, streaming drafts, thought buffer and
//! turn state.

use super::outbox::STOPPED_LINE;
use super::text::{sanitize_text, wrap_chunks};
use super::{BackendKind, DEFAULT_WIDTH, MAX_LINES, PendingApproval, PendingDiff};
use crate::markdown::MdKind;

/// Live thought buffer cap (bytes; the tail is kept).
const THOUGHT_CAP: usize = 4096;
/// Thought lines shown in the live block, and the per-line display cap.
const THOUGHT_LINES: usize = 3;
const THOUGHT_LINE_CAP: usize = 240;

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
    /// Host is up but the session starts on the first submit (muse, fresh tab).
    pub session_deferred: bool,
    /// Grey-out reason when this tab's backend degraded (e.g. no login).
    pub tab_degraded: Option<String>,
    /// Bring-up in flight (token, resume id), run off the event loop; the
    /// result applies only while the token still matches.
    pub connecting: Option<(u64, Option<String>)>,
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
            session_deferred: false,
            tab_degraded: None,
            connecting: None,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::app::test_support::test_store;

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
