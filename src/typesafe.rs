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

/// S4-F1: the single HTTP client constructor. Redirects are never followed:
/// this client carries a Bearer token, so a 302 (malicious proxy or
/// compromised endpoint) must surface as an error instead of resending
/// `Authorization` elsewhere. TypeSafe's pinned HTTPS endpoint does not
/// redirect in normal operation, so direct scoring is unaffected.
fn build_http_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
}

impl Client {
    pub fn new(api_key: String) -> Result<Self, String> {
        let http =
            build_http_client().map_err(|e| format!("typesafe http client: {e}"))?;
        Ok(Self {
            api_key,
            http,
            url: DEFAULT_URL.to_string(),
        })
    }

    /// Test-only: point the client at a loopback URL. Production stays on
    /// [`DEFAULT_URL`].
    #[cfg(test)]
    fn with_url(api_key: String, url: String) -> Result<Self, String> {
        let http =
            build_http_client().map_err(|e| format!("typesafe http client: {e}"))?;
        Ok(Self {
            api_key,
            http,
            url,
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
        // S4-F2: redact tool and body before either leaves the process.
        // Agent-controlled titles can carry secrets; redact at this boundary
        // so all callers are covered. Scoring is already opt-in
        // (no key → no client → no request).
        let tool = redact_secrets(tool);
        let body = truncate(&redact_secrets(body), BODY_CAP);
        let payload = approval_request(&tool, &body);
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

/// S4-F2: marker replacing suspected secret values before the approval
/// body is POSTed to TypeSafe for scoring.
pub const REDACTED: &str = "[REDACTED]";

/// Key cores matched case-insensitively (non-alphanumerics stripped) on the
/// left of `=`/`:` before the value is redacted. Over-redaction is the safe
/// direction here: the body is scored, never executed.
const SENSITIVE_KEY_CORES: &[&str] = &[
    "apikey", "apisecret", "secret", "token", "password", "passwd", "pwd",
    "bearer", "authorization", "privatekey", "secretkey", "accesskey",
    "credential", "passphrase",
];

/// `(prefix, minimum trailing token chars)` redacted wherever they appear.
/// Public vendor prefixes only — no secret material lives in this table.
/// Deliberate allowlist: bare `gh_` is omitted (too short / noisy).
const SECRET_PREFIXES: &[(&str, usize)] = &[
    ("AKIA", 12),
    ("ASIA", 12),
    ("ghp_", 8),
    ("gho_", 8),
    ("ghu_", 8),
    ("ghs_", 8),
    ("ghr_", 8),
    ("github_pat_", 8),
    ("glpat-", 8),
    ("sk_live_", 8),
    ("sk_test_", 8),
    ("sk-", 8),
    ("npm_", 8),
    ("hf_", 8),
    ("xoxb-", 8),
    ("xoxp-", 8),
    ("xoxa-", 8),
    ("xoxs-", 8),
    ("xoxo-", 8),
    ("AIza", 8),
];

/// Redact common secret patterns: PEM blocks, well-known token prefixes,
/// `Bearer` tokens, and `key=value` / `key: value` pairs with sensitive
/// keys. Idempotent; non-secret prose passes through. Standalone
/// high-entropy strings with no known prefix or key are a known residual
/// (documented, accepted): prefix/key matching is deliberately
/// allowlist-based so ordinary diff text is never eaten.
pub fn redact_secrets(text: &str) -> String {
    redact_key_values(&redact_token_patterns(&redact_pem_blocks(text)))
}

/// Replace `-----BEGIN … ----- … -----END … -----` blocks wholesale,
/// ending right after the `-----END … -----` marker so trailing text on the
/// same line (e.g. a JSON closing quote) survives. A `BEGIN` with no `END`
/// is truncated input: fail closed and drop the tail.
fn redact_pem_blocks(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("-----BEGIN") {
        let body = &rest[start..];
        let Some(end_rel) = body.find("-----END") else {
            out.push_str(&rest[..start]);
            out.push_str(REDACTED);
            return out;
        };
        // First `-----` after `-----END` closes the END marker.
        let after = &body[end_rel + "-----END".len()..];
        let marker_len = after
            .find("-----")
            .map(|i| i + "-----".len())
            .unwrap_or(0);
        let end = end_rel + "-----END".len() + marker_len;
        out.push_str(&rest[..start]);
        out.push_str(REDACTED);
        rest = &body[end..];
    }
    out.push_str(rest);
    out
}

fn is_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'~' | b'+' | b'/' | b'=')
}

fn secret_prefix_at(s: &str) -> Option<(usize, usize)> {
    // ASCII case-insensitive: vendors emit mixed case (`akia…`, `SK-…`).
    SECRET_PREFIXES
        .iter()
        .find(|(p, _)| {
            s.len() >= p.len() && s.as_bytes()[..p.len()].eq_ignore_ascii_case(p.as_bytes())
        })
        .map(|(p, min)| (p.len(), *min))
}

/// `Bearer <token>` at the start of `s` (caller guarantees a word boundary
/// before it). Returns the total byte length on match.
fn bearer_token_at(s: &str) -> Option<usize> {
    if s.len() < 7 || !s[..6].eq_ignore_ascii_case("bearer") {
        return None;
    }
    let b = s.as_bytes();
    let mut j = 6;
    while j < b.len() && (b[j] == b' ' || b[j] == b'\t') {
        j += 1;
    }
    if j == 6 {
        return None; // e.g. "bearerbond", not a credential
    }
    let mut end = j;
    let mut chars = 0;
    while end < b.len() && is_token_byte(b[end]) {
        end += 1;
        chars += 1;
    }
    if chars >= 8 { Some(end) } else { None }
}

fn redact_token_patterns(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < b.len() {
        let rest = &text[i..];
        if let Some((plen, min)) = secret_prefix_at(rest) {
            let mut end = i + plen;
            let mut chars = 0;
            while end < b.len() && is_token_byte(b[end]) && chars < 512 {
                end += 1;
                chars += 1;
            }
            if chars >= min {
                out.push_str(REDACTED);
                i = end;
                continue;
            }
            // Prefix without a token tail (prose like "sk-"): fall through
            // and copy one char.
        }
        let boundary = i == 0 || (!b[i - 1].is_ascii_alphanumeric() && b[i - 1] != b'_');
        if boundary {
            if let Some(total) = bearer_token_at(rest) {
                out.push_str(&text[i..i + 6]);
                out.push(' ');
                out.push_str(REDACTED);
                i += total;
                continue;
            }
        }
        let ch = rest.chars().next().expect("non-empty");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn is_key_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'"' | b'\'')
}

fn key_is_sensitive(raw_key: &str) -> bool {
    let norm: String = raw_key
        .bytes()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| (c.to_ascii_lowercase()) as char)
        .collect();
    !norm.is_empty() && SENSITIVE_KEY_CORES.iter().any(|core| norm.contains(core))
}

fn redact_key_values(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (idx, line) in text.split('\n').enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        redact_kv_line(line, &mut out);
    }
    out
}

/// Redact the value after every `=`/`:` whose key token is sensitive.
/// Quoted values redact inside the quotes; unquoted values run to end of
/// line (spaces do NOT stop the value — multi-word secrets fail closed),
/// stopping early only at `,;&|` and unmatched `)]}`, with bracket depth
/// tracked so `[..]`/`(..)`/`{..}` groups (including earlier `[REDACTED]`
/// markers) survive intact. `://` URL schemes are never separators.
fn redact_kv_line(line: &str, out: &mut String) {
    let b = line.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'=' || c == b':' {
            if c == b':' && b.get(i + 1) == Some(&b'/') && b.get(i + 2) == Some(&b'/') {
                out.push(':');
                i += 1;
                continue;
            }
            // Key token immediately before the separator (`key => v` skips
            // the `>` and surrounding spaces).
            let mut k = i;
            while k > 0 && (b[k - 1] == b' ' || b[k - 1] == b'\t') {
                k -= 1;
            }
            if k > 0 && b[k - 1] == b'>' {
                k -= 1;
                while k > 0 && (b[k - 1] == b' ' || b[k - 1] == b'\t') {
                    k -= 1;
                }
            }
            let key_end = k;
            while k > 0 && is_key_byte(b[k - 1]) {
                k -= 1;
            }
            if key_is_sensitive(&line[k..key_end]) {
                out.push(c as char);
                let mut j = i + 1;
                while j < b.len() && (b[j] == b' ' || b[j] == b'\t') {
                    j += 1;
                }
                out.push_str(&line[i + 1..j]);
                if j < b.len() && (b[j] == b'"' || b[j] == b'\'') {
                    let q = b[j] as char;
                    let mut e = j + 1;
                    while e < b.len() && b[e] != b[j] {
                        e += 1;
                    }
                    out.push(q);
                    out.push_str(REDACTED);
                    if e < b.len() {
                        out.push(q);
                        i = e + 1;
                    } else {
                        i = e;
                    }
                } else {
                    let mut e = j;
                    let mut depth = 0;
                    while e < b.len() {
                        match b[e] {
                            b'[' | b'(' | b'{' => depth += 1,
                            b']' | b')' | b'}' if depth > 0 => depth -= 1,
                            b'\r' | b',' | b';' | b'&' | b'|' if depth == 0 => break,
                            b']' | b')' | b'}' => break,
                            _ => {}
                        }
                        e += 1;
                    }
                    if e == j {
                        i = j; // empty value: nothing to redact
                    } else {
                        out.push_str(REDACTED);
                        i = e;
                    }
                }
                continue;
            }
            out.push(c as char);
            i += 1;
        } else {
            let ch = line[i..].chars().next().expect("non-empty");
            out.push(ch);
            i += ch.len_utf8();
        }
    }
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

    /// Read a single HTTP/1.x header block from a test connection.
    #[cfg(test)]
    fn read_http_headers(conn: &mut std::net::TcpStream) -> String {
        use std::io::Read;
        conn.set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .ok();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        while buf.len() < 16_384
            && !buf.windows(4).any(|w| w == b"\r\n\r\n")
        {
            match conn.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(_) => break,
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    #[tokio::test]
    async fn redirect_policy_does_not_follow_or_forward_authorization() {
        // S4-F1: hop-1 302s to a distinct hop-2 host:port. Client must surface
        // 302 (Policy::none), keep Authorization on the first hop only, and
        // never connect to hop-2 (no auth forward).
        let hop1 = std::net::TcpListener::bind("127.0.0.1:0").expect("bind hop1");
        let hop1_port = hop1.local_addr().expect("addr").port();
        let hop2 = std::net::TcpListener::bind("127.0.0.1:0").expect("bind hop2");
        let hop2_port = hop2.local_addr().expect("addr").port();
        assert_ne!(hop1_port, hop2_port, "need distinct targets");
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let handle = std::thread::spawn(move || {
            let (mut conn, _) = hop1.accept().expect("accept redirect");
            let first = read_http_headers(&mut conn);
            let has_auth = first.lines().any(|l| {
                l.eq_ignore_ascii_case("authorization: Bearer super-secret-token")
                    || l.to_ascii_lowercase()
                        == "authorization: bearer super-secret-token"
            });
            if !has_auth {
                let _ = tx.send("missing-auth-on-first-hop".to_string());
                return;
            }
            let resp = format!(
                "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{hop2_port}/landed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            {
                use std::io::Write;
                conn.write_all(resp.as_bytes()).expect("write 302");
            }
            drop(conn);
            // Distinct landing target must receive zero connections.
            hop2.set_nonblocking(true).expect("nonblocking");
            let start = std::time::Instant::now();
            while start.elapsed() < std::time::Duration::from_millis(800) {
                match hop2.accept() {
                    Ok((mut c, _)) => {
                        let req = read_http_headers(&mut c);
                        let leaked = req.lines().any(|l| {
                            l.to_ascii_lowercase().starts_with("authorization:")
                        });
                        let _ = tx.send(if leaked {
                            "followed-with-auth".to_string()
                        } else {
                            "followed".to_string()
                        });
                        return;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                    Err(e) => {
                        let _ = tx.send(format!("accept-error:{e}"));
                        return;
                    }
                }
            }
            let _ = tx.send("not-followed".to_string());
        });

        let http = build_http_client().expect("build client");
        let url = format!("http://127.0.0.1:{hop1_port}/score");
        let status = http
            .get(&url)
            .bearer_auth("super-secret-token")
            .send()
            .await
            .expect("send")
            .status();
        assert_eq!(
            status.as_u16(),
            302,
            "client must surface the redirect, not follow it"
        );
        let outcome = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("server verdict");
        handle.join().expect("join server");
        assert_eq!(
            outcome, "not-followed",
            "client followed the redirect (Authorization at risk)"
        );
    }

    #[test]
    fn redact_secrets_redacts_common_patterns() {
        // S4-F2: env-style, JSON-style, well-known token prefixes, Bearer,
        // and PEM blocks must not survive; unrelated text must.
        let input = concat!(
            "export TYPESAFE_API_KEY=\"sk-live-topsecret123456\"\n",
            "stripe_live=sk_live_51AbCdEfGhIjKlMnOpQr\n",
            "stripe_test=sk_test_51AbCdEfGhIjKlMnOpQr\n",
            "password: hunter2-hunter2\n",
            "aws_access_key_id=AKIAIOSFODNN7EXAMPLE\n",
            "aws_sts=ASIAIOSFODNN7EXAMPLE\n",
            "token ghp_abcdefghijklmnop123456\n",
            "gitlab glpat-abcdefghijklmnop123456\n",
            "registry npm_abcdefghijklmnop123456\n",
            "huggingface hf_abcdefghijklmnop123456\n",
            "Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.payload.sig\n",
            "{\"private_key\": \"-----BEGIN RSA PRIVATE KEY-----\\nMIIBOg==\\n-----END RSA PRIVATE KEY-----\"}\n",
            "safe line: read src/main.rs and run cargo test\n",
        );
        let out = redact_secrets(input);
        for secret in [
            "sk-live-topsecret123456",
            "sk_live_51AbCdEfGhIjKlMnOpQr",
            "sk_test_51AbCdEfGhIjKlMnOpQr",
            "hunter2-hunter2",
            "AKIAIOSFODNN7EXAMPLE",
            "ASIAIOSFODNN7EXAMPLE",
            "ghp_abcdefghijklmnop123456",
            "glpat-abcdefghijklmnop123456",
            "npm_abcdefghijklmnop123456",
            "hf_abcdefghijklmnop123456",
            "eyJhbGciOiJIUzI1NiJ9.payload.sig",
            "MIIBOg==",
        ] {
            assert!(!out.contains(secret), "leaked {secret}: {out}");
        }
        assert!(out.contains(REDACTED), "no redaction marker: {out}");
        assert!(
            out.contains("read src/main.rs"),
            "non-secret text lost: {out}"
        );
    }

    #[test]
    fn redact_secrets_prefixes_are_case_insensitive() {
        // S4-GAP-4: vendor prefixes match regardless of ASCII case.
        let out = redact_secrets(
            "keys akiaIOSFODNN7EXAMPLE and SK-abcdefghijklmnop live nearby",
        );
        assert!(!out.contains("akiaIOSFODNN7EXAMPLE"), "leaked: {out}");
        assert!(!out.contains("SK-abcdefghijklmnop"), "leaked: {out}");
        assert!(out.contains(REDACTED), "no marker: {out}");
        assert!(out.contains("live nearby"), "prose lost: {out}");
    }

    #[test]
    fn redact_secrets_keeps_inline_prose_but_kills_values() {
        // S4-F2: `key=value` buried mid-line still redacts the value while
        // a high-entropy token elsewhere on the same line is also caught.
        let raw = "run tests then cat ~/.aws/credentials with AKIAIOSFODNN7EXAMPLE and password=hunter2-hunter2";
        let out = redact_secrets(raw);
        assert!(!out.contains("AKIAIOSFODNN7EXAMPLE"), "leaked: {out}");
        assert!(!out.contains("hunter2-hunter2"), "leaked: {out}");
        assert!(out.contains("run tests"), "prose lost: {out}");
    }

    #[test]
    fn approval_payload_uses_redacted_body() {
        // S4-F2 / S4-GAP-1: server-side shape carries redacted tool + body;
        // scoring questions stay intact.
        let raw_tool = "Bash AKIAIOSFODNN7EXAMPLE";
        let raw_body = "rm -rf /tmp/x with AKIAIOSFODNN7EXAMPLE and password=hunter2-hunter2";
        let tool = redact_secrets(raw_tool);
        let body = redact_secrets(raw_body);
        let req = approval_request(&tool, &body);
        let tool_s = req["state"]["tool"].as_str().expect("tool");
        let body_s = req["state"]["body"].as_str().expect("body");
        assert!(!tool_s.contains("AKIAIOSFODNN7EXAMPLE"), "leaked tool: {tool_s}");
        assert!(!body_s.contains("AKIAIOSFODNN7EXAMPLE"), "leaked: {body_s}");
        assert!(!body_s.contains("hunter2-hunter2"), "leaked: {body_s}");
        assert!(tool_s.contains(REDACTED), "no tool marker: {tool_s}");
        assert!(body_s.contains(REDACTED), "no marker: {body_s}");
        assert_eq!(req["questions"]["risk"]["type"], "score");
        assert_eq!(req["questions"]["touches_secrets"]["type"], "noul");
        assert_eq!(req["questions"]["destructive"]["type"], "noul");
    }

    #[tokio::test]
    async fn judge_approval_posts_redacted_body_on_wire() {
        // S4-GAP-2: drop redact inside judge_approval and this must fail —
        // assert the HTTP JSON body, not just a local approval_request helper.
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let handle = std::thread::spawn(move || {
            let (mut conn, _) = listener.accept().expect("accept");
            conn.set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .ok();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 2048];
            // Read until Content-Length body is complete (or timeout).
            let mut content_len = None;
            loop {
                match conn.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    Err(_) => break,
                }
                if content_len.is_none() {
                    if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&buf[..end]);
                        content_len = headers.lines().find_map(|l| {
                            let lower = l.to_ascii_lowercase();
                            lower
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                        });
                    }
                }
                if let Some(cl) = content_len {
                    if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        if buf.len() >= end + 4 + cl {
                            break;
                        }
                    }
                }
                if buf.len() > 64 * 1024 {
                    break;
                }
            }
            let raw = String::from_utf8_lossy(&buf).into_owned();
            let body_json = raw
                .split("\r\n\r\n")
                .nth(1)
                .unwrap_or("")
                .to_string();
            let answers = concat!(
                r#"{"answers":{"risk":{"score":0.2,"confidence":0.9},"#,
                r#""touches_secrets":{"noul":0.1},"destructive":{"noul":0.1}}}"#,
            );
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                answers.len(),
                answers
            );
            let _ = conn.write_all(resp.as_bytes());
            let _ = tx.send(body_json);
        });

        let client = Client::with_url(
            "test-key".into(),
            format!("http://127.0.0.1:{port}/score"),
        )
        .expect("client");
        let tool = "Edit AKIAIOSFODNN7EXAMPLE";
        let body = "patch with sk_live_51AbCdEfGhIjKlMnOp and password=hunter2-hunter2";
        let _ = client
            .judge_approval(tool, body)
            .await
            .expect("judge");
        let posted = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("posted body");
        handle.join().expect("join");
        for secret in [
            "AKIAIOSFODNN7EXAMPLE",
            "sk_live_51AbCdEfGhIjKlMnOp",
            "hunter2-hunter2",
        ] {
            assert!(!posted.contains(secret), "wire leaked {secret}: {posted}");
        }
        let v: serde_json::Value = serde_json::from_str(&posted).expect("json");
        let tool_s = v["state"]["tool"].as_str().expect("tool");
        let body_s = v["state"]["body"].as_str().expect("body");
        assert!(tool_s.contains(REDACTED), "tool not redacted: {tool_s}");
        assert!(body_s.contains(REDACTED), "body not redacted: {body_s}");
        assert_eq!(v["questions"]["risk"]["type"], "score");
        assert_eq!(v["questions"]["touches_secrets"]["type"], "noul");
        assert_eq!(v["questions"]["destructive"]["type"], "noul");
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
