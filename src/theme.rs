//! Central TUI theme (grok-build-inspired).
//!
//! Every widget color in [`crate::ui`] flows from one [`Theme`], selected
//! by [`ThemeKind`]. Two dark palettes: [`ThemeKind::GrokNight`] (default,
//! tokens from grok-build's `groknight.rs`) and [`ThemeKind::TokyoNight`].
//! Foregrounds, borders, and accents are themed; the only background fills
//! are the user-prompt block (`user_bg`) and code rows (`code_bg`). The
//! whole-screen background is never painted, so the UI stays readable on
//! any terminal background.
//!
//! Switch live with `/theme [name]` (bare `/theme` cycles); the choice
//! persists to `[ui] theme` in the config file like `/vim`.

use ratatui::style::Color;
use std::str::FromStr;

const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::Rgb(r, g, b)
}

/// Selectable palette. Dark-only by design (see module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemeKind {
    /// Neutral dark base, magenta assistant accent (grok-build default).
    #[default]
    GrokNight,
    /// Blue-tinted Tokyo Night dark palette.
    TokyoNight,
}

impl ThemeKind {
    /// All themes in cycle order (bare `/theme` walks this list).
    pub const ALL: [ThemeKind; 2] = [ThemeKind::GrokNight, ThemeKind::TokyoNight];

    /// Canonical config name (`[ui] theme = "..."`).
    pub fn name(self) -> &'static str {
        match self {
            ThemeKind::GrokNight => "groknight",
            ThemeKind::TokyoNight => "tokyonight",
        }
    }

    /// Canonical names in cycle order (help text, error messages).
    pub fn names() -> Vec<&'static str> {
        Self::ALL.iter().map(|k| k.name()).collect()
    }

    /// Next theme, wrapping around.
    pub fn cycle(self) -> ThemeKind {
        match self {
            ThemeKind::GrokNight => ThemeKind::TokyoNight,
            ThemeKind::TokyoNight => ThemeKind::GrokNight,
        }
    }
}

impl FromStr for ThemeKind {
    type Err = String;

    /// Case-insensitive; accepts grok-build-style aliases (`dark`,
    /// `grok-night`, `tokyo`, ...).
    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw.trim().to_lowercase().as_str() {
            "groknight" | "grok-night" | "grok" | "dark" => Ok(ThemeKind::GrokNight),
            "tokyonight" | "tokyo-night" | "tokyo" => Ok(ThemeKind::TokyoNight),
            _ => Err(format!(
                "unknown theme {raw} — {}",
                ThemeKind::names().join(" · ")
            )),
        }
    }
}

/// Resolved widget colors for one [`ThemeKind`].
#[derive(Debug, Clone, Copy)]
pub struct Theme {
    // -- text --
    pub text_primary: Color,
    pub text_secondary: Color,
    pub muted: Color,
    /// Dim `  │  ` separators (shortcuts row, header tabs).
    pub sep: Color,
    /// Input-box title: model/backend name and its ` · ` dot.
    pub subtle: Color,
    pub dot: Color,
    // -- transcript roles --
    pub user: Color,
    pub assistant: Color,
    pub running: Color,
    pub tool: Color,
    pub ok: Color,
    pub err: Color,
    pub warn: Color,
    pub plan: Color,
    pub skill: Color,
    pub path: Color,
    /// Inline `code` foreground (Markdown answers).
    #[allow(dead_code)]
    pub inline_code: Color,
    // -- fills --
    pub user_bg: Color,
    /// Fenced-code row background (Markdown answers).
    #[allow(dead_code)]
    pub code_bg: Color,
    // -- chrome --
    pub tab_active: Color,
    pub tab_busy: Color,
    pub border: Color,
    pub border_active: Color,
    // -- mode pill backgrounds --
    pub mode_normal: Color,
    pub mode_insert: Color,
    pub mode_search: Color,
    pub mode_picker: Color,
    pub mode_sessions: Color,
}

impl Theme {
    pub fn get(kind: ThemeKind) -> Self {
        match kind {
            ThemeKind::GrokNight => Self::groknight(),
            ThemeKind::TokyoNight => Self::tokyonight(),
        }
    }

    /// grok-build GrokNight: gray ramp on #141414 + TokyoNight accents.
    pub fn groknight() -> Self {
        Self {
            text_primary: rgb(228, 228, 228),   // #e4e4e4
            text_secondary: rgb(190, 190, 190), // #bebebe
            muted: rgb(129, 134, 143),          // #81868f
            sep: rgb(63, 67, 73),               // #3f4349
            subtle: rgb(115, 115, 116),         // #737374
            dot: rgb(94, 100, 108),             // #5e646c

            user: rgb(196, 167, 231),      // lavender #c4a7e7
            assistant: rgb(187, 154, 247), // magenta #bb9af7
            running: rgb(125, 207, 255),   // cyan
            tool: rgb(120, 120, 120),      // bright gray #787878
            ok: rgb(158, 206, 106),        // green #9ece6a
            err: rgb(247, 118, 142),       // red #f7768e
            warn: rgb(224, 175, 104),      // yellow #e0af68
            plan: rgb(255, 219, 141),      // gold #FFDB8D
            skill: rgb(122, 162, 247),     // blue #7aa2f7
            path: rgb(255, 158, 100),      // orange #ff9e64
            inline_code: rgb(125, 207, 223), // cyan #7dcfdf

            user_bg: rgb(15, 18, 22), // #0f1216
            code_bg: rgb(38, 41, 47), // #26292f

            tab_active: rgb(196, 167, 231), // lavender
            tab_busy: rgb(125, 207, 255),   // cyan
            border: rgb(52, 48, 72),        // #343048 dim prompt chrome
            border_active: rgb(90, 84, 122), // #5a547a focused chrome

            mode_normal: rgb(158, 206, 106),
            mode_insert: rgb(224, 175, 104),
            mode_search: rgb(187, 154, 247),
            mode_picker: rgb(125, 207, 255),
            mode_sessions: rgb(122, 162, 247),
        }
    }

    /// TokyoNight Night: blue-tinted dark base, same role layout.
    pub fn tokyonight() -> Self {
        Self {
            text_primary: rgb(192, 202, 245),   // #c0caf5
            text_secondary: rgb(169, 177, 214), // #a9b1d6
            muted: rgb(86, 95, 137),            // #565f89
            sep: rgb(52, 58, 84),
            subtle: rgb(105, 114, 158),
            dot: rgb(76, 84, 122),

            user: rgb(187, 154, 247),
            assistant: rgb(187, 154, 247),
            running: rgb(125, 207, 255),
            tool: rgb(154, 165, 206), // #9aa5ce
            ok: rgb(158, 206, 106),
            err: rgb(247, 118, 142),
            warn: rgb(224, 175, 104),
            plan: rgb(255, 219, 141),
            skill: rgb(122, 162, 247),
            path: rgb(255, 158, 100),
            inline_code: rgb(125, 207, 255),

            user_bg: rgb(30, 32, 48),
            code_bg: rgb(36, 40, 59),

            tab_active: rgb(224, 175, 104),
            tab_busy: rgb(125, 207, 255),
            border: rgb(42, 46, 63),
            border_active: rgb(86, 95, 137),

            mode_normal: rgb(158, 206, 106),
            mode_insert: rgb(224, 175, 104),
            mode_search: rgb(187, 154, 247),
            mode_picker: rgb(125, 207, 255),
            mode_sessions: rgb(122, 162, 247),
        }
    }

    /// Mode pill background for the status bar.
    pub fn mode_color(self, mode: crate::app::Mode) -> Color {
        match mode {
            crate::app::Mode::Normal => self.mode_normal,
            crate::app::Mode::Insert => self.mode_insert,
            crate::app::Mode::Search => self.mode_search,
            crate::app::Mode::Picker => self.mode_picker,
            crate::app::Mode::Sessions => self.mode_sessions,
        }
    }

    /// Diff-modal / risk color for a TypeSafe band.
    pub fn risk(self, band: crate::typesafe::RiskBand) -> Color {
        match band {
            crate::typesafe::RiskBand::Low => self.ok,
            crate::typesafe::RiskBand::Med => self.warn,
            crate::typesafe::RiskBand::High => self.err,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_groknight() {
        assert_eq!(ThemeKind::default(), ThemeKind::GrokNight);
        assert_eq!(ThemeKind::default().name(), "groknight");
    }

    #[test]
    fn parses_canonical_names_and_aliases() {
        for (raw, want) in [
            ("groknight", ThemeKind::GrokNight),
            ("Grok-Night", ThemeKind::GrokNight),
            ("dark", ThemeKind::GrokNight),
            ("grok", ThemeKind::GrokNight),
            ("tokyonight", ThemeKind::TokyoNight),
            ("tokyo-night", ThemeKind::TokyoNight),
            ("TOKYO", ThemeKind::TokyoNight),
            ("  dark  ", ThemeKind::GrokNight),
        ] {
            assert_eq!(raw.parse::<ThemeKind>().expect(raw), want);
        }
    }

    #[test]
    fn unknown_name_errors_and_lists_choices() {
        let err = "paper"
            .parse::<ThemeKind>()
            .expect_err("paper is light-only");
        assert!(err.contains("groknight"), "lists choices: {err}");
        assert!(err.contains("tokyonight"), "lists choices: {err}");
    }

    #[test]
    fn cycle_walks_all_and_wraps() {
        let mut kind = ThemeKind::GrokNight;
        for _ in 0..ThemeKind::ALL.len() {
            kind = kind.cycle();
        }
        assert_eq!(kind, ThemeKind::GrokNight, "full cycle wraps");
        assert_eq!(ThemeKind::GrokNight.cycle(), ThemeKind::TokyoNight);
        assert_eq!(ThemeKind::TokyoNight.cycle(), ThemeKind::GrokNight);
    }

    #[test]
    fn palettes_stay_distinct_and_readable() {
        let dark = Theme::get(ThemeKind::GrokNight);
        let tokyo = Theme::get(ThemeKind::TokyoNight);
        assert_ne!(dark.text_primary, tokyo.text_primary);
        for th in [dark, tokyo] {
            assert_ne!(th.text_primary, th.muted, "primary must differ from dim");
            assert_ne!(th.border, th.border_active, "focus must be visible");
            assert_ne!(th.ok, th.err, "risk bands must differ");
        }
    }

    #[test]
    fn risk_maps_bands_to_traffic_colors() {
        use crate::typesafe::RiskBand;
        let th = Theme::get(ThemeKind::GrokNight);
        assert_eq!(th.risk(RiskBand::Low), th.ok);
        assert_eq!(th.risk(RiskBand::Med), th.warn);
        assert_eq!(th.risk(RiskBand::High), th.err);
    }
}
