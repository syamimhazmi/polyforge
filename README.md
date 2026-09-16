# polyforge

Vim-modal multi-tab TUI shell for the Muse Spark coding agent.

Built with Rust (`ratatui` + `crossterm` + `tokio`). Talks MSP over stdio when
`provider = "muse"`, or runs a quota-free mock backend for UI work.

> Prototype (M2). Interaction model and Muse wire are the focus — not a
> production client.

## Run

```sh
cargo run
```

Needs a real tty (alternate screen + mouse). Inside tmux: wheel and
`Ctrl-u/d` scroll the TUI viewport — tmux copy-mode is never involved.

## Keys

| mode | keys |
| ---- | ---- |
| NORMAL | `j/k`/arrows line · `Ctrl-u/d` half-page · `g` top · `G` bottom · `/` search · `n/N` next/prev · `i/a` insert · `1/2/3`/`Tab` tabs · `m` mouse toggle · `q` or `Ctrl-c` quit |
| INSERT | type · `←/→` move · `Enter` send · `Esc`/`Ctrl-[` normal |
| SEARCH | `Enter` find · `Esc` cancel |
| DIFF modal | `y` approve · `n` reject · `a` approve-all · `q`/`Esc` later |

Mouse: wheel = 3 lines, `Shift+wheel` = page. `●` tab = busy (streams in
background), bell rings on done.

## Config

`~/.config/polyforge/config.toml` (defaults to `muse` when absent):

```toml
[polyforge]
provider = "muse"   # default backend for all tabs: muse | codex | mock
# workspace = "/path"  # default: cwd

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
```

Press `P` on any tab for the provider picker (`j/k` + `Enter`, `1-4`
quick-pick, `Esc` cancels): the tab respawns under the chosen backend with
a FRESH session — history never carries over. Tab bar shows each tab's
backend (`s1:muse`).

| provider | behavior |
| -------- | -------- |
| `mock` | M1 streaming fake + local diff card |
| `muse` | spawns `muse serve`, one MSP session per tab (`approvalMode: onRequest`) |

Muse path: streams `item/delta` to the transcript, raises
`approval/requested` as the y/n/a/q modal (`q` denies with "re-ask later"),
rings on `turn/completed`. Muse owns `session.jsonl` persistence.

No login: tabs show `muse: not logged in — run \`muse login\`` (or the
`codex login` equivalent) and stay navigable.

Sessions resume across restarts (Q10): each tab's transcript appends to
`$XDG_DATA_HOME/polyforge/sessions/tab{N}.jsonl` (JSON-escaped, capped at
50k lines with boot-time compaction) plus a `tab{N}.meta.json` with the
backend and remote id. On boot the transcript replays, then muse re-attaches
via `session/resume`, codex via `thread/resume`, agy via `--conversation`
— a failed resume starts fresh and says so. `R` / picker respawn forgets
the tab's stored transcript and starts clean.

Codex approvals answer `approved` / `approved_for_session` / `denied`
(`accept*` family for file edits when offered). Codex `q`/Later maps to
wire `denied` (not a true defer) when those choices exist. Permissions and
free-text prompts can only be closed locally for now — the turn stays
parked server-side. Mid-turn file writes render as `codex: ~ path`
(`+` add, `-` delete) per-file lines, and guardian auto-reviews show as
`codex: reviewing …` / `codex: guardian approved — …` (matched by
segment-tail, since the exact wire prefix is unconfirmed live).

Antigravity runs one `agy` child per tab (`--input-format stream-json`,
documented headless protocol): deltas stream, tool steps render with
outcome, `result` ends the turn with a bell. Per your decision, agy tabs
are visible-but-ungated — headless agy has no interactive approval, so
workspace writes auto-allow and Ask-actions soft-deny (their notices
appear in the transcript from stderr); nothing is ever auto-approved by
us. `provider = "agy"` (or `antigravity`) makes it the default.

## Live smoke

Needs Muse login + quota:

```sh
cargo run            # provider defaults to muse/meta
# i → "reply with exactly: forge-ok" → Enter
```

Expect streamed reply, `muse: done ✓`, and a bell. Then try an edit and
approve/deny from the modal. Zero-quota UI work: `provider = "mock"`.

## Known prototype deviations

- Single `g` jumps to top (real `gg` arrives with the key engine).
- Long lines wrap (greedy, wide-char aware); scroll offsets are display rows.
