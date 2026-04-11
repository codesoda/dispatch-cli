# Multi-Agent PRD Review

Two Claude Code instances collaborate through dispatch — one writes a PRD, the other reviews it, and they iterate until the reviewer approves.

## Prerequisites

- `dispatch` CLI on PATH
- `claude` CLI (Claude Code)
- `tmux` (`brew install tmux` / `apt install tmux`)
- `jq` (`brew install jq` / `apt install jq`)

## Run it

```bash
./examples/review-agent/setup.sh
```

This opens a tmux session with three panes:

```
┌─────────────────────────────────────────┐
│            Dispatch Broker              │
├────────────────────┬────────────────────┤
│                    │                    │
│   PRD Reviewer     │    PRD Writer      │
│   (Claude Code)    │   (Claude Code)    │
│                    │                    │
│                    │    ← you type      │
│                    │      here          │
└────────────────────┴────────────────────┘
```

The writer (bottom-right) will ask you what the PRD should be about. The reviewer (bottom-left) is already listening. Just describe your feature and watch them collaborate.

Press `Ctrl+B` then `D` to detach from the session. `tmux kill-session -t dispatch-prd` to stop everything.

## What happens

```
                    ┌──────────┐
  describe feature  │          │
  ────────────────> │  Writer  │
                    │          │
                    └────┬─────┘
                         │
                   writes prd-draft.md
                         │
                  dispatch send (path)
                         │
                         ▼
                    ┌──────────┐
                    │          │
                    │ Reviewer │
                    │          │
                    └────┬─────┘
                         │
                   reads prd-draft.md
                   reviews content
                         │
                  dispatch send (feedback)
                         │
                         ▼
                    ┌──────────┐
                    │          │
                    │  Writer  │ ◄── revises prd-draft.md
                    │          │
                    └────┬─────┘
                         │
                    (repeat until approved
                     or 3 rounds done)
                         │
                         ▼
                    writes prd-final.md
```

1. **Writer** interviews you, drafts the PRD to `prd-draft.md`, and sends the file path to the reviewer
2. **Reviewer** reads the file, reviews it, and sends structured feedback back
3. **Writer** revises `prd-draft.md` based on the feedback and sends it back
4. They repeat until the reviewer approves or 3 rounds have passed
5. **Writer** copies the approved version to `prd-final.md`

## Under the hood

All communication flows through a Unix domain socket at `/tmp/dispatch-cli/sockets/<cell_id>.sock`. Each agent is a separate process that talks to the broker using CLI commands:

| Command | What it does |
|---------|-------------|
| `dispatch serve` | Starts the broker on a Unix socket |
| `dispatch register` | Announces an agent with name, role, and capabilities |
| `dispatch team` | Lists all registered agents (writer discovers reviewer this way) |
| `dispatch send` | Sends a JSON message to another agent's mailbox |
| `dispatch listen` | Long-polls until a message arrives or timeout |
| `dispatch heartbeat` | Keeps the agent's registration alive (5 min TTL) |

Messages are queued in-memory by the broker and delivered once via FIFO. There's no persistence — if the broker restarts, mailboxes are empty.

The writer and reviewer communicate using JSON messages with a `type` field:

```json
{"type": "review_request", "version": 1, "path": "/path/to/prd-draft.md"}
{"type": "review_feedback", "version": 1, "verdict": "revise", "feedback": "..."}
```

## Files

| File | Purpose |
|------|---------|
| `setup.sh` | Launches broker + both agents in a tmux session |
| `writer.prompt.md` | Role instructions for the PRD writer agent |
| `reviewer.prompt.md` | Role instructions for the PRD reviewer agent |
| `dispatch-comms.md` | Shared communication guide — reusable for any dispatch worker |
