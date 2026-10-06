//! Mouse drag selection in the transcript and the text it copies.

use super::App;
use super::session::Session;
use super::text::{col_to_char, sanitize_text};

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

impl App {
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
        if let Some((line, ch)) = self.cell_to_text(col, row)
            && let Some(sel) = self.sel.as_mut()
        {
            sel.focus_line = line;
            sel.focus_char = ch;
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(app.sel_begin(1 + 4, 1));
        app.sel_extend(1 + 4, 1 + 1);
        assert_eq!(app.selected_text().as_deref(), Some("bbbbcc\ndddd"));
        // Reversed drag normalizes the same way.
        assert!(app.sel_begin(1 + 4, 1 + 1));
        app.sel_extend(1 + 4, 1);
        assert_eq!(app.selected_text().as_deref(), Some("bbbbcc\ndddd"));
        // Click without drag copies nothing.
        assert!(app.sel_begin(1 + 2, 1));
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
}
