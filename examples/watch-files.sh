#!/usr/bin/env bash
# watch-files.sh — Filesystem watcher that sends stale/refresh messages via Dispatch
#
# Watches one or more paths for changes and notifies a specific worker through the
# Dispatch CLI. Uses debouncing to avoid flooding the worker with rapid-fire events.
#
# Requirements:
#   - fswatch (macOS: brew install fswatch, Linux: apt install fswatch)
#   - jq
#   - dispatch CLI on PATH
#
# Usage:
#   ./watch-files.sh --worker-id <WORKER_ID> [--debounce SECS] [--from NAME] <PATH> [PATH...]
#
# Examples:
#   # Watch src/ and notify a code reviewer
#   WORKER_ID=$(dispatch team | jq -r '.workers[] | select(.role == "code.reviewer") | .id')
#   ./watch-files.sh --worker-id "$WORKER_ID" --from file-watcher src/
#
#   # Watch specific files with a 5-second debounce
#   ./watch-files.sh --worker-id abc123 --debounce 5 src/main.rs src/lib.rs
#
#   # Watch a test directory for folder-level refresh signals
#   ./watch-files.sh --worker-id "$WORKER_ID" --from ci-watcher tests/

set -euo pipefail

# --- Defaults ---
DEBOUNCE_SECS=2
FROM_IDENTITY="file-watcher"
WORKER_ID=""
WATCH_PATHS=()

# --- Color / TTY detection ---
if [ -t 2 ] && [ -z "${NO_COLOR:-}" ]; then
  RED='\033[0;31m'
  GREEN='\033[0;32m'
  YELLOW='\033[0;33m'
  RESET='\033[0m'
else
  RED=''
  GREEN=''
  YELLOW=''
  RESET=''
fi

log() { echo -e "${GREEN}[watch]${RESET} $*" >&2; }
warn() { echo -e "${YELLOW}[watch]${RESET} $*" >&2; }
die() { echo -e "${RED}[watch] error:${RESET} $*" >&2; exit 1; }

# --- Argument parsing ---
while [[ $# -gt 0 ]]; do
  case "$1" in
    --worker-id)
      WORKER_ID="$2"; shift 2 ;;
    --debounce)
      DEBOUNCE_SECS="$2"; shift 2 ;;
    --from)
      FROM_IDENTITY="$2"; shift 2 ;;
    --help|-h)
      echo "Usage: $0 --worker-id <ID> [--debounce SECS] [--from NAME] <PATH> [PATH...]"
      echo ""
      echo "Watches filesystem paths and sends stale/refresh messages to a Dispatch worker."
      echo ""
      echo "Options:"
      echo "  --worker-id ID    Target worker ID (required)"
      echo "  --debounce SECS   Seconds to wait before sending (default: 2)"
      echo "  --from NAME       Sender identity (default: file-watcher)"
      echo "  -h, --help        Show this help"
      echo ""
      echo "If a path is a directory, folder-level 'refresh' messages are sent."
      echo "If a path is a file, file-level 'stale' messages are sent."
      exit 0
      ;;
    -*)
      die "unknown option: $1" ;;
    *)
      WATCH_PATHS+=("$1"); shift ;;
  esac
done

# --- Validation ---
[ -z "$WORKER_ID" ] && die "missing required --worker-id"
[ ${#WATCH_PATHS[@]} -eq 0 ] && die "no paths to watch — provide at least one file or directory"
command -v fswatch >/dev/null 2>&1 || die "fswatch not found — install with: brew install fswatch (macOS) or apt install fswatch (Linux)"
command -v dispatch >/dev/null 2>&1 || die "dispatch CLI not found — build with: cargo build --release"
command -v jq >/dev/null 2>&1 || die "jq not found — install with: brew install jq (macOS) or apt install jq (Linux)"

# --- Debounce tracking ---
# Tracks the last time a message was sent per scope to avoid duplicates within the debounce window.
declare -A LAST_SENT

now_epoch() {
  date +%s
}

should_debounce() {
  local scope="$1"
  local now
  now=$(now_epoch)
  local last="${LAST_SENT[$scope]:-0}"
  local elapsed=$(( now - last ))
  if [ "$elapsed" -lt "$DEBOUNCE_SECS" ]; then
    return 0  # true — should debounce (skip)
  fi
  return 1  # false — ok to send
}

mark_sent() {
  local scope="$1"
  LAST_SENT[$scope]=$(now_epoch)
}

# --- Determine message type based on what changed ---
# If the changed path is inside a watched directory, send a file-level "stale" message.
# If the changed path IS a watched directory (or a directory itself), send a folder-level "refresh".
send_notification() {
  local changed_path="$1"

  # Determine if this is a file-level stale or folder-level refresh
  local msg_type="stale"
  local scope="$changed_path"
  local reason="file modified"

  if [ -d "$changed_path" ]; then
    msg_type="refresh"
    reason="directory changed"
  else
    # Check if any watched path is a parent directory of this file
    for watched in "${WATCH_PATHS[@]}"; do
      if [ -d "$watched" ]; then
        # Normalize: if the changed file is directly in the watched dir,
        # and it looks like a broad change, consider folder-level refresh
        local watched_abs
        watched_abs=$(cd "$watched" 2>/dev/null && pwd)
        local parent_dir
        parent_dir=$(dirname "$changed_path")
        if [ "$parent_dir" = "$watched_abs" ] || [ "$changed_path" = "$watched_abs" ]; then
          # Still file-level — individual file changed within watched dir
          msg_type="stale"
          reason="file modified"
        fi
      fi
    done
  fi

  # Apply debounce
  if should_debounce "$scope"; then
    return
  fi

  local body
  body=$(jq -cn --arg type "$msg_type" --arg scope "$scope" --arg reason "$reason" \
    '{type: $type, scope: $scope, reason: $reason}')

  log "sending $msg_type for: $scope"

  if dispatch send --to "$WORKER_ID" --body "$body" --from "$FROM_IDENTITY" >/dev/null 2>&1; then
    mark_sent "$scope"
  else
    warn "failed to send $msg_type message for: $scope"
  fi
}

# --- Main loop ---
log "watching ${#WATCH_PATHS[@]} path(s) for changes (debounce: ${DEBOUNCE_SECS}s)"
log "target worker: $WORKER_ID"
log "sender identity: $FROM_IDENTITY"
for p in "${WATCH_PATHS[@]}"; do
  if [ -d "$p" ]; then
    log "  directory: $p (folder-level refresh on dir changes, file-level stale on file changes)"
  else
    log "  file: $p (file-level stale)"
  fi
done
log "press Ctrl-C to stop"

# fswatch outputs one changed path per line.
# --latency sets fswatch's own batching window (complements our debounce).
fswatch --latency "${DEBOUNCE_SECS}" "${WATCH_PATHS[@]}" | while IFS= read -r changed; do
  send_notification "$changed"
done
