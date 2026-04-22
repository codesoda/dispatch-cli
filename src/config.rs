use std::collections::hash_map::DefaultHasher;
use std::env;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::errors::DispatchError;

/// Runtime configuration for Dispatch, resolved from multiple sources.
#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    /// Human-readable project name (shown in monitor dashboard).
    pub name: Option<String>,
    /// The cell identity for this project.
    pub cell_id: String,
    /// Backend URL (if configured).
    pub backend: Option<String>,
    /// The project root (directory containing dispatch.config.toml, or cwd).
    pub project_root: PathBuf,
    /// Absolute path of the `dispatch.config.toml` that produced this
    /// resolution, or `None` when no config file was found. Propagated to
    /// spawned agents via `DISPATCH_CONFIG_PATH` so child `dispatch` calls
    /// resolve the same config even when their cwd doesn't contain it.
    pub config_file_path: Option<PathBuf>,
    /// Working directory for agents. Defaults to project_root, overridden by `cwd` in config.
    pub agent_cwd: PathBuf,
    /// Monitor dashboard port (from config or CLI flag).
    pub monitor_port: Option<u16>,
    /// Open the monitor dashboard in a browser on serve.
    pub monitor_open: bool,
    /// Default TTL in seconds for agents that don't specify one.
    pub default_ttl: Option<u64>,
    /// Agent definitions to launch on serve.
    pub agents: Vec<ResolvedAgentConfig>,
    /// Scheduled heartbeat commands.
    pub heartbeats: Vec<HeartbeatConfig>,
}

/// Agent config after prompt_file has been resolved to prompt text.
#[derive(Debug, Clone)]
pub struct ResolvedAgentConfig {
    pub name: String,
    pub role: String,
    pub description: String,
    pub adapter: crate::adapter::Adapter,
    /// Full shell command — set only for `adapter = Command`.
    pub command: Option<String>,
    /// Extra args appended to the adapter-assembled argv (claude/codex).
    pub extra_args: Vec<String>,
    pub prompt: Option<String>,
    /// The resolved absolute path to the prompt file, if one was specified.
    /// Used by adapters as stdin source (claude/codex) or for `{prompt_file}`
    /// substitution in command-adapter shell strings.
    pub prompt_file_path: Option<PathBuf>,
    pub ttl: Option<u64>,
    /// Issue #43: when true, the claude adapter is launched with
    /// `--output-format stream-json --verbose` so per-tool-use entries
    /// appear in the agent log.
    pub stream_json: bool,
    /// Whether `dispatch serve` should auto-launch and supervise this agent.
    pub launch: bool,
    /// When true, the adapter omits its headless flag (`-p` for claude,
    /// `exec` for codex) so the agent opens in REPL / interactive mode.
    /// Mutually exclusive with `launch = true` (an interactive REPL can't
    /// be supervised); when both are set, `launch` wins with a warning.
    pub interactive: bool,
}

/// On-disk config file shape.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    /// Human-readable project name.
    pub name: Option<String>,
    /// Explicit cell identity override.
    pub cell_id: Option<String>,
    /// Backend URL.
    pub backend: Option<String>,
    /// Working directory for agents. Relative paths are resolved from the
    /// config file's directory. If omitted, agents run from the config
    /// file's directory.
    pub cwd: Option<String>,
    /// Default TTL in seconds for agents that don't specify one.
    pub default_ttl: Option<u64>,
    /// Monitor dashboard configuration.
    pub monitor: Option<MonitorConfig>,
    /// Agent definitions to launch on serve.
    #[serde(default)]
    pub agents: Vec<AgentConfig>,
    /// Scheduled heartbeat commands.
    #[serde(default)]
    pub heartbeats: Vec<HeartbeatConfig>,
}

/// On-disk heartbeat (scheduled command) definition.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeartbeatConfig {
    /// Name for this heartbeat (shown in monitor/logs).
    pub name: String,
    /// Shell command to execute.
    pub command: String,
    /// Interval in seconds between executions.
    pub every: u64,
    /// Initial delay in seconds before the first execution.
    #[serde(default)]
    pub after: Option<u64>,
}

/// On-disk monitor configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MonitorConfig {
    pub port: u16,
    /// Open the dashboard in the default browser on serve.
    #[serde(default)]
    pub open: bool,
}

/// On-disk agent definition.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    pub name: String,
    pub role: String,
    pub description: String,
    /// Which adapter to use: `command`, `claude`, or `codex`.
    pub adapter: crate::adapter::Adapter,
    /// Full shell command — required when `adapter = "command"`, ignored otherwise.
    pub command: Option<String>,
    /// Extra args appended to the adapter-assembled argv (claude/codex).
    #[serde(default)]
    pub extra_args: Vec<String>,
    pub prompt: Option<String>,
    pub prompt_file: Option<String>,
    pub ttl: Option<u64>,
    /// Whether `dispatch serve` should auto-start this agent under the
    /// supervisor. `false` (the default) prints a copy-paste command at
    /// startup instead so you can run the agent yourself.
    #[serde(default)]
    pub launch: bool,
    /// Issue #43: when true, the claude adapter is launched with
    /// `--output-format stream-json --verbose` so per-tool-use entries
    /// appear in the agent log. Verification mechanism — without it, a
    /// hallucinated register call and a real one are visually identical
    /// in the log. Default off so logs stay quiet for normal use.
    #[serde(default)]
    pub stream_json: bool,
    /// When true, the adapter omits its headless flag (`-p` for claude,
    /// `exec` for codex) so the agent opens in REPL / interactive mode.
    /// Mutually exclusive with `launch = true`; resolution emits a warning
    /// and falls back to non-interactive + launch when both are set.
    #[serde(default)]
    pub interactive: bool,
}

/// Resolve an agent config by reading prompt_file if specified and validating
/// adapter-specific requirements.
fn resolve_agent_config(
    agent: &AgentConfig,
    project_root: &Path,
) -> Result<ResolvedAgentConfig, DispatchError> {
    use crate::adapter::Adapter;

    // Reject names that can't be used as a single on-disk filename
    // component. The HTTP boundaries (`api_agent_start/stop/restart`) already
    // gate on `is_safe_name`, but the `launch_all` / `spawn_agent` path
    // derives the issue-#43 boot-prompt filename from `sanitize_name`, which
    // lossily collapses non-`[A-Za-z0-9_-]` characters to `_`. Two configs
    // like `alice/foo` and `alice_foo` would both map to
    // `alice_foo.boot.prompt`, silently overwriting each other. Enforce the
    // same rule at config time so both paths use the identical gate.
    if !crate::backend::orchestrator::is_safe_name(&agent.name) {
        return Err(DispatchError::AgentConfigError {
            name: agent.name.clone(),
            reason:
                "agent name must be non-empty and contain only ASCII alphanumerics, '-', or '_'"
                    .into(),
        });
    }

    if agent.adapter == Adapter::Command && agent.command.is_none() {
        return Err(DispatchError::AgentConfigError {
            name: agent.name.clone(),
            reason: "adapter = \"command\" requires `command = \"...\"`".into(),
        });
    }

    // Claude/codex adapters pipe prompts as stdin from `prompt_file`. An
    // inline `prompt = "..."` would be silently dropped, launching the agent
    // with empty stdin; reject it up front so the misconfiguration surfaces.
    if matches!(agent.adapter, Adapter::Claude | Adapter::Codex)
        && agent.prompt.is_some()
        && agent.prompt_file.is_none()
    {
        return Err(DispatchError::AgentConfigError {
            name: agent.name.clone(),
            reason: format!(
                "adapter = \"{}\" requires `prompt_file = \"...\"` (inline `prompt` is not supported on this adapter)",
                agent.adapter
            ),
        });
    }

    let (prompt, prompt_file_path) = match (&agent.prompt, &agent.prompt_file) {
        (Some(_), Some(_)) => {
            return Err(DispatchError::AgentConfigError {
                name: agent.name.clone(),
                reason: "cannot specify both 'prompt' and 'prompt_file'".into(),
            });
        }
        (Some(p), None) => (Some(p.clone()), None),
        (None, Some(path)) => {
            let full_path = project_root.join(path);
            let content = std::fs::read_to_string(&full_path).map_err(|_| {
                DispatchError::PromptFileNotFound {
                    name: agent.name.clone(),
                    path: full_path.clone(),
                }
            })?;
            let abs_path = full_path.canonicalize().unwrap_or(full_path);
            (Some(content), Some(abs_path))
        }
        (None, None) => (None, None),
    };

    // An interactive REPL can't run under the supervisor — the supervisor
    // owns stdin/stdout and the whole point of interactive mode is a TTY.
    // Rather than fail the whole config, warn loudly and keep `launch = true`
    // (the more useful default for an auto-start workflow). The user's
    // original intent — "run this agent interactively" — would have
    // required them to drop `launch = true` anyway.
    let interactive = if agent.interactive && agent.launch {
        eprintln!(
            "dispatch: warning: agent '{}' has both `interactive = true` and `launch = true`; \
             these are mutually exclusive — keeping `launch = true` (headless) and ignoring `interactive`.",
            agent.name
        );
        false
    } else {
        agent.interactive
    };

    Ok(ResolvedAgentConfig {
        name: agent.name.clone(),
        role: agent.role.clone(),
        description: agent.description.clone(),
        adapter: agent.adapter,
        command: agent.command.clone(),
        extra_args: agent.extra_args.clone(),
        prompt,
        prompt_file_path,
        ttl: agent.ttl,
        stream_json: agent.stream_json,
        launch: agent.launch,
        interactive,
    })
}

/// Find `dispatch.config.toml` in the current directory.
/// Returns the path to the config file and the directory containing it.
pub fn find_config_file(cwd: &Path) -> Option<(PathBuf, PathBuf)> {
    let candidate = cwd.join("dispatch.config.toml");
    if candidate.is_file() {
        Some((candidate, cwd.to_path_buf()))
    } else {
        None
    }
}

/// Load and parse a config file from disk.
pub fn load_config_file(path: &Path) -> Result<ConfigFile, DispatchError> {
    let contents = std::fs::read_to_string(path).map_err(|_| DispatchError::ConfigNotFound {
        path: path.to_path_buf(),
    })?;
    toml::from_str(&contents).map_err(|e| DispatchError::ConfigInvalid {
        path: path.to_path_buf(),
        reason: e.to_string(),
    })
}

/// Derive a stable cell ID by hashing the canonical project root path.
pub fn derive_cell_id(project_root: &Path) -> String {
    let canonical = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());
    let mut hasher = DefaultHasher::new();
    canonical.to_string_lossy().hash(&mut hasher);
    let hash = hasher.finish();
    format!("cell-{hash:016x}")
}

/// Config file template written by `dispatch init`.
const CONFIG_TEMPLATE: &str = "\
# Dispatch configuration
# https://github.com/codesoda/dispatch-cli

# Human-readable project name (shown in monitor dashboard).
# name = \"My Project\"

# Cell identity for this project.
# If omitted, a stable ID is derived from the project directory path.
# Override precedence: --cell-id flag > DISPATCH_CELL_ID env var > this value > derived
# cell_id = \"my-project\"

# Default TTL in seconds for agents that don't specify their own (default: 3600)
# default_ttl = 3600

# Monitor dashboard — starts an HTTP dashboard on serve
# [monitor]
# port = 8384
# open = true  # open the dashboard in your default browser

# Agent definitions — auto-started by `dispatch serve` when launch = true.
#
# When `launch = true` AND `prompt_file` is set (the managed-agent flow),
# dispatch pre-registers the worker server-side at spawn time, injects
# DISPATCH_WORKER_ID into the agent's environment, and feeds the agent a
# one-line boot prompt. The first thing the model does is run
# `dispatch register --worker-id \"$DISPATCH_WORKER_ID\" ... --for-agent`,
# whose response body is the contents of `prompt_file` — so the role prompt
# lands in the model's tool result instead of being narrated up front (this
# kills a class of hallucination where the model fakes the register step).
#
# When `launch = false`, dispatch prints the command for you to copy into a
# separate terminal and the agent registers itself the legacy way.
#
# [[agents]]
# name = \"reviewer\"
# role = \"code-reviewer\"
# description = \"Reviews code changes\"
# adapter = \"claude\"                            # one of: command | claude | codex
# extra_args = [\"--model\", \"sonnet\"]          # appended to the adapter's argv
# prompt_file = \"prompts/reviewer.md\"            # role prompt body (see above)
# launch = true
# ttl = 3600
# stream_json = false                            # when true, claude is launched with
#                                                # `--output-format stream-json --verbose`
#                                                # so per-tool-use entries appear in the
#                                                # agent log (issue #43 verification).
#
# # `command` adapter — for bash-script / non-LLM workers:
# [[agents]]
# name = \"bash-worker\"
# role = \"worker\"
# description = \"Scripted worker\"
# adapter = \"command\"
# command = \"scripts/worker.sh --verbose\"
# launch = true

# Interactive coordinator agent — printed as a ready-to-paste command at
# serve startup. `launch = false` (the default) keeps the orchestrator out
# of its lifecycle; you run it yourself in a terminal. If a `prompt_file`
# is set, dispatch pre-registers a worker server-side and the printed
# command uses the issue-#43 boot-prompt bootstrap — the agent's first
# tool call is `dispatch register --for-agent`, which returns the prompt
# body from the broker rather than embedding it in a multi-kB shell string.
# [[agents]]
# name = \"coordinator\"
# role = \"coordinator\"
# description = \"Coordinates chat between the user and other agents\"
# adapter = \"claude\"
# extra_args = [\"--dangerously-skip-permissions\", \"--model\", \"sonnet\"]
# prompt_file = \"prompts/coordinator.md\"
# launch = false
# interactive = true                            # drops the headless flag
#                                                # (`-p` for claude, `exec` for
#                                                # codex) so the printed
#                                                # copy-paste command opens the
#                                                # vendor CLI's REPL. Mutually
#                                                # exclusive with launch = true.
# ttl = 7200

# Scheduled heartbeats — commands run on a timer while the broker is running
# [[heartbeats]]
# name = \"check-prs\"
# command = \"dispatch send --to $GITHUB_AGENT --body '{\\\"type\\\":\\\"check_prs\\\"}'\"
# every = 120
# after = 30  # optional: wait this long before the first execution
";

/// Create a `dispatch.config.toml` in `cwd` with commented-out defaults.
///
/// Returns the path to the created file.
/// Errors if the file already exists in `cwd`.
/// Warns on stderr if a config exists in a parent directory.
pub fn init_config(cwd: &Path) -> Result<PathBuf, DispatchError> {
    let config_path = cwd.join("dispatch.config.toml");

    if config_path.is_file() {
        return Err(DispatchError::ConfigAlreadyExists { path: config_path });
    }

    std::fs::write(&config_path, CONFIG_TEMPLATE)?;
    Ok(config_path)
}

/// Resolve configuration with full precedence:
/// CLI flag > env var > config file > derived fallback.
///
/// If `cli_config_path` is provided, that file is loaded directly and
/// `project_root` is set to its parent directory.  Otherwise we look for
/// `dispatch.config.toml` in `cwd`.
pub fn resolve_config(
    cli_cell_id: Option<&str>,
    cli_config_path: Option<&Path>,
    cwd: &Path,
) -> Result<ResolvedConfig, DispatchError> {
    let env_cell_id = env::var("DISPATCH_CELL_ID").ok();
    let env_config_path = env::var("DISPATCH_CONFIG_PATH").ok();
    resolve_config_with_env(
        cli_cell_id,
        env_cell_id.as_deref(),
        cli_config_path,
        env_config_path.as_deref(),
        cwd,
    )
}

/// Env-parameterized entry point. Keeps `std::env::set_var` out of tests —
/// see `hooks::resolve_socket_path_with_env` (src/hooks/mod.rs) for the
/// same pattern.
fn resolve_config_with_env(
    cli_cell_id: Option<&str>,
    env_cell_id: Option<&str>,
    cli_config_path: Option<&Path>,
    env_config_path: Option<&str>,
    cwd: &Path,
) -> Result<ResolvedConfig, DispatchError> {
    // Treat an empty env value the same as unset (matches
    // resolve_socket_path_with_env).  `cell_id` handling below does NOT
    // currently filter empty; leaving that unchanged in this change to
    // keep scope tight.
    let env_config_path = env_config_path.filter(|s| !s.is_empty()).map(Path::new);
    let effective_config_path = cli_config_path.or(env_config_path);
    resolve_config_inner(cli_cell_id, env_cell_id, effective_config_path, cwd)
}

/// Absolutize `path` against `cwd` without touching the filesystem. Used as
/// a fallback when `canonicalize()` fails on a path that nonetheless loaded
/// successfully (rare — permissions on an ancestor dir).
fn absolutize(cwd: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn resolve_config_inner(
    cli_cell_id: Option<&str>,
    env_cell_id: Option<&str>,
    cli_config_path: Option<&Path>,
    cwd: &Path,
) -> Result<ResolvedConfig, DispatchError> {
    // Locate config: explicit --config path, or dispatch.config.toml in cwd.
    // Canonicalize only AFTER load succeeds so we never propagate a garbage
    // relative path via DISPATCH_CONFIG_PATH. `project_root` stays derived
    // from the raw (non-canonicalized) path so downstream path-joins match
    // the pre-change behavior bit-for-bit (canonicalize resolves symlinks
    // like `/var` → `/private/var` on macOS and would surprise callers).
    let (config_file, project_root, config_file_path) = if let Some(path) = cli_config_path {
        let config = load_config_file(path)?;
        let abs = path
            .canonicalize()
            .unwrap_or_else(|_| absolutize(cwd, path));
        let root = path.parent().unwrap_or(cwd).to_path_buf();
        (Some(config), root, Some(abs))
    } else if let Some((config_path, root)) = find_config_file(cwd) {
        let config = load_config_file(&config_path)?;
        let abs = config_path.canonicalize().unwrap_or(config_path);
        (Some(config), root, Some(abs))
    } else {
        (None, cwd.to_path_buf(), None)
    };

    // Resolve cell_id with precedence: CLI > env > config > derived
    let cell_id = if let Some(id) = cli_cell_id {
        id.to_string()
    } else if let Some(id) = env_cell_id {
        id.to_string()
    } else if let Some(ref config) = config_file {
        if let Some(ref id) = config.cell_id {
            id.clone()
        } else {
            derive_cell_id(&project_root)
        }
    } else {
        derive_cell_id(&project_root)
    };

    // Extract fields from config file
    let (name, backend, default_ttl, config_cwd, monitor_config, raw_agents, heartbeats) =
        match config_file {
            Some(c) => (
                c.name,
                c.backend,
                c.default_ttl,
                c.cwd,
                c.monitor,
                c.agents,
                c.heartbeats,
            ),
            None => (None, None, None, None, None, vec![], vec![]),
        };

    // Resolve agent working directory: config cwd (relative to project_root) or project_root
    let agent_cwd = if let Some(ref cwd_path) = config_cwd {
        let resolved = project_root.join(cwd_path);
        resolved.canonicalize().unwrap_or(resolved)
    } else {
        project_root.clone()
    };
    let monitor_port = monitor_config.as_ref().map(|m| m.port);
    let monitor_open = monitor_config.as_ref().is_some_and(|m| m.open);

    // Resolve agent prompt files
    let agents: Vec<ResolvedAgentConfig> = raw_agents
        .iter()
        .map(|a| resolve_agent_config(a, &project_root))
        .collect::<Result<_, _>>()?;

    Ok(ResolvedConfig {
        name,
        cell_id,
        backend,
        project_root,
        config_file_path,
        agent_cwd,
        monitor_port,
        monitor_open,
        default_ttl,
        agents,
        heartbeats,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_find_config_in_current_dir() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("dispatch.config.toml");
        fs::write(&config_path, "").unwrap();

        let result = find_config_file(tmp.path());
        assert!(result.is_some());
        let (path, root) = result.unwrap();
        assert_eq!(path, config_path);
        assert_eq!(root, tmp.path());
    }

    #[test]
    fn test_find_config_does_not_walk_parents() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("dispatch.config.toml");
        fs::write(&config_path, "").unwrap();

        let child = tmp.path().join("subdir");
        fs::create_dir(&child).unwrap();

        let result = find_config_file(&child);
        assert!(result.is_none());
    }

    #[test]
    fn test_find_config_not_found() {
        let tmp = TempDir::new().unwrap();
        let result = find_config_file(tmp.path());
        assert!(result.is_none());
    }

    #[test]
    fn test_load_config_file_valid() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("dispatch.config.toml");
        fs::write(
            &config_path,
            r#"
cell_id = "my-cell"
backend = "https://example.com"
"#,
        )
        .unwrap();

        let config = load_config_file(&config_path).unwrap();
        assert_eq!(config.cell_id, Some("my-cell".to_string()));
        assert_eq!(config.backend, Some("https://example.com".to_string()));
    }

    #[test]
    fn test_load_config_file_denies_unknown_fields() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("dispatch.config.toml");
        fs::write(&config_path, r#"unknown_field = "oops""#).unwrap();

        let result = load_config_file(&config_path);
        assert!(result.is_err());
    }

    #[test]
    fn test_derive_cell_id_deterministic() {
        let tmp = TempDir::new().unwrap();
        let id1 = derive_cell_id(tmp.path());
        let id2 = derive_cell_id(tmp.path());
        assert_eq!(id1, id2);
        assert!(id1.starts_with("cell-"));
    }

    #[test]
    fn test_derive_cell_id_different_paths() {
        let tmp1 = TempDir::new().unwrap();
        let tmp2 = TempDir::new().unwrap();
        let id1 = derive_cell_id(tmp1.path());
        let id2 = derive_cell_id(tmp2.path());
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_resolve_config_cli_override() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("dispatch.config.toml");
        fs::write(&config_path, r#"cell_id = "from-config""#).unwrap();

        let resolved = resolve_config_inner(Some("from-cli"), None, None, tmp.path()).unwrap();
        assert_eq!(resolved.cell_id, "from-cli");
    }

    #[test]
    fn test_resolve_config_env_override() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("dispatch.config.toml");
        fs::write(&config_path, r#"cell_id = "from-config""#).unwrap();

        let resolved = resolve_config_inner(None, Some("from-env"), None, tmp.path()).unwrap();
        assert_eq!(resolved.cell_id, "from-env");
    }

    #[test]
    fn test_resolve_config_from_file() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("dispatch.config.toml");
        fs::write(&config_path, r#"cell_id = "from-config""#).unwrap();

        let resolved = resolve_config_inner(None, None, None, tmp.path()).unwrap();
        assert_eq!(resolved.cell_id, "from-config");
    }

    #[test]
    fn test_resolve_config_derived_fallback() {
        let tmp = TempDir::new().unwrap();

        let resolved = resolve_config_inner(None, None, None, tmp.path()).unwrap();
        assert!(resolved.cell_id.starts_with("cell-"));
    }

    #[test]
    fn test_resolve_config_precedence_cli_over_env() {
        let tmp = TempDir::new().unwrap();

        let resolved =
            resolve_config_inner(Some("from-cli"), Some("from-env"), None, tmp.path()).unwrap();
        assert_eq!(resolved.cell_id, "from-cli");
    }

    #[test]
    fn test_init_config_creates_file() {
        let tmp = TempDir::new().unwrap();
        let result = init_config(tmp.path());
        assert!(result.is_ok());

        let path = result.unwrap();
        assert_eq!(path, tmp.path().join("dispatch.config.toml"));
        assert!(path.is_file());

        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.contains("# cell_id = \"my-project\""));

        // Template must be valid TOML (all active lines are comments)
        let parsed: Result<ConfigFile, _> = toml::from_str(&contents);
        assert!(parsed.is_ok());
    }

    #[test]
    fn test_init_config_already_exists() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("dispatch.config.toml");
        fs::write(&config_path, "").unwrap();

        let result = init_config(tmp.path());
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("already exists"),
            "expected 'already exists' in error, got: {err}"
        );
    }

    #[test]
    fn test_resolve_config_explicit_config_path() {
        let tmp = TempDir::new().unwrap();
        let config_dir = tmp.path().join("other");
        fs::create_dir(&config_dir).unwrap();
        let config_path = config_dir.join("dispatch.config.toml");
        fs::write(&config_path, r#"cell_id = "explicit""#).unwrap();

        // cwd is tmp root, config is in other/ — project_root should be other/
        let resolved = resolve_config_inner(None, None, Some(&config_path), tmp.path()).unwrap();
        assert_eq!(resolved.cell_id, "explicit");
        assert_eq!(resolved.project_root, config_dir);
    }

    #[test]
    fn agent_config_parses_claude_adapter() {
        let tmp = TempDir::new().unwrap();
        let prompt_path = tmp.path().join("reviewer.md");
        fs::write(&prompt_path, "you are a reviewer").unwrap();
        let config_path = tmp.path().join("dispatch.config.toml");
        fs::write(
            &config_path,
            r#"
[[agents]]
name = "reviewer"
role = "reviewer"
description = "reviews"
adapter = "claude"
extra_args = ["--model", "sonnet"]
prompt_file = "reviewer.md"
launch = true
"#,
        )
        .unwrap();

        let resolved = resolve_config_inner(None, None, None, tmp.path()).unwrap();
        assert_eq!(resolved.agents.len(), 1);
        let a = &resolved.agents[0];
        assert_eq!(a.adapter, crate::adapter::Adapter::Claude);
        assert_eq!(a.extra_args, vec!["--model", "sonnet"]);
        assert!(a.launch);
        assert!(a.command.is_none());
        assert!(a.prompt_file_path.is_some());
        // Issue #43: stream_json defaults to false so existing configs see
        // no behavior change.
        assert!(!a.stream_json, "stream_json must default to false");
    }

    /// Issue #43: `stream_json = true` round-trips through TOML and lands
    /// on the resolved config. Verified separately by the claude adapter
    /// test that translates this flag into argv.
    #[test]
    fn agent_config_parses_stream_json_flag() {
        let tmp = TempDir::new().unwrap();
        let prompt_path = tmp.path().join("reviewer.md");
        fs::write(&prompt_path, "you are a reviewer").unwrap();
        let config_path = tmp.path().join("dispatch.config.toml");
        fs::write(
            &config_path,
            r#"
[[agents]]
name = "reviewer"
role = "reviewer"
description = "reviews"
adapter = "claude"
prompt_file = "reviewer.md"
stream_json = true
"#,
        )
        .unwrap();

        let resolved = resolve_config_inner(None, None, None, tmp.path()).unwrap();
        assert!(
            resolved.agents[0].stream_json,
            "stream_json = true in TOML must land on the resolved config",
        );
    }

    #[test]
    fn agent_config_rejects_claude_adapter_with_inline_prompt() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("dispatch.config.toml");
        fs::write(
            &config_path,
            r#"
[[agents]]
name = "reviewer"
role = "reviewer"
description = "reviews"
adapter = "claude"
prompt = "you are a reviewer"
"#,
        )
        .unwrap();

        let err = resolve_config_inner(None, None, None, tmp.path()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("prompt_file") && msg.contains("claude"),
            "expected prompt_file-required error for claude, got: {msg}"
        );
    }

    #[test]
    fn agent_config_rejects_codex_adapter_with_inline_prompt() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("dispatch.config.toml");
        fs::write(
            &config_path,
            r#"
[[agents]]
name = "worker"
role = "worker"
description = "codex"
adapter = "codex"
prompt = "be helpful"
"#,
        )
        .unwrap();

        let err = resolve_config_inner(None, None, None, tmp.path()).unwrap_err();
        assert!(err.to_string().contains("prompt_file"));
    }

    #[test]
    fn agent_config_parses_command_adapter() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("dispatch.config.toml");
        fs::write(
            &config_path,
            r#"
[[agents]]
name = "worker"
role = "worker"
description = "bash worker"
adapter = "command"
command = "./worker.sh --verbose"
"#,
        )
        .unwrap();

        let resolved = resolve_config_inner(None, None, None, tmp.path()).unwrap();
        let a = &resolved.agents[0];
        assert_eq!(a.adapter, crate::adapter::Adapter::Command);
        assert_eq!(a.command.as_deref(), Some("./worker.sh --verbose"));
        assert!(!a.launch);
        assert!(a.extra_args.is_empty());
    }

    #[test]
    fn agent_config_rejects_command_adapter_without_command() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("dispatch.config.toml");
        fs::write(
            &config_path,
            r#"
[[agents]]
name = "broken"
role = "worker"
description = "no command"
adapter = "command"
"#,
        )
        .unwrap();

        let err = resolve_config_inner(None, None, None, tmp.path()).unwrap_err();
        assert!(
            err.to_string().contains("adapter = \"command\""),
            "expected command-required error, got: {err}"
        );
    }

    #[test]
    fn agent_config_rejects_missing_adapter_field() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("dispatch.config.toml");
        fs::write(
            &config_path,
            r#"
[[agents]]
name = "legacy"
role = "worker"
description = "old shape"
command = "./worker.sh"
"#,
        )
        .unwrap();

        let err = resolve_config_inner(None, None, None, tmp.path()).unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("adapter"),
            "expected adapter-related parse error, got: {err}"
        );
    }

    #[test]
    fn agent_config_rejects_unknown_adapter_value() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("dispatch.config.toml");
        fs::write(
            &config_path,
            r#"
[[agents]]
name = "x"
role = "r"
description = "d"
adapter = "gpt"
"#,
        )
        .unwrap();

        assert!(resolve_config_inner(None, None, None, tmp.path()).is_err());
    }

    /// Agent names must pass `is_safe_name` at resolve time so the
    /// boot-prompt filename (derived via lossy `sanitize_name`) cannot
    /// collide across distinct raw names under `dispatch serve`.
    #[test]
    fn agent_config_rejects_name_with_unsafe_characters() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("dispatch.config.toml");
        fs::write(
            &config_path,
            r#"
[[agents]]
name = "alice/foo"
role = "worker"
description = "d"
adapter = "command"
command = "./run.sh"
"#,
        )
        .unwrap();

        let err = resolve_config_inner(None, None, None, tmp.path()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("alice/foo") && msg.contains("ASCII"),
            "expected safe-name rejection for 'alice/foo', got: {msg}"
        );
    }

    /// `interactive = true` on a `launch = false` agent round-trips
    /// through TOML unchanged. The adapter uses this to drop `-p` (claude)
    /// / `exec` (codex) so the printed copy-paste command opens the vendor
    /// CLI in its REPL instead of headless mode.
    #[test]
    fn agent_config_parses_interactive_flag() {
        let tmp = TempDir::new().unwrap();
        let prompt_path = tmp.path().join("coord.md");
        fs::write(&prompt_path, "you are the coordinator").unwrap();
        let config_path = tmp.path().join("dispatch.config.toml");
        fs::write(
            &config_path,
            r#"
[[agents]]
name = "coordinator"
role = "coordinator"
description = "the human-run coordinator"
adapter = "claude"
prompt_file = "coord.md"
launch = false
interactive = true
"#,
        )
        .unwrap();

        let resolved = resolve_config_inner(None, None, None, tmp.path()).unwrap();
        assert_eq!(resolved.agents.len(), 1);
        let a = &resolved.agents[0];
        assert!(a.interactive, "interactive = true must round-trip");
        assert!(!a.launch);
    }

    /// `interactive = true + launch = true` is contradictory (the
    /// supervisor owns stdin/stdout, so there's no TTY for a REPL). We
    /// warn and prefer `launch = true` (headless), keeping the config
    /// loadable rather than forcing a hard failure that would strand a
    /// user who typed the wrong combo.
    #[test]
    fn agent_config_warns_and_prefers_launch_when_both_set() {
        let tmp = TempDir::new().unwrap();
        let prompt_path = tmp.path().join("r.md");
        fs::write(&prompt_path, "role").unwrap();
        let config_path = tmp.path().join("dispatch.config.toml");
        fs::write(
            &config_path,
            r#"
[[agents]]
name = "reviewer"
role = "reviewer"
description = "reviews"
adapter = "claude"
prompt_file = "r.md"
launch = true
interactive = true
"#,
        )
        .unwrap();

        let resolved = resolve_config_inner(None, None, None, tmp.path()).unwrap();
        let a = &resolved.agents[0];
        assert!(a.launch, "launch must win when both are set");
        assert!(
            !a.interactive,
            "interactive must be forced off when launch is on",
        );
    }

    /// `interactive` defaults to false so existing configs see no
    /// behavior change after the new field is introduced.
    #[test]
    fn agent_config_interactive_defaults_false() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("dispatch.config.toml");
        fs::write(
            &config_path,
            r#"
[[agents]]
name = "worker"
role = "worker"
description = "d"
adapter = "command"
command = "./run.sh"
"#,
        )
        .unwrap();

        let resolved = resolve_config_inner(None, None, None, tmp.path()).unwrap();
        assert!(!resolved.agents[0].interactive);
    }

    /// Issue #45: `[main_agent]` has been removed in favor of a regular
    /// `[[agents]] launch = false + prompt_file` entry. `ConfigFile`'s
    /// `deny_unknown_fields` means a legacy config with `[main_agent]`
    /// fails parse with a clear pointer rather than silently ignoring
    /// the section.
    #[test]
    fn config_rejects_legacy_main_agent_table() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("dispatch.config.toml");
        fs::write(
            &config_path,
            r#"
[main_agent]
command = "claude"
model = "opus"
"#,
        )
        .unwrap();

        let err = resolve_config_inner(None, None, None, tmp.path()).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("main_agent"),
            "expected error to reference main_agent, got: {msg}"
        );
    }

    /// `config_file_path` carries the discovered file path as an absolute
    /// canonical path, so agents spawned by `dispatch serve` can propagate
    /// it via `DISPATCH_CONFIG_PATH` regardless of their working directory.
    #[test]
    fn resolved_config_carries_path_when_discovered() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("dispatch.config.toml");
        fs::write(&config_path, "").unwrap();

        let resolved = resolve_config_inner(None, None, None, tmp.path()).unwrap();
        let expected = config_path.canonicalize().unwrap();
        assert_eq!(
            resolved.config_file_path.as_deref(),
            Some(expected.as_path())
        );
    }

    /// Explicit `--config <path>` flow: the absolute path threads through
    /// to `ResolvedConfig` so the injected env var points at the right file.
    #[test]
    fn resolved_config_carries_path_when_flag_given() {
        let tmp = TempDir::new().unwrap();
        let config_dir = tmp.path().join("other");
        fs::create_dir(&config_dir).unwrap();
        let config_path = config_dir.join("dispatch.config.toml");
        fs::write(&config_path, "").unwrap();

        let resolved = resolve_config_inner(None, None, Some(&config_path), tmp.path()).unwrap();
        let expected = config_path.canonicalize().unwrap();
        assert_eq!(
            resolved.config_file_path.as_deref(),
            Some(expected.as_path())
        );
    }

    /// Cwd without a config file: `config_file_path` stays `None` so the
    /// orchestrator emits no `DISPATCH_CONFIG_PATH` env var (regression
    /// guard — existing configs see byte-identical env).
    #[test]
    fn resolved_config_none_when_no_config_file() {
        let tmp = TempDir::new().unwrap();

        let resolved = resolve_config_inner(None, None, None, tmp.path()).unwrap();
        assert!(resolved.config_file_path.is_none());
    }

    /// `DISPATCH_CONFIG_PATH` acts as a fallback to `--config`, mirroring how
    /// `DISPATCH_SOCKET_PATH` already works for the broker socket.
    #[test]
    fn resolve_config_with_env_honors_dispatch_config_path() {
        let tmp = TempDir::new().unwrap();
        let config_dir = tmp.path().join("elsewhere");
        fs::create_dir(&config_dir).unwrap();
        let config_path = config_dir.join("dispatch.config.toml");
        fs::write(&config_path, r#"cell_id = "from-env-path""#).unwrap();

        let resolved = resolve_config_with_env(
            None,
            None,
            None,
            Some(config_path.to_str().unwrap()),
            tmp.path(),
        )
        .unwrap();
        assert_eq!(resolved.cell_id, "from-env-path");
    }

    /// CLI flag beats env var — matches the stated precedence order
    /// (CLI > env > discovery) from the config docs.
    #[test]
    fn resolve_config_with_env_prefers_cli_flag_over_env() {
        let tmp = TempDir::new().unwrap();
        let cli_dir = tmp.path().join("cli");
        fs::create_dir(&cli_dir).unwrap();
        let cli_config = cli_dir.join("dispatch.config.toml");
        fs::write(&cli_config, r#"cell_id = "from-cli""#).unwrap();

        let env_dir = tmp.path().join("env");
        fs::create_dir(&env_dir).unwrap();
        let env_config = env_dir.join("dispatch.config.toml");
        fs::write(&env_config, r#"cell_id = "from-env""#).unwrap();

        let resolved = resolve_config_with_env(
            None,
            None,
            Some(&cli_config),
            Some(env_config.to_str().unwrap()),
            tmp.path(),
        )
        .unwrap();
        assert_eq!(resolved.cell_id, "from-cli");
    }

    /// Empty `DISPATCH_CONFIG_PATH` is treated as unset — matches the
    /// `resolve_socket_path_with_env` idiom and avoids a hard error when
    /// a parent shell exports the var blank.
    #[test]
    fn resolve_config_with_env_treats_empty_string_as_unset() {
        let tmp = TempDir::new().unwrap();

        let resolved = resolve_config_with_env(None, None, None, Some(""), tmp.path()).unwrap();
        assert!(resolved.config_file_path.is_none());
    }

    /// `DISPATCH_CONFIG_PATH` pointing at a missing file errors out — same
    /// failure mode as `--config nonexistent.toml`, documented as a
    /// breaking-change surface.
    #[test]
    fn resolve_config_with_env_hard_errors_on_missing_file() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("does-not-exist.toml");

        let err = resolve_config_with_env(
            None,
            None,
            None,
            Some(missing.to_str().unwrap()),
            tmp.path(),
        )
        .unwrap_err();
        // ConfigNotFound — exactly what `--config <missing>` emits today.
        assert!(
            err.to_string().to_lowercase().contains("not found")
                || err.to_string().to_lowercase().contains("no such"),
            "expected not-found error, got: {err}"
        );
    }

    /// Regression guard for the `absolutize` fallback: when canonicalize
    /// fails (e.g. permission denied on an ancestor dir), we must still
    /// produce an absolute path, not echo the raw relative input. The
    /// fallback is what stands between `DISPATCH_CONFIG_PATH` and a
    /// worthless value in rare failure modes.
    #[test]
    fn absolutize_joins_relative_paths_against_cwd() {
        let tmp = TempDir::new().unwrap();
        let relative = Path::new("foo/bar.toml");
        let joined = absolutize(tmp.path(), relative);
        assert!(joined.is_absolute());
        assert_eq!(joined, tmp.path().join("foo/bar.toml"));
    }

    /// `absolutize` leaves already-absolute paths untouched.
    #[test]
    fn absolutize_passes_absolute_paths_through() {
        let tmp = TempDir::new().unwrap();
        let already_abs = tmp.path().join("x.toml");
        assert_eq!(absolutize(tmp.path(), &already_abs), already_abs);
    }

    /// Discovered path is always absolute — even a `TempDir` root (already
    /// absolute) gets canonicalized to resolve `/tmp` → `/private/tmp` on
    /// macOS, guaranteeing the stored path is what downstream code can
    /// stat regardless of how the test env was configured.
    #[test]
    fn discovered_config_path_is_absolute() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("dispatch.config.toml");
        fs::write(&config_path, "").unwrap();

        let resolved = resolve_config_inner(None, None, None, tmp.path()).unwrap();
        let stored = resolved.config_file_path.expect("path must be set");
        assert!(
            stored.is_absolute(),
            "discovered path must be absolute, got: {}",
            stored.display()
        );
    }
}
