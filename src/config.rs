use std::collections::hash_map::DefaultHasher;
use std::env;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::errors::DispatchError;

/// Shipped fallback for the "listen again" instruction. Used when
/// neither a per-agent `[[agents]]` `continue_instruction` nor a global one is
/// set. Tells the agent to keep long-polling and not to stop on its own — the
/// coordinator owns the stop decision via the worker's control state.
pub const DEFAULT_CONTINUE_INSTRUCTION: &str =
    "No task right now. Run `dispatch listen` again and keep waiting. Do not stop until dispatch tells you to.";

/// Shipped fallback for the one-line boot prompt fed to a managed agent at
/// launch. Used when neither a per-agent `[[agents]]` `boot_prompt` nor a
/// file-level one is set. Kept deliberately minimal: the agent's first
/// observable action is the bare `dispatch register --for-agent`, whose
/// response body (the agent's `prompt_file`) carries the real role prompt —
/// so the boot line only has to trigger that one call. Resolved at config
/// time as per-agent **>** file-level, with this default applied at write
/// time by `write_boot_prompt`.
pub const DEFAULT_BOOT_PROMPT: &str = "Run: dispatch register --for-agent\n";

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
    /// Drain window (seconds) a `stopping` worker lingers before the broker
    /// finalizes it. `None` → the broker's built-in default applies.
    pub stopping_drain_secs: Option<u64>,
    /// Global "listen again" instruction. The fallback when an agent
    /// has no per-agent override; `None` → the shipped
    /// [`DEFAULT_CONTINUE_INSTRUCTION`]. Resolve via
    /// [`ResolvedConfig::continue_instruction_for`], never read directly, so
    /// the per-agent > global > default precedence stays in one place.
    pub continue_instruction: Option<String>,
    /// When true, broker events that reference a prompt/packet body may include
    /// the full body. Default `false`: bodies are logged by hash +
    /// byte size only, never the full text, so prompts don't leak into the
    /// event history / logs.
    pub log_prompt_bodies: bool,
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
    /// Effective default `--timeout` (seconds) for this agent's
    /// `dispatch listen` calls, injected as `DISPATCH_LISTEN_TIMEOUT`.
    /// Already resolved at config time as per-agent override **>** global
    /// `listen_timeout`; `None` means "unset" and the CLI's built-in 270s
    /// default applies.
    pub listen_timeout: Option<u64>,
    /// Per-agent override of the "listen again" instruction. `None`
    /// falls back to the global `continue_instruction`, then the shipped
    /// default. Read via [`ResolvedConfig::continue_instruction_for`].
    pub continue_instruction: Option<String>,
    /// The one-line boot prompt written to `<name>.boot.prompt` and fed to the
    /// agent at launch. Already resolved at config time as per-agent
    /// `[[agents]]` override **>** file-level `boot_prompt`; `None` means both
    /// are unset and the shipped [`DEFAULT_BOOT_PROMPT`] applies at write time.
    pub boot_prompt: Option<String>,
    /// When true, the claude adapter is launched with
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
    /// Global default `--timeout` (seconds) for `dispatch listen`, injected
    /// into spawned agents as `DISPATCH_LISTEN_TIMEOUT`. Overridable per
    /// agent in `[[agents]]`. When unset, the CLI's built-in 270s applies.
    pub listen_timeout: Option<u64>,
    /// Drain window (seconds) a `stopping` worker lingers before the broker
    /// finalizes it. When unset, the broker's built-in default (10s) applies.
    pub stopping_drain_secs: Option<u64>,
    /// Global "listen again" instruction returned by the stop hook and the
    /// `listen --for-agent` timeout renderer. Overridable per agent
    /// in `[[agents]]`. When unset, the shipped default applies.
    pub continue_instruction: Option<String>,
    /// File-level boot prompt fed to every managed agent in this file at
    /// launch (the first thing the model runs). Overridable per agent in
    /// `[[agents]]`. When unset, the shipped [`DEFAULT_BOOT_PROMPT`]
    /// (`Run: dispatch register --for-agent`) applies — keep it minimal; the
    /// real role prompt arrives as the response to that register call.
    pub boot_prompt: Option<String>,
    /// When true, broker events may log full prompt/packet bodies. Default
    /// `false` — bodies are recorded as hash + byte size only.
    #[serde(default)]
    pub log_prompt_bodies: bool,
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
    /// Per-agent override of the global `listen_timeout` (seconds). Injected
    /// as `DISPATCH_LISTEN_TIMEOUT` so this agent's bare `dispatch listen`
    /// long-polls for the configured duration.
    pub listen_timeout: Option<u64>,
    /// Per-agent override of the global `continue_instruction`.
    pub continue_instruction: Option<String>,
    /// Per-agent override of the file-level `boot_prompt`.
    pub boot_prompt: Option<String>,
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
    global_listen_timeout: Option<u64>,
    global_boot_prompt: Option<&str>,
) -> Result<ResolvedAgentConfig, DispatchError> {
    use crate::adapter::Adapter;

    // Reject names that can't be used as a single on-disk filename
    // component. The HTTP boundaries (`api_agent_start/stop/restart`) already
    // gate on `is_safe_name`, but the `launch_all` / `spawn_agent` path
    // derives the boot-prompt filename from `sanitize_name`, which
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
        // Per-agent override wins over the global default; `None` here means
        // both are unset and the CLI's built-in 270s default applies.
        listen_timeout: agent.listen_timeout.or(global_listen_timeout),
        // Kept raw (per-agent only): `continue_instruction_for` layers global
        // and the shipped default on top, so don't fold them in here.
        continue_instruction: agent.continue_instruction.clone(),
        // Per-agent override wins over the file-level boot prompt; `None` here
        // means both are unset and DEFAULT_BOOT_PROMPT applies at write time.
        boot_prompt: agent
            .boot_prompt
            .clone()
            .or_else(|| global_boot_prompt.map(str::to_string)),
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

# Default `dispatch listen` timeout in seconds. Injected into spawned agents
# as DISPATCH_LISTEN_TIMEOUT so a bare `dispatch listen` long-polls for this
# duration. Overridable per agent in [[agents]]. (default: 270)
# listen_timeout = 270

# How long (seconds) a worker stays `stopping` after `dispatch agent stop`
# before the broker finalizes it. This drain window lets a dying agent's late
# stop-hook call still see `stopping` (and exit cleanly) rather than racing the
# record's removal. (default: 10)
# stopping_drain_secs = 10

# What to tell an agent when it has no task right now — returned by the stop
# hook (to keep the agent listening) and by `dispatch listen --for-agent` on a
# timeout while the worker is still `active`. Overridable per agent in
# [[agents]]. When unset, a built-in default is used.
# continue_instruction = \"No task right now. Run `dispatch listen` again and keep waiting. Do not stop until dispatch tells you to.\"

# One-line boot prompt fed to every managed agent at launch — the first thing
# the model runs. Keep it minimal: its only job is to trigger
# `dispatch register --for-agent`, whose response body (the agent's prompt_file)
# carries the real role prompt. Overridable per agent in [[agents]].
# (default: \"Run: dispatch register --for-agent\")
# boot_prompt = \"Run: dispatch register --for-agent\"

# Log full prompt/packet bodies in broker events. Default false — bodies are
# recorded by hash + byte size only, so prompts don't leak into the event
# history or logs. Enable only for debugging.
# log_prompt_bodies = false

# Monitor dashboard — starts an HTTP dashboard on serve
# [monitor]
# port = 8384
# open = true  # open the dashboard in your default browser

# Agent definitions — auto-started by `dispatch serve` when launch = true.
#
# When `launch = true` AND `prompt_file` is set (the managed-agent flow),
# dispatch pre-registers the worker server-side at spawn time, injects the
# agent's identity into its environment (DISPATCH_WORKER_ID / DISPATCH_AGENT_NAME
# / DISPATCH_AGENT_ROLE / DISPATCH_AGENT_DESCRIPTION), and feeds the agent a
# one-line boot prompt. The first thing the model does is run the bare
# `dispatch register --for-agent` (identity all from env), whose response body
# is the contents of `prompt_file` — so the role prompt lands in the model's
# tool result instead of being narrated up front (this kills a class of
# hallucination where the model fakes the register step).
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
# listen_timeout = 540                           # per-agent override of the global
#                                                # listen_timeout (seconds)
# boot_prompt = \"Run: dispatch register --for-agent\"  # per-agent override of the
#                                                # file-level boot_prompt
# stream_json = false                            # when true, claude is launched with
#                                                # `--output-format stream-json --verbose`
#                                                # so per-tool-use entries appear in the
#                                                # agent log (verifies real register calls).
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
# command uses the boot-prompt bootstrap — the agent's first
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

    let (
        name,
        backend,
        default_ttl,
        global_listen_timeout,
        stopping_drain_secs,
        continue_instruction,
        global_boot_prompt,
        log_prompt_bodies,
        config_cwd,
        monitor_config,
        raw_agents,
        heartbeats,
    ) = match config_file {
        Some(c) => (
            c.name,
            c.backend,
            c.default_ttl,
            c.listen_timeout,
            c.stopping_drain_secs,
            c.continue_instruction,
            c.boot_prompt,
            c.log_prompt_bodies,
            c.cwd,
            c.monitor,
            c.agents,
            c.heartbeats,
        ),
        None => (
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            false,
            None,
            None,
            vec![],
            vec![],
        ),
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
        .map(|a| {
            resolve_agent_config(
                a,
                &project_root,
                global_listen_timeout,
                global_boot_prompt.as_deref(),
            )
        })
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
        stopping_drain_secs,
        continue_instruction,
        log_prompt_bodies,
        agents,
        heartbeats,
    })
}

impl ResolvedConfig {
    /// Resolve the "listen again" instruction for `agent_name` with precedence
    /// per-agent `[[agents]]` override **>** global `continue_instruction` **>**
    /// shipped [`DEFAULT_CONTINUE_INSTRUCTION`].
    ///
    /// `agent_name` is the agent's `DISPATCH_AGENT_NAME` when known; `None`
    /// (ad-hoc session, or a name not in this config) skips straight to the
    /// global/default fallback. The single resolver feeds both the stop hook
    /// and the `listen --for-agent` timeout renderer so the
    /// two can't drift.
    pub fn continue_instruction_for(&self, agent_name: Option<&str>) -> String {
        if let Some(name) = agent_name {
            if let Some(text) = self
                .agents
                .iter()
                .find(|a| a.name == name)
                .and_then(|a| a.continue_instruction.as_deref())
            {
                return text.to_string();
            }
        }
        self.continue_instruction
            .as_deref()
            .unwrap_or(DEFAULT_CONTINUE_INSTRUCTION)
            .to_string()
    }
}

#[cfg(test)]
mod tests;
