#!/usr/bin/env bash
# workflow.sh — End-to-end multi-role coding workflow using Dispatch
#
# Demonstrates 5 workers coordinating a plan -> implement -> review -> test -> ship
# pipeline in a single cell, all on one machine with no external dependencies.
#
# Workers:
#   planner          — Receives a task, produces a plan, passes to implementer
#   code.implementer — Takes a plan, "implements" it, passes to reviewer
#   code.reviewer    — Reviews the implementation, passes to test runner
#   test.runner      — Runs tests against the implementation, passes to shipper
#   code.shipper     — Ships the result, reports completion
#
# Requirements:
#   - dispatch CLI on PATH (cargo build --release)
#   - jq (brew install jq / apt install jq)
#
# Usage:
#   ./examples/workflow.sh
#
# The script starts the broker, registers all 5 workers, kicks off a task,
# and runs each worker's listen loop to completion. All output goes to stderr
# so you can watch the workflow progress in real time.

set -euo pipefail

# --- Color / TTY detection ---
if [ -t 2 ] && [ -z "${NO_COLOR:-}" ]; then
  RED='\033[0;31m'
  GREEN='\033[0;32m'
  YELLOW='\033[0;33m'
  BLUE='\033[0;34m'
  MAGENTA='\033[0;35m'
  CYAN='\033[0;36m'
  BOLD='\033[1m'
  RESET='\033[0m'
else
  RED=''
  GREEN=''
  YELLOW=''
  BLUE=''
  MAGENTA=''
  CYAN=''
  BOLD=''
  RESET=''
fi

# --- Logging helpers (all to stderr) ---
log_planner()     { echo -e "${BLUE}[planner]${RESET}      $*" >&2; }
log_implementer() { echo -e "${GREEN}[implementer]${RESET}  $*" >&2; }
log_reviewer()    { echo -e "${YELLOW}[reviewer]${RESET}     $*" >&2; }
log_test()        { echo -e "${MAGENTA}[test.runner]${RESET}  $*" >&2; }
log_shipper()     { echo -e "${CYAN}[shipper]${RESET}      $*" >&2; }
log_system()      { echo -e "${BOLD}[workflow]${RESET}      $*" >&2; }
die()             { echo -e "${RED}[workflow] error:${RESET} $*" >&2; exit 1; }

# --- Prerequisite checks ---
command -v dispatch >/dev/null 2>&1 || die "dispatch CLI not found — build with: cargo build --release"
command -v jq >/dev/null 2>&1 || die "jq not found — install with: brew install jq (macOS) or apt install jq (Linux)"

# --- Cleanup on exit ---
BROKER_PID=""
cleanup() {
  log_system "shutting down..."
  if [ -n "$BROKER_PID" ] && kill -0 "$BROKER_PID" 2>/dev/null; then
    kill "$BROKER_PID" 2>/dev/null || true
    wait "$BROKER_PID" 2>/dev/null || true
  fi
  log_system "done."
}
trap cleanup EXIT

# --- Start the broker ---
log_system "starting broker..."
dispatch serve &
BROKER_PID=$!
sleep 1

if ! kill -0 "$BROKER_PID" 2>/dev/null; then
  die "broker failed to start"
fi
log_system "broker running (pid $BROKER_PID)"

# --- Register all 5 workers ---
log_system "registering workers..."

PLANNER_ID=$(dispatch register \
  --name planner \
  --role planner \
  --description "Breaks down tasks into implementation plans" \
  --capability "planning:Decomposes tasks into steps" \
  --capability "estimation:Estimates complexity" \
  | jq -r '.worker_id')
log_planner "registered (id: ${PLANNER_ID:0:8}...)"

IMPLEMENTER_ID=$(dispatch register \
  --name implementer \
  --role code.implementer \
  --description "Writes code based on plans" \
  --capability "coding:Writes production code" \
  --capability "refactoring:Restructures existing code" \
  | jq -r '.worker_id')
log_implementer "registered (id: ${IMPLEMENTER_ID:0:8}...)"

REVIEWER_ID=$(dispatch register \
  --name reviewer \
  --role code.reviewer \
  --description "Reviews code for quality and correctness" \
  --capability "review:Checks code quality" \
  --capability "security:Identifies security issues" \
  | jq -r '.worker_id')
log_reviewer "registered (id: ${REVIEWER_ID:0:8}...)"

RUNNER_ID=$(dispatch register \
  --name test-runner \
  --role test.runner \
  --description "Runs tests and reports results" \
  --capability "testing:Executes test suites" \
  --capability "coverage:Measures code coverage" \
  | jq -r '.worker_id')
log_test "registered (id: ${RUNNER_ID:0:8}...)"

SHIPPER_ID=$(dispatch register \
  --name shipper \
  --role code.shipper \
  --description "Ships approved and tested code" \
  --capability "deploy:Deploys to production" \
  --capability "release:Creates release artifacts" \
  | jq -r '.worker_id')
log_shipper "registered (id: ${SHIPPER_ID:0:8}...)"

log_system "all 5 workers registered"

# --- Show the team ---
log_system "current team:"
dispatch team | jq -r '.workers[] | "  \(.role)\t\(.name)\t\(.id[0:8])..."' >&2

# --- Worker functions ---
# Each worker waits for a message, processes it, and sends the result to the next worker.

run_planner() {
  log_planner "waiting for task..."
  local response
  response=$(dispatch listen --worker-id "$PLANNER_ID" --timeout 30)
  local status
  status=$(echo "$response" | jq -r '.status')

  if [ "$status" != "ok" ]; then
    die "planner: unexpected response: $response"
  fi

  local body
  body=$(echo "$response" | jq -r '.body // empty')
  local task
  task=$(echo "$body" | jq -r '.task // empty')

  log_planner "received task: $task"
  log_planner "creating implementation plan..."
  sleep 2

  # Simulate planning work
  local plan
  plan=$(jq -cn \
    --arg task "$task" \
    --arg step1 "Parse input requirements" \
    --arg step2 "Create data structures" \
    --arg step3 "Implement core logic" \
    --arg step4 "Add error handling" \
    '{task: $task, plan: [$step1, $step2, $step3, $step4], estimated_files: 2}')

  log_planner "plan ready, sending to implementer"

  dispatch send \
    --to "$IMPLEMENTER_ID" \
    --from "$PLANNER_ID" \
    --body "$plan" >/dev/null
}

run_implementer() {
  log_implementer "waiting for plan..."
  local response
  response=$(dispatch listen --worker-id "$IMPLEMENTER_ID" --timeout 30)
  local status
  status=$(echo "$response" | jq -r '.status')

  if [ "$status" != "ok" ]; then
    die "implementer: unexpected response: $response"
  fi

  local body
  body=$(echo "$response" | jq -r '.body // empty')
  local task
  task=$(echo "$body" | jq -r '.task // empty')
  local steps
  steps=$(echo "$body" | jq -r '.plan | length')

  log_implementer "received plan for: $task ($steps steps)"
  log_implementer "implementing..."
  sleep 3

  # Simulate implementation work
  local implementation
  implementation=$(jq -cn \
    --arg task "$task" \
    --arg files "src/feature.rs, src/feature_test.rs" \
    --arg lines_added "47" \
    --arg lines_removed "3" \
    '{task: $task, files_changed: $files, lines_added: ($lines_added|tonumber), lines_removed: ($lines_removed|tonumber), status: "implemented"}')

  log_implementer "implementation complete, sending to reviewer"

  dispatch send \
    --to "$REVIEWER_ID" \
    --from "$IMPLEMENTER_ID" \
    --body "$implementation" >/dev/null
}

run_reviewer() {
  log_reviewer "waiting for code to review..."
  local response
  response=$(dispatch listen --worker-id "$REVIEWER_ID" --timeout 30)
  local status
  status=$(echo "$response" | jq -r '.status')

  if [ "$status" != "ok" ]; then
    die "reviewer: unexpected response: $response"
  fi

  local body
  body=$(echo "$response" | jq -r '.body // empty')
  local task
  task=$(echo "$body" | jq -r '.task // empty')
  local files
  files=$(echo "$body" | jq -r '.files_changed // empty')

  log_reviewer "reviewing: $task (files: $files)"
  sleep 1
  log_reviewer "checking code quality..."
  sleep 1
  log_reviewer "checking security..."
  sleep 1

  # Simulate review work
  local review
  review=$(jq -cn \
    --arg task "$task" \
    --arg verdict "approved" \
    --arg note "Code is clean, no security issues found" \
    '{task: $task, review_verdict: $verdict, notes: $note, issues_found: 0}')

  log_reviewer "review complete: approved, sending to test runner"

  dispatch send \
    --to "$RUNNER_ID" \
    --from "$REVIEWER_ID" \
    --body "$review" >/dev/null
}

run_test_runner() {
  log_test "waiting for reviewed code..."
  local response
  response=$(dispatch listen --worker-id "$RUNNER_ID" --timeout 30)
  local status
  status=$(echo "$response" | jq -r '.status')

  if [ "$status" != "ok" ]; then
    die "test runner: unexpected response: $response"
  fi

  local body
  body=$(echo "$response" | jq -r '.body // empty')
  local task
  task=$(echo "$body" | jq -r '.task // empty')
  local verdict
  verdict=$(echo "$body" | jq -r '.review_verdict // empty')

  log_test "running tests for: $task (review: $verdict)"
  log_test "executing test suite..."
  sleep 2

  # Simulate test execution
  local results
  results=$(jq -cn \
    --arg task "$task" \
    --arg tests_run "12" \
    --arg tests_passed "12" \
    --arg tests_failed "0" \
    --arg coverage "94.2" \
    '{task: $task, tests_run: ($tests_run|tonumber), tests_passed: ($tests_passed|tonumber), tests_failed: ($tests_failed|tonumber), coverage_pct: ($coverage|tonumber), result: "pass"}')

  log_test "all tests passed (12/12, 94.2% coverage), sending to shipper"

  dispatch send \
    --to "$SHIPPER_ID" \
    --from "$RUNNER_ID" \
    --body "$results" >/dev/null
}

run_shipper() {
  log_shipper "waiting for tested code..."
  local response
  response=$(dispatch listen --worker-id "$SHIPPER_ID" --timeout 30)
  local status
  status=$(echo "$response" | jq -r '.status')

  if [ "$status" != "ok" ]; then
    die "shipper: unexpected response: $response"
  fi

  local body
  body=$(echo "$response" | jq -r '.body // empty')
  local task
  task=$(echo "$body" | jq -r '.task // empty')
  local test_result
  test_result=$(echo "$body" | jq -r '.result // empty')
  local coverage
  coverage=$(echo "$body" | jq -r '.coverage_pct // empty')

  log_shipper "shipping: $task (tests: $test_result, coverage: ${coverage}%)"
  sleep 1
  log_shipper "creating release..."
  sleep 1
  log_shipper "deploying..."
  sleep 1

  log_shipper "shipped successfully!"
}

# --- Start all workers listening in background ---
# Each worker runs in a background subshell, waiting for its message.
log_system ""
log_system "=== starting workflow: plan -> implement -> review -> test -> ship ==="
log_system ""

run_planner &
PLANNER_PID=$!

run_implementer &
IMPLEMENTER_PID=$!

run_reviewer &
REVIEWER_PID=$!

run_test_runner &
TEST_PID=$!

run_shipper &
SHIPPER_PID=$!

# Give workers a moment to start listening
sleep 1

# --- Kick off the workflow ---
# An external caller sends a task to the planner to start the pipeline.
TASK_BODY=$(jq -cn --arg task "Add user authentication endpoint" \
  '{task: $task, priority: "high", requester: "product-team"}')

log_system "sending task to planner: \"Add user authentication endpoint\""
dispatch send \
  --to "$PLANNER_ID" \
  --from "caller" \
  --body "$TASK_BODY" >/dev/null

# --- Wait for pipeline to complete ---
# Workers run sequentially through the pipeline: planner -> implementer -> reviewer -> test runner -> shipper
wait "$PLANNER_PID" 2>/dev/null || true
wait "$IMPLEMENTER_PID" 2>/dev/null || true
wait "$REVIEWER_PID" 2>/dev/null || true
wait "$TEST_PID" 2>/dev/null || true
wait "$SHIPPER_PID" 2>/dev/null || true

log_system ""
log_system "=== workflow complete ==="
log_system ""
log_system "pipeline: planner -> implementer -> reviewer -> test.runner -> shipper"
log_system "result: task shipped successfully"
