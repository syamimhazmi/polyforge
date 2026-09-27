//! Central TUI theme (grok-build-inspired).
//!
//! Every widget color in [`crate::ui`] flows from one [`Theme`], selected
//! by [`ThemeKind`]. Two dark palettes: [`ThemeKind::GrokNight`] (default,
//! tokens from grok-build's `groknight.rs`) and [`ThemeKind::TokyoNight`].
//! Only foregrounds, borders, and accents are themed — never background
//! fills — so the UI stays readable on any terminal background.
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
            text_primary: rgb(225, 225, 225),   // #e1e1e1
            text_secondary: rgb(200, 200, 200), // #c8c8c8
            muted: rgb(108, 108, 108),          // #6c6c6c

            user: rgb(125, 207, 255),      // cyan #7dcfff
            assistant: rgb(187, 154, 247), // magenta #bb9af7
            running: rgb(125, 207, 255),   // cyan
            tool: rgb(120, 120, 120),      // bright gray #787878
            ok: rgb(158, 206, 106),        // green #9ece6a
            err: rgb(247, 118, 142),       // red #f7768e
            warn: rgb(224, 175, 104),      // yellow #e0af68
            plan: rgb(255, 219, 141),      // gold #FFDB8D
            skill: rgb(122, 162, 247),     // blue #7aa2f7
            path: rgb(255, 158, 100),      // orange #ff9e64

            tab_active: rgb(224, 175, 104), // gold
            tab_busy: rgb(125, 207, 255),   // cyan
            border: rgb(50, 50, 55),        // #323237 dim prompt chrome
            border_active: rgb(80, 80, 88), // #505058 focused chrome

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

            user: rgb(125, 207, 255),
            assistant: rgb(187, 154, 247),
            running: rgb(125, 207, 255),
            tool: rgb(154, 165, 206), // #9aa5ce
            ok: rgb(158, 206, 106),
            err: rgb(247, 118, 142),
            warn: rgb(224, 175, 104),
            plan: rgb(255, 219, 141),
            skill: rgb(122, 162, 247),
            path: rgb(255, 158, 100),

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
