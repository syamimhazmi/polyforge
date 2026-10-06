//! Text helpers: display-width wrapping, terminal-escape sanitizing and
//! column/char mapping.

/// Number of display rows `line` occupies at `width` (always ≥ 1).
/// ASCII lines (the common case) skip the per-char width walk: every
/// byte is one column, so the count is plain division over the byte
/// length. Must agree with `wrap_chunks` (greedy packs `width` bytes).
pub fn wrap_rows(line: &str, width: usize) -> usize {
    let w = width.max(1);
    if line.is_ascii() {
        return line.len().div_ceil(w).max(1);
    }
    wrap_chunks(line, w).len().max(1)
}

pub(super) fn wrap_chunks(line: &str, width: usize) -> Vec<String> {
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
            '\x1b' if it.next() == Some('\\') => {
                break;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::app::session::Session;

    #[test]
    fn short_id_shows_id_tail() {
        assert_eq!(short_id("abc"), "abc");
        assert_eq!(short_id("1789516800000-1234-5"), "0-1234-5");
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
}
