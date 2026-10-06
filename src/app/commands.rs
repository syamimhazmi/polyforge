//! Slash commands: suggestions, completion and the command handlers.

use super::App;

/// Slash commands available in Insert mode. The first element is the
/// display template; `accept_slash_completion` inserts `insert` instead
/// (e.g. `/sessions` completes to `/sessions ` so a query can follow).
pub const SLASH_COMMANDS: [(&str, &str, &str); 7] = [
    (
        "/sessions [query]",
        "/sessions ",
        "browse previous sessions",
    ),
    ("/new", "/new", "fresh session in this tab"),
    ("/tab new", "/tab new", "open a tab (max 3)"),
    ("/tab close", "/tab close", "close this tab"),
    ("/theme [name]", "/theme ", "switch theme (bare = next)"),
    ("/vim", "/vim", "toggle vim keymap"),
    ("/help", "/help", "command + key summary"),
];

impl App {
    /// Indices into `SLASH_COMMANDS` matching the active tab's input as
    /// a prefix. Empty unless the input starts with `/`; typing args
    /// past a complete command (e.g. `/sessions foo`) hides the list.
    pub fn slash_matches(&self) -> Vec<usize> {
        let input = self.active().input.as_str();
        if !input.starts_with('/') {
            return Vec::new();
        }
        SLASH_COMMANDS
            .iter()
            .enumerate()
            .filter(|(_, (template, _, _))| template.starts_with(input))
            .map(|(i, _)| i)
            .collect()
    }

    /// Move the suggestion highlight, wrapping around the current list.
    /// `delta` is +1 (next) or -1 (previous). No-op when the list is empty.
    pub fn cycle_cmd_sel(&mut self, delta: i32) {
        let n = self.slash_matches().len();
        if n == 0 {
            self.cmd_sel = 0;
            return;
        }
        self.cmd_sel = (self.cmd_sel as i32 + delta).rem_euclid(n as i32) as usize;
    }

    /// Replace the input with the highlighted suggestion, cursor to the
    /// end. Returns false when no suggestion is showing.
    pub fn accept_slash_completion(&mut self) -> bool {
        let matches = self.slash_matches();
        let Some(&i) = matches.get(self.cmd_sel.min(matches.len().saturating_sub(1))) else {
            return false;
        };
        let (_, insert, _) = SLASH_COMMANDS[i];
        let s = self.active_mut();
        s.input = insert.to_string();
        s.cursor = s.input.chars().count();
        self.cmd_sel = 0;
        true
    }

    /// Slash commands (typed in Insert mode, never sent to a backend):
    /// `/sessions [query]`, `/new`, `/tab new`, `/tab close`, `/theme [name]`,
    /// `/vim`, `/help`.
    pub fn run_command(&mut self, cmd: &str) {
        let mut parts = cmd.split_whitespace();
        match parts.next().unwrap_or("") {
            "/sessions" => {
                let query: Vec<&str> = parts.collect();
                self.open_session_chooser(query.join(" "));
            }
            // Fresh session in the ACTIVE tab (command form of R; `/tab
            // new` puts the fresh session in a new tab instead).
            "/new" => {
                let cur = self.sessions[self.active].backend;
                self.respawn_active(cur);
            }
            "/tab" => match parts.next().unwrap_or("") {
                "new" => self.open_tab(),
                "close" => {
                    if let Err(msg) = self.close_active_tab() {
                        self.flash = msg;
                    }
                }
                _ => self.flash = "usage: /tab new · /tab close".to_string(),
            },
            "/vim" => self.toggle_vim(),
            "/theme" => {
                let arg = parts.next().map(|s| s.to_string());
                self.run_theme_command(arg);
            }
            "/help" => self.show_help(),
            _ => {
                self.flash =
                    "unknown command — /sessions · /new · /tab new · /tab close · /theme · /vim · /help"
                        .to_string()
            }
        }
    }

    /// Flip the vim keymap and persist it to the config file so the
    /// choice sticks across runs. A failed save keeps the in-memory
    /// value and says so (never blocks typing).
    pub fn toggle_vim(&mut self) {
        self.vim = !self.vim;
        let mut cfg = crate::config::Config::load();
        cfg.polyforge.vim = self.vim;
        match cfg.save() {
            Ok(()) => {
                self.flash = format!(
                    "vim mode {} (saved to {})",
                    if self.vim { "on" } else { "off" },
                    crate::config::Config::path().display()
                );
            }
            Err(e) => {
                self.flash = format!(
                    "vim mode {} (NOT saved: {e})",
                    if self.vim { "on" } else { "off" }
                );
            }
        }
    }

    /// Switch the UI theme: `/theme <name>` applies it, bare `/theme`
    /// cycles to the next palette. Persists to `[ui] theme` like `/vim`
    /// so the choice sticks across runs; a failed save keeps the
    /// in-memory theme and says so.
    pub fn run_theme_command(&mut self, arg: Option<String>) {
        let kind = match arg.as_deref() {
            None => self.theme.cycle(),
            Some(name) => match name.parse::<crate::theme::ThemeKind>() {
                Ok(k) => k,
                Err(e) => {
                    self.flash = e;
                    return;
                }
            },
        };
        self.theme = kind;
        let mut cfg = crate::config::Config::load();
        cfg.ui.theme = kind.name().to_string();
        match cfg.save() {
            Ok(()) => {
                self.flash = format!(
                    "theme {} (saved to {})",
                    kind.name(),
                    crate::config::Config::path().display()
                );
            }
            Err(e) => {
                self.flash = format!("theme {} (NOT saved: {e})", kind.name());
            }
        }
    }

    /// UI-only command help (never persisted: detached from the sink).
    fn show_help(&mut self) {
        let sink = self.active_mut().sink.take();
        let move_keys = if self.vim {
            "j/k line · g/G top/bottom · Space/i/a type"
        } else {
            "arrows/HOME/END/PgUp/PgDn · Space/Enter types · /vim for vim keys"
        };
        for l in [
            "commands (Insert mode, Enter sends):".to_string(),
            "  /sessions [query] — browse previous sessions, Enter views + continues, d deletes"
                .to_string(),
            "  /new — fresh session in this tab (same as R)".to_string(),
            "  /tab new — open a tab (max 3), same backend as current".to_string(),
            "  /tab close — close this tab, killing its session".to_string(),
            "  /theme [name] — switch theme, bare cycles (saved to config)".to_string(),
            "  /vim — toggle vim keymap (saved to config)".to_string(),
            format!("keys (Normal mode): {move_keys} · P provider · R fresh · Esc stop turn · q quit"),
        ] {
            self.active_mut().push_line(l);
        }
        self.active_mut().sink = sink;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::BackendKind;
    use crate::app::test_support::lock_config_env;

    #[test]
    fn slash_commands_never_reach_a_backend() {
        let mut app = App::new();
        app.active_mut().backend = BackendKind::Codex;
        app.active_mut().remote_id = Some("thread-1".into());
        app.active_mut().input = "/tab new".to_string();
        app.submit();
        assert_eq!(app.sessions.len(), 2, "/tab new opens a tab");
        assert!(app.outbox.submits.is_empty());
        assert!(
            !app.sessions[0].lines.iter().any(|l| l.contains("/tab new")),
            "command must not echo as a prompt"
        );
        app.active_mut().input = "/nope".to_string();
        app.submit();
        assert!(app.flash.contains("unknown command"));
        assert!(app.outbox.submits.is_empty());
    }

    #[test]
    fn vim_command_toggles_and_persists() {
        // Isolate the real config file (serialized with the /theme
        // persistence test via CONFIG_ENV_LOCK).
        let _env = lock_config_env();
        let dir = std::env::temp_dir().join(format!("pf-vim-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let old = std::env::var_os("XDG_CONFIG_HOME");
        // SAFETY: nothing else in this suite reads XDG_CONFIG_HOME, and
        // the original value is restored before this test returns.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", &dir);
        }
        let mut app = App::new();
        assert!(!app.vim);
        app.toggle_vim();
        assert!(app.vim);
        assert!(app.flash.contains("vim mode on"));
        let raw = std::fs::read_to_string(dir.join("polyforge").join("config.toml"))
            .expect("config saved");
        assert!(raw.contains("vim = true"), "choice persisted: {raw}");
        // Toggle back: the file follows, so a reboot stays in normal keys.
        app.toggle_vim();
        assert!(!app.vim);
        let raw = std::fs::read_to_string(dir.join("polyforge").join("config.toml"))
            .expect("config saved");
        assert!(raw.contains("vim = false"), "choice persisted: {raw}");
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
    fn theme_unknown_name_leaves_theme_and_lists_choices() {
        let mut app = App::new();
        let before = app.theme;
        app.run_theme_command(Some("paper".to_string()));
        assert_eq!(app.theme, before);
        assert!(app.flash.contains("unknown theme"), "flash: {}", app.flash);
        assert!(app.flash.contains("groknight"), "flash: {}", app.flash);
    }

    #[test]
    fn theme_command_cycles_and_persists() {
        // Same XDG isolation pattern as vim_command_toggles_and_persists
        // (serialized via CONFIG_ENV_LOCK).
        let _env = lock_config_env();
        let dir = std::env::temp_dir().join(format!("pf-theme-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let old = std::env::var_os("XDG_CONFIG_HOME");
        // SAFETY: nothing else in this suite reads XDG_CONFIG_HOME, and
        // the original value is restored before this test returns.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", &dir);
        }
        let mut app = App::new();
        assert_eq!(app.theme, crate::theme::ThemeKind::GrokNight);
        // Bare /theme cycles to the next palette and saves it.
        app.run_theme_command(None);
        assert_eq!(app.theme, crate::theme::ThemeKind::TokyoNight);
        assert!(
            app.flash.contains("theme tokyonight"),
            "flash: {}",
            app.flash
        );
        let raw = std::fs::read_to_string(dir.join("polyforge").join("config.toml"))
            .expect("config saved");
        assert!(
            raw.contains("theme = \"tokyonight\""),
            "choice persisted: {raw}"
        );
        // Named /theme applies directly (alias accepted) and saves back.
        app.run_theme_command(Some("dark".to_string()));
        assert_eq!(app.theme, crate::theme::ThemeKind::GrokNight);
        let raw = std::fs::read_to_string(dir.join("polyforge").join("config.toml"))
            .expect("config saved");
        assert!(
            raw.contains("theme = \"groknight\""),
            "choice persisted: {raw}"
        );
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
    fn slash_bare_prefix_lists_every_command() {
        let mut app = App::new();
        app.active_mut().input = "/".to_string();
        let got = app.slash_matches();
        assert_eq!(got.len(), SLASH_COMMANDS.len());
        assert_eq!(got, (0..SLASH_COMMANDS.len()).collect::<Vec<_>>());
    }

    #[test]
    fn slash_prefix_filters_and_tab_pair_matches() {
        let mut app = App::new();
        app.active_mut().input = "/s".to_string();
        let names: Vec<&str> = app
            .slash_matches()
            .iter()
            .map(|&i| SLASH_COMMANDS[i].0)
            .collect();
        assert_eq!(names, vec!["/sessions [query]"]);
        // "/t" narrows to the /tab spellings + /theme.
        app.active_mut().input = "/t".to_string();
        let names: Vec<&str> = app
            .slash_matches()
            .iter()
            .map(|&i| SLASH_COMMANDS[i].0)
            .collect();
        assert_eq!(names, vec!["/tab new", "/tab close", "/theme [name]"]);
        app.active_mut().input = "/tab ".to_string();
        assert_eq!(app.slash_matches().len(), 2);
        app.active_mut().input = "/tab c".to_string();
        let names: Vec<&str> = app
            .slash_matches()
            .iter()
            .map(|&i| SLASH_COMMANDS[i].0)
            .collect();
        assert_eq!(names, vec!["/tab close"]);
    }

    #[test]
    fn slash_matches_hide_without_prefix_or_past_args() {
        let mut app = App::new();
        assert!(app.slash_matches().is_empty(), "empty input shows nothing");
        app.active_mut().input = "hello".to_string();
        assert!(app.slash_matches().is_empty());
        // Args past a complete command hide the list (user is typing a query).
        app.active_mut().input = "/sessions foo".to_string();
        assert!(app.slash_matches().is_empty());
        app.active_mut().input = "/nope".to_string();
        assert!(app.slash_matches().is_empty());
    }

    #[test]
    fn slash_cycle_wraps_and_accept_inserts() {
        let mut app = App::new();
        // "/t" matches /tab new, /tab close, /theme [name].
        app.active_mut().input = "/t".to_string();
        app.cycle_cmd_sel(1);
        assert_eq!(app.cmd_sel, 1);
        app.cycle_cmd_sel(1);
        assert_eq!(app.cmd_sel, 2, "third match (/theme)");
        app.cycle_cmd_sel(1);
        assert_eq!(app.cmd_sel, 0, "wraps past the end");
        app.cycle_cmd_sel(-1);
        assert_eq!(app.cmd_sel, 2, "wraps past the start");
        assert!(app.accept_slash_completion());
        assert_eq!(app.active().input, "/theme ");
        assert_eq!(app.active().cursor, "/theme ".chars().count());
        // Sessions completes with a trailing space for the query.
        app.active_mut().input = "/s".to_string();
        assert!(app.accept_slash_completion());
        assert_eq!(app.active().input, "/sessions ");
        // Nothing showing: accept is a no-op.
        app.active_mut().input = "hi".to_string();
        assert!(!app.accept_slash_completion());
        assert_eq!(app.active().input, "hi");
    }
}
