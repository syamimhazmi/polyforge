//! Transcript scrolling.

use super::App;

impl App {
    // -- scrolling (viewport-relative; clamped on read) --

    fn max_scroll(&self) -> usize {
        let s = self.active();
        let w = self.viewport_width.max(1);
        let total = s.total_rows + s.draft_display_rows(w);
        total.saturating_sub(self.viewport_height.max(1))
    }

    pub(super) fn set_scroll(&mut self, v: usize) {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::{VH, bottom_app};

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
}
