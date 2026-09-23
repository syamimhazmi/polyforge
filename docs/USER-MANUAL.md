# polyforge — User Manual

polyforge is a vim-modal, multi-tab terminal UI for chatting with coding
agents. One tab = one agent session. It fronts five stdio backends: a
quota-free offline `mock`, Muse Spark (`muse`), Codex (`codex`),
Antigravity (`agy`), and Grok (`grok` / ACP).

> Prototype. The interaction model and the agent wiring are the focus —
> not a production client.

## 1. Starting and quitting

```sh
cargo run
```

Needs a real terminal (alternate screen + mouse). Inside tmux, the wheel
and `Ctrl-u/d` scroll the TUI viewport directly — tmux copy-mode is never
involved.

**Select-to-copy:** press and drag across the transcript to highlight
(reverse-video); releasing the button copies, same contract as the grok
CLI. One release fires every active leg: the native clipboard (`pbcopy`
on macOS), the tmux buffer (`tmux load-buffer`) when inside tmux, and
OSC 52 toward the outer terminal (always on Linux; on macOS only in
tmux/SSH/display-less containers or with a wrap sink). Helpers run with
a 2-second deadline so a wedged `pbcopy` can never hang the UI, and the
selection is also written to `POLYFORGE_COPY_FILE` (default
`$XDG_DATA_HOME/polyforge/last-copy.txt`, mode 0600, single-file
overwrite — no history kept; wipe with `rm` on that path) unless
`POLYFORGE_CLIPBOARD_NO_BACKUP` is set — the status flash
names that path when every clipboard leg misses. A custom
`POLYFORGE_COPY_FILE` must stay under `$XDG_DATA_HOME/polyforge/`
(relative paths remap there; anything else is rejected) and a symlink at
`polyforge` or below is refused. Selections over 100 KiB skip the OSC 52 leg
but still reach the other legs and the backup file. Kill OSC 52 entirely
with `POLYFORGE_CLIPBOARD_NO_OSC52`; advertise a wrapping sink with
`POLYFORGE_OSC52_SINK`. A plain click clears the highlight. Selection
maps through scroll, wrapped rows, and wide characters, so what you
highlight is what gets copied. Toggle mouse capture entirely with `m`
(when off, your terminal's native selection applies instead).

- Every launch opens **one tab with a FRESH session**. Nothing from a
  previous run is replayed automatically.
- Quit with `q` (Normal mode). Quitting ends all live backend
  connections; stored transcripts stay on disk (see §7). (`Ctrl-c`
  also ends the process from the terminal, skipping graceful shutdown.)

Zero-quota UI practice: set `provider = "mock"` in the config (see §8)
and everything runs offline.

## 2. The screen

From top to bottom:

1. **Tab bar** — one entry per tab: `○ s1:muse (1)`. `●` means the tab is
   busy (an answer streams in even while you look at another tab); `○`
   means idle. The active tab is highlighted, with its backend and number.
2. **Transcript** — the active tab's conversation, oldest at top. Your
   prompts show as `> …`. A bell rings whenever a tab's job finishes.
3. **Status bar** — mode badge, `tab 1/2 s1`, busy/idle, scroll position,
   mouse state, and the latest notice (`flash`).
4. **Input box** — its title always names the keys available in the
   current mode.

## 3. Modes and keys

The mode badge tells you where keystrokes go.

### NORMAL — navigate

| key | action |
| --- | ------ |
| arrows | scroll one line |
| `PgUp` / `PgDn` | half-page up / down |
| `Home` / `End` | jump to top / bottom |
| `/` | search mode |
| `n` / `N` | next / previous search hit |
| `Space` / `Enter` | insert mode (type; `Space` works in vim mode too) |
| digits, `Tab` | switch tabs (digits follow the live tab count) |
| `P` | provider picker (switch backend = fresh session) |
| `R` | respawn active tab with a FRESH session, same backend |
| `m` | mouse capture on/off |
| `q` | quit |

**Vim keymap** (off by default): type `/vim` (Insert mode) to enable
`j/k` line scroll, `Ctrl-u/d` half-page, `g`/`G` top/bottom, and `i/a`
to start typing — `Enter` stops entering insert mode. Type `/vim` again
to switch back. The choice is saved to `vim = true/false` under
`[polyforge]` in the config file, so it sticks across runs; the input
box title shows `NORMAL·vim` while active. Everything else (`/`, `n/N`,
arrows, `Home/End`, `PgUp/PgDn`, `P`, `R`, digits, `Tab`, `m`, `q`)
works identically in both keymaps, and the picker/sessions lists always
use `j/k`.

### INSERT — type and send

Type, move with `←/→`, `Enter` sends, `Esc` (or `Ctrl-[`) back to Normal.
Input starting with `/` is a **command**, never sent to the agent:

| command | action |
| ------- | ------ |
| `/sessions [query]` | browse previous sessions, most active first; query filters like `grok sessions search` |
| `/new` | fresh session in this tab (same as `R`) |
| `/tab new` | open a tab (max 3), same backend as current |
| `/tab close` | close this tab, killing its session |
| `/vim` | toggle the vim keymap (saved to config) |
| `/help` | print the command + key summary into the transcript |

Unknown `/…` input flashes the command list instead of sending anything.

### SEARCH — find in transcript

`/`, type a query, `Enter` finds (`n`/`N` step through hits), `Esc`
cancels.

### PICKER — switch provider (`P`)

`j/k` move, `Enter` switches, `1-4` quick-picks, `Esc` cancels.
Switching **always starts a fresh session** — history never carries over.
The tab's old session stays stored and browsable via `/sessions`.

| # | backend |
| - | ------- |
| 1 | mock — offline fake, no quota |
| 2 | muse — Muse Spark via `muse serve` |
| 3 | codex — Codex via `codex app-server` |
| 4 | agy — Antigravity via `agy` (visible, ungated — see §6) |
| 5 | grok — Grok via `grok agent stdio` (ACP — see §6) |

### SESSIONS — continue a previous session (`/sessions`)

Session management follows the grok CLI. The list mirrors
`grok sessions list`: one row per session with id, backend, created and
updated timestamps, and summary (first-prompt title, else the first
transcript line), ordered most-active-first. A `/sessions <query>`
argument filters like `grok sessions search` (title, summary, or id).

`j/k` move, `Enter` loads, `d` deletes permanently
(`grok sessions delete` — refused while the session is open in a live
tab), `1-9` quick-picks, `Esc` cancels. Loading
replays the transcript into the active tab **and re-attaches the live
session**, so you continue where you left off (muse `session/resume`,
codex `thread/resume`, grok `session/resume`, agy `--conversation`).
A failed re-attach starts fresh and says so in the transcript.

### DIFF modal — approve agent writes

When an agent wants to change files, a centered card shows the file and
the diff, and it steals `y/n/a/q`:

| key | action |
| --- | ------ |
| `y` | approve this change |
| `n` | reject this change |
| `a` | approve this and all following changes |
| `q` / `Esc` | later — close the card without deciding |

On muse this answers the live approval; on codex `q`/Later maps to wire
`denied` when the server offers those choices. Permissions and free-text
server prompts can only be closed locally for now — the turn stays parked
server-side, and the transcript says so.

## 4. Tabs

- Boot = 1 tab. `/tab new` grows to 3; the new tab becomes active and its
  session is brought up automatically.
- `Tab` cycles; digits jump (`1` = first tab). With one tab, other digits
  do nothing.
- `/tab close` drops the tab's queued work and kills its session: an agy
  child process is killed, grok gets a best-effort `session/close`;
  muse/codex remote sessions have no vendor kill API, so the remote id is
  abandoned while the transcript stays stored.
  Remaining tabs are renumbered (`s1…`) and queued work follows its tab.
- The last tab cannot be closed — `R` starts it fresh instead.

## 5. Sessions and freshness

Think of it as: **a tab is a view, a session is history.**

- Boot, `R`, `/new`, picker switches, and `/tab new` all mint a **new session
  id**. The previous session's transcript is never deleted or overwritten
  (except explicit `d` in the chooser).
- `/sessions` is the way back to previous sessions.
- The banner `(viewing N lines from … — … continues, R starts fresh)`
  is UI-only and is never written to disk.

## 6. Backends

| backend | behavior |
| ------- | -------- |
| `mock` | Offline streaming fake + local diff card. Streams two lines per tick, then `mock: done ✓`. For UI practice and tests. |
| `muse` | Spawns `muse serve`, one MSP session per tab (`approvalMode: onRequest`). Streams `item/delta`, raises `approval/requested` as the y/n/a/q modal, rings on `turn/completed`. |
| `codex` | Talks to `codex app-server` over stdio: thread per tab, `turn/start` on send, deltas to transcript, `turn/completed` to the bell. Approvals arrive as server→client requests, answered `approved` / `approved_for_session` / `denied` (`accept*` for file edits when offered). Mid-turn writes render as `codex: ~ path (already applied)` (`+` add, `-` delete) — **those writes already applied server-side and were never gated by y/n**; the y/n modal only covers requests the server actually routes through it. Guardian auto-reviews show as `codex: reviewing …` / `codex: guardian approved — … [already applied — no y/n modal]`. |
| `agy` | One `agy` child per tab (`--input-format/--output-format stream-json`). Deltas stream, tool steps render with outcome, `result` ends the turn with a bell. **Visible but ungated**: headless agy has no interactive approval round-trip, so no modal can ever fire on agy tabs — workspace writes auto-allow and Ask-actions soft-deny, with notices in the transcript. Nothing is ever pre-granted in your settings. **Trust assumption**: opening an agy tab prints an `agy: WARNING — no y/n approval modal …` line (plus flash) — opening the session trusts the agent with workspace writes. |
| `grok` | One shared `grok agent stdio` host, one ACP session per tab. Text streams line by line, thoughts show as one truncated `grok: ∴ …` line, tool calls as `grok: ⚙ title [kind]` with `✓`/`✗` outcomes, plans as `grok: plan (N steps)`. The prompt result rings the bell (`grok: done ✓`). Permission requests raise the y/n/a/q modal, mapped to ACP kinds (`allow_once` / `allow_always` / `reject_*`); `q` answers `cancelled`. Question-style requests (`ask_user_question`, `exit_plan_mode`, `mcp/elicit`) are not approvals — they respond `cancelled` with a transcript line. |

**No login?** The tab stays navigable and shows the exact fix, e.g.
`muse: not logged in — run \`muse login\`` (or the `codex login`
equivalent). Providers whose host fails to start are greyed out with the
reason; only the failing backend is affected.

## 7. Where transcripts live

`$XDG_DATA_HOME/polyforge/sessions/` (fallback
`~/.local/share/polyforge/sessions/`):

- `sess-{id}.jsonl` — one JSON-escaped line per transcript line.
- `sess-{id}.meta.json` — backend, remote id, created + updated times, title.

Every push appends through a buffered sink, so a crash loses at most one
frame's lines. Files over 50,000 lines are compacted to the tail on load.
`/sessions` lists at most the newest 50. If the store directory can't be
opened, the app runs **storageless** (sessions work, nothing persists)
instead of crashing.

## 8. Configuration

`$XDG_CONFIG_HOME/polyforge/config.toml` (fallback
`~/.config/polyforge/config.toml`). Absent or empty = muse defaults.
Secrets never live here — `muse`/`codex` own their credentials
(`muse login`, Keychain).

```toml
[polyforge]
provider = "muse"   # default backend for fresh tabs: muse | codex | agy | grok | mock
# workspace = "/path"  # default: the directory you launched from
# vim = false  # vim keymap (j/k/g/G/i/a); /vim toggles it live and saves here

[muse]
# bin = "muse"
# provider_id = "meta"  # or "echo" for offline dev
# model = "muse-spark-1.2"  # omitted = server default

[codex]
# bin = "codex"
# model = "gpt-5.6"  # omitted = server default

[agy]
# bin = "agy"
# model = "..."  # omitted = server default (--model)
# agent = "..."  # omitted = server default (--agent)

[grok]
# bin = "grok"
# model = "grok-4.1"  # omitted = server default (session/set_config_option)
```

Environment overrides (checked before the file): `POLYFORGE_MUSE_BIN`,
`POLYFORGE_MUSE_PROVIDER`, `POLYFORGE_CODEX_BIN`, `POLYFORGE_CODEX_MODEL`,
`POLYFORGE_AGY_BIN`, `POLYFORGE_GROK_BIN`, `POLYFORGE_GROK_MODEL`.

## 9. First-run smoke test

Needs a Muse login + quota:

```sh
cargo run            # provider defaults to muse/meta
# i → "reply with exactly: forge-ok" → Enter
```

Expect a streamed reply, `muse: done ✓`, and a bell. Then try something
that edits a file and approve/deny from the modal. Quota-free
alternative: `provider = "mock"` exercises tabs, search, picker,
`/sessions`, and the diff card offline.

## 10. Troubleshooting

| symptom | meaning / fix |
| ------- | ------------- |
| `muse: not logged in — run \`muse login\`` | Backend unavailable; tab still navigable. Log in and `P` → respawn. |
| `host not running — press P to respawn` | The provider host died or never started; respawn retries bringup. |
| `…: resume failed (…) — started fresh` | Stored remote id was stale (server forgot it); you got a new session, history intact. |
| `no previous sessions yet` | `/sessions` with an empty store. Send something first. |
| `can't close the last tab` | By design — `R` starts it fresh. |
| `already 3 tabs (max)` | Prototype cap; close one first. |
| agy tab never answers | `agy` needs a localhost listener; sandboxed/network-restricted environments deny it. |
| grok tab never answers | Same sandbox boundary as agy — plus login: `grok: not logged in` means sign in via `grok` first. `grok: sandbox denied session setup` means the environment blocked the child. |
| grok approval does nothing | Answers map exact ACP kinds (`allow_once` / `allow_always` / `reject_*`). Unknown vocab fails closed with `cancelled`. `q` is ACP `cancelled`, not a true defer. |
| Nothing persists across runs | Storageless mode — the store dir couldn't be created; check `XDG_DATA_HOME` / disk permissions. |
