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
provider = "muse"   # or "mock" for quota-free UI work
# workspace = "/path"  # default: cwd

[muse]
# bin = "muse"
# provider_id = "meta"  # or "echo" for offline dev
# model = "muse-spark-1.2"  # omitted = server default
```

| provider | behavior |
| -------- | -------- |
| `mock` | M1 streaming fake + local diff card |
| `muse` | spawns `muse serve`, one MSP session per tab (`approvalMode: onRequest`) |

Muse path: streams `item/delta` to the transcript, raises
`approval/requested` as the y/n/a/q modal (`q` denies with "re-ask later"),
rings on `turn/completed`. Muse owns `session.jsonl` persistence.

No login: tabs show `muse: not logged in — run \`muse login\`` and stay
navigable.

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
- Long lines are truncated, not wrapped, so scroll offsets stay exact.
