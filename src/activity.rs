//! Live-activity helpers (grok-build style): spinner, phase label, wave
//! brightness for the transcript rail, elapsed formatting. Pure functions
//! over `Duration` so they are unit-testable without a clock.

use ratatui::style::Color;
use std::time::Duration;

/// What the agent is doing right now while a tab is busy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Thinking,
    Responding,
}

impl Phase {
    /// Status-bar label for the phase.
    pub fn label(self) -> &'static str {
        match self {
            Phase::Thinking => "Thinking…",
            Phase::Responding => "Responding…",
        }
    }
}

/// Braille spinner frames.
pub const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

/// Spinner frame for `elapsed` (one frame per ~133ms, 4 ticks at 30fps).
pub fn spinner_frame(elapsed: Duration) -> &'static str {
    SPINNER[(elapsed.as_millis() / 133) as usize % SPINNER.len()]
}

/// Rows per full wave period (grok-build default).
const WAVE_ROWS: f32 = 32.0;
/// Wave speed in rad/s (0.15 rad/tick at 30fps).
const WAVE_SPEED: f32 = 4.5;

/// Wave brightness in [0,1] for transcript `row` at `elapsed`.
pub fn wave_brightness(elapsed: Duration, row: usize) -> f32 {
    let phase = (row as f32 / WAVE_ROWS) * std::f32::consts::TAU;
    let s = (elapsed.as_secs_f32() * WAVE_SPEED + phase).sin();
    s * s
}

/// Linear blend `from` → `to` at `t` (clamped). Non-RGB colors yield `to`.
pub fn blend(from: Color, to: Color, t: f32) -> Color {
    let (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) = (from, to) else {
        return to;
    };
    let t = t.clamp(0.0, 1.0);
    let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
    Color::Rgb(mix(r1, r2), mix(g1, g2), mix(b1, b2))
}

/// "3s", "1m 05s", "1h 02m".
pub fn format_elapsed(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m {:02}s", s / 60, s % 60)
    } else {
        format!("{}h {:02}m", s / 3600, (s % 3600) / 60)
    }
}

/// Compact token count for the turn row: "999", "1.23k", "12.3k", "123k",
/// "1.23m", "12.3m". The bucket is chosen AFTER rounding, so 9_999 reads
/// "10.0k" (not "10.00k") and 999_999 reads "1.00m" (not "1000k").
pub fn format_tokens_short(n: u64) -> String {
    if n < 1_000 {
        return n.to_string();
    }
    let k2 = (n + 5) / 10; // hundredths of a thousand
    if k2 < 1_000 {
        return format!("{}.{:02}k", k2 / 100, k2 % 100);
    }
    let k1 = (n + 50) / 100;
    if k1 < 1_000 {
        return format!("{}.{}k", k1 / 10, k1 % 10);
    }
    let k0 = (n + 500) / 1_000;
    if k0 < 1_000 {
        return format!("{k0}k");
    }
    let m2 = (n + 5_000) / 10_000;
    if m2 < 1_000 {
        return format!("{}.{:02}m", m2 / 100, m2 % 100);
    }
    let m1 = (n + 50_000) / 100_000;
    format!("{}.{}m", m1 / 10, m1 % 10)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn spinner_cycles_and_wraps() {
        assert_eq!(spinner_frame(ms(0)), "⠋");
        assert_eq!(spinner_frame(ms(133)), "⠙");
        assert_eq!(spinner_frame(ms(133 * 7)), "⠧");
        assert_eq!(spinner_frame(ms(133 * 8)), "⠋");
        assert_eq!(spinner_frame(ms(133 * 9)), "⠙");
    }

    #[test]
    fn wave_in_range_and_varies_by_row() {
        for row in 0..64 {
            let b = wave_brightness(ms(700), row);
            assert!((0.0..=1.0).contains(&b));
        }
        assert_ne!(wave_brightness(ms(700), 0), wave_brightness(ms(700), 4));
    }

    #[test]
    fn blend_endpoints_and_fallback() {
        let a = Color::Rgb(10, 20, 30);
        let b = Color::Rgb(110, 120, 130);
        assert_eq!(blend(a, b, 0.0), a);
        assert_eq!(blend(a, b, 1.0), b);
        assert_eq!(blend(a, b, 2.0), b);
        assert_eq!(blend(a, b, 0.5), Color::Rgb(60, 70, 80));
        assert_eq!(blend(Color::Red, b, 0.3), b);
        assert_eq!(blend(a, Color::Blue, 0.3), Color::Blue);
    }

    #[test]
    fn elapsed_formats() {
        let s = Duration::from_secs;
        assert_eq!(format_elapsed(s(0)), "0s");
        assert_eq!(format_elapsed(s(59)), "59s");
        assert_eq!(format_elapsed(s(60)), "1m 00s");
        assert_eq!(format_elapsed(s(65)), "1m 05s");
        assert_eq!(format_elapsed(s(3600 + 120)), "1h 02m");
    }

    #[test]
    fn token_counts_round_then_bucket() {
        let cases = [
            (0, "0"),
            (999, "999"),
            (1_000, "1.00k"),
            (1_234, "1.23k"),
            (9_999, "10.0k"),
            (10_049, "10.0k"),
            (12_345, "12.3k"),
            (99_999, "100k"),
            (100_000, "100k"),
            (999_999, "1.00m"),
            (1_234_567, "1.23m"),
            (12_345_678, "12.3m"),
        ];
        for (n, want) in cases {
            assert_eq!(format_tokens_short(n), want, "n={n}");
        }
    }
}
