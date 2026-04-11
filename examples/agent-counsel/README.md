# Agent Counsel

Ask a question. Four of tech's most iconic minds weigh in. A chair synthesises their perspectives into a briefing.

Six Claude Code instances coordinate through dispatch — a main agent, a chair, and four counselors — each running as an independent process communicating via message passing.

## The counsel

| Counselor | Lens |
|-----------|------|
| **Elon Musk** | First principles, bold bets, vertical integration |
| **Steve Jobs** | Product taste, simplicity, user experience |
| **Bill Gates** | Platform strategy, ecosystem moats, competitive positioning |
| **Jeff Bezos** | Customer obsession, flywheels, long-term compounding |

## Prerequisites

- `dispatch` CLI on PATH
- `claude` CLI (Claude Code)
- `tmux` (`brew install tmux`)
- `jq` (`brew install jq`)

## Run it

```bash
./examples/agent-counsel/setup.sh
```

This opens a tmux session:

```
┌──────────────────────────────────────────────┐
│                   Broker                     │
├────────────┬─────────────────────────────────┤
│   Chair    │                                 │
├────────────┤                                 │
│   Elon     │        Main Agent               │
├────────────┤        ← you type here          │
│   Jobs     │                                 │
├────────────┤                                 │
│   Gates    │                                 │
├────────────┤                                 │
│   Bezos    │                                 │
└────────────┴─────────────────────────────────┘
```

The main agent (right, large pane) asks you for a question. The left column shows the chair and four counselors working. Navigate panes with `Ctrl+H/J/K/L` or `Ctrl+B` + arrow keys.

## What happens

```
                         ┌──────────┐
   "Should we build      │   Main   │
    a marketplace?" ───> │  Agent   │
                         └────┬─────┘
                              │ dispatch send
                              ▼
                         ┌──────────┐
                         │  Chair   │
                         └────┬─────┘
                              │ dispatch send (fan-out)
                 ┌────────┬───┴───┬────────┐
                 ▼        ▼       ▼        ▼
              ┌──────┐ ┌──────┐ ┌──────┐ ┌──────┐
              │ Elon │ │ Jobs │ │Gates │ │Bezos │
              └──┬───┘ └──┬───┘ └──┬───┘ └──┬───┘
                 │        │       │        │
                 └────────┴───┬───┴────────┘
                              │ dispatch send (fan-in)
                              ▼
                         ┌──────────┐
                         │  Chair   │ ── may follow up
                         └────┬─────┘    with individual
                              │          counselors
                              │ dispatch send (briefing)
                              ▼
                         ┌──────────┐
    briefing with all    │   Main   │
    perspectives    <─── │  Agent   │
                         └──────────┘
```

1. **Main agent** sends your question to the chair
2. **Chair** fans out the question to all four counselors
3. **Counselors** each respond with their perspective (3-5 sentences, opinionated)
4. **Chair** reads all responses — if there's disagreement or a shallow take, follows up with specific counselors (up to 2 extra rounds)
5. **Chair** consolidates everything into a structured briefing: bottom line, each perspective, points of agreement/tension, and a synthesis
6. **Main agent** presents the briefing to you

## Models

- **Main agent**: Opus (default) — handles user interaction
- **Chair + counselors**: Sonnet — fast, capable, cost-efficient for focused tasks

## Files

| File | Purpose |
|------|---------|
| `setup.sh` | Launches broker + all 6 agents in a tmux session |
| `main.prompt.md` | User-facing agent that relays questions |
| `chair.prompt.md` | Coordinator that manages the counsel |
| `counselor-musk.prompt.md` | Elon Musk persona |
| `counselor-jobs.prompt.md` | Steve Jobs persona |
| `counselor-gates.prompt.md` | Bill Gates persona |
| `counselor-bezos.prompt.md` | Jeff Bezos persona |
| `dispatch-comms.md` | Shared communication guide for all workers |
