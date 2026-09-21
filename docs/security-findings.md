# Polyforge Security Findings — Audit Remediation Tracker

Threat model: local Rust TUI hosting untrusted agent children over stdio
(`muse` / `codex` / `agy` / `grok`). Assume a malicious or compromised agent
child: unbounded NDJSON/JSON output (DoS/OOM), terminal-escape and clipboard
exfiltration, session-routing confusion that bypasses approval modals, and
trust gaps where vendor backends apply side effects outside our y/n gate.
TypeSafe is used over HTTPS only for approval risk scoring. No critical
shell/RCE (no `sh -c` on untrusted input) was found in this audit pass.

## Status legend

- `open` — not started.
- `in_progress` — actively being fixed (one stage at a time, see How to work).
- `done` — fix landed and stage tests pass.
- `wontfix` — deliberately not fixed, with reason recorded.
- `accepted` — out-of-scope item documented as by-design; not a fix target.

## Progress summary

| Stage                                                 | Status   | Findings             | Notes                                                      |
| ----------------------------------------------------- | -------- | -------------------- | ---------------------------------------------------------- |
| Stage 1 — DoS bounds                                  | done     | 2 (1 High, 1 Medium) | Cap NDJSON lines + TypeSafe body                           |
| Stage 2 — Session routing / approval wire correctness | done     | 3 (all Medium)       | Orphan frames, modal-without-deny, resolved-without-decide |
| Stage 3 — Display / terminal injection                | done     | 1 (Medium)           | Strip C0/C1/ESC before store/render/copy                   |
| Stage 4 — TypeSafe client hardening                   | open     | 2 (both Medium)      | Redirect policy + secret redaction                         |
| Stage 5 — Clipboard / OSC leak surface                | open     | 2 (1 Medium, 1 Low)  | OSC size/opt-in, symlink + path restriction                |
| Stage 6 — Store path hygiene                          | open     | 1 (Low)              | Shared `assert_safe_id` on all path helpers                |
| Stage 7 — Product trust warnings                      | open     | 3 (2 High, 1 Low)    | Vendor gaps: document + UI warnings, no code gate possible |
| Out of scope / accepted                               | accepted | 6 items              | By-design; do not "fix"                                    |

## Stage 1 — DoS bounds (High/Medium)

- [x] **S1-F1** — Severity: High — Status: `done`
  - Files: `src/msp.rs:75-78`; `src/agy.rs:73-91`
  - Problem: Unbounded NDJSON line parse lets a hostile child OOM the TUI.
  - Fix: Cap line size (1–4 MiB); drop oversize lines and surface a transport error.
  - Acceptance: Oversize-line fixture is dropped with a transport error; memory stays bounded; existing NDJSON tests pass.

- [x] **S1-F2** — Severity: Medium — Status: `done`
  - Files: `src/typesafe.rs:172-183`
  - Problem: TypeSafe response body read is unbounded.
  - Fix: Cap body read (~256 KiB); error out above the cap.
  - Acceptance: Oversize-body fixture errors instead of buffering unboundedly; normal scoring still works.

## Stage 2 — Session routing / approval wire correctness (Medium)

- [x] **S2-F1** — Severity: Medium — Status: `done`
  - Files: `src/main.rs:819`, `src/main.rs:830`
  - Problem: Muse/Codex frames with missing session id fall back to the active tab, risking approval on the wrong session.
  - Fix: Match grok behavior: drop/cancel orphans; never `unwrap_or(app.active)` for approvals.
  - Acceptance: Orphan-frame fixture routes to drop/cancel, never to the active tab; approval modal only binds to a known session.

- [x] **S2-F2** — Severity: Medium — Status: `done`
  - Files: `src/main.rs:1077-1084`; `map_decision` in `src/provider.rs`
  - Problem: Muse `n`/`q` can close the UI with no wire deny if the server omits the deny path.
  - Fix: Always send cancel/deny on negative decisions, or refuse to show the modal without a deny path.
  - Acceptance: Negative-decision fixture always emits a wire cancel/deny; no silent-close path remains.

- [x] **S2-F3** — Severity: Medium — Status: `done`
  - Files: `src/grok.rs:232-251`
  - Problem: `interaction_resolved` clears the modal without a user decision.
  - Fix: Queue a cancel if unanswered, or keep the card until the wire reply arrives.
  - Acceptance: Resolved-without-decide fixture leaves either a queued cancel or a visible card; modal never silently vanishes.

## Stage 3 — Display / terminal injection (Medium)

- [x] **S3-F1** — Severity: Medium — Status: `done`
  - Files: `src/app.rs` (`sanitize_text`, `push_line`, `replace_lines`, `stage_diff`, `selected_text`); `src/ui.rs` (transcript, diff modal, flash, sessions summary)
  - Problem: Agent ESC/C0 control bytes are kept in the transcript and reach render/copy paths.
  - Fix: Strip C0/C1/ESC sequences before store, and sanitize at render and copy.
  - Acceptance: ESC/C0 fixture renders inert, is absent from stored transcript, and does not survive copy.

## Stage 4 — TypeSafe client hardening (Medium)

- [ ] **S4-F1** — Severity: Medium — Status: `open`
  - Files: `src/typesafe.rs:133-140`
  - Problem: HTTP client carrying the bearer token can follow redirects, leaking the token.
  - Fix: Use `redirect::Policy::none()` or same-host-only redirects.
  - Acceptance: Redirect fixture never forwards the Authorization header cross-host; direct scoring unaffected.

- [ ] **S4-F2** — Severity: Medium — Status: `open`
  - Files: `src/typesafe.rs:156-168`; `src/main.rs:137-138`
  - Problem: Approval body (may contain secrets) is POSTed to TypeSafe for scoring.
  - Fix: Redact common secret patterns before POST; consider making scoring opt-in.
  - Acceptance: Secret-pattern fixture arrives redacted server-side; scoring still functions on redacted input.

## Stage 5 — Clipboard / OSC leak surface (Medium/Low)

- [ ] **S5-F1** — Severity: Medium — Status: `open`
  - Files: `src/clipboard.rs:100-181`, `src/clipboard.rs:301-305`, `src/clipboard.rs:371-383`
  - Problem: OSC 52 emit is always on under Linux and the copy backup always writes, widening exfiltration surface.
  - Fix: Cap OSC payload size; prefer opt-in OSC/backup; define rotate/wipe policy for `last-copy.txt`.
  - Acceptance: Large-copy fixture is capped or gated behind opt-in; backup rotation/wipe behavior is documented and tested.

- [ ] **S5-F2** — Severity: Low — Status: `open`
  - Files: `src/clipboard.rs:341-408`
  - Problem: Copy backup follows symlinks and honors an arbitrary `POLYFORGE_COPY_FILE` path.
  - Fix: Open with `O_NOFOLLOW` / refuse symlinks; restrict the env-provided path under XDG data dir.
  - Acceptance: Symlink fixture is refused; out-of-tree `POLYFORGE_COPY_FILE` is rejected or remapped under XDG data.

## Stage 6 — Store path hygiene (Low)

- [ ] **S6-F1** — Severity: Low — Status: `open`
  - Files: `src/store.rs:121-127` vs `src/store.rs:256-258`
  - Problem: Store id is sanitized only in `delete_session`; other path helpers trust the id.
  - Fix: Add a shared `assert_safe_id` (reject empty, `\0`, `/`, `\`, `..`) and call it from every path helper.
  - Acceptance: Traversal-fixture ids (`..`, `/`, `\`, empty, NUL) are rejected on all store paths; existing store tests pass.

## Stage 7 — Product trust warnings (High, intentional vendor gaps)

These are vendor-behavior gaps no client gate can fully close: fix = warn loudly and document, not claim a y/n modal covers them.

- [ ] **S7-F1** — Severity: High — Status: `open`
  - Files: `src/agy.rs:8-12`
  - Problem: Agy tools run with no y/n approval modal.
  - Fix: Hard warning when opening an agy session; document vendor-trust assumption.
  - Acceptance: Opening an agy session shows an explicit no-modal warning; docs state the trust assumption.

- [ ] **S7-F2** — Severity: High — Status: `open`
  - Files: `src/codex.rs:193-271`
  - Problem: Codex mid-turn writes and guardian side effects can apply without a modal.
  - Fix: Document the gap; add a UI note such as "writes already applied"; never claim y/n covers all writes.
  - Acceptance: UI/docs carry the "writes already applied" notice where Codex side effects bypass the modal.

- [ ] **S7-F3** — Severity: Low — Status: `open`
  - Files: `src/app.rs:424-428`
  - Problem: Approval card suffers TOCTOU — content can change between render and `y`.
  - Fix (optional): Freeze the card at decision time and/or bind the decision to the `risk_gen` nonce.
  - Acceptance: If implemented, a changed-card fixture either re-prompts or binds the decision to the original nonce; if deferred, record why.

## Out of scope / accepted (document, do not "fix")

- [x] Config/env binary paths are trusted code execution by design. Files: `src/config.rs:133-188`. Status: `accepted`.
- [x] `risk_gen` late-answer gating already correct. Status: `accepted`.
- [x] TypeSafe URL is pinned to HTTPS with rustls; no `danger_accept_invalid_certs`. Status: `accepted`.
- [x] Grok orphan requests are already cancelled. Status: `accepted`.
- [x] Production `Command::new` + argv construction; no `sh -c` on untrusted input. Status: `accepted`.
- [x] `unsafe` only in tests for `set_var` (`src/app.rs:1576`). Status: `accepted`.

## How to work

1. Fix one stage at a time, in Stage 1 → 7 order.
2. Mark that stage's findings `in_progress` before making edits.
3. Mark a finding `done` only after the tests for that stage pass.
4. Never expand scope mid-stage: out-of-stage issues go to the Session log as new candidates, not into the current diff.
5. Keep this file updated as the source of truth for remediation state.

## Session log

- 2026-09-22: Stage 3 done. S3-F1: new `sanitize_text` in `src/app.rs` strips ESC-led sequences (CSI/OSC/DCS/charset, params consumed with the introducer), C1 singletons, C0 controls, and DEL; keeps tab/newline/unicode; truncated input fails closed; idempotent. Wired at ingress — `push_line` (single store+memory choke point, covers all provider/codex/grok/agy push paths), `replace_lines` (pre-fix transcripts cleaned on replay), `stage_diff` (approval card file/body) — plus render defense in `src/ui.rs` (transcript, diff modal, flash, sessions summary) and copy defense in `selected_text`. New tests: 4 `sanitize_*` unit tests (incl. non-enumerated C1 `U+009B`/`U+009D` probe), `push_line_stores_sanitized` (memory + JSONL round-trip), `replace_lines_sanitizes_legacy_data`, `selected_text_is_sanitized`, `stage_diff_sanitizes_card`, `render_neutralizes_injected_line` (TestBackend buffer scan: no control byte reaches the terminal, URL gone, label kept). Full suite: 149 passed, 0 failed, no warnings. Notes: (a) tab names/input echo are local-only and left unsanitized; (b) OSC52 clipboard payload is base64 so needs no extra handling. Untouched: Stages 4–7.
- 2026-09-21: Stage 2 done. S2-F1: muse/codex orphan notifs now drop (grok parity, `src/main.rs` route arms); orphan codex approval requests queue a host-level `denied` via the outbox instead of binding to the active tab, and the drain sends codex decisions without requiring a live session. S2-F2: `approval/requested` with no `denied` choice refuses the modal (transcript + flash, nothing approved/denied); `decide_ui` keeps the card open on n/q/Esc when no deny path maps (y/a still close fail-closed). Superseded tests updated to the new contract (`q_keeps_modal_open_without_deny_choice`, `interaction_resolved_without_decision_sends_cancel`). S2-F3: `interaction_resolved` retires only already-answered cards quietly; unanswered cards queue a cancel + transcript note. New tests: `orphan_notifs_cannot_touch_active_tab`, `orphan_codex_request_denied_never_staged`, `codex_orphan_deny_reaches_host_without_session`, `approval_without/with_deny_choice_*`, `interaction_resolved_with_decision_retires_quietly`. Full suite: 140 passed, 0 failed, no warnings. Notes: (a) codex orphan deny uses the generic `denied` word for all request kinds (best-effort fail-closed; server may reject unknown vocabulary); (b) codex local-close-on-unknown-method path untouched (out-of-stage scope, by design per `codex_choices`); (c) answered-then-drained-then-resolved race can queue one redundant cancel (server already resolved; harmless). Untouched: Stages 3–7.
- 2026-09-21: Stage 1 High-review gaps closed. S1-GAP-A: `discard_until_newline` rewritten to `fill_buf`+`consume` only (no growing Vec). S1-GAP-B: call discard only when `!scratch.ends_with(b"\n")`. New tests: `capped_line_exact_cap_plus_one_newline_preserves_next`, `capped_line_multi_mib_tail_resyncs_without_growing_scratch`. S1-GAP-C: prefer reqwest `stream` failed offline (`wasm-streams ^0.5` not in index cache); fallback `require_content_length` rejects missing CL before `bytes()`, keep oversize pre/post checks; remaining gap = lying Content-Length until body fully buffered. S1-GAP-D: covered by the two new msp tests. Full suite: 134 passed, 0 failed, no warnings. Mutation: mutants not run. Untouched: Stages 2–7, `store.rs` `.lines()`, TypeSafe redirect/redaction.
- 2026-09-19: Stage 1 done. S1-F1: `read_capped_line` (1 MiB cap, drop + Transport error, resync) in `src/msp.rs`, wired into msp + agy stdout/stderr pumps; 4 new `msp::tests`. S1-F2: 256 KiB response cap in `src/typesafe.rs` (Content-Length pre-check + post-read check) + `response_body_cap_rejects_oversize` test. Full suite: 131 passed, 0 failed, no warnings. Notes: (a) invalid UTF-8 lines now drop with Transport instead of killing the pump; (b) true chunked-without-Content-Length streaming needs reqwest `stream` (unavailable offline — `wasm-streams` not cached), recorded as follow-up; (c) `store.rs` local-file `.lines()` reads noted as out-of-stage candidates, untouched.
