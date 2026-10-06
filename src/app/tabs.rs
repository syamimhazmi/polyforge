//! Tabs and the `/sessions` chooser: open, close, respawn and resume.

use super::outbox::OutboxRespawn;
use super::session::Session;
use super::text::short_id;
use super::{App, BackendKind, MAX_SESSIONS, Mode};

impl App {
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
        if backend == BackendKind::Agy {
            self.pending_agy_kill.push(tab);
        }
        if backend == BackendKind::Grok
            && let Some(id) = self.sessions[tab].remote_id.clone()
        {
            self.pending_grok_close.push(id);
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
        if s.backend == BackendKind::Claude
            && let Some(id) = s.remote_id.clone()
        {
            self.pending_claude_kill.push(id);
        }
    }

    pub fn respawn_active(&mut self, backend: BackendKind) {
        let tab = self.active;
        // Drop queued work for this tab; it belongs to the old session.
        self.outbox.submits.retain(|o| o.tab != tab);
        self.outbox.decides.retain(|o| o.tab != tab);
        self.outbox.respawns.retain(|o| o.tab != tab);
        self.outbox.stops.retain(|o| o.tab != tab);
        self.queue_claude_kill(tab);
        // Fresh session id on disk too (old files stay for `/sessions`).
        self.attach_fresh_store(tab);
        {
            let s = self.active_mut();
            s.backend = backend;
            s.remote_id = None;
            s.session_deferred = false;
            s.tokens = None;
            s.tab_degraded = None;
            s.busy = false;
            s.reset_turn_state();
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::{lock_config_env, test_store};

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
}
