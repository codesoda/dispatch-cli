# Dispatch

Multi-agent, multi-vendor orchestration system.

## Rust CLI Best Practices

This project follows the patterns established across codesoda's Rust CLI projects (bugatti, agentmark, totp). These are the standards to follow when building and extending this CLI.

### Project Bootstrap

- Start with `cargo new`, add `clap` with derive feature: `cargo add clap -F derive`
- Use a lib/bin split: `src/lib.rs` exposes modules, `src/main.rs` is a thin wrapper that parses args and delegates
- Keep `Cargo.toml` clean: edition 2021, explicit feature flags, no wildcard dependencies
- **Always use async code** with `tokio` as the runtime. All I/O, HTTP, and command execution must be async. Use `#[tokio::main]` in `main.rs` and `async fn` throughout

### CLI Shape & Argument Parsing

- Use Clap's derive API for argument parsing
- Set `arg_required_else_help = true` so users never see a blank wall
- Document exit codes in `after_help`
- Link to docs/repo/llms.txt from `--help` output
- Use subcommands with typed flags for each command
- Each subcommand gets its own doc comment (shown in `--help`)

```rust
#[derive(Parser)]
#[command(name = "dispatch", version, about, arg_required_else_help = true,
    after_help = "\
Exit codes:
  0  Success
  1  Runtime error
  2  Configuration error

Docs: https://github.com/user/dispatch")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}
```

### Error Handling

- Use `thiserror` for typed, actionable error enums
- Every error message should say: what went wrong, where it happened, what to do about it
- Format: `"<problem> at <location> -- <remediation>"`
- For simple tools, `eprintln!` + `process::exit(N)` with a meaningful exit code is fine
- Never use `.unwrap()` in production paths

```rust
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("config not found at {path} -- run `dispatch init` first")]
    NotFound { path: PathBuf },
}
```

### Configuration

- Use `serde` + `toml` for config files
- Use `#[serde(deny_unknown_fields)]` to catch typos at parse time
- Layer precedence (highest to lowest): CLI flags > ENV vars > project file > global file > defaults
- Config path: `~/.config/dispatch/config.toml` or `~/.dispatch/config.toml`
- A good `init` command writes a config file with commented-out options so users discover features without docs

### Output & Color

- **stdout** is for data (machine-parseable). **stderr** is for status/progress
- Respect `NO_COLOR` environment variable
- Detect TTY before emitting ANSI codes: `std::io::stdout().is_terminal() && std::env::var("NO_COLOR").is_err()`
- Use semantic color constants, not inline escape codes scattered through the codebase
- Clean stdout means your CLI works with `|`, `jq`, `xargs`, `$()`

### Logging & Diagnostics

- Use `tracing` + `tracing-subscriber` for structured logging
- Log to files, not the terminal: use `RollingFileAppender` with daily rotation
- Priority: env var (`DISPATCH_LOG`) > config > default (`info`)
- Annotate command handlers with `#[instrument]` for automatic span context
- Set `with_ansi(false)` on file loggers

### Testing

- Split environment-dependent code from pure logic
- Inject I/O boundaries: accept `&mut dyn BufRead` / `&mut dyn Write` for interactive commands
- Pattern: `run_cmd(args)` (public, uses real env) -> `execute_cmd(deps, args)` (testable core, injected deps) -> returns typed result
- Use `tempfile` for temp directories, `assert_cmd` for binary integration tests
- Use trait-based dependency injection for external services (e.g., `ProcessRunner` trait for subprocess calls)
- Tests use mock implementations, not real external services

### CI (GitHub Actions)

- Four commands: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo build --all-targets`, `cargo test`
- Treat warnings as errors: `RUSTFLAGS: "-D warnings"`
- Use `Swatinem/rust-cache@v2` with `cache-on-failure: true`
- Set `CARGO_INCREMENTAL: "0"` for reproducible CI builds
- Use `concurrency` with `cancel-in-progress: true` to cancel stale runs
- Toolchain: `dtolnay/rust-toolchain@stable` with `components: rustfmt, clippy`

### Installation & Distribution

- Provide a dual-mode `install.sh`: detects if running from a cloned repo (build from source) or via `curl | sh` (download pre-built binary)
- Detect platform with `uname -s` / `uname -m`, map to Rust target triples
- Install to `~/.local/bin/` or a tool-specific bin dir, check PATH
- Handle macOS codesigning (`codesign --remove-signature` or ad-hoc sign for downloaded binaries)
- Shell install scripts need their own color/TTY detection

### Releases

- Tag-driven: push a git tag, CI builds binaries for each platform, creates a GitHub release
- Use a build matrix for cross-platform binaries (aarch64-apple-darwin, x86_64-unknown-linux-gnu, etc.)
- Generate SHA256 checksums for all artifacts
- Extract release notes from CHANGELOG.md automatically
- Keep CHANGELOG.md updated as you go, not at release time

### Update Checks

- Check for updates passively after successful runs, in a background thread with a short timeout
- Use the GitHub `/releases/latest` redirect trick to discover the latest tag (no API token needed)
- Guard conditions: skip if `DISPATCH_NO_UPDATE_CHECK=1`, skip if not a TTY (CI/piped), throttle to once per 24h
- Key crates: `semver` for version comparison, `self-replace` for atomic binary swap
- Use async HTTP (`reqwest` without `blocking` feature) for update checks

### Release Profile

```toml
[profile.release]
strip = true
lto = true
```

### Build Script

- Use `build.rs` to expose compile-time info (e.g., target triple via `cargo:rustc-env`)
- Use `include_dir!` if you need to embed assets at compile time

### Agent-Friendly Design

- Agents will be primary consumers of this CLI -- design for that
- `--help` must link to docs, repo, and llms.txt
- stdout is clean, parseable data with no decoration
- stderr for human-readable status messages
- Support `$ENV_VAR` syntax for secrets (keeps them out of shell history and process lists)
- Exit codes are documented and meaningful
- Every subcommand has its own `--help`

### Key Crates

| Crate                                 | Purpose                                   |
| ------------------------------------- | ----------------------------------------- |
| `clap` (derive)                       | Argument parsing                          |
| `serde` + `toml`                      | Config serialization                      |
| `thiserror`                           | Typed error enums                         |
| `tracing` + `tracing-subscriber`      | Structured logging                        |
| `reqwest` (rustls-tls, no `blocking`) | Async HTTP client                         |
| `semver`                              | Version comparison                        |
| `self-replace`                        | Atomic binary swap for updates            |
| `tempfile`                            | Temp dirs in tests                        |
| `assert_cmd`                          | Binary integration tests                  |
| `tokio` (full)                        | Async runtime (always — all I/O is async) |

### Code Quality Rules

- Run `cargo fmt --check && cargo clippy -- -D warnings && cargo build && cargo test` before committing
- No `.unwrap()` in non-test code
- Prefer `rustls-tls` over `native-tls` for fewer system dependencies
- Use `deny_unknown_fields` on all config structs
- Keep modules focused: one responsibility per file
- Command handlers return `Result<()>`, `main.rs` converts errors to stderr + non-zero exit

### Ralph Workflow

- After completing each step, commit and push to `origin` immediately
- Do not accumulate changes across steps — commit and push after every step
