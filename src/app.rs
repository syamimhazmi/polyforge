//! M1 PROTOTYPE state model: tabs + vim-modal input + scrollback + mock jobs.
//! In-memory only. Everything here is throwaway; M2 replaces `mock` with a
//! real provider and adds persistence.

/// In-memory scrollback cap per session (older lines spill nowhere in M1).
pub const MAX_LINES: usize = 50_000;
/// Max parallel mock sessions (agreed M1 scope: ~3).
pub const MAX_SESSIONS: usize = 3;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    Search,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Normal => "NORMAL",
            Mode::Insert => "INSERT",
            Mode::Search => "SEARCH",
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

/// Which backend a tab run uses. One provider per session (spec Q7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BackendKind {
    #[default]
    Mock,
    Muse,
}

/// Async muse work queued by the sync App, performed by the main loop.
#[derive(Debug, Default)]
pub struct OutboxSubmit {
    pub tab: usize,
    pub prompt: String,
}

#[derive(Debug, Default)]
pub struct OutboxDecide {
    pub tab: usize,
    pub approval_id: String,
    pub requirement_id: serde_json::Value,
    pub choice_id: String,
    pub feedback: Option<String>,
}

#[derive(Debug, Default)]
pub struct Outbox {
    pub submits: Vec<OutboxSubmit>,
    pub decides: Vec<OutboxDecide>,
}

pub struct Session {
    pub name: String,
    /// Transcript lines, oldest first. Capped at MAX_LINES.
    pub lines: Vec<String>,
    /// Index of the first visible line in the transcript viewport.
    pub scroll: usize,
    pub busy: bool,
    /// Mock output lines still waiting to stream in.
    pub queue: Vec<String>,
    /// Staged diff shown once `queue` drains.
    pub diff_after: Option<PendingDiff>,
    pub pending_diff: Option<PendingDiff>,
    /// Live MSP approval behind the card (muse backend only).
    pub pending_approval: Option<PendingApproval>,
    pub input: String,
    /// Char-index cursor inside `input` (insert mode).
    pub cursor: usize,
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
            input: String::new(),
            cursor: 0,
        }
    }

    pub fn push_line(&mut self, line: String) {
        self.lines.push(line);
        if self.lines.len() > MAX_LINES {
            let drop = self.lines.len() - MAX_LINES;
            self.lines.drain(..drop);
            self.scroll = self.scroll.saturating_sub(drop);
        }
    }
}

pub struct App {
    pub sessions: Vec<Session>,
    pub active: usize,
    pub backend: BackendKind,
    /// Muse session id per tab (muse backend; None = failed to start).
    pub muse_sessions: Vec<Option<String>>,
    /// Grey-out reason when the muse backend degraded (e.g. no login).
    pub muse_degraded: Option<String>,
    pub outbox: Outbox,
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
}

impl App {
    pub fn new() -> Self {
        let mut app = Self {
            sessions: (1..=MAX_SESSIONS)
                .map(|i| Session::new(&format!("s{i}")))
                .collect(),
            active: 0,
            backend: BackendKind::Mock,
            muse_sessions: Vec::new(),
            muse_degraded: None,
            outbox: Outbox::default(),
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
        s.lines.len().saturating_sub(self.viewport_height.max(1))
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
        let s = self.active_mut();
        s.scroll = s.lines.len().saturating_sub(vh);
    }

    // -- submit / tick (mock streaming; all tabs tick = background agents) --

    pub fn submit(&mut self) {
        let prompt = std::mem::take(&mut self.active_mut().input);
        self.active_mut().cursor = 0;
        if prompt.trim().is_empty() {
            return;
        }
        let tab = self.active;
        let backend = self.backend;
        let sid = self.muse_sessions.get(tab).and_then(|o| o.clone());
        let degraded = self.muse_degraded.clone();
        let s = self.active_mut();
        s.push_line(format!("> {prompt}"));
        match backend {
            BackendKind::Mock => crate::mock::start_job(s, &prompt),
            BackendKind::Muse => match sid {
                Some(_) => {
                    s.busy = true;
                    self.outbox.submits.push(OutboxSubmit { tab, prompt });
                }
                None => {
                    let why =
                        degraded.unwrap_or_else(|| "muse session unavailable".to_string());
                    s.push_line(format!("muse: {why}"));
                }
            },
        }
        self.mode = Mode::Normal;
        self.stick_to_bottom();
    }

    /// Close out a muse approval from the UI side. The turn itself may keep
    /// running; completion (and the bell) arrives via turn/completed.
    /// Returns true when a card was actually closed.
    pub fn muse_approved(&mut self, decision: &str) -> bool {
        let s = self.active_mut();
        let Some(diff) = s.pending_diff.take() else {
            return false;
        };
        s.pending_approval.take();
        s.push_line(format!("muse: {} {} [{decision}]", muse_mark(decision), diff.file));
        self.stick_to_bottom();
        true
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
            if !s.busy || s.pending_diff.is_some() {
                continue; // idle, or waiting on y/n/a/q
            }
            let pinned = s.scroll + vh >= s.lines.len();
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
                s.scroll = s.lines.len().saturating_sub(vh);
            }
            if s.queue.is_empty() {
                if let Some(diff) = s.diff_after.take() {
                    s.pending_diff = Some(diff);
                } else {
                    s.busy = false;
                    s.push_line("mock: done ✓".to_string());
                    if pinned {
                        s.scroll = s.lines.len().saturating_sub(vh);
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
            self.set_scroll(idx);
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
        self.set_scroll(idx);
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
        // Place the viewport explicitly: must not depend on stick_to_bottom().
        let tail = app.active().lines.len().saturating_sub(VH);
        app.active_mut().scroll = tail;
        app
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
