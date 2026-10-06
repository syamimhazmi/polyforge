//! Syncing tab state to the transcript store.

use super::App;

impl App {
    /// Record a tab's stored session (backend + remote id + title) for
    /// `/sessions`, bumping UPDATED to now. No-op when storageless or the
    /// tab has no store id.
    pub fn save_tab_meta(&mut self, tab: usize) {
        let now = crate::store::now_millis();
        if let Some(s) = self.sessions.get_mut(tab) {
            s.updated_at = now;
        }
        if let (Some(store), Some(s)) = (self.store.as_ref(), self.sessions.get(tab))
            && let Some(id) = s.store_id.as_deref()
        {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::test_store;

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
}
