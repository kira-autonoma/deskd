# deskd Setup Guide

## Prerequisites

- Linux VPS with sudo access
- Go 1.26+ and Rust (via mise)
- Claude Code CLI (`npm install -g @anthropic-ai/claude-code`)
- GitHub CLI (`gh`)
- Telegram bot tokens (one per agent, via @BotFather)

## Install deskd

```bash
cd ~/dev/tools/deskd
cargo build --release
sudo cp target/release/deskd /usr/local/bin/deskd
```

One binary at `/usr/local/bin/deskd`. All users share it.

## Create OS users

One user per agent. Agents can't access each other's files.

```bash
sudo useradd -m -s /bin/bash dev
sudo useradd -m -s /bin/bash kira
sudo usermod -aG docker dev kira  # if using Docker for anything
```

## Set up Claude auth per user

Each user needs Claude API access:

```bash
# Copy from root or run `claude auth login` as each user
sudo cp ~/.claude.json /home/dev/.claude.json
sudo cp ~/.claude/.credentials.json /home/dev/.claude/.credentials.json
sudo chown -R dev:dev /home/dev/.claude*

# Same for kira
sudo cp ~/.claude.json /home/kira/.claude.json
sudo cp ~/.claude/.credentials.json /home/kira/.claude/.credentials.json
sudo chown -R kira:kira /home/kira/.claude*
```

## Set up GitHub auth per user

```bash
# Copy gh config or run `gh auth login` as each user
sudo mkdir -p /home/dev/.config/gh /home/kira/.config/gh
sudo cp ~/.config/gh/hosts.yml /home/dev/.config/gh/
sudo cp /path/to/kira/gh/hosts.yml /home/kira/.config/gh/
sudo chown -R dev:dev /home/dev/.config
sudo chown -R kira:kira /home/kira/.config
sudo -u dev gh auth setup-git
sudo -u kira gh auth setup-git
```

## Config files

### workspace.yaml (root-owned, manages all agents)

Lives in your home repo. Defines top-level agents, their users, and Telegram bots.

```yaml
agents:
  - name: dev
    unix_user: dev
    work_dir: /home/dev
    command:
      - claude
      - --output-format
      - stream-json
      - --verbose
      - --dangerously-skip-permissions
      - --model
      - claude-sonnet-4-6
      - --max-turns
      - "100"
    telegram:
      token: ${DEV_BOT_TOKEN}

  - name: kira
    unix_user: kira
    work_dir: /home/kira
    command:
      - claude
      - --output-format
      - stream-json
      - --verbose
      - --dangerously-skip-permissions
      - --model
      - claude-sonnet-4-6
      - --max-turns
      - "100"
    telegram:
      token: ${KIRA_BOT_TOKEN}
```

Tokens use `${ENV_VAR}` syntax — set them before starting deskd.

### deskd.yaml (per-user, in agent's work_dir)

Defines the agent's system prompt, model, sub-agents, Telegram routes, schedules.

```yaml
model: claude-sonnet-4-6
system_prompt: "You are a dev agent..."
max_turns: 100

telegram:
  routes:
    - chat_id: -1003563053115

agents: []
schedules: []
channels: []
```

## Start deskd

```bash
# Set tokens
export DEV_BOT_TOKEN="your-dev-bot-token"
export KIRA_BOT_TOKEN="your-kira-bot-token"

# Start (foreground)
deskd serve --config ~/workspace.yaml

# Or background
nohup deskd serve --config ~/workspace.yaml > /tmp/deskd-serve.log 2>&1 &
```

deskd will:
1. Read workspace.yaml
2. For each agent: create isolated bus, start Telegram adapter, start worker
3. Workers wait for messages from Telegram or bus

## Verify

```bash
# Check logs
tail -f /tmp/deskd-serve.log

# Send a test message
deskd agent send dev "say hello" --socket /home/dev/.deskd/bus.sock

# Check agent status
deskd agent list --socket /home/dev/.deskd/bus.sock
```

## Troubleshooting

| Error | Cause | Fix |
|-------|-------|-----|
| `No response from claude` | Stale session_id in agent state | Clear `session_id` in `~/.deskd/agents/<name>.yaml` |
| `telegram bus task exited` | Write half of bus connection dropped | Update deskd (fixed in latest) |
| `Permission denied` on bus socket | Socket owned by wrong user | deskd creates sockets with 0777 perms |
| `unknown option --mcp-server` | Old claude CLI or wrong flag | Use `--mcp-config` with JSON string |
| MCP server `status: failed` | `DESKD_BUS_SOCKET` not set | Env vars are injected by deskd into claude process |
| Claude refuses `--dangerously-skip-permissions` | Running as root | Use `unix_user` in config to run as non-root |

## Architecture

```
deskd serve --config workspace.yaml
│
├── agent: dev (unix_user: dev)
│   ├── bus (/home/dev/.deskd/bus.sock)
│   ├── worker → claude process
│   └── telegram adapter (dev's bot)
│
└── agent: kira (unix_user: kira)
    ├── bus (/home/kira/.deskd/bus.sock)
    ├── worker → claude process
    └── telegram adapter (kira's bot)
```

Each agent is fully isolated: own user, own bus, own Telegram bot, own Claude session.
