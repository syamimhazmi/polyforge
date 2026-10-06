# polyforge

One terminal UI for several coding agents: vim-modal, one tab per agent session.

<!-- SCREENSHOT PLACEHOLDER: docs/screenshot.png (TUI with tabs and a streamed reply) -->

One tab = one agent session. Built with Rust (`ratatui` + `crossterm` +
`tokio`). Fronts five stdio backends plus an offline mock:

| backend | wire |
| ------- | ---- |
| `muse` | Muse Spark via `muse serve` (MSP JSON-RPC) |
| `codex` | OpenAI Codex via `codex app-server` |
| `agy` | Antigravity via `agy` stream-json |
| `grok` | Grok via `grok agent stdio` (ACP) |
| `claude` | Claude Code via `claude -p` stream-json |
| `mock` | Offline fake for UI work (no quota) |

> Prototype. Interaction model and agent wiring are the focus — not a
> production client.

Full operator guide: [`docs/USER-MANUAL.md`](docs/USER-MANUAL.md).

## Install

```sh
cargo install --git https://github.com/syamimhazmi/polyforge
```

Then install and log in to the agent CLIs you want (`muse`, `codex`,
`agy`, `grok`, `claude`). The `mock` backend needs none.

## Usage

**Start a session in a project.** polyforge works in the directory you
launch it from. On first launch it asks for a provider and saves it.

```sh
cd ~/Code/my-project
polyforge
# Enter → type a prompt → Enter; when the agent wants to write: y/n/a/q on the diff
```

**Try it offline, no quota.** Set the mock backend in
`~/.config/polyforge/config.toml`:

```toml
[polyforge]
provider = "mock"
```

**Run two agents side by side, then pick up later.**

```text
Enter, /tab new, Enter   open a second tab (same backend)
Esc, P                   switch this tab's backend, e.g. to claude
1 / 2 or Tab             jump between tabs
Enter, /sessions, Enter  next time: this workspace's sessions; Enter continues one
```

Empty tabs open on a welcome dashboard (logo, backend status, workspace,
theme, keymap, first keys); the first transcript line replaces it.

Needs a real tty (alternate screen + mouse). Inside tmux: wheel and
`Ctrl-u/d` scroll the TUI viewport — tmux copy-mode is never involved.

## Keys

| mode | keys |
| ---- | ---- |
| NORMAL (default keys) | arrows line · `PgUp/PgDn` half-page · `Home/End` top/bottom · `/` search · `n/N` next/prev · `Space`/`Enter` insert · digits/`Tab` tabs · `P` provider · `R` fresh session · `m` mouse toggle · `q` quit |
| NORMAL·vim (`/vim`) | `j/k` line · `Ctrl-u/d` half-page · `g`/`G` top/bottom · `Space`/`i`/`a` insert (rest as above; `Enter` does nothing) |
| INSERT | type · `←/→` move · `Enter` send · `Esc`/`Ctrl-[` normal · `/sessions` `/new` `/tab new` `/tab close` `/theme` `/vim` `/help` commands |
| SEARCH | `Enter` find · `Esc` cancel |
| PICKER | `j/k` move · `Enter` switch · `1-5` quick · `Esc` cancel |
| SESSIONS | `j/k` move · `Enter` view + continue · `d` delete · `1-9` quick · `Esc` cancel |
| DIFF modal | `y` approve · `n` reject · `a` approve-all · `q`/`Esc` later |

Mouse: wheel = 3 lines, `Shift+wheel` = page. Drag across the transcript
to highlight; releasing copies via native clipboard + tmux buffer + OSC 52
(grok-CLI parity, always backed up to `last-copy.txt`). `●` tab = busy
(streams in background), bell rings on done.

## Config

`~/.config/polyforge/config.toml`. On first launch (no `provider` set)
polyforge asks you to pick a provider and saves the choice here:

```toml
[polyforge]
provider = "muse"   # default backend: muse | codex | agy | grok | claude | mock (unset = ask at launch)
# workspace = "/path"  # default: cwd
# vim = false  # vim keymap (j/k/g/G/i/a); toggled live with /vim (saves here)

[ui]
# theme = "groknight"  # groknight (default) | tokyonight; /theme switches live and saves here

[muse]
# bin = "muse"
# provider_id = "meta"  # or "echo" for offline dev
# model = "muse-spark-1.2"  # omitted = server default

[codex]
# bin = "codex"
# model = "gpt-5.6"  # omitted = server default

[agy]
# bin = "agy"
# model = "..."  # omitted = server default
# agent = "..."  # omitted = server default

[grok]
# bin = "grok"
# model = "grok-4.1"  # omitted = server default (session/set_config_option)

[claude]
# bin = "claude"
# model = "opus"                   # omitted = Claude Code default (--model)
# permission_mode = "acceptEdits"  # omitted = your Claude settings' default; unallowed tools ask y/n
```

Press `P` on any tab for the provider picker (`j/k` + `Enter`, `1-5`
quick-pick, `Esc` cancels): the tab respawns under the chosen backend with
a FRESH session — history never carries over. The header row shows each tab's
backend (`s1:muse`).

## What each backend does

| provider | behavior |
| -------- | -------- |
| `mock` | Streaming fake + local diff card. Offline UI practice. |
| `muse` | `muse serve`, one MSP session per tab (`approvalMode: onRequest`). Streams deltas; `approval/requested` → y/n/a/q modal; `turn/completed` rings the bell. |
| `codex` | `codex app-server`, one thread per tab. Approvals answer `approved` / `approved_for_session` / `denied` (`accept*` when offered). Mid-turn writes and guardian reviews render as one-liners. |
| `agy` | One `agy` child per tab (stream-json). Visible but ungated: no interactive approval modal — workspace writes auto-allow, Ask-actions soft-deny (stderr notices in the transcript). |
| `grok` | One shared `grok agent stdio` host (ACP), one session per tab. Chunks / thoughts / tools / plans stream; `session/request_permission` → y/n/a/q mapped to ACP option kinds; `q` and unsupported requests answer `cancelled`. |
| `claude` | One `claude -p` child per tab (`--input-format/--output-format stream-json`), session id chosen up front (`--session-id`, resumed with `--resume`). Text and tool calls render; failed tool results and permission denials show in the transcript; `result` ends the turn with a bell. Tools your Claude settings don't pre-allow raise the y/n/a/q modal (`--permission-prompt-tool stdio`); `a` applies Claude's suggested "don't ask again" rule. |

No login: tabs stay navigable and show the fix (`muse login`, `codex login`,
or sign in via `grok`). A failed host greys out only that backend.

## Sessions and tabs

Boot opens ONE tab with a FRESH session. Transcripts append to
`$XDG_DATA_HOME/polyforge/sessions/sess-{id}.jsonl` (capped at 50k lines)
plus `sess-{id}.meta.json` (backend, remote id, timestamps, title, workspace).

Session management follows the grok CLI shape:

- `/sessions [query]` — list this workspace's sessions only, grouped by
  provider (active tab's first), most-active-first; query filters like search
- `Enter` — replay transcript + re-attach remote (`session/resume` /
  `thread/resume` / `--conversation`; failed resume starts fresh and says so)
- `d` — delete stored session (refused while open in a live tab)
- `/new` or `R` — fresh session in the active tab (same backend)
- `/tab new` — new tab (max 3, same backend as current)
- `/tab close` — kill the tab's session (agy child killed; grok best-effort
  `session/close`; claude child killed; muse/codex remotes abandoned,
  transcript kept)

The last tab cannot be closed — `R` starts it fresh instead.

## Docs

- [User manual](docs/USER-MANUAL.md), including a [first-run smoke test](docs/USER-MANUAL.md#9-first-run-smoke-test)
- [TypeSafe approval risk](docs/approval-risk.md): LOW/MED/HIGH risk band on the diff modal
- [Known prototype deviations](docs/known-deviations.md)
- [Security findings](docs/security-findings.md)
