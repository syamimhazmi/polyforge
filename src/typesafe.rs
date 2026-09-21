//! TypeSafe System One client for harness judgments.
//!
//! Code owns control flow. Jev supplies narrow judgments over unstructured
//! approval text (risk Score + secret/destructive Nouls). Key load order:
//! `TYPESAFE_API_KEY` env, then `~/.config/typesafe/env` (or
//! `$XDG_CONFIG_HOME/typesafe/env`). Missing key → client is None; the TUI
//! still works.

use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

const DEFAULT_URL: &str = "https://api.typesafe.ai/v1/systemone";
const MODEL: &str = "jev-latest";
/// Cap approval body sent as state (tokens + latency).
const BODY_CAP: usize = 4_000;
/// S1-F2: cap the response body we buffer (~256 KiB). A compromised or
/// malfunctioning endpoint must not grow our buffer without limit.
const MAX_RESPONSE_BYTES: u64 = 256 * 1024;

/// Reject a response body length without reading it. Returns the error
/// message when `len` exceeds [`MAX_RESPONSE_BYTES`].
fn response_too_large(len: u64) -> Option<String> {
    if len > MAX_RESPONSE_BYTES {
        Some(format!(
            "typesafe response body of {len} bytes exceeds {MAX_RESPONSE_BYTES}-byte cap"
        ))
    } else {
        None
    }
}

/// S1-GAP-C: without reqwest `stream`, refuse bodies with no declared size.
fn require_content_length(len: Option<u64>) -> Result<u64, String> {
    len.ok_or_else(|| {
        "typesafe response missing Content-Length; refusing unbounded body read".into()
    })
}
const HTTP_TIMEOUT: Duration = Duration::from_secs(8);

/// Risk band derived in code from the composite score.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RiskBand {
    Low,
    Med,
    High,
}

impl RiskBand {
    pub fn label(self) -> &'static str {
        match self {
            RiskBand::Low => "LOW",
            RiskBand::Med => "MED",
            RiskBand::High => "HIGH",
        }
    }
}

/// One approval judgment: raw TypeSafe answers + code-composed band.
#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalJudgment {
    pub risk: f64,
    pub risk_confidence: f64,
    pub touches_secrets: f64,
    pub destructive: f64,
    pub composite: f64,
    pub band: RiskBand,
    pub uncertain: bool,
}

impl ApprovalJudgment {
    /// Weights live in code (composite scoring). Change without re-asking.
    pub fn compose(risk: f64, risk_confidence: f64, touches_secrets: f64, destructive: f64) -> Self {
        let risk_norm = (risk / 2.0).clamp(0.0, 1.0);
        let composite = (0.50 * risk_norm + 0.30 * destructive + 0.20 * touches_secrets)
            .clamp(0.0, 1.0);
        let band = if composite < 0.35 {
            RiskBand::Low
        } else if composite < 0.65 {
            RiskBand::Med
        } else {
            RiskBand::High
        };
        // Low Score confidence OR noul near coin-flip → flag for the user.
        let uncertain = risk_confidence < 0.50
            || (0.40..0.60).contains(&touches_secrets)
            || (0.40..0.60).contains(&destructive);
        Self {
            risk,
            risk_confidence,
            touches_secrets,
            destructive,
            composite,
            band,
            uncertain,
        }
    }

    pub fn from_answers(answers: &Value) -> Result<Self, String> {
        let risk = answers
            .get("risk")
            .and_then(|a| a.get("score"))
            .and_then(|v| v.as_f64())
            .ok_or_else(|| "missing answers.risk.score".to_string())?;
        let risk_confidence = answers
            .get("risk")
            .and_then(|a| a.get("confidence"))
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let touches_secrets = answers
            .get("touches_secrets")
            .and_then(|a| a.get("noul"))
            .and_then(|v| v.as_f64())
            .ok_or_else(|| "missing answers.touches_secrets.noul".to_string())?;
        let destructive = answers
            .get("destructive")
            .and_then(|a| a.get("noul"))
            .and_then(|v| v.as_f64())
            .ok_or_else(|| "missing answers.destructive.noul".to_string())?;
        Ok(Self::compose(
            risk,
            risk_confidence,
            touches_secrets,
            destructive,
        ))
    }

    /// One-line summary for the DIFF modal / flash.
    pub fn summary_line(&self) -> String {
        let flag = if self.uncertain { "?" } else { "" };
        format!(
            "risk {}{}  score {:.2} (c{:.2})  secrets {:.2}  destroy {:.2}",
            self.band.label(),
            flag,
            self.risk,
            self.risk_confidence,
            self.touches_secrets,
            self.destructive
        )
    }
}

/// HTTP client for System One. Cheap to clone (shared connection pool).
#[derive(Clone)]
pub struct Client {
    api_key: String,
    http: reqwest::Client,
    url: String,
}

impl Client {
    pub fn new(api_key: String) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .map_err(|e| format!("typesafe http client: {e}"))?;
        Ok(Self {
            api_key,
            http,
            url: DEFAULT_URL.to_string(),
        })
    }

    /// Env first, then `~/.config/typesafe/env`.
    pub fn from_env() -> Option<Self> {
        let key = load_api_key()?;
        match Self::new(key) {
            Ok(c) => Some(c),
            Err(e) => {
                eprintln!("polyforge: typesafe client disabled: {e}");
                None
            }
        }
    }

    pub async fn judge_approval(
        &self,
        tool: &str,
        body: &str,
    ) -> Result<ApprovalJudgment, String> {
        let body = truncate(body, BODY_CAP);
        let payload = approval_request(tool, &body);
        let resp = self
            .http
            .post(&self.url)
            .bearer_auth(&self.api_key)
            .json(&payload)
            .send()
            .await
            .map_err(|e| format!("typesafe request failed: {e}"))?;
        let status = resp.status();
        // S1-F2 / S1-GAP-C: require Content-Length and refuse declared-huge
        // bodies before buffering. reqwest `stream` needs `wasm-streams`
        // ^0.5 (not cached offline), so chunked/unknown-length bodies are
        // rejected rather than read unboundedly. Lying Content-Length still
        // relies on the post-`bytes()` check (remaining gap).
        let len = require_content_length(resp.content_length())?;
        if let Some(err) = response_too_large(len) {
            return Err(err);
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| format!("typesafe read body: {e}"))?;
        if let Some(err) = response_too_large(bytes.len() as u64) {
            return Err(err);
        }
        let text =
            String::from_utf8(bytes.to_vec()).map_err(|e| format!("typesafe read body: {e}"))?;
        if !status.is_success() {
            return Err(format!(
                "typesafe HTTP {status}: {}",
                truncate(&text, 200)
            ));
        }
        let parsed: SystemOneResponse = serde_json::from_str(&text)
            .map_err(|e| format!("typesafe parse: {e}"))?;
        ApprovalJudgment::from_answers(&parsed.answers)
    }
}

#[derive(Debug, Deserialize)]
struct SystemOneResponse {
    answers: Value,
}

/// Request body for approval risk (shared with tests).
pub fn approval_request(tool: &str, body: &str) -> Value {
    json!({
        "state": {
            "tool": tool,
            "body": body,
        },
        "model": MODEL,
        "questions": {
            "risk": {
                "type": "score",
                "instructions": {
                    "question": "How risky is approving this coding-agent tool call for the local workspace?",
                    "focus": "Judge blast radius if the user clicks approve.",
                    "inspect": ["`tool`", "`body`"],
                },
                "criteria": [
                    {
                        "what": "Safe read or trivial local change",
                        "examples": ["read a source file", "format one module"],
                    },
                    {
                        "what": "Local write with limited blast radius",
                        "examples": ["edit one project file", "run tests"],
                    },
                    {
                        "what": "Destructive or secret-touching change",
                        "examples": ["delete files", "touch credentials", "broad shell"],
                    },
                ],
            },
            "touches_secrets": {
                "type": "noul",
                "instructions": {
                    "question": "Does this tool call read or write credentials, API keys, or secrets?",
                    "inspect": ["`tool`", "`body`"],
                },
                "criteria": {
                    "true": "Touches credentials, keys, tokens, or secret files",
                    "false": "No credential or secret access",
                },
            },
            "destructive": {
                "type": "noul",
                "instructions": {
                    "question": "Would approving this delete data, overwrite broadly, or run irreversible shell?",
                    "inspect": ["`tool`", "`body`"],
                },
                "criteria": {
                    "true": "Deletes, mass-overwrites, or irreversible shell",
                    "false": "Reversible or narrowly scoped change",
                },
            },
        },
    })
}

/// `TYPESAFE_API_KEY` env, else parse config file.
pub fn load_api_key() -> Option<String> {
    if let Ok(k) = std::env::var("TYPESAFE_API_KEY") {
        let k = k.trim().to_string();
        if !k.is_empty() {
            return Some(k);
        }
    }
    let path = typesafe_env_path()?;
    let text = std::fs::read_to_string(path).ok()?;
    parse_env_file(&text)
}

fn typesafe_env_path() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        let p = PathBuf::from(xdg).join("typesafe/env");
        if p.is_file() {
            return Some(p);
        }
    }
    let home = std::env::var_os("HOME")?;
    let p = PathBuf::from(home).join(".config/typesafe/env");
    p.is_file().then_some(p)
}

/// Parse `KEY=value` / `export KEY=value` lines. Only `TYPESAFE_API_KEY`.
pub fn parse_env_file(text: &str) -> Option<String> {
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim();
        let Some((key, val)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != "TYPESAFE_API_KEY" {
            continue;
        }
        let val = val.trim().trim_matches(|c| c == '"' || c == '\'');
        if !val.is_empty() {
            return Some(val.to_string());
        }
    }
    None
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_env_export_and_plain() {
        assert_eq!(
            parse_env_file("export TYPESAFE_API_KEY=abc123\n").as_deref(),
            Some("abc123")
        );
        assert_eq!(
            parse_env_file("TYPESAFE_API_KEY=\"xyz\"\n").as_deref(),
            Some("xyz")
        );
        assert_eq!(parse_env_file("# comment\nOTHER=1\n"), None);
        assert_eq!(parse_env_file("export TYPESAFE_API_KEY=\n"), None);
    }

    #[test]
    fn compose_low_med_high_and_uncertain() {
        let low = ApprovalJudgment::compose(0.2, 0.9, 0.05, 0.05);
        assert_eq!(low.band, RiskBand::Low);
        assert!(!low.uncertain);
        assert!(low.summary_line().starts_with("risk LOW"));

        let med = ApprovalJudgment::compose(1.0, 0.8, 0.2, 0.3);
        assert_eq!(med.band, RiskBand::Med);

        let high = ApprovalJudgment::compose(2.0, 1.0, 0.97, 0.96);
        assert_eq!(high.band, RiskBand::High);
        assert!(!high.uncertain);
        assert!(high.summary_line().contains("HIGH"));

        let unsure = ApprovalJudgment::compose(1.0, 0.3, 0.5, 0.1);
        assert!(unsure.uncertain);
        assert!(unsure.summary_line().contains('?'));
    }

    #[test]
    fn compose_band_boundaries_are_exclusive_on_upper() {
        // composite = 0.50*(risk/2) + 0.30*destructive + 0.20*secrets
        // risk=0, secrets=0, destructive=d → composite = 0.30*d
        // 0.30*d < 0.35 → d < 0.35/0.30 ≈ 1.166; d=1.0 → 0.30 LOW
        let at_low = ApprovalJudgment::compose(0.0, 1.0, 0.0, 1.0);
        assert!((at_low.composite - 0.30).abs() < 1e-9);
        assert_eq!(at_low.band, RiskBand::Low);

        // Exactly 0.35 → MED (threshold is < 0.35 for LOW).
        // 0.50*(risk/2) = 0.35 → risk/2 = 0.70 → risk = 1.4; secrets=0, destroy=0
        let at_med = ApprovalJudgment::compose(1.4, 1.0, 0.0, 0.0);
        assert!((at_med.composite - 0.35).abs() < 1e-9);
        assert_eq!(at_med.band, RiskBand::Med);

        // Exactly 0.65 → HIGH.
        let at_high = ApprovalJudgment::compose(2.0, 1.0, 0.0, 0.5);
        // 0.50*1.0 + 0.30*0.5 + 0 = 0.65
        assert!((at_high.composite - 0.65).abs() < 1e-9);
        assert_eq!(at_high.band, RiskBand::High);
    }

    #[test]
    fn compose_divides_risk_by_two_not_multiplies() {
        // If / became *, risk=2 → risk_norm=4 clamped to 1 still, so use risk=0.5:
        // /2 → 0.25; *2 → 1.0. Composite with zero nouls: 0.125 vs 0.50.
        let j = ApprovalJudgment::compose(0.5, 1.0, 0.0, 0.0);
        assert!((j.composite - 0.125).abs() < 1e-9, "got {}", j.composite);
        assert_eq!(j.band, RiskBand::Low);
    }

    #[test]
    fn uncertain_flags_any_single_signal() {
        // Only low Score confidence.
        let a = ApprovalJudgment::compose(0.0, 0.49, 0.0, 0.0);
        assert!(a.uncertain);
        // Only secrets coin-flip.
        let b = ApprovalJudgment::compose(0.0, 1.0, 0.45, 0.0);
        assert!(b.uncertain);
        // Only destructive coin-flip.
        let c = ApprovalJudgment::compose(0.0, 1.0, 0.0, 0.55);
        assert!(c.uncertain);
        // All clear.
        let d = ApprovalJudgment::compose(0.0, 0.50, 0.39, 0.39);
        assert!(!d.uncertain);
    }

    #[test]
    fn truncate_respects_cap_and_char_boundary() {
        assert_eq!(truncate("hi", 10), "hi");
        assert_eq!(truncate("abcdefghij", 5), "abcde…");
        // Mid-char cut of a multi-byte glyph must back up.
        let s = "あいう";
        let cut = truncate(s, 4); // あ is 3 bytes; end backs to 3
        assert!(cut.ends_with('…'));
        assert!(cut.starts_with('あ'));
        assert!(!cut.contains('\u{fffd}'));
    }

    #[test]
    fn parse_env_skips_blank_and_comment_lines() {
        let text = "\n# TYPESAFE_API_KEY=nope\n\nexport TYPESAFE_API_KEY=ok\n";
        assert_eq!(parse_env_file(text).as_deref(), Some("ok"));
    }

    #[test]
    fn from_answers_reads_score_and_nouls() {
        let answers = json!({
            "risk": { "type": "score", "score": 2.0, "confidence": 1.0 },
            "touches_secrets": { "type": "noul", "noul": 0.97 },
            "destructive": { "type": "noul", "noul": 0.96 },
        });
        let j = ApprovalJudgment::from_answers(&answers).expect("parse");
        assert_eq!(j.band, RiskBand::High);
        assert!((j.risk - 2.0).abs() < 1e-9);
    }

    #[test]
    fn from_answers_errors_on_missing() {
        let answers = json!({ "risk": { "type": "score", "score": 1.0 } });
        assert!(ApprovalJudgment::from_answers(&answers).is_err());
    }

    #[test]
    fn approval_request_has_three_questions() {
        let req = approval_request("Bash", "rm -rf /");
        assert_eq!(req["model"], MODEL);
        assert_eq!(req["state"]["tool"], "Bash");
        let q = &req["questions"];
        assert_eq!(q["risk"]["type"], "score");
        assert_eq!(q["touches_secrets"]["type"], "noul");
        assert_eq!(q["destructive"]["type"], "noul");
        assert_eq!(q["risk"]["criteria"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn response_body_cap_rejects_oversize() {
        // S1-F2: ~256 KiB cap, rejected before/after buffering.
        assert_eq!(MAX_RESPONSE_BYTES, 256 * 1024);
        assert!(response_too_large(MAX_RESPONSE_BYTES).is_none());
        assert!(response_too_large(MAX_RESPONSE_BYTES + 1).is_some());
        assert!(response_too_large(u64::MAX).is_some());
        let err = response_too_large(1_000_000).expect("oversize");
        assert!(err.contains("exceeds"), "unexpected message: {err}");
    }

    #[test]
    fn response_missing_content_length_is_rejected() {
        // S1-GAP-C fallback: without stream, missing CL is refused.
        let err = require_content_length(None).expect_err("missing CL");
        assert!(err.contains("Content-Length"), "unexpected: {err}");
        assert!(err.contains("refusing unbounded"), "unexpected: {err}");
        assert_eq!(require_content_length(Some(1024)).unwrap(), 1024);
        assert_eq!(
            require_content_length(Some(MAX_RESPONSE_BYTES)).unwrap(),
            MAX_RESPONSE_BYTES
        );
    }

    #[test]
    fn live_env_file_parses_when_present() {
        // Integration smoke: the operator's key file must parse. Never
        // assert the secret value.
        let path = dirs_config_typesafe();
        let Some(path) = path else { return };
        let text = std::fs::read_to_string(&path).expect("read env");
        assert!(
            parse_env_file(&text).is_some(),
            "{} must define TYPESAFE_API_KEY",
            path.display()
        );
    }

    fn dirs_config_typesafe() -> Option<PathBuf> {
        typesafe_env_path()
    }

    #[tokio::test]
    async fn live_high_risk_shell_scores_high() {
        // When the operator key file (or env) exists, from_env must succeed.
        // Skipping silently would miss a from_env → None mutant.
        let client = match Client::from_env() {
            Some(c) => c,
            None if typesafe_env_path().is_none()
                && std::env::var_os("TYPESAFE_API_KEY").is_none() =>
            {
                eprintln!("skip: no TYPESAFE_API_KEY");
                return;
            }
            None => panic!("TYPESAFE_API_KEY present but Client::from_env returned None"),
        };
        let j = client
            .judge_approval(
                "Bash",
                "rm -rf ~/.ssh && cat ~/.aws/credentials",
            )
            .await
            .expect("live typesafe call");
        assert_eq!(j.band, RiskBand::High);
        assert!(j.touches_secrets >= 0.8, "secrets={}", j.touches_secrets);
        assert!(j.destructive >= 0.8, "destructive={}", j.destructive);
    }

    #[test]
    fn load_api_key_reads_config_when_env_unset() {
        // Soft check: if the file exists, load_api_key must return Some
        // even when we only rely on the file (env may also be set in CI).
        if typesafe_env_path().is_none() {
            return;
        }
        assert!(
            load_api_key().is_some(),
            "config file exists but load_api_key returned None"
        );
    }
}
