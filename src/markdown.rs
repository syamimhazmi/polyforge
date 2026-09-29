//! Markdown for agent answers, one raw line at a time. Pure: no I/O, no
//! app state. The transcript stays `Vec<String>`; the only cross-line
//! state is whether a fenced code block is open, carried by [`MdKind`]
//! (one per stored line, see `Session::kinds`).
//!
//! [`md_display`] is the raw-line → display mapping for answer lines: it
//! strips markers (`##`, `**`, backticks, link urls) and styles what is
//! left. Unclosed markers render literally, since streamed lines can be
//! split mid-span.

use ratatui::{
    style::{Modifier, Style},
    text::Span,
};

use crate::theme::Theme;

/// What a stored transcript line is, markdown-wise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MdKind {
    /// Not an answer line (prompt, thought, system/tool line): no markdown.
    Plain,
    /// Answer prose outside a fence.
    Text,
    /// Opening ```` ``` ```` / `~~~` line (shows the language name).
    FenceOpen,
    /// Closing fence line.
    FenceClose,
    /// Inside a fence: shown verbatim.
    Code,
}

impl MdKind {
    /// A fence is open after a line of this kind.
    pub fn in_fence(self) -> bool {
        matches!(self, MdKind::FenceOpen | MdKind::Code)
    }

    /// Rows drawn on the code background.
    pub fn is_code_row(self) -> bool {
        matches!(self, MdKind::FenceOpen | MdKind::FenceClose | MdKind::Code)
    }
}

fn fence_marker(line: &str) -> Option<char> {
    let t = line.trim_start();
    ['`', '~']
        .into_iter()
        .find(|m| t.chars().take_while(|c| c == m).count() >= 3)
}

/// Kind of an answer line given whether a fence is already open.
pub fn classify(line: &str, in_fence: bool) -> MdKind {
    match (in_fence, fence_marker(line)) {
        (true, Some(m)) if line.trim().chars().all(|c| c == m) => MdKind::FenceClose,
        (true, _) => MdKind::Code,
        (false, Some(_)) => MdKind::FenceOpen,
        (false, None) => MdKind::Text,
    }
}

/// `---` / `***` / `___` alone on a line.
pub fn is_rule(line: &str) -> bool {
    let t = line.trim();
    let mut cs = t.chars();
    match cs.next() {
        Some(c @ ('-' | '*' | '_')) => t.chars().count() >= 3 && cs.all(|x| x == c),
        _ => false,
    }
}

/// Display text and styled spans for one line. A rule shows a single `─`
/// (the renderer stretches it across the row).
pub fn md_display(raw: &str, kind: MdKind, th: Theme) -> (String, Vec<Span<'static>>) {
    let code = Style::default().fg(th.text_secondary).bg(th.code_bg);
    let mut spans: Vec<Span<'static>> = Vec::new();
    match kind {
        MdKind::Plain => spans.push(Span::styled(raw.to_string(), Style::default().fg(th.text_primary))),
        MdKind::Code => spans.push(Span::styled(raw.to_string(), code)),
        MdKind::FenceClose => {}
        MdKind::FenceOpen => {
            let lang = raw.trim_start().trim_start_matches(['`', '~']).trim();
            let lang = lang.split_whitespace().next().unwrap_or("");
            if !lang.is_empty() {
                spans.push(Span::styled(lang.to_string(), code.fg(th.muted)));
            }
        }
        MdKind::Text => text_line(raw, th, &mut spans),
    }
    (spans.iter().map(|s| s.content.as_ref()).collect(), spans)
}

fn text_line(raw: &str, th: Theme, out: &mut Vec<Span<'static>>) {
    let base = Style::default().fg(th.text_primary);
    let muted = Style::default().fg(th.muted);
    if is_rule(raw) {
        out.push(Span::styled("─", muted));
        return;
    }
    let trimmed = raw.trim_start();
    let indent = &raw[..raw.len() - trimmed.len()];
    let mut cx = Ctx { th, pipes: false };
    // Heading: `#`..`######` then a space (or nothing).
    let hashes = trimmed.chars().take_while(|&c| c == '#').count();
    if (1..=6).contains(&hashes) && (trimmed.len() == hashes || trimmed[hashes..].starts_with(' ')) {
        let mut style = base.fg(th.user).add_modifier(Modifier::BOLD);
        if hashes == 1 {
            style = style.add_modifier(Modifier::UNDERLINED);
        }
        inline(&chars(trimmed[hashes..].trim_start()), style, &cx, out);
        return;
    }
    if !indent.is_empty() {
        out.push(Span::styled(indent.to_string(), base));
    }
    // Bullet: `- ` / `* ` / `+ `.
    if let Some(rest) = ["- ", "* ", "+ "].iter().find_map(|m| trimmed.strip_prefix(m)) {
        out.push(Span::styled("• ", muted));
        inline(&chars(rest), base, &cx, out);
        return;
    }
    // Numbered: `12. ` / `3) `.
    let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 && (trimmed[digits..].starts_with(". ") || trimmed[digits..].starts_with(") ")) {
        out.push(Span::styled(trimmed[..digits + 2].to_string(), muted));
        inline(&chars(&trimmed[digits + 2..]), base, &cx, out);
        return;
    }
    // Table row: keep the text, mute the pipes.
    cx.pipes = trimmed.starts_with('|') && trimmed.matches('|').count() >= 2;
    inline(&chars(trimmed), base, &cx, out);
}

struct Ctx {
    th: Theme,
    pipes: bool,
}

fn chars(s: &str) -> Vec<char> {
    s.chars().collect()
}

/// First index >= `from` where `pat` starts.
fn find(cs: &[char], from: usize, pat: &[char]) -> Option<usize> {
    (from..cs.len().saturating_sub(pat.len() - 1)).find(|&k| cs[k..].starts_with(pat))
}

fn inline(cs: &[char], base: Style, cx: &Ctx, out: &mut Vec<Span<'static>>) {
    let mut cur = String::new();
    let mut i = 0usize;
    fn flush(cur: &mut String, style: Style, out: &mut Vec<Span<'static>>) {
        if !cur.is_empty() {
            out.push(Span::styled(std::mem::take(cur), style));
        }
    }
    while i < cs.len() {
        let c = cs[i];
        let next = cs.get(i + 1).copied();
        match c {
            '`' => {
                if let Some(j) = find(cs, i + 1, &['`']).filter(|&j| j > i + 1) {
                    flush(&mut cur, base, out);
                    let style = base.fg(cx.th.inline_code).add_modifier(Modifier::BOLD);
                    out.push(Span::styled(cs[i + 1..j].iter().collect::<String>(), style));
                    i = j + 1;
                    continue;
                }
            }
            '*' | '_' | '~' if next == Some(c) => {
                let alnum_before = i > 0 && cs[i - 1].is_alphanumeric();
                let word = c == '_';
                let close = (!(word && alnum_before))
                    .then(|| find(cs, i + 2, &[c, c]))
                    .flatten()
                    .filter(|&j| j > i + 2 && !(word && cs.get(j + 2).is_some_and(|x| x.is_alphanumeric())));
                if let Some(j) = close {
                    flush(&mut cur, base, out);
                    let m = if c == '~' { Modifier::CROSSED_OUT } else { Modifier::BOLD };
                    inline(&cs[i + 2..j], base.add_modifier(m), cx, out);
                    i = j + 2;
                    continue;
                }
                // Unclosed: both markers are literal.
                cur.push(c);
                cur.push(c);
                i += 2;
                continue;
            }
            '*' if next.is_some_and(|n| !n.is_whitespace()) => {
                let close = (i + 2..cs.len()).find(|&k| {
                    cs[k] == '*' && !cs[k - 1].is_whitespace() && cs.get(k + 1) != Some(&'*')
                });
                if let Some(j) = close {
                    flush(&mut cur, base, out);
                    inline(&cs[i + 1..j], base.add_modifier(Modifier::ITALIC), cx, out);
                    i = j + 1;
                    continue;
                }
            }
            '[' => {
                let link = find(cs, i + 1, &[']', '('])
                    .filter(|&j| j > i + 1)
                    .and_then(|j| find(cs, j + 2, &[')']).map(|e| (j, e)));
                if let Some((j, e)) = link {
                    flush(&mut cur, base, out);
                    inline(&cs[i + 1..j], base.add_modifier(Modifier::UNDERLINED), cx, out);
                    i = e + 1;
                    continue;
                }
            }
            '|' if cx.pipes => {
                flush(&mut cur, base, out);
                out.push(Span::styled("|", Style::default().fg(cx.th.muted)));
                i += 1;
                continue;
            }
            _ => {}
        }
        cur.push(c);
        i += 1;
    }
    flush(&mut cur, base, out);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn th() -> Theme {
        Theme::groknight()
    }

    fn text(raw: &str) -> String {
        md_display(raw, MdKind::Text, th()).0
    }

    /// (content, fg, modifiers) per span.
    fn spans(raw: &str) -> Vec<(String, Option<ratatui::style::Color>, Modifier)> {
        md_display(raw, MdKind::Text, th())
            .1
            .into_iter()
            .map(|s| (s.content.to_string(), s.style.fg, s.style.add_modifier))
            .collect()
    }

    #[test]
    fn headings_hide_markers_and_go_lavender_bold() {
        assert_eq!(text("## Markdown sample"), "Markdown sample");
        let s = spans("## Title");
        assert_eq!(s[0].1, Some(th().user));
        assert!(s[0].2.contains(Modifier::BOLD) && !s[0].2.contains(Modifier::UNDERLINED));
        assert!(spans("# Big")[0].2.contains(Modifier::UNDERLINED));
        assert_eq!(text("#hashtag"), "#hashtag", "needs a space");
        assert_eq!(text("####### seven"), "####### seven");
    }

    #[test]
    fn bold_italic_code_strike_link() {
        assert_eq!(text("a **b** c"), "a b c");
        assert!(spans("a **b** c")[1].2.contains(Modifier::BOLD));
        assert_eq!(text("a __b__ c"), "a b c");
        assert_eq!(text("a *b* c"), "a b c");
        assert!(spans("a *b* c")[1].2.contains(Modifier::ITALIC));
        let s = spans("use `cargo` now");
        assert_eq!(s[1].0, "cargo");
        assert_eq!(s[1].1, Some(th().inline_code));
        assert!(s[1].2.contains(Modifier::BOLD));
        assert_eq!(text("~~gone~~"), "gone");
        assert!(spans("~~gone~~")[0].2.contains(Modifier::CROSSED_OUT));
        assert_eq!(text("see [docs](https://x.y/z) ok"), "see docs ok");
        assert!(spans("see [docs](https://x.y/z) ok")[1].2.contains(Modifier::UNDERLINED));
        assert_eq!(text("**a `b` c**"), "a b c", "nesting");
    }

    #[test]
    fn unclosed_markers_stay_literal() {
        assert_eq!(text("a **b"), "a **b");
        assert_eq!(text("a `b"), "a `b");
        assert_eq!(text("a *b"), "a *b");
        assert_eq!(text("a ~~b"), "a ~~b");
        assert_eq!(text("[x](y"), "[x](y");
        assert_eq!(text("2 * 3 * 4"), "2 * 3 * 4");
    }

    #[test]
    fn snake_case_untouched() {
        assert_eq!(text("call my_var_name and _x_"), "call my_var_name and _x_");
        assert_eq!(text("__init__ here"), "init here", "double underscore is bold");
        assert_eq!(text("a__b__c"), "a__b__c");
    }

    #[test]
    fn lists() {
        assert_eq!(text("- item"), "• item");
        assert_eq!(text("* item"), "• item");
        assert_eq!(text("+ item"), "• item");
        assert_eq!(text("    - nested **x**"), "    • nested x");
        assert_eq!(spans("- item")[0].1, Some(th().muted));
        assert_eq!(text("1. step"), "1. step");
        assert_eq!(spans("12. step")[0], ("12. ".to_string(), Some(th().muted), Modifier::empty()));
        assert_eq!(text("*emph* first"), "emph first", "not a bullet");
        assert_eq!(text("-not a bullet"), "-not a bullet");
    }

    #[test]
    fn rules_and_tables() {
        for r in ["---", "***", "___", "-----"] {
            assert!(is_rule(r));
            assert_eq!(text(r), "─");
        }
        assert!(!is_rule("--") && !is_rule("-*-") && !is_rule("---x"));
        assert_eq!(text("| a | **b** |"), "| a | b |");
        let s = spans("| a | b |");
        assert_eq!(s[0], ("|".to_string(), Some(th().muted), Modifier::empty()));
        assert_eq!(text("a | b"), "a | b");
    }

    #[test]
    fn fence_state_machine() {
        let mut prev: Option<MdKind> = None;
        let mut kinds = Vec::new();
        for l in ["hi", "```rust", "let x = 1;", "# not a heading", "```", "after", "~~~", "x", "y", "~~~"] {
            let k = classify(l, prev.is_some_and(MdKind::in_fence));
            kinds.push(k);
            prev = Some(k);
        }
        use MdKind::*;
        assert_eq!(
            kinds,
            [Text, FenceOpen, Code, Code, FenceClose, Text, FenceOpen, Code, Code, FenceClose]
        );
    }

    #[test]
    fn code_and_fence_rows() {
        let (t, sp) = md_display("# **x** `y`", MdKind::Code, th());
        assert_eq!(t, "# **x** `y`");
        assert_eq!(sp[0].style.bg, Some(th().code_bg));
        assert_eq!(md_display("```rust", MdKind::FenceOpen, th()).0, "rust");
        assert_eq!(md_display("```", MdKind::FenceOpen, th()).0, "");
        assert_eq!(md_display("```", MdKind::FenceClose, th()).0, "");
        assert_eq!(md_display("**x**", MdKind::Plain, th()).0, "**x**");
    }
}
