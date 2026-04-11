#!/usr/bin/env bash
# setup.sh — Launch the review-agent PRD review workflow in a tmux session.
#
# Opens a tmux session with three panes:
#   - Top:          dispatch broker
#   - Bottom-left:  Claude Code (PRD reviewer)
#   - Bottom-right: Claude Code (PRD writer)
#
# Usage:
#   ./examples/review-agent/setup.sh

set -euo pipefail

die() { echo "error: $*" >&2; exit 1; }

# --- Prerequisites ---
command -v dispatch >/dev/null 2>&1 || die "dispatch CLI not found — install with: curl -sSf https://raw.githubusercontent.com/codesoda/dispatch-cli/main/install.sh | sh"
command -v tmux >/dev/null 2>&1 || die "tmux is required — install with: brew install tmux (macOS) or apt install tmux (Linux)"
command -v claude >/dev/null 2>&1 || die "claude CLI not found — see https://docs.anthropic.com/en/docs/claude-code"
command -v jq >/dev/null 2>&1 || die "jq not found — install with: brew install jq (macOS) or apt install jq (Linux)"

# --- Resolve paths ---
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
COMMS="$SCRIPT_DIR/dispatch-comms.md"
WRITER="$SCRIPT_DIR/writer.prompt.md"
REVIEWER="$SCRIPT_DIR/reviewer.prompt.md"

SESSION="dispatch-prd"

# --- Kill existing session if present ---
tmux kill-session -t "$SESSION" 2>/dev/null || true

# --- Init config if needed ---
if [ ! -f "dispatch.config.toml" ]; then
  dispatch init >/dev/null 2>&1 || true
fi

# --- Build tmux layout ---
# Create session with the broker in the first pane
tmux new-session -d -s "$SESSION" -c "$(pwd)" \
  "echo '=== Dispatch Broker ===' && dispatch serve; read"

# Wait for broker socket to appear
echo "Waiting for broker to start..."
for _i in $(seq 1 50); do
  if ls /tmp/dispatch-cli/sockets/*.sock >/dev/null 2>&1; then
    break
  fi
  sleep 0.2
done
if ! ls /tmp/dispatch-cli/sockets/*.sock >/dev/null 2>&1; then
  tmux kill-session -t "$SESSION" 2>/dev/null || true
  die "broker socket did not appear within 10s"
fi
echo "Broker ready."

# Split bottom-left: reviewer
tmux split-window -v -t "$SESSION" -c "$(pwd)" \
  "claude \"Read the file $COMMS for how dispatch communication works. Then read $REVIEWER and follow its instructions. Start by registering yourself and listening for review requests.\""

# Split bottom-right: writer
tmux split-window -h -t "$SESSION" -c "$(pwd)" \
  "claude \"Read the file $COMMS for how dispatch communication works. Then read $WRITER and follow its instructions. Start by asking me what the PRD should be about.\""

# Make the broker pane smaller (top 20%)
tmux select-pane -t "$SESSION:0.0"
tmux resize-pane -y 8

# Focus on the writer pane (bottom-right) since the user interacts there
tmux select-pane -t "$SESSION:0.2"

# --- Attach ---
echo "Launching dispatch PRD review session..."
tmux attach -t "$SESSION"
