# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
- Monitor UI: events history drawer, messages tab, status taglines, ack-aware row colors
- Per-agent log files at `logs/<name>.log`

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
