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
| Stage 4 — TypeSafe client hardening                   | done     | 2 (both Medium)      | Redirect policy + secret redaction                         |
| Stage 5 — Clipboard / OSC leak surface                | done     | 2 (1 Medium, 1 Low)  | OSC size/opt-in, symlink + path restriction                |
| Stage 6 — Store path hygiene                          | done     | 1 (Low)              | Shared `assert_safe_id` on all path helpers                |
| Stage 7 — Product trust warnings                      | done     | 3 (2 High, 1 Low)    | Vendor gaps: document + UI warnings, no code gate possible |
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

- [x] **S4-F1** — Severity: Medium — Status: `done`
  - Files: `src/typesafe.rs` (`build_http_client`, redirect tests)
  - Problem: HTTP client carrying the bearer token can follow redirects, leaking the token.
  - Fix: Use `redirect::Policy::none()` or same-host-only redirects.
  - Acceptance: Redirect fixture never forwards the Authorization header cross-host; direct scoring unaffected.

- [x] **S4-F2** — Severity: Medium — Status: `done`
  - Files: `src/typesafe.rs` (`judge_approval`, `redact_secrets`); `src/main.rs` (passes tool/body)
  - Problem: Approval tool/body (may contain secrets) is POSTed to TypeSafe for scoring.
  - Fix: Redact common secret patterns on tool and body before POST; scoring stays opt-in.
  - Acceptance: Secret-pattern fixture arrives redacted server-side; scoring still functions on redacted input.

## Stage 5 — Clipboard / OSC leak surface (Medium/Low)

- [x] **S5-F1** — Severity: Medium — Status: `done`
  - Files: `src/clipboard.rs:100-181`, `src/clipboard.rs:301-305`, `src/clipboard.rs:371-383`
  - Problem: OSC 52 emit is always on under Linux and the copy backup always writes, widening exfiltration surface.
  - Fix: Cap OSC payload size; prefer opt-in OSC/backup; define rotate/wipe policy for `last-copy.txt`.
  - Acceptance: Large-copy fixture is capped or gated behind opt-in; backup rotation/wipe behavior is documented and tested.

- [x] **S5-F2** — Severity: Low — Status: `done`
  - Files: `src/clipboard.rs:341-408`
  - Problem: Copy backup follows symlinks and honors an arbitrary `POLYFORGE_COPY_FILE` path.
  - Fix: Open with `O_NOFOLLOW` / refuse symlinks; restrict the env-provided path under XDG data dir.
  - Acceptance: Symlink fixture is refused; out-of-tree `POLYFORGE_COPY_FILE` is rejected or remapped under XDG data.

## Stage 6 — Store path hygiene (Low)

- [x] **S6-F1** — Severity: Low — Status: `done`
  - Files: `src/store.rs` (`assert_safe_id`, `transcript_path`, `meta_path`, `load_transcript`, `first_line`, `rewrite`, `open_sink`, `load_meta`, `save_meta`, `delete_session`)
  - Problem: Store id is sanitized only in `delete_session`; other path helpers trust the id.
  - Fix: Add a shared `assert_safe_id` (reject empty, `\0`, `/`, `\`, `..`) and call it from every path helper.
  - Acceptance: Traversal-fixture ids (`..`, `/`, `\`, empty, NUL) are rejected on all store paths; existing store tests pass.

## Stage 7 — Product trust warnings (High, intentional vendor gaps)

These are vendor-behavior gaps no client gate can fully close: fix = warn loudly and document, not claim a y/n modal covers them.

- [x] **S7-F1** — Severity: High — Status: `done`
  - Files: `src/agy.rs` (`AGY_NO_MODAL_WARNING`, `push_agy_open_notice`); `src/main.rs` (`open_tab_session` Agy arm); `docs/USER-MANUAL.md` (§6 agy row)
  - Problem: Agy tools run with no y/n approval modal.
  - Fix: Hard warning when opening an agy session; document vendor-trust assumption.
  - Acceptance: Opening an agy session shows an explicit no-modal warning; docs state the trust assumption.

- [x] **S7-F2** — Severity: High — Status: `done`
  - Files: `src/codex.rs` (`patchUpdated` arm, `approvalReview/completed` arm); `docs/USER-MANUAL.md` (§6 codex row)
  - Problem: Codex mid-turn writes and guardian side effects can apply without a modal.
  - Fix: Document the gap; add a UI note such as "writes already applied"; never claim y/n covers all writes.
  - Acceptance: UI/docs carry the "writes already applied" notice where Codex side effects bypass the modal.

- [x] **S7-F3** — Severity: Low — Status: `done`
  - Files: `src/app.rs` (`Session::stage_diff`)
  - Problem: Approval card suffers TOCTOU — content can change between render and `y`.
  - Fix (optional): Freeze the card at decision time and/or bind the decision to the `risk_gen` nonce.
  - Acceptance: If implemented, a changed-card fixture either re-prompts or binds the decision to the original nonce; if deferred, record why.

## Out of scope / accepted (document, do not "fix")

- [x] Config/env binary paths are trusted code execution by design. Files: `src/config.rs:133-188`. Status: `accepted`.
- [x] `risk_gen` late-answer gating already correct. Status: `accepted`.
- [x] TypeSafe URL is pinned to HTTPS with rustls; no `danger_accept_invalid_certs`. Status: `accepted`.
- [x] Grok orphan requests are already cancelled. Status: `accepted`.
- [x] Production `Command::new` + argv construction; no `sh -c` on untrusted input. Status: `accepted`.
- [x] `unsafe` is the Stage 5 clipboard `openat`/`mkdirat`/`fchmod` walk (`src/clipboard.rs`) plus test-only `set_var` (`src/app.rs`). Status: `accepted`.

## How to work

1. Fix one stage at a time, in Stage 1 → 7 order.
2. Mark that stage's findings `in_progress` before making edits.
3. Mark a finding `done` only after the tests for that stage pass.
4. Never expand scope mid-stage: out-of-stage issues go to the Session log as new candidates, not into the current diff.
5. Keep this file updated as the source of truth for remediation state.

## Session log

- 2026-09-23: Stage 7 re-review. S7-F1, S7-F2, and S7-F3 stay `done`. No in-stage gap, so no fix agent. Read-only Grok 4.7 high pass: `IN_STAGE_CODE_FIX: no`. Parent agreed. Session-log only, not fixed: S7-CAND-1 `stage_diff` (`src/app.rs:551`) re-prompts with `push_line` only and does not set `flash`; `push_line` (`src/app.rs:461`) does not move `scroll`, so the line can sit under the DIFF modal (`src/ui.rs:32`) while flash stays the caller's wants-approval text (`src/provider.rs:239`, `src/codex.rs:370`, `src/grok.rs:545`). Written acceptance is that transcript re-prompt, locked by `stage_replacing_open_card_reprompts`. S7-CAND-2 `open_notice_warns_no_modal_fresh_and_resumed` (`src/agy.rs:338`) calls `push_agy_open_notice` directly, so deleting the open call and flash set at `src/main.rs:472` still passes. `open_tab_session` is async and spawns. No pure seam wraps that `Ok` path. S7-CAND-3 no in-tree schema names a guardian status besides `"approved"` (`src/codex.rs:269`). S7-CAND-4 `README.md:101` and `README.md:102` omit the manual's "already applied" and open-warning sentences. Tracker file list is `docs/USER-MANUAL.md` §6 (`:194`, `:195`). S7-CAND-5 no `features/*.feature` covers Stage 7. No Gherkin runner in `Cargo.toml`. Unit tests already lock the warning strings. S7-CAND-6 `outputDelta` (`src/codex.rs:229`) stays silent. The already-applied line is only on `patchUpdated` (`src/codex.rs:189`). A write that arrives only as `outputDelta` is unconfirmed. S7-CAND-7 `item/completed` (`src/codex.rs:133`) can `push_text` nested text with no "already applied" phrase. No in-tree schema says that frame is the write. Mutation: cargo-mutants not run. No suite run this pass (no code change).

- 2026-09-23: Stage 7 done. S7-F1: new `AGY_NO_MODAL_WARNING` + `push_agy_open_notice` in `src/agy.rs`, wired into `open_tab_session` Agy arm in `src/main.rs` — every successful open (fresh or resumed) prints the no-modal warning to the transcript and sets the flash; failed opens warn about nothing (no live agent). The old pre-spawn "continuing previous conversation" line moved into the helper so the resume context still prints, now only on success. New test `open_notice_warns_no_modal_fresh_and_resumed`. S7-F2: `patchUpdated` lines now read `codex: {mark} {path} (already applied)` (empty case: `files updated (already applied — not gated by y/n)`); approved guardian reviews append `[already applied — no y/n modal]`; other guardian statuses unchanged. Existing codex tests updated to the new contract. S7-F3: `stage_diff` now pushes "approval card updated … review again before y" when replacing an undecided card (the re-prompt option); first stages stay silent. Rationale for no nonce-freeze: render→keypress is race-free (single-threaded event loop — the key always answers the last rendered card); the only intra-client TOCTOU is card replacement, now re-prompted; server-side content changes past the request stay outside any client gate by design. New test `stage_replacing_open_card_reprompts`. Docs: §6 codex/agy rows in `docs/USER-MANUAL.md` carry the trust wording. Full suite: 169 passed, 0 failed, no warnings (167 pre-existing + 2 new; the 2 loopback typesafe tests need unsandboxed bind, pass escalated). Mutation: cargo-mutants not run. All 7 stages done.

- 2026-09-23: Stage 6 re-review. S6-F1 stays `done`. No in-stage gap, so no fix agent. Read-only Grok 4.7 high pass: no id that passes `assert_safe_id` escapes the store dir. `debug_assert` on private `transcript_path` / `meta_path` is not a release bypass; every public helper returns before the join. `rewrite` already returns `InvalidInput` before `File::create`. Session-log only, not fixed: S6-CAND-1 `choose_session` (`src/app.rs:947`) still copies listing `remote_id` / `title` / `backend` and sets `store_id` when `load_meta` returns None for planted id `..` (`src/store.rs:585`); `open_sink`, `load_transcript`, and `save_meta` still refuse. S6-CAND-2 in-store symlink follow on open/create/write/remove (`src/store.rs:143`, `170`, `183`, `198`, `211`, `222`, `242`, `284`) is outside the S6-F1 id contract (`..`, `/`, `\`, empty, NUL). Untouched: Stage 7.

- 2026-09-22: Stage 6 done. S6-F1: new shared `assert_safe_id` in `src/store.rs` rejects empty, NUL, `/`, `\`, and any `..` substring (minted `{millis}-{pid}-{seq}` ids never contain dots). Called from every path helper: `load_transcript` (empty vec), `first_line`/`load_meta`/`open_sink` (None), `save_meta` (no-op), `rewrite` (InvalidInput), `delete_session` (false, replacing its weaker inline check that missed `\` and `..`); `transcript_path`/`meta_path` carry `debug_assert`. New tests: `assert_safe_id_allows_minted_ids_and_rejects_traversal`, `traversal_ids_are_rejected_on_all_store_paths` (all ops fail closed, store dir stays empty, legit id still round-trips), `planted_traversal_meta_cannot_launder_through_list` (`sess-...meta.json` lists as id `..` with fallback preview, all ops refuse). Written first and observed failing (`assert_safe_id` not found), then fixed. Full suite: 167 passed, 0 failed, no warnings (164 pre-existing + 3 new; the 2 loopback typesafe tests need unsandboxed bind, pass escalated). Untouched: Stage 7.

- 2026-09-22: Stage 5 gaps S5-GAP-1 and S5-GAP-2 closed. Stage 5 stays done. `write_copy_backup_to(subtree, path, text)` walks from the anchor (parent of `<data-dir>/polyforge`) with `openat`/`mkdirat`/`fchmod`. Ancestors above the subtree may be followed (`File::open` on the anchor). From `polyforge` down, existing components are opened `O_NOFOLLOW`: intermediate dirs `O_NOFOLLOW|O_DIRECTORY|O_CLOEXEC` (`mkdirat` 0700 only when missing), final file `O_NOFOLLOW|O_CREAT|O_TRUNC|O_WRONLY|O_CLOEXEC` then `fchmod` that fd to 0600. No chmod-by-path on this walk. Only `ELOOP` maps to `PermissionDenied`. macOS `O_NOFOLLOW|O_DIRECTORY` on a symlink returns `ENOTDIR` (measured here, errno 20); a second `O_NOFOLLOW` open without `O_DIRECTORY` yields `ELOOP` for that symlink and leaves a real non-directory as `ENOTDIR`. Non-normal components (`..`, `.`) are `InvalidInput` before any mkdir under the subtree. Flags checked, not guessed: macOS from this host's MacOSX.sdk `sys/fcntl.h` (`O_NOFOLLOW` 0x100, `O_DIRECTORY` 0x100000, `O_CLOEXEC` 0x1000000, `O_CREAT` 0x200, `O_TRUNC` 0x400, `O_WRONLY` 0x1, `ELOOP` 62). Linux x86*64 from glibc 2.40 `bits/fcntl-linux.h` plus Linux v6.12 `asm-generic/fcntl.h` (`O_NOFOLLOW` 0400000, `O_DIRECTORY` 0200000, `O_CLOEXEC` 02000000, `O_CREAT` 0100, `O_TRUNC` 01000, `ELOOP` 40). Linux aarch64 overrides `O_NOFOLLOW` 0100000 and `O_DIRECTORY` 040000 (glibc 2.40 `aarch64/bits/fcntl.h` and `arch/arm64` uapi `fcntl.h`); the other `O*\*`match the generic header. No Linux headers on this machine, and the Linux walk was not executed here. No new crate. Tests added:`backup_refuses_polyforge_dir_symlink`, `backup_refuses_subdir_symlink`. Updated: `backup_refuses_symlink`, `backup_overwrites_without_rotation`(still one file, second contents, mode 0600).`features/clipboard_hardening.feature`and the select-to-copy paragraph in`docs/USER-MANUAL.md`mention a symlink at`polyforge`or below. Full suite: 164 passed, 0 failed, no warnings (162 previous + 2 new). Mutation: cargo-mutants not run. Untouched: OSC 52 default-on, the 100 KiB cap,`POLYFORGE_CLIPBOARD_NO_OSC52`, `POLYFORGE_CLIPBOARD_NO_BACKUP`, S5-GAP-3, S5-GAP-4, Stages 6–7.

- 2026-09-22: Stage 5 done. S5-F1: new MAX_OSC52_RAW_BYTES (100 KiB) + osc52_allowed gate in src/clipboard.rs — copy_text_or_file skips the OSC 52 leg over the cap (native/tmux legs + backup unaffected); new POLYFORGE_CLIPBOARD_NO_BACKUP opt-out via Env::no_backup; rotation/wipe policy documented (single-file truncate, rm to wipe, spool files unlinked at creation). S5-F2: resolve_copy_path pure resolver — default <base>/polyforge/last-copy.txt, relative POLYFORGE_COPY_FILE remapped under <base>/polyforge/, absolute must stay in-subtree (lexical .. normalization, out-of-tree rejected InvalidInput); refuse_symlink (PermissionDenied, pre+post mkdir check) in write_copy_backup_to. Breaking change: ~/ and /tmp-style POLYFORGE_COPY_FILE values outside the subtree now rejected. New tests: osc52_large_copy_is_capped (boundary), resolve_copy_path_defaults_and_restricts, backup_refuses_symlink (target untouched), backup_overwrites_without_rotation (content + no siblings + 0600). features/clipboard_hardening.feature covers S5-F1/S5-F2. Full suite: 162 passed, 0 failed, no warnings (158 pre-existing + 4 new; the 2 loopback typesafe tests need unsandboxed bind, pass escalated). Untouched: Stages 6–7.

- 2026-09-22: Stage 4 re-review gaps closed (S4-GAP-1..7). S4-GAP-1: `judge_approval` now runs `redact_secrets` on `tool` as well as `body` before `approval_request`. S4-GAP-2: `Client::with_url` (cfg(test) only) + `judge_approval_posts_redacted_body_on_wire` asserts captured POST JSON has redacted `state.tool`/`state.body`, intact questions, raw secrets absent. S4-GAP-3: SECRET*PREFIXES gains `sk_live*`/`sk*test*`/`ASIA`/`glpat-`/`npm*`/`hf*`(bare`gh\_`still omitted). S4-GAP-4:`secret_prefix_at`ASCII case-insensitive; test`redact_secrets_prefixes_are_case_insensitive`. S4-GAP-5/6: redirect test uses two distinct loopback ports; asserts first-hop `Authorization: Bearer super-secret-token`, zero accepts on landing port. S4-GAP-7: `features/typesafe_hardening.feature`covers S4-F1/S4-F2. Wording: client returns HTTP 302 under`Policy::none`; `judge_approval`maps non-success status to`Err` after the body read. Full suite: 158 passed, 0 failed. Mutation: cargo-mutants not run. Untouched: Stages 5–7.
- 2026-09-22: Stage 4 done. S4-F1: new `build_http_client` in `src/typesafe.rs` sets `redirect::Policy::none()` — client returns HTTP 302 (not followed); `judge_approval` maps non-success to `Err`, so the Bearer credential is never resent; pinned HTTPS endpoint never redirects normally so scoring is unaffected. Test runs a loopback 302 server and asserts zero follow-up connections (needs loopback bind; run sandboxed it fails, escalated it passes). S4-F2: new `redact_secrets` applied in `judge_approval` before truncate+POST — PEM blocks (marker-to-marker, truncated fails closed), vendor token prefixes (AKIA/gh*/github_pat*/sk-/xox\*/AIza + Bearer rule), and sensitive-key `=`/`:` values (quoted in-quote, unquoted to EOL with bracket-depth tracking so prior markers survive; `://` exempt). Scoring stays opt-in (no key → no client → no request). Test failures fixed during work: PEM pass ate the END-line tail and merged the next line into an unterminated quote (now ends at the END marker), kv split prior markers at `]` (now depth-tracked), unquoted values stopped at spaces and leaked multi-word secrets (now to-EOL). Residual: standalone high-entropy strings with no known prefix/key are out of scope by design. Full suite: 156 passed, 0 failed, no warnings (152 pre-existing + 4 new). Untouched: Stages 5–7.
- 2026-09-22: Stage 3 done. S3-F1: new `sanitize_text` in `src/app.rs` strips ESC-led sequences (CSI/OSC/DCS/charset, params consumed with the introducer), C1 singletons, C0 controls, and DEL; keeps tab/newline/unicode; truncated input fails closed; idempotent. Wired at ingress — `push_line` (single store+memory choke point, covers all provider/codex/grok/agy push paths), `replace_lines` (pre-fix transcripts cleaned on replay), `stage_diff` (approval card file/body) — plus render defense in `src/ui.rs` (transcript, diff modal, flash, sessions summary) and copy defense in `selected_text`. New tests: 4 `sanitize_*` unit tests (incl. non-enumerated C1 `U+009B`/`U+009D` probe), `push_line_stores_sanitized` (memory + JSONL round-trip), `replace_lines_sanitizes_legacy_data`, `selected_text_is_sanitized`, `stage_diff_sanitizes_card`, `render_neutralizes_injected_line` (TestBackend buffer scan: no control byte reaches the terminal, URL gone, label kept). Full suite: 149 passed, 0 failed, no warnings. Notes: (a) tab names/input echo are local-only and left unsanitized; (b) OSC52 clipboard payload is base64 so needs no extra handling. Untouched: Stages 4–7.
- 2026-09-21: Stage 2 done. S2-F1: muse/codex orphan notifs now drop (grok parity, `src/main.rs` route arms); orphan codex approval requests queue a host-level `denied` via the outbox instead of binding to the active tab, and the drain sends codex decisions without requiring a live session. S2-F2: `approval/requested` with no `denied` choice refuses the modal (transcript + flash, nothing approved/denied); `decide_ui` keeps the card open on n/q/Esc when no deny path maps (y/a still close fail-closed). Superseded tests updated to the new contract (`q_keeps_modal_open_without_deny_choice`, `interaction_resolved_without_decision_sends_cancel`). S2-F3: `interaction_resolved` retires only already-answered cards quietly; unanswered cards queue a cancel + transcript note. New tests: `orphan_notifs_cannot_touch_active_tab`, `orphan_codex_request_denied_never_staged`, `codex_orphan_deny_reaches_host_without_session`, `approval_without/with_deny_choice_*`, `interaction_resolved_with_decision_retires_quietly`. Full suite: 140 passed, 0 failed, no warnings. Notes: (a) codex orphan deny uses the generic `denied` word for all request kinds (best-effort fail-closed; server may reject unknown vocabulary); (b) codex local-close-on-unknown-method path untouched (out-of-stage scope, by design per `codex_choices`); (c) answered-then-drained-then-resolved race can queue one redundant cancel (server already resolved; harmless). Untouched: Stages 3–7.
- 2026-09-21: Stage 1 High-review gaps closed. S1-GAP-A: `discard_until_newline` rewritten to `fill_buf`+`consume` only (no growing Vec). S1-GAP-B: call discard only when `!scratch.ends_with(b"\n")`. New tests: `capped_line_exact_cap_plus_one_newline_preserves_next`, `capped_line_multi_mib_tail_resyncs_without_growing_scratch`. S1-GAP-C: prefer reqwest `stream` failed offline (`wasm-streams ^0.5` not in index cache); fallback `require_content_length` rejects missing CL before `bytes()`, keep oversize pre/post checks; remaining gap = lying Content-Length until body fully buffered. S1-GAP-D: covered by the two new msp tests. Full suite: 134 passed, 0 failed, no warnings. Mutation: mutants not run. Untouched: Stages 2–7, `store.rs` `.lines()`, TypeSafe redirect/redaction.
- 2026-09-19: Stage 1 done. S1-F1: `read_capped_line` (1 MiB cap, drop + Transport error, resync) in `src/msp.rs`, wired into msp + agy stdout/stderr pumps; 4 new `msp::tests`. S1-F2: 256 KiB response cap in `src/typesafe.rs` (Content-Length pre-check + post-read check) + `response_body_cap_rejects_oversize` test. Full suite: 131 passed, 0 failed, no warnings. Notes: (a) invalid UTF-8 lines now drop with Transport instead of killing the pump; (b) true chunked-without-Content-Length streaming needs reqwest `stream` (unavailable offline — `wasm-streams` not cached), recorded as follow-up; (c) `store.rs` local-file `.lines()` reads noted as out-of-stage candidates, untouched.
