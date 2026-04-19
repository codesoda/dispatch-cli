# Dispatch

Multi-agent, multi-vendor orchestration system. A project-local CLI and embedded broker for coordinating long-lived agent sessions and lightweight scripts inside a single cell.

## Install

### Pre-built binary (macOS / Linux)

```sh
curl -sSf https://raw.githubusercontent.com/codesoda/dispatch-cli/main/install.sh | sh
```

Downloads the latest release binary from GitHub, verifies the checksum, and installs to `~/.dispatch/bin/`.

### From a clone (requires Rust)

```sh
git clone https://github.com/codesoda/dispatch-cli.git
cd dispatch-cli
./install.sh
```

Builds from source with `cargo build --release`.

Both methods symlink into `~/.local/bin/` by default. Pass `--skip-symlink` to opt out. Run `./install.sh --help` for all options and environment overrides.

## Quick Start

```bash
# Start the broker (in a separate terminal)
dispatch serve

# Register a worker
dispatch register --name my-worker --role coder \
  --description "Writes code" --capability rust

# Send a message to a worker
dispatch send --to <worker-id> --body '{"type":"task","detail":"implement login"}'

# Listen for messages (long-poll)
dispatch listen --worker-id <worker-id>

# Acknowledge a received message
dispatch ack --worker-id <worker-id> --message-id <msg-id> --note "starting work"

# Publish status while working
dispatch heartbeat --worker-id <worker-id> --status "implementing login 2/5"

# Check what everyone's doing
dispatch status

# View recent broker events
dispatch events --since 10m

# Inspect a worker's inbox without consuming
dispatch messages --worker-id <worker-id>
```

## Architecture

Dispatch runs as a **cell** — a single broker process that coordinates workers on one machine. Workers register themselves, exchange direct messages, and use TTL-based liveness to stay active.

- **Broker**: Embedded server on a Unix domain socket (no external dependencies)
- **Workers**: Processes that register and communicate through the broker
- **Messages**: Direct, queued messages delivered via long-poll
- **Cell identity**: Derived from the project root path, overridable via config or CLI flag

## Commands

### Core

| Command | Description |
|---------|-------------|
| `dispatch init` | Create a `dispatch.config.toml` in the current directory |
| `dispatch serve` | Start the broker (prints agent commands; use `--launch` to auto-start) |
| `dispatch register` | Register a worker (`--evict` replaces existing by name) |
| `dispatch team` | List active workers |
| `dispatch send` | Send a direct message to a worker |
| `dispatch listen` | Long-poll for incoming messages |
| `dispatch heartbeat` | Renew worker TTL (add `--status "doing X"` to publish status) |

### Introspection

| Command | Description |
|---------|-------------|
| `dispatch ack` | Acknowledge receipt of a message (`--message-id`, `--note`) |
| `dispatch status` | View worker status taglines (or `--clear` to wipe) |
| `dispatch events` | Query event history (`--type`, `--worker`, `--since`, `--limit`) |
| `dispatch messages` | Inspect message history (`--worker-id`, `--unacked`, `--sent`) |

### Agent lifecycle

| Command | Description |
|---------|-------------|
| `dispatch agent start <name\|id>` | Start a configured agent by name or worker ID |
| `dispatch agent stop <name\|id>` | Stop a running agent (skips restart budget) |
| `dispatch agent restart <name\|id>` | Stop and re-spawn |

### Vendor hooks

| Command | Description |
|---------|-------------|
| `dispatch codex-hook install` | Register a Stop hook in `.codex/hooks.json` + enable `features.codex_hooks` |
| `dispatch codex-hook uninstall` | Remove the hook (preserves other codex config) |
| `dispatch claude-hook install` | Merge a Stop hook into `.claude/settings.json` |
| `dispatch claude-hook uninstall` | Remove the Stop hook entry |
| `dispatch codex-hook stop` / `dispatch claude-hook stop` | Hook handler invoked by the vendor CLI — prints a JSON block decision |

## Configuration

Dispatch searches upward from the current directory for `dispatch.config.toml`:

```toml
cell_id = "my-project"
```

Override precedence (highest wins): `--cell-id` flag > `DISPATCH_CELL_ID` env var > config file > derived from path.

## Agent orchestration

`dispatch serve` can launch and supervise agents declared in config. Each agent picks an **adapter** (how to invoke the underlying binary) and opts in to auto-launch with `launch = true`:

```toml
[[agents]]
name = "implementer"
role = "code.implementer"
description = "Writes code based on plans"
adapter = "codex"                      # "command" | "claude" | "codex"
prompt_file = "prompts/implementer.md" # piped as stdin to claude/codex
launch = true
extra_args = [
  "-s", "danger-full-access",
  "-m", "gpt-5.4",
  "-c", "model_reasoning_effort=\"xhigh\"",
]

[[agents]]
name = "reviewer"
role = "code.reviewer"
description = "Reviews code for quality"
adapter = "claude"
prompt_file = "prompts/reviewer.md"
launch = true
extra_args = ["--dangerously-skip-permissions", "--model", "sonnet"]

[[agents]]
name = "watcher"
role = "file.watcher"
description = "Watches files and forwards stale messages"
adapter = "command"
command = "./examples/watch-files.sh --worker-id $DISPATCH_WORKER_ID src/"
launch = true
```

Run `dispatch serve --launch` to auto-spawn every agent with `launch = true`. Agents with `launch = false` (the default) are printed as ready-to-paste commands so you can run them in separate terminals.

### Supervisor + auto-restart

Each supervised agent runs under a task that restarts it on exit with exponential backoff: **1s, 2s, 4s, 8s, 16s, 30s (cap)**. After **5 consecutive unstable failures** the supervisor gives up and marks the agent `crashed` in the monitor UI. An agent that ran for at least 30s before exiting gets a fresh restart budget, so a long-lived process that dies once isn't treated as flaky.

Use `dispatch agent {start,stop,restart} <name|worker-id>` to intervene while the broker is up.

### Vendor hooks (keeping LLM agents alive)

LLM vendor CLIs (`claude`, `codex`) finish their turn and exit by default. To keep them alive for the next dispatch message, install a Stop hook that tells the vendor to block the exit:

```sh
# In your project root:
dispatch codex-hook install       # writes .codex/hooks.json + enables features.codex_hooks
dispatch claude-hook install      # merges a Stop hook into .claude/settings.json
```

The hook runs `dispatch {codex,claude}-hook stop`, which emits `{"decision":"block","reason":"..."}` on stdout when a dispatch broker is reachable on the project's socket. When it can't reach a broker (dispatch is shutting down, was never started, or the agent is running outside a dispatch project), the hook prints nothing and exits `0` so the vendor can stop the agent cleanly instead of pinning it alive. Uninstall with `... uninstall`.

### Monitor dashboard

`dispatch serve --monitor <port>` (or `[monitor] port = ...` in config) starts a dashboard at `http://localhost:<port>`. Each configured agent gets a card:

- **Adapter badge** (`codex` / `claude` / `command`) and a **state pill** (running / starting / restarting / crashed / stopped / unmanaged).
- **Current status tagline** (whatever the agent last set via `dispatch status` / `dispatch heartbeat --status`) plus the last two prior taglines as faded context.
- **Uptime** for running agents and **last heartbeat age** for registered workers, ticked locally every second so the card stays live between polls.
- **Start / Stop / Restart** buttons wired to `POST /api/agents/{name}/{action}`. A **Copy cmd** button appears for `launch = false` agents so you can paste the launch command into another terminal.

The card endpoints are unauthenticated and local-loopback only — the same posture as `POST /api/shutdown`.

## Stale & Refresh Control Messages

Dispatch itself does not manage worker caches, file watchers, or freshness state. Instead, **stale** and **refresh** are ordinary direct messages with a body convention that workers interpret however they see fit.

This keeps the broker simple and lets each worker define its own cache-invalidation logic.

### Convention

Control messages use a JSON body with a `type` field:

- **`stale`** — tells a worker that a specific resource has changed and any cached state derived from it may be outdated.
- **`refresh`** — tells a worker that a scope of resources has been updated and it should re-read from the source.

The `scope` field is a path or identifier that narrows what the message applies to. Workers decide how to react — they might drop a cache entry, reload a file, re-run analysis, or ignore the message entirely.

### Example: Sending a Stale Message

A file has been edited. Notify the code reviewer worker that its cached analysis of that file is no longer valid:

```bash
# Find the reviewer's worker ID
dispatch team | jq -r '.workers[] | select(.role == "code.reviewer") | .id'

# Tell it the file is stale
dispatch send \
  --to <reviewer-worker-id> \
  --body '{"type":"stale","scope":"src/auth/login.rs","reason":"file modified"}' \
  --from watcher
```

The worker receives this in its `dispatch listen` loop:

```json
{
  "status": "ok",
  "message_id": "a1b2c3d4-...",
  "from": "watcher",
  "to": "f5e6d7c8-...",
  "body": "{\"type\":\"stale\",\"scope\":\"src/auth/login.rs\",\"reason\":\"file modified\"}"
}
```

### Example: Sending a Refresh Message

A folder's contents have been regenerated. Tell the test runner to re-scan for test files:

```bash
dispatch send \
  --to <test-runner-worker-id> \
  --body '{"type":"refresh","scope":"tests/","reason":"test fixtures regenerated"}' \
  --from ci-pipeline
```

### Example: Worker Reacting to Control Messages

A worker's listen loop can parse the body and take action based on the message type:

```bash
#!/usr/bin/env bash
# worker-loop.sh — a worker that reacts to stale/refresh messages

WORKER_ID="$1"

while true; do
  RESPONSE=$(dispatch listen --worker-id "$WORKER_ID" --timeout 60)
  STATUS=$(echo "$RESPONSE" | jq -r '.status')

  if [ "$STATUS" = "ok" ]; then
    BODY=$(echo "$RESPONSE" | jq -r '.body // empty')
    if [ -n "$BODY" ]; then
      MSG_TYPE=$(echo "$BODY" | jq -r '.type // empty')
      SCOPE=$(echo "$BODY" | jq -r '.scope // empty')

      case "$MSG_TYPE" in
        stale)
          echo "Cache invalidated for: $SCOPE" >&2
          # Drop cached analysis for the specific file
          rm -f ".cache/analysis/${SCOPE}.json" 2>/dev/null
          ;;
        refresh)
          echo "Refreshing scope: $SCOPE" >&2
          # Re-scan the directory for changes
          # (your refresh logic here)
          ;;
        *)
          echo "Received message: $BODY" >&2
          # Handle other message types
          ;;
      esac
    fi
  fi
  # Timeout responses are normal — just loop back to listen
done
```

## Filesystem Watcher

The `examples/watch-files.sh` script demonstrates how to observe filesystem changes and send stale/refresh messages through the Dispatch CLI. It uses [fswatch](https://emcrisostomo.github.io/fswatch/) to monitor paths and includes built-in debouncing to avoid flooding workers with rapid-fire events.

### Prerequisites

- `fswatch` — `brew install fswatch` (macOS) or `apt install fswatch` (Linux)
- `jq` — `brew install jq` (macOS) or `apt install jq` (Linux)
- `dispatch` CLI on PATH

### Quick Start

```bash
# Start the broker
dispatch serve &

# Register a worker
WORKER_ID=$(dispatch register --name reviewer --role code.reviewer --description "Reviews code" | jq -r '.worker_id')

# Start the watcher pointing at the worker
./examples/watch-files.sh --worker-id "$WORKER_ID" --from file-watcher src/
```

### How It Works

1. The watcher monitors one or more filesystem paths using `fswatch`
2. When a **file** changes, it sends a **stale** message with the file path as scope
3. When a **directory** changes, it sends a **refresh** message with the directory path as scope
4. A debounce window (default: 2 seconds) prevents duplicate messages for rapid edits
5. Messages are sent via `dispatch send` — no internal broker APIs are used

### Connecting to a Specific Worker

Use `dispatch team` to find workers by role, then pass the ID to the watcher:

```bash
# Find the code reviewer's worker ID
REVIEWER=$(dispatch team | jq -r '.workers[] | select(.role == "code.reviewer") | .id')

# Watch src/ and notify the reviewer of changes
./examples/watch-files.sh --worker-id "$REVIEWER" --from file-watcher src/

# Watch specific files with a longer debounce
./examples/watch-files.sh --worker-id "$REVIEWER" --debounce 5 --from file-watcher src/main.rs src/lib.rs
```

### Options

| Option | Default | Description |
|--------|---------|-------------|
| `--worker-id ID` | (required) | Target worker to notify |
| `--debounce SECS` | `2` | Seconds to wait before sending |
| `--from NAME` | `file-watcher` | Sender identity in messages |

### Key Points

- **Dispatch does not inspect message bodies.** The `stale`/`refresh` convention is purely between senders and receivers.
- **Any process can send control messages.** File watchers, CI pipelines, git hooks, or other workers can all use `dispatch send`.
- **Workers define their own reaction.** One worker might drop a cache entry; another might re-run an entire analysis pipeline. Dispatch doesn't prescribe behavior.
- **The `scope` field is free-form.** Use file paths, directory paths, module names, or any identifier meaningful to the worker.

## Multi-Role Workflow Example

The `examples/workflow.sh` script demonstrates a complete coding workflow with 5 workers coordinating in a single cell:

| Worker | Role | Responsibility |
|--------|------|----------------|
| planner | `planner` | Breaks down tasks into implementation plans |
| implementer | `code.implementer` | Writes code based on plans |
| reviewer | `code.reviewer` | Reviews code for quality and correctness |
| test-runner | `test.runner` | Runs tests and reports results |
| shipper | `code.shipper` | Ships approved and tested code |

### Pipeline Flow

```
caller -> planner -> implementer -> reviewer -> test.runner -> shipper
            |            |             |            |            |
         plan task    write code    review it    run tests     ship it
```

### Running the Example

```bash
# Build dispatch first
cargo build --release

# Run the full workflow
./examples/workflow.sh
```

The script handles everything automatically:

1. Starts the broker (`dispatch serve`)
2. Registers all 5 workers with their roles and capabilities
3. Each worker enters a `dispatch listen` loop waiting for work
4. A caller sends a task to the planner to kick off the pipeline
5. Each worker processes its message and passes the result to the next worker
6. The pipeline completes when the shipper finishes

### How It Works

- **No permission model.** Worker behavior comes from how the process is launched, not from Dispatch enforcing roles. The planner sends to the implementer because the script tells it to, not because Dispatch restricts messaging.
- **No external broker.** Everything runs on one machine using the embedded Unix domain socket broker.
- **Workers are independent processes.** Each worker runs in a background subshell with its own listen loop. They communicate only through `dispatch send` and `dispatch listen`.
- **Messages carry the workflow state.** Each worker receives context from the previous stage (the plan, the implementation, the review verdict, the test results) as a JSON message body.

### Extending the Workflow

To add a new stage (e.g., a linter between implementer and reviewer):

```bash
# Register the new worker
LINTER_ID=$(dispatch register \
  --name linter \
  --role code.linter \
  --description "Lints code for style issues" \
  | jq -r '.worker_id')

# In the implementer, send to linter instead of reviewer
dispatch send --to "$LINTER_ID" --from "$IMPLEMENTER_ID" --body "$implementation"

# In the linter, forward to reviewer after checking
dispatch send --to "$REVIEWER_ID" --from "$LINTER_ID" --body "$lint_result"
```
