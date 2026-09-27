//! M2 configuration: TOML at `$XDG_CONFIG_HOME/polyforge/config.toml`
//! (`~/.config/polyforge/config.toml`). Secrets are never stored here —
//! `muse` owns its credentials (`muse login` / Keychain); a missing login
//! surfaces as a greyed-out provider with the fix command (spec Q9).

use std::path::PathBuf;

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, Default)]
pub struct Config {
    #[serde(default)]
    pub polyforge: Core,
    #[serde(default)]
    pub ui: UiCfg,
    #[serde(default, rename = "muse")]
    pub muse_cfg: MuseCfg,
    #[serde(default, rename = "codex")]
    pub codex_cfg: CodexCfg,
    #[serde(default, rename = "agy")]
    pub agy_cfg: AgyCfg,
    #[serde(default, rename = "grok")]
    pub grok_cfg: GrokCfg,
    #[serde(default, rename = "claude")]
    pub claude_cfg: ClaudeCfg,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, Default)]
pub struct Core {
    /// "muse" | "codex" | "agy" | "grok" | "claude" | "mock". Unset = first run:
    /// boot asks for a provider and saves the choice back here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Session workspace root. Default: process cwd at startup.
    #[serde(default)]
    pub workspace: Option<String>,
    /// Vim keymap (j/k/g/G/i/a, default off). Toggled live with `/vim`,
    /// which saves it back here so the choice sticks across runs.
    #[serde(default)]
    pub vim: bool,
}

fn default_theme() -> String {
    "groknight".to_string()
}

/// `[ui]` appearance settings. Unknown/missing theme names fall back to
/// the default at read time (see [`Config::theme_kind`]).
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct UiCfg {
    /// "groknight" (default) | "tokyonight" (+ aliases). Live-switched
    /// with `/theme`, which saves back here so the choice sticks.
    #[serde(default = "default_theme")]
    pub theme: String,
}

impl Default for UiCfg {
    fn default() -> Self {
        Self {
            theme: default_theme(),
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, Default)]
pub struct MuseCfg {
    /// `muse` binary. Default: "muse" on PATH.
    #[serde(default)]
    pub bin: Option<String>,
    /// MSP provider routing, e.g. "meta" (live) or "echo" (offline dev).
    /// Default: "meta".
    #[serde(default)]
    pub provider_id: Option<String>,
    /// Model override. Omitted = server default.
    #[serde(default)]
    pub model: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, Default)]
pub struct AgyCfg {
    /// `agy` binary. Default: "agy" on PATH.
    #[serde(default)]
    pub bin: Option<String>,
    /// Model override (`--model`). Omitted = server default.
    #[serde(default)]
    pub model: Option<String>,
    /// Agent override (`--agent`). Omitted = server default.
    #[serde(default)]
    pub agent: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, Default)]
pub struct CodexCfg {
    /// `codex` binary. Default: "codex" on PATH.
    #[serde(default)]
    pub bin: Option<String>,
    /// Model override. Omitted = server default.
    #[serde(default)]
    pub model: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, Default)]
pub struct GrokCfg {
    /// `grok` binary. Default: "grok" on PATH.
    #[serde(default)]
    pub bin: Option<String>,
    /// Model override (session/set_config_option). Omitted = server default.
    #[serde(default)]
    pub model: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, Default)]
pub struct ClaudeCfg {
    /// `claude` binary. Default: "claude" on PATH.
    #[serde(default)]
    pub bin: Option<String>,
    /// Model override (`--model`). Omitted = Claude Code default.
    #[serde(default)]
    pub model: Option<String>,
    /// `--permission-mode` (e.g. "acceptEdits"). Omitted = your Claude
    /// settings' default; tools they don't pre-allow raise the y/n modal.
    #[serde(default)]
    pub permission_mode: Option<String>,
}

impl Config {
    pub fn path() -> PathBuf {
        let base = std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
                PathBuf::from(home).join(".config")
            });
        base.join("polyforge").join("config.toml")
    }

    pub fn load() -> Self {
        let path = Self::path();
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        if text.trim().is_empty() {
            return Self::default();
        }
        toml::from_str(&text).unwrap_or_default()
    }

    /// Persist back to the config file (creates parent dirs). Used by
    /// `/vim` so the keymap choice sticks across runs.
    pub fn save(&self) -> std::io::Result<()> {
        Self::save_to(&Self::path(), self)
    }

    /// Save to an explicit path (tests).
    pub fn save_to(path: &std::path::Path, cfg: &Self) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string(cfg)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(path, text)
    }

    pub fn muse_bin(&self) -> String {
        std::env::var("POLYFORGE_MUSE_BIN")
            .ok()
            .or_else(|| self.muse_cfg.bin.clone())
            .unwrap_or_else(|| "muse".to_string())
    }

    pub fn muse_provider_id(&self) -> Option<String> {
        std::env::var("POLYFORGE_MUSE_PROVIDER")
            .ok()
            .or_else(|| self.muse_cfg.provider_id.clone())
    }

    pub fn codex_bin(&self) -> String {
        std::env::var("POLYFORGE_CODEX_BIN")
            .ok()
            .or_else(|| self.codex_cfg.bin.clone())
            .unwrap_or_else(|| "codex".to_string())
    }

    pub fn codex_model(&self) -> Option<String> {
        std::env::var("POLYFORGE_CODEX_MODEL")
            .ok()
            .or_else(|| self.codex_cfg.model.clone())
    }

    /// Active TUI theme. Unknown names (hand-edited typos) fall back to
    /// the default instead of failing the whole config load.
    pub fn theme_kind(&self) -> crate::theme::ThemeKind {
        self.ui.theme.parse().unwrap_or_default()
    }

    /// Default backend for fresh tabs (the picker can switch per tab).
    pub fn default_backend(&self) -> crate::app::BackendKind {
        match self.polyforge.provider.as_deref().unwrap_or("muse") {
            "mock" => crate::app::BackendKind::Mock,
            "codex" => crate::app::BackendKind::Codex,
            "agy" | "antigravity" => crate::app::BackendKind::Agy,
            "grok" | "xai" => crate::app::BackendKind::Grok,
            "claude" | "anthropic" => crate::app::BackendKind::Claude,
            _ => crate::app::BackendKind::Muse,
        }
    }

    pub fn grok_bin(&self) -> String {
        std::env::var("POLYFORGE_GROK_BIN")
            .ok()
            .or_else(|| self.grok_cfg.bin.clone())
            .unwrap_or_else(|| "grok".to_string())
    }

    pub fn grok_model(&self) -> Option<String> {
        std::env::var("POLYFORGE_GROK_MODEL")
            .ok()
            .or_else(|| self.grok_cfg.model.clone())
    }

    pub fn claude_bin(&self) -> String {
        std::env::var("POLYFORGE_CLAUDE_BIN")
            .ok()
            .or_else(|| self.claude_cfg.bin.clone())
            .unwrap_or_else(|| "claude".to_string())
    }

    /// Extra `claude` args from config (model, permission mode).
    pub fn claude_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if let Some(m) = self.claude_cfg.model.clone() {
            args.extend(["--model".to_string(), m]);
        }
        if let Some(pm) = self.claude_cfg.permission_mode.clone() {
            args.extend(["--permission-mode".to_string(), pm]);
        }
        args
    }

    pub fn agy_bin(&self) -> String {
        std::env::var("POLYFORGE_AGY_BIN")
            .ok()
            .or_else(|| self.agy_cfg.bin.clone())
            .unwrap_or_else(|| "agy".to_string())
    }

    pub fn workspace_root(&self) -> String {
        if let Some(w) = &self.polyforge.workspace {
            return w.clone();
        }
        std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| ".".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_select_muse_with_server_model() {
        let c: Config = toml::from_str("").unwrap();
        assert!(c.polyforge.provider.is_none(), "unset = first run");
        assert_eq!(c.default_backend(), crate::app::BackendKind::Muse);
        assert!(!c.polyforge.vim, "vim keymap defaults off");
        assert!(c.muse_cfg.model.is_none());
        assert!(c.muse_cfg.provider_id.is_none());
    }

    #[test]
    fn save_round_trips_vim_flag() {
        let dir = std::env::temp_dir().join(format!("pf-cfg-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("sub").join("config.toml");
        let mut c = Config::default();
        c.polyforge.vim = true;
        Config::save_to(&path, &c).expect("save");
        let raw = std::fs::read_to_string(&path).expect("read");
        let back: Config = toml::from_str(&raw).expect("parse");
        assert!(back.polyforge.vim);
        assert!(back.polyforge.provider.is_none(), "unset stays unset");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn theme_defaults_and_parses_with_typo_fallback() {
        let c: Config = toml::from_str("").unwrap();
        assert_eq!(c.ui.theme, "groknight");
        assert_eq!(c.theme_kind(), crate::theme::ThemeKind::GrokNight);
        let c: Config = toml::from_str("[ui]\ntheme = \"tokyo\"\n").unwrap();
        assert_eq!(c.theme_kind(), crate::theme::ThemeKind::TokyoNight);
        let c: Config = toml::from_str("[ui]\ntheme = \"papre\"\n").unwrap();
        assert_eq!(
            c.theme_kind(),
            crate::theme::ThemeKind::GrokNight,
            "typo falls back, never breaks load"
        );
    }

    #[test]
    fn grok_provider_selects_grok_backend() {
        let c: Config =
            toml::from_str("[polyforge]\nprovider = \"grok\"\n[grok]\nmodel = \"grok-4.1\"\n")
                .unwrap();
        assert_eq!(c.default_backend(), crate::app::BackendKind::Grok);
        assert_eq!(c.grok_bin(), "grok");
        assert_eq!(c.grok_model().as_deref(), Some("grok-4.1"));
    }

    #[test]
    fn claude_provider_selects_claude_backend() {
        let c: Config = toml::from_str(
            "[polyforge]\nprovider = \"claude\"\n\
             [claude]\nmodel = \"opus\"\npermission_mode = \"acceptEdits\"\n",
        )
        .unwrap();
        assert_eq!(c.default_backend(), crate::app::BackendKind::Claude);
        assert_eq!(c.claude_bin(), "claude");
        assert_eq!(
            c.claude_args(),
            ["--model", "opus", "--permission-mode", "acceptEdits"]
        );
    }

    #[test]
    fn parses_full_config() {
        let c: Config = toml::from_str(
            "[polyforge]\nprovider = \"mock\"\nworkspace = \"/tmp/w\"\n\
             [muse]\nbin = \"/opt/muse\"\nprovider_id = \"echo\"\nmodel = \"muse-spark-1.2\"\n",
        )
        .unwrap();
        assert_eq!(c.polyforge.provider.as_deref(), Some("mock"));
        assert_eq!(c.workspace_root(), "/tmp/w");
        assert_eq!(c.muse_bin(), "/opt/muse");
        assert_eq!(c.muse_provider_id().as_deref(), Some("echo"));
    }
}
