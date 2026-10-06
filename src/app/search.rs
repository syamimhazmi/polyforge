//! Transcript search (`/` in Normal mode).

use super::App;

impl App {
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
