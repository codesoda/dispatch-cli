# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.5.1] - 2026-04-22

### Added
- `launch = false` agents with a `prompt_file` now use the same register-prompt-bootstrap as supervised agents. At serve startup dispatch pre-registers a worker server-side and prints a pasteable command that includes `DISPATCH_WORKER_ID=<uuid>` and redirects stdin from a one-line boot prompt (`Run: dispatch register --worker-id "$DISPATCH_WORKER_ID" ... --for-agent`). The agent's first tool call idempotently claims the pre-registered worker and retrieves the role prompt — same mechanism as supervised agents, just with the user launching the process.
- `AgentOrchestrator::pre_register_unmanaged(ctx, config)` — factored from the managed-spawn pre-register flow; returns `(worker_id, boot_prompt_path)` for the serve-time banner to render.
- `build_agent_command` accepts a `worker_id: Option<&str>` parameter; when `Some`, emits `DISPATCH_WORKER_ID=<id>` so the bootstrap works on paste.

### Changed
- **Breaking:** dropped the `--launch` flag on `dispatch serve`. `launch = true` in the agent config is now the single source of truth: those agents auto-spawn under the supervisor; `launch = false` agents are printed for copy-paste. Removes a redundant CLI-vs-config toggle.
- **Breaking:** removed `[main_agent]` / `MainAgentConfig`. The main-agent concept collapses into a regular `[[agents]]` entry with `launch = false` (plus a `prompt_file` if you want the bootstrap). Configs with `[main_agent]` now fail parse with `unknown field 'main_agent'` — migrate by renaming the table to `[[agents]]` and adding `launch = false`. The previous schema didn't support `adapter`, `extra_args`, `role`, `description`, or `ttl`, so the unified shape is strictly more expressive.
- Monitor dashboard: the pinned "main agent" slot is gone. All agents render uniformly; the hardcoded `role = "coordinator"` label for the main slot no longer exists (agents now show their configured role).
- `dispatch init` template updated to show a commented `[[agents]] launch = false + prompt_file` example for the coordinator-style use case.

## [0.5.0] - 2026-04-21

### Added
- Dispatch now pre-registers `launch = true` agents with a `prompt_file` server-side at spawn time, injects `DISPATCH_WORKER_ID` into the agent's environment, and feeds the model a one-line boot prompt (`Run: dispatch register --worker-id "$DISPATCH_WORKER_ID" ... --for-agent`). The model's first observable action is a real tool call by construction — eliminates a class of hallucination where agents skipped `dispatch register` and emitted fabricated status messages (issue #43).
- `BrokerRequest::Register` accepts `worker_id: Option<String>` and `role_prompt: Option<String>`. Re-registering an existing id with a matching name+role is an idempotent claim (same id returned, TTL renewed); a mismatched claim is rejected. The broker stores the supplied prompt keyed by worker id and returns it in the `WorkerRegistered` response so a spawned agent can fetch its role prompt as its first tool result.
- `dispatch register --worker-id <id>` — CLI flag for claiming a pre-assigned worker id.
- `dispatch register --role-prompt <body>` — CLI flag used by the orchestrator to ship prompt content into the broker.
- `dispatch register --for-agent` — routes the role prompt body to stdout and the JSON envelope to stderr. Exits nonzero when no prompt is stored so the supervisor can restart.
- `stream_json = true` on `[[agents]]` entries — launches the claude adapter with `--output-format stream-json --verbose` so per-tool-use entries appear in the agent log. Verification mechanism for the pre-register work: without it, hallucinated and real `dispatch register` calls are indistinguishable in the log.
- Supervisor re-registers managed workers on every respawn (idempotent claim renews TTL, fresh-creates if the record was garbage-collected during downtime).

### Changed
- `launch = false` agents are unchanged — they continue to register themselves via the legacy stdin-pipe flow. The new pre-register machinery is opt-in via `launch = true` + a configured `prompt_file`.
- `examples/dispatch-comms.md` splits the "Register yourself" guidance into managed (`$DISPATCH_WORKER_ID` present → claim with `--for-agent`) and unmanaged (legacy self-register) sections.

## [0.4.1] - 2026-04-19

### Added
- Adapter abstraction for agent launches — each `[[agents]]` entry declares `adapter = "command" | "claude" | "codex"` instead of embedding a shell one-liner
- `extra_args = [...]` on agents — adapter-appended argv without touching the launch string
- Per-agent `launch = true/false` flag (auto-start only agents with `launch = true`)
- Prompt files now piped as stdin for the `claude` and `codex` adapters (no more `< {prompt_file}` shell redirect)
- Introspection commands: `dispatch ack`, `dispatch status`, `dispatch heartbeat --status`, `dispatch events`, `dispatch messages`
- `dispatch agent {start,stop,restart} <name|worker-id>` — lifecycle control for managed agents over the broker socket; name or worker ID accepted
- Agent supervisor with exponential-backoff auto-restart (1s → 30s cap, 5 consecutive failures = crashed; counter resets after 30s stable)
- `dispatch codex-hook {stop,install,uninstall}` — writes `.codex/hooks.json` + enables `features.codex_hooks`; Stop handler emits a block decision so the agent keeps listening for dispatch messages
- `dispatch claude-hook {stop,install,uninstall}` — same pattern via `.claude/settings.local.json` (merges into existing `settings.json` if present)
- `POST /api/agents/{name}/start|stop|restart` — monitor UI and external callers can drive the orchestrator directly (local-loopback, unauthenticated; matches `/api/shutdown`)
- `Worker.status_history` — last 3 prior status taglines retained per worker with dedupe, surfaced via `/api/team`
- `AgentState::Running` now carries `started_at` (Unix seconds) alongside `pid` for client-side uptime
- Monitor dashboard: agent cards grid showing supervisor state per agent (running/starting/restarting/crashed/stopped), PID, short worker ID; clicking a card opens the existing agent detail view
- Expanded monitor agent cards: adapter badge, current + last 2 prior statuses, uptime, heartbeat age, Start/Stop/Restart buttons, and a Copy-command button for `launch = false` agents
- Toast feedback in the dashboard for action outcomes (disables buttons while in-flight)
- `GET /api/agents/state` — live supervisor state for each managed agent
- Monitor UI: events history drawer, messages tab, status taglines, ack-aware row colors
- Per-agent log files at `logs/<name>.log`

### Changed
- `dispatch {codex,claude}-hook stop` probes the broker socket before emitting the block decision: when dispatch is unreachable the hook prints nothing and exits 0 so the vendor can stop the agent cleanly
- `dispatch status --clear` no longer wipes the historical status buffer — clear is a display-level reset only

### Changed
- Agent configs require `adapter = "..."`; the old `command = "..."` shell-one-liner shape no longer parses and must be migrated

## [0.3.1] - 2026-04-16

### Added
- `open` option in `[monitor]` config — automatically opens the dashboard in your default browser on `dispatch serve`
- `--config <PATH>` global flag — specify an explicit config file path instead of requiring `dispatch.config.toml` in the current directory

### Changed
- Config file lookup no longer walks parent directories — it checks the current directory only, or uses the explicit `--config` path
- `project_root` is set to the directory containing the config file, so `prompt_file` and other relative paths resolve from the config location

## [0.3.0] - 2026-04-13

### Added
- Declarative agent orchestration via `[[agents]]` in `dispatch.config.toml` — agents auto-launch on `dispatch serve`
- `[main_agent]` config section — prints a ready-to-paste command for the interactive main session
- `[monitor]` config section — configure the dashboard port in config instead of only via CLI flag
- `dispatch register --ttl <SECONDS>` — override the default 5-minute worker TTL per registration
- `--from <WORKER_ID>` global CLI flag — identifies the calling worker and renews TTL on every command
- Scheduled heartbeats via `[[heartbeats]]` in config — run commands on a timer with optional `after` delay
- `name` field in config — human-readable project name shown in the monitor dashboard title
- Monitor dashboard redesigned: sidebar navigation, dashboard stats, agent detail view, event volume sparkline
- `GET /api/health` — returns server timestamps, uptime, message/request stats, version
- `GET /api/agents` — returns configured agent definitions, main agent, and heartbeat configs
- `POST /api/shutdown` — stop the server from the monitor UI (with confirmation)
- Stop Server button in monitor header with connection state indicator
- Event payloads now include full structured data (from, to, body) visible in console and web UI
- Console output during `dispatch serve` shows timestamped event log for all broker activity
- Broker tracks `messages_sent`, `messages_delivered`, `requests_handled` counters
- Agent subprocesses receive `DISPATCH_CELL_ID`, `DISPATCH_SOCKET_PATH`, `DISPATCH_MONITOR_URL`, `DISPATCH_AGENT_NAME`, `DISPATCH_AGENT_ROLE` env vars
- Embedded dispatch-comms instructions auto-prepended to LLM agent prompts

### Changed
- Agent processes spawn in their own process group for reliable cleanup of entire process trees
- Shutdown kills process groups (SIGTERM then SIGKILL) and reaps children to prevent zombies
- Integration tests use RAII drop guard for broker cleanup (no more orphaned test processes)
- Monitor UI sends absolute UTC timestamps; client ticks uptime/TTL locally every second
- TTL renewed on `team`, `send`, and `listen` commands when `--from` is provided

## [0.2.0] - 2026-04-12

### Added
- `dispatch serve --monitor <PORT>` — optional HTTP dashboard showing live team list and event stream via SSE
- Dark-themed monitor UI embedded in the binary (no external files), auto-refreshes team every 2s

## [0.1.1] - 2026-04-11

### Fixed
- Broker socket path moved to `/tmp/dispatch-cli/sockets/` to avoid Unix domain socket `SUN_LEN` limit (104 bytes on macOS) when project paths are deeply nested

### Changed
- Workflow example now includes simulated execution delays for a more realistic demo (~14s total pipeline)

## [0.1.0] - 2026-04-11

### Added
- CLI skeleton with Clap derive API (`dispatch --help`, `--version`)
- Configuration resolution with `dispatch init` to scaffold `dispatch.toml`
- Embedded local broker server with `dispatch serve`
- Worker registration, team listing, and heartbeat monitoring
- Direct message sending between workers
- Long-poll listen loop for receiving messages
- Stale worker detection and refresh control messages
- Trait-based backend module with local implementation
- Binary integration tests for all CLI commands
- GitHub Actions CI workflow (fmt, clippy, build, test)

[Unreleased]: https://github.com/codesoda/dispatch-cli/compare/v0.3.1...HEAD
[0.3.1]: https://github.com/codesoda/dispatch-cli/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/codesoda/dispatch-cli/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/codesoda/dispatch-cli/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/codesoda/dispatch-cli/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/codesoda/dispatch-cli/releases/tag/v0.1.0
