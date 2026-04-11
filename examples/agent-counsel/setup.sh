#!/usr/bin/env bash
# setup.sh — Launch the agent counsel in a tmux session.
#
# Opens a tmux session with two windows:
#   Window 1 "counsel":  chair + 4 counselors (2x2 grid + chair)
#   Window 2 "main":     main agent (user interacts here)
#   Broker runs in the background.
#
# Usage:
#   ./examples/agent-counsel/setup.sh

set -euo pipefail

die() { echo "error: $*" >&2; exit 1; }

# --- Prerequisites ---
command -v dispatch >/dev/null 2>&1 || die "dispatch CLI not found"
command -v tmux >/dev/null 2>&1 || die "tmux is required — brew install tmux"
command -v claude >/dev/null 2>&1 || die "claude CLI not found"
command -v jq >/dev/null 2>&1 || die "jq not found — brew install jq"

# --- Resolve paths ---
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
COMMS="$SCRIPT_DIR/dispatch-comms.md"

SESSION="agent-counsel"

# --- Kill existing session if present ---
tmux kill-session -t "$SESSION" 2>/dev/null || true

# --- Clean up any stale broker ---
rm -f /tmp/dispatch-cli/sockets/*.sock 2>/dev/null

# --- Init config in example directory ---
if [ ! -f "$SCRIPT_DIR/dispatch.config.toml" ]; then
  (cd "$SCRIPT_DIR" && dispatch init >/dev/null 2>&1) || true
fi

# --- Start broker in background ---
echo "Starting broker..."
(cd "$SCRIPT_DIR" && dispatch serve) &
BROKER_PID=$!

for _i in $(seq 1 50); do
  if ls /tmp/dispatch-cli/sockets/*.sock >/dev/null 2>&1; then break; fi
  sleep 0.2
done
if ! ls /tmp/dispatch-cli/sockets/*.sock >/dev/null 2>&1; then
  kill "$BROKER_PID" 2>/dev/null || true
  die "broker socket did not appear within 10s"
fi
echo "Broker ready (pid $BROKER_PID)."

# --- Helper to build claude launch command ---
agent_cmd() {
  local model="$1"
  local prompt_file="$2"
  local extra="$3"
  echo "claude --model $model \"Read the file $COMMS for how dispatch communication works. Then read $prompt_file and follow its instructions. $extra\""
}

# --- Window 1: counsel (chair + 4 counselors) ---

# Pane 0: chair
tmux new-session -d -s "$SESSION" -n "counsel" -c "$SCRIPT_DIR" \
  "$(agent_cmd sonnet "$SCRIPT_DIR/chair.prompt.md" "Start by registering, finding all counselors via dispatch team, then listening for questions.")"

# Pane 1: musk (right of chair)
tmux split-window -h -t "$SESSION:counsel.0" -c "$SCRIPT_DIR" \
  "$(agent_cmd sonnet "$SCRIPT_DIR/counselor-musk.prompt.md" "Start by registering and listening.")"

# Pane 2: jobs (below chair)
tmux split-window -v -t "$SESSION:counsel.0" -c "$SCRIPT_DIR" \
  "$(agent_cmd sonnet "$SCRIPT_DIR/counselor-jobs.prompt.md" "Start by registering and listening.")"

# Pane 3: gates (below musk)
tmux split-window -v -t "$SESSION:counsel.1" -c "$SCRIPT_DIR" \
  "$(agent_cmd sonnet "$SCRIPT_DIR/counselor-gates.prompt.md" "Start by registering and listening.")"

# Pane 4: bezos (below jobs)
tmux split-window -v -t "$SESSION:counsel.2" -c "$SCRIPT_DIR" \
  "$(agent_cmd sonnet "$SCRIPT_DIR/counselor-bezos.prompt.md" "Start by registering and listening.")"

# --- Window 2: main agent (user interacts here) ---

tmux new-window -t "$SESSION" -n "main" -c "$SCRIPT_DIR" \
  "$(agent_cmd opus "$SCRIPT_DIR/main.prompt.md" "Start by asking the user what question they want to put to the counsel.")"

# --- Focus on main window ---
tmux select-window -t "$SESSION:main"

# --- Cleanup broker on session exit ---
tmux set-hook -t "$SESSION" session-closed "run-shell 'kill $BROKER_PID 2>/dev/null || true'"

# --- Attach ---
echo "Launching agent counsel..."
echo "  Ctrl+B N  — switch to counsel window to watch the advisors"
echo "  Ctrl+B P  — switch back to main window"
echo "  Ctrl+B D  — detach from session"
tmux attach -t "$SESSION"
