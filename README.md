# polyforge

Vim-modal multi-tab TUI for coding agents.

One tab = one agent session. Built with Rust (`ratatui` + `crossterm` +
`tokio`). Fronts five stdio backends:

| backend | wire |
| ------- | ---- |
| `muse` | Muse Spark via `muse serve` (MSP JSON-RPC) |
| `codex` | OpenAI Codex via `codex app-server` |
| `agy` | Antigravity via `agy` stream-json |
| `grok` | Grok via `grok agent stdio` (ACP) |
| `mock` | Offline fake for UI work (no quota) |

> Prototype. Interaction model and agent wiring are the focus — not a
> production client.

Full operator guide: [`docs/USER-MANUAL.md`](docs/USER-MANUAL.md).

## Run

```sh
cargo run
```

Needs a real tty (alternate screen + mouse). Inside tmux: wheel and
`Ctrl-u/d` scroll the TUI viewport — tmux copy-mode is never involved.

## Keys

| mode | keys |
| ---- | ---- |
| NORMAL (default keys) | arrows line · `PgUp/PgDn` half-page · `Home/End` top/bottom · `/` search · `n/N` next/prev · `Space`/`Enter` insert · digits/`Tab` tabs · `P` provider · `R` fresh session · `m` mouse toggle · `q` quit |
| NORMAL·vim (`/vim`) | `j/k` line · `Ctrl-u/d` half-page · `g`/`G` top/bottom · `Space`/`i`/`a` insert (rest as above; `Enter` does nothing) |
| INSERT | type · `←/→` move · `Enter` send · `Esc`/`Ctrl-[` normal · `/sessions` `/new` `/tab new` `/tab close` `/vim` `/help` commands |
| SEARCH | `Enter` find · `Esc` cancel |
| PICKER | `j/k` move · `Enter` switch · `1-5` quick · `Esc` cancel |
| SESSIONS | `j/k` move · `Enter` view + continue · `d` delete · `1-9` quick · `Esc` cancel |
| DIFF modal | `y` approve · `n` reject · `a` approve-all · `q`/`Esc` later |

Mouse: wheel = 3 lines, `Shift+wheel` = page. Drag across the transcript
to highlight; releasing copies via native clipboard + tmux buffer + OSC 52
(grok-CLI parity, always backed up to `last-copy.txt`). `●` tab = busy
(streams in background), bell rings on done.

## Config

`~/.config/polyforge/config.toml` (defaults to `muse` when absent):

```toml
[polyforge]
provider = "muse"   # default backend: muse | codex | agy | grok | mock
# workspace = "/path"  # default: cwd
# vim = false  # vim keymap (j/k/g/G/i/a); toggled live with /vim (saves here)

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
```

Press `P` on any tab for the provider picker (`j/k` + `Enter`, `1-5`
quick-pick, `Esc` cancels): the tab respawns under the chosen backend with
a FRESH session — history never carries over. Tab bar shows each tab's
backend (`s1:muse`).

## TypeSafe approval risk

When a DIFF approval opens, polyforge asks TypeSafe (Jev) three judgments
in one call: risk Score, secrets Noul, destructive Noul. Code composes a
LOW/MED/HIGH band and shows it on the modal. y/n/a/q stay under user
control.

Key load order:

1. `TYPESAFE_API_KEY` environment variable
2. `~/.config/typesafe/env` (or `$XDG_CONFIG_HOME/typesafe/env`)

Missing key: approvals work unscored.

## What each backend does

| provider | behavior |
| -------- | -------- |
| `mock` | Streaming fake + local diff card. Offline UI practice. |
| `muse` | `muse serve`, one MSP session per tab (`approvalMode: onRequest`). Streams deltas; `approval/requested` → y/n/a/q modal; `turn/completed` rings the bell. |
| `codex` | `codex app-server`, one thread per tab. Approvals answer `approved` / `approved_for_session` / `denied` (`accept*` when offered). Mid-turn writes and guardian reviews render as one-liners. |
| `agy` | One `agy` child per tab (stream-json). Visible but ungated: no interactive approval modal — workspace writes auto-allow, Ask-actions soft-deny (stderr notices in the transcript). |
| `grok` | One shared `grok agent stdio` host (ACP), one session per tab. Chunks / thoughts / tools / plans stream; `session/request_permission` → y/n/a/q mapped to ACP option kinds; `q` and unsupported requests answer `cancelled`. |

No login: tabs stay navigable and show the fix (`muse login`, `codex login`,
or sign in via `grok`). A failed host greys out only that backend.

## Sessions and tabs

Boot opens ONE tab with a FRESH session. Transcripts append to
`$XDG_DATA_HOME/polyforge/sessions/sess-{id}.jsonl` (capped at 50k lines)
plus `sess-{id}.meta.json` (backend, remote id, timestamps, title).

Session management follows the grok CLI shape:

- `/sessions [query]` — list most-active-first; query filters like search
- `Enter` — replay transcript + re-attach remote (`session/resume` /
  `thread/resume` / `--conversation`; failed resume starts fresh and says so)
- `d` — delete stored session (refused while open in a live tab)
- `/new` or `R` — fresh session in the active tab (same backend)
- `/tab new` — new tab (max 3, same backend as current)
- `/tab close` — kill the tab's session (agy child killed; grok best-effort
  `session/close`; muse/codex remotes abandoned, transcript kept)

The last tab cannot be closed — `R` starts it fresh instead.

## Live smoke

Needs Muse login + quota (or switch provider):

```sh
cargo run            # provider defaults to muse/meta
# i → "reply with exactly: forge-ok" → Enter
```

Expect a streamed reply, `muse: done ✓`, and a bell. Then try an edit and
approve/deny from the modal. Zero-quota UI work: `provider = "mock"`.

## Known prototype deviations

- Single `g` jumps to top (real `gg` arrives with the key engine).
- Long lines wrap (greedy, wide-char aware); scroll offsets are display rows.
