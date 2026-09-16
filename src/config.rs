//! M2 configuration: TOML at `$XDG_CONFIG_HOME/polyforge/config.toml`
//! (`~/.config/polyforge/config.toml`). Secrets are never stored here —
//! `muse` owns its credentials (`muse login` / Keychain); a missing login
//! surfaces as a greyed-out provider with the fix command (spec Q9).

use std::path::PathBuf;

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, Default)]
pub struct Config {
    #[serde(default)]
    pub polyforge: Core,
    #[serde(default, rename = "muse")]
    pub muse_cfg: MuseCfg,
    #[serde(default, rename = "codex")]
    pub codex_cfg: CodexCfg,
    #[serde(default, rename = "agy")]
    pub agy_cfg: AgyCfg,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct Core {
    /// "muse" | "mock". Mock stays for quota-free UI work.
    #[serde(default = "default_provider")]
    pub provider: String,
    /// Session workspace root. Default: process cwd at startup.
    #[serde(default)]
    pub workspace: Option<String>,
}

impl Default for Core {
    fn default() -> Self {
        Self {
            provider: default_provider(),
            workspace: None,
        }
    }
}

fn default_provider() -> String {
    "muse".to_string()
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

    /// Default backend for fresh tabs (the picker can switch per tab).
    pub fn default_backend(&self) -> crate::app::BackendKind {
        match self.polyforge.provider.as_str() {
            "mock" => crate::app::BackendKind::Mock,
            "codex" => crate::app::BackendKind::Codex,
            "agy" | "antigravity" => crate::app::BackendKind::Agy,
            _ => crate::app::BackendKind::Muse,
        }
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
        assert_eq!(c.polyforge.provider, "muse");
        assert!(c.muse_cfg.model.is_none());
        assert!(c.muse_cfg.provider_id.is_none());
    }

    #[test]
    fn parses_full_config() {
        let c: Config = toml::from_str(
            "[polyforge]\nprovider = \"mock\"\nworkspace = \"/tmp/w\"\n\
             [muse]\nbin = \"/opt/muse\"\nprovider_id = \"echo\"\nmodel = \"muse-spark-1.2\"\n",
        )
        .unwrap();
        assert_eq!(c.polyforge.provider, "mock");
        assert_eq!(c.workspace_root(), "/tmp/w");
        assert_eq!(c.muse_bin(), "/opt/muse");
        assert_eq!(c.muse_provider_id().as_deref(), Some("echo"));
    }
}
