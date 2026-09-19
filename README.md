# usagebar

Live subscription usage for the AI coding tools on this machine, one panel per provider.

Every number on screen is read from a vendor surface at that moment. Nothing is estimated,
extrapolated, or reconstructed from token counts, so a percent that looks wrong is wrong at
the vendor, not in the arithmetic here. When a provider reports nothing, the panel says so
instead of inventing a figure.

## What it reads

| Provider | Source | Auth |
| --- | --- | --- |
| Claude | `GET api.anthropic.com/api/oauth/usage` (`anthropic-beta: oauth-2025-04-20`) | `~/.claude/.credentials.json`, refreshed in place when expired |
| Codex | `GET chatgpt.com/backend-api/wham/usage`; falls back to the last `rate_limits` snapshot in `~/.codex/sessions` | `~/.codex/auth.json` |
| OpenCode Go | `GET opencode.ai/zen/go/v1/usage` per live key | `credential` table in `~/.local/share/opencode/opencode.db` |
| Cursor | `GET cursor.com/api/usage-summary` | `~/.cursor/auth.json`, cookie built as `sub::jwt` |
| Grok | `GET cli-chat-proxy.grok.com/v1/billing?format=credits` | `~/.grok/auth.json` |
| Devin | `POST server.codeium.com/.../GetUserStatus` (Connect RPC), remaining flipped to used | `windsurf_api_key` in `~/.local/share/devin/credentials.toml` |
| Command Code | `GET api.commandcode.ai/alpha/billing/credits` | `~/.commandcode/auth.json` |

Providers without credentials are simply absent. A provider that fails keeps its own error
text on its panel.

## Run

```sh
cargo run --release              # TUI, refreshes every 60s
cargo run --release -- --once    # one snapshot as text
cargo run --release -- --json    # one snapshot as JSON
cargo run --release -- --interval 15
```

Keys: `q` quit, `r` refresh now, `space` pause.

## Notes

The sparkline under each percentage is this tool's own readings over time, drawn on an
absolute 0-100 scale. It is the only value not sent by a vendor, and it is labelled by
construction: a flat line at 90% looks nothing like one at 5%.

When a poll fails, the panel keeps the last good reading and marks it `stale` with its age
instead of blanking out. Panels only read `unavailable` when there is no previous number to
stand on.

Provider endpoints are undocumented and rate limited, so keep the interval at a minute or
more. Polling Claude's usage endpoint every few seconds earns a 429.

`--json` is the integration surface; the schema is `{captured_at, reports[]}` with a
`health.state` of `ok`, `stale`, `no_quota`, or `unavailable` per provider.
