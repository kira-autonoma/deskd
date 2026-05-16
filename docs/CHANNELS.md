# Claude Code Channels — deskd setup (#451)

deskd implements the [Claude Code Channels protocol](https://code.claude.com/docs/en/channels-reference)
so Telegram messages arrive in a running Claude Code session as
`<channel source="deskd-telegram" chat_id="..." user_id="...">body</channel>`
tags instead of requiring `read_inbox` polling.

This is the operator recipe. The protocol details and architecture decisions
live in PR #451.

## When to use it

Use channels for **agents that run an interactive Claude REPL** (subscription
billed) instead of `claude -p` one-shots (credit billed). Anthropic's SDK
billing change on 2026-06-15 makes channels the cost-effective path for
long-running agent traffic.

If you're running `claude -p` per task, the existing `deskd mcp` MCP server
(with `read_inbox` polling) is the right path — channels add nothing.

## .mcp.json wiring

In the agent's project directory (or `~/.claude.json` for user-level):

```json
{
  "mcpServers": {
    "deskd": {
      "command": "deskd",
      "args": ["mcp-channel", "--agent", "<agent-name>"]
    }
  }
}
```

`<agent-name>` is the deskd agent name from `workspace.yaml` —
the same value you'd pass to `deskd mcp --agent ...`.

The `deskd mcp-channel` subcommand:

* identifies itself as `deskd-telegram` (so `<channel source="deskd-telegram">`)
* advertises `experimental.claude/channel: {}` in its initialize response
* exposes the `reply` tool with `{chat_id, text}` inputs
* subscribes to `telegram.in:*` and `agent:<name>` on the deskd bus
* emits `notifications/claude/channel` frames over stdout when matching
  bus messages arrive

The subprocess inherits `DESKD_BUS_SOCKET` from deskd's serve environment,
so no extra wiring is needed when launched under `deskd serve`.

## Starting the Claude REPL

During the research preview, custom channel plugins require the development
flag:

```bash
claude --dangerously-load-development-channels server:deskd
```

The `server:deskd` form pairs with the `deskd` key in `.mcp.json`.
Anthropic's allowlist gating doesn't apply to entries whitelisted by this
flag (see [the channels research preview notice](https://code.claude.com/docs/en/channels#research-preview)).

## End-to-end test

With the above wired:

1. `deskd serve --config workspace.yaml` (so the bus is up and Telegram
   adapter is polling).
2. In another terminal: `claude --dangerously-load-development-channels server:deskd`.
3. DM the agent's Telegram bot (the chat must be on the agent's allowlist).
4. The Claude session receives:
   ```
   <channel source="deskd-telegram" chat_id="..." user_id="...">
   your message
   </channel>
   ```
5. Ask Claude `reply hello` — Claude calls the `reply` tool with
   `{chat_id: "...", text: "hello"}`, deskd publishes to
   `telegram.out:<chat_id>`, and the Telegram adapter delivers the message.

## Meta key sanitization

Per the channels reference, meta keys must be `[a-zA-Z0-9_]`. deskd enforces
this on emit: keys with hyphens, dots, or other characters are silently
dropped. Bus messages flowing through the channel emitter surface these meta
keys (all values are strings):

| Meta key     | Source                                  |
| ------------ | --------------------------------------- |
| `chat_id`    | `payload.telegram_chat_id`              |
| `user_id`    | `payload.telegram_sender.id`            |
| `chat_name`  | `payload.telegram_chat_name`            |
| `message_id` | `payload.telegram_message_id`           |
| `source`     | bus message `source` (routing context)  |
| `target`     | bus message `target` (routing context)  |
| `bus_id`     | bus message `id` (deduplication aid)    |

These appear as attributes on the rendered `<channel>` tag.

## Drop-on-no-listener

Per the spec, notifications are not acknowledged. If the Claude session has
ended (broken stdout pipe), deskd drops the event silently and continues
reading from the bus — channels are best-effort.

## Out of scope here

* **Permission relay** (`claude/channel/permission`) — separate ticket.
  Claude Code will still surface tool-use prompts in the local terminal.
* **tmux launcher** — see #452 for the recipe to run the Claude REPL inside
  a persistent tmux session so it survives SSH disconnects.
* **Cross-machine attach** — out of preview scope.
* **Plugin packaging** (`/plugin install deskd`) — deferred until channel
  plugins are accepted into the official allowlist.

## Troubleshooting

* "Failed to connect" on `/mcp` inside Claude: the subprocess crashed
  during startup. Check `~/.claude/debug/<session>.txt` for the stderr
  trace; `RUST_LOG=debug` on the deskd-side reveals more detail.
* Tag arrives but `source` is `deskd` instead of `deskd-telegram`: you're
  pointing at the plain `deskd mcp` server, not `deskd mcp-channel`. Update
  `.mcp.json`.
* `meta` keys with hyphens missing from the tag: that's the sanitization
  rule — rename your bus payload keys to use underscores.
