# usagebar

Live subscription usage for the AI coding tools on this machine, one panel per provider.

Every number on screen is read from a vendor surface at that moment. Nothing is estimated,
extrapolated, or reconstructed from token counts, so a percent that looks wrong is wrong at
the vendor, not in the arithmetic here. When a provider reports nothing, the panel says so
instead of inventing a figure.

## What it reads

| Provider | Source | Credential |
| --- | --- | --- |
| Claude Code subscription | `GET api.anthropic.com/api/oauth/usage` (`anthropic-beta: oauth-2025-04-20`) | OAuth pair from `~/.claude/.credentials.json`, refreshed and written back in place |
| Codex with ChatGPT login | `GET chatgpt.com/backend-api/wham/usage`; falls back to the last `rate_limits` snapshot in `~/.codex/sessions` | `~/.codex/auth.json` |
| OpenCode Go | `GET opencode.ai/zen/go/v1/usage` per key | `credential` table in `~/.local/share/opencode/opencode.db` |
| Cursor individual usage | `GET cursor.com/api/usage-summary` | `~/.cursor/auth.json`, cookie built as `sub::jwt` |
| Grok CLI | `GET cli-chat-proxy.grok.com/v1/billing?format=credits` | `~/.grok/auth.json` |
| Devin | `POST server.codeium.com/.../GetUserStatus` (Connect RPC), remaining flipped to used | `windsurf_api_key` in `~/.local/share/devin/credentials.toml` |
| Command Code | `GET api.commandcode.ai/alpha/billing/credits` | `~/.commandcode/auth.json` |

Vendor paths follow the vendor's own environment overrides (`CLAUDE_CONFIG_DIR`,
`CODEX_HOME`, and so on). Secrets live in the platform config directory under
`usagebar/credentials.json`; on Unix that is normally `~/.config/usagebar`. The config
file never holds one.

## Setup

The first run scans the machine and opens with the logins it found. `space` includes or
excludes one, `a` connects another provider, and enter goes to the finish screen.

For providers with a local CLI, setup starts with that provider's normal saved login path.
Sign in with the provider's own CLI, then press enter to check the account. Raw token entry
is under `F2` Advanced. OpenCode Go uses its API key because it has no login file import.

Every manual connection is checked against the vendor before it saves. If a check fails,
enter retries it; `ctrl+s` is the explicit way to keep the account unchecked. A check only
reads: it never refreshes or rewrites a credential.

Nothing is written until the finish screen. Esc skips setup and records that choice as
`"detect": true` with an empty account list, which means "keep scanning each run"; removing
every account later writes `"detect": false`, so an empty list is a choice, not a reset.

`s` opens the same screens later to add another account.

## Keys

| Key | Action |
| --- | --- |
| `q` | quit |
| `r` | refresh now |
| `space` | pause |
| `s` | setup: accounts, order, sort mode, interval |
| `d` | details for one account; `←` `→` changes account and `↑` `↓` scrolls |

The bottom bar shows controls for the current screen and selected setup row. Tab and
shift+tab also move between setup fields. Long account, provider, and detail lists scroll
while keeping the selected row visible.

## Accounts and order

One account is one panel. Two Claude logins, or two OpenCode Go keys, are two panels with
their own readings and history.

`s` lists accounts in display order:

| Key | Action |
| --- | --- |
| `space` | show or hide the account |
| `shift+↑` `shift+↓` | move it |
| `x` | remove it, twice, because that forgets its credentials |
| `enter` | on a setting, change it |

Sort is `manual`, which is the list order, or `smart`, which is worst first. Interval is any
number of seconds, five or more. Changes save to `~/.config/usagebar/config.json` as you
make them:

```json
{
  "version": 1,
  "interval_secs": 60,
  "sort": "manual",
  "detect": false,
  "accounts": [
    { "id": "claude", "provider": "claude", "label": "work" },
    { "id": "codex", "provider": "codex", "hidden": true }
  ]
}
```

`USAGEBAR_CONFIG_DIR` moves both files somewhere else. With no accounts configured, every
run scans the vendor files, which is what the tool did before accounts existed.

## Install

Download a macOS, Linux, or Windows binary from the
[latest release](https://github.com/ghandhitechnology/usagebar/releases/latest), extract it,
and put `usagebar` (`usagebar.exe` on Windows) somewhere on `PATH`.

To build from source:

```sh
cargo install --path .
ln -sf usagebar ~/.cargo/bin/usge        # optional shorter name on Unix
```

Then either name works from any directory:

```sh
usge                # TUI
usge --once         # one snapshot as text
```

## Run

```sh
cargo run --release              # TUI, refreshes every 60s
cargo run --release -- --once    # one snapshot as text
cargo run --release -- --json    # one snapshot as JSON
cargo run --release -- --interval 15   # overrides the saved interval for this run
```

### In a tmux pane

It is a plain terminal program, so it drops straight into a pane with no nesting:

```sh
tmux split-window -h -l 84 'usge'          # 84-column side panel on the right
```

The layout adapts to whatever the pane gives it, in this order:

| Pane | Drawing |
| --- | --- |
| Wide and tall | Panel per provider, bar plus reset line per window |
| Narrow or short | Panel per provider, one line per window with the countdown inline |
| Short enough that panels would be squeezed | One line per provider, tightest window only |

It never shrinks panels until the numbers disappear. If even the last form runs out of rows,
it says how many panels it is holding back instead of dropping them quietly.

To see the UI without opening a terminal at all:

```sh
usagebar --render --sizes 80x24,140x45    # draw frames to stdout, ANSI and all
```

## Notes

`d` opens everything one account reported: each window with its exact reset time, then the
flat vendor numbers. For Codex that includes the credit balance, banked reset credits and
per-model availability; for the others, whatever the response carried that a panel had no
room for. Terminal width calculations handle Hangul, combining marks, and emoji without
splitting a visible character or shifting the cursor.

When a poll fails, the panel keeps the last good reading and marks it `stale` with its true
age instead of blanking out. A cached Codex rollout is also marked stale. Panels only read
`unavailable` when there is no previous number to stand on.

Claude rotates its refresh token. usagebar writes a rotated pair back to the file it read it
from when that file still holds the pair it rotated from, and it re-reads the file before
every poll, so a login refreshed by Claude Code itself is picked up rather than shadowed.

Provider endpoints are undocumented and rate limited, so keep the interval at a minute or
more. Polling Claude's usage endpoint every few seconds earns a 429.

`--json` is the integration surface; the schema is `{captured_at, reports[]}` with
`account_id`, `label`, `facts[]`, and a `health.state` of `ok`, `stale`, `no_quota`, or
`unavailable` per account.
