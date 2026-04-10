# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/codesoda/dispatch-cli/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/codesoda/dispatch-cli/releases/tag/v0.1.0
