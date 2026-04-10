# Dispatch

Multi-agent, multi-vendor orchestration system. A project-local CLI and embedded broker for coordinating long-lived agent sessions and lightweight scripts inside a single cell.

## Quick Start

```bash
# Build
cargo build --release

# Start the broker (in a separate terminal)
dispatch serve

# Register a worker
dispatch register --name my-worker --role coder --description "Writes code"

# Send a message to a worker
dispatch send --to <worker-id> --body "Hello, worker!"

# Listen for messages (long-poll)
dispatch listen --worker-id <worker-id>
```

## Architecture

Dispatch runs as a **cell** — a single broker process that coordinates workers on one machine. Workers register themselves, exchange direct messages, and use TTL-based liveness to stay active.

- **Broker**: Embedded server on a Unix domain socket (no external dependencies)
- **Workers**: Processes that register and communicate through the broker
- **Messages**: Direct, queued messages delivered via long-poll
- **Cell identity**: Derived from the project root path, overridable via config or CLI flag

## Commands

| Command | Description |
|---------|-------------|
| `dispatch serve` | Start the embedded broker |
| `dispatch register` | Register a worker with name, role, and capabilities |
| `dispatch team` | List active workers |
| `dispatch send` | Send a direct message to a worker |
| `dispatch listen` | Long-poll for incoming messages |
| `dispatch heartbeat` | Renew worker liveness TTL |

## Configuration

Dispatch searches upward from the current directory for `dispatch.config.toml`:

```toml
cell_id = "my-project"
```

Override precedence (highest wins): `--cell-id` flag > `DISPATCH_CELL_ID` env var > config file > derived from path.

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
