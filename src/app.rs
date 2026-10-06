//! M1 PROTOTYPE state model: tabs + vim-modal input + scrollback + mock jobs.
//! In-memory only. Everything here is throwaway; M2 replaces `mock` with a
//! real provider and adds persistence.

mod commands;
mod outbox;
mod scroll;
mod search;
mod selection;
mod session;
mod store_sync;
mod tabs;
#[cfg(test)]
mod test_support;
mod text;

pub use commands::SLASH_COMMANDS;
use outbox::FORCED_STOP_LINE;
pub use outbox::{Outbox, OutboxDecide, OutboxRespawn, OutboxStop, OutboxSubmit};
use selection::Selection;
pub use session::{Session, take_u64};
pub use text::{sanitize_text, short_id, wrap_rows};

/// In-memory scrollback cap per session (older lines spill nowhere in M1).
pub const MAX_LINES: usize = 50_000;

/// Assumed viewport width until the first render measures the real one.
pub const DEFAULT_WIDTH: usize = 100;

/// Max open tabs (agreed M1 scope: ~3). Boot opens exactly one.
pub const MAX_SESSIONS: usize = 3;

/// First user prompt kept as the `/sessions` title (chars).
pub const TITLE_LEN: usize = 48;

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
        let deferred = self.sessions[tab].session_deferred;
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
                    // The drain starts the session before sending the turn.
                    None if deferred => {
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
    use crate::app::outbox::STOPPED_LINE;
    use crate::app::test_support::bottom_app;

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
}
