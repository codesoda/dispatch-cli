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
    /// Monitor dashboard port (from config or CLI flag).
    pub monitor_port: Option<u16>,
    /// Auto-open the monitor in the default browser.
    pub monitor_launch: bool,
    /// Agent definitions to launch on serve.
    pub agents: Vec<ResolvedAgentConfig>,
    /// Main interactive agent (printed as a command, not auto-launched).
    pub main_agent: Option<MainAgentConfig>,
    /// Scheduled heartbeat commands.
    pub heartbeats: Vec<HeartbeatConfig>,
    /// Defaults for ad-hoc agents spawned from the monitor UI.
    pub agent_defaults: Option<AgentDefaultsConfig>,
}

/// Agent config after prompt_file has been resolved to prompt text.
#[derive(Debug, Clone)]
pub struct ResolvedAgentConfig {
    pub name: String,
    pub role: String,
    pub description: String,
    pub command: String,
    pub prompt: Option<String>,
    pub ttl: Option<u64>,
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
    /// Monitor dashboard configuration.
    pub monitor: Option<MonitorConfig>,
    /// Agent definitions to launch on serve.
    #[serde(default)]
    pub agents: Vec<AgentConfig>,
    /// Main interactive agent configuration.
    pub main_agent: Option<MainAgentConfig>,
    /// Scheduled heartbeat commands.
    #[serde(default)]
    pub heartbeats: Vec<HeartbeatConfig>,
    /// Defaults for ad-hoc agents spawned from the monitor UI.
    pub agent_defaults: Option<AgentDefaultsConfig>,
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

/// Defaults for ad-hoc agents spawned from the monitor UI.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct AgentDefaultsConfig {
    /// Default command to run (e.g. "claude --model sonnet").
    pub command: String,
    /// Default role for ad-hoc agents.
    pub role: Option<String>,
    /// Default description.
    pub description: Option<String>,
    /// Default TTL in seconds.
    pub ttl: Option<u64>,
}

/// On-disk monitor configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MonitorConfig {
    pub port: u16,
    /// Auto-open the monitor dashboard in the default browser.
    #[serde(default)]
    pub launch: bool,
}

/// On-disk agent definition.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    pub name: String,
    pub role: String,
    pub description: String,
    pub command: String,
    pub prompt: Option<String>,
    pub prompt_file: Option<String>,
    pub ttl: Option<u64>,
}

/// On-disk main agent definition.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MainAgentConfig {
    pub command: String,
    pub model: Option<String>,
    pub prompt: Option<String>,
    pub prompt_file: Option<String>,
}

/// Resolve an agent config by reading prompt_file if specified.
fn resolve_agent_config(
    agent: &AgentConfig,
    project_root: &Path,
) -> Result<ResolvedAgentConfig, DispatchError> {
    let prompt = match (&agent.prompt, &agent.prompt_file) {
        (Some(_), Some(_)) => {
            return Err(DispatchError::AgentConfigError {
                name: agent.name.clone(),
                reason: "cannot specify both 'prompt' and 'prompt_file'".into(),
            });
        }
        (Some(p), None) => Some(p.clone()),
        (None, Some(path)) => {
            let full_path = project_root.join(path);
            let content = std::fs::read_to_string(&full_path).map_err(|_| {
                DispatchError::PromptFileNotFound {
                    name: agent.name.clone(),
                    path: full_path,
                }
            })?;
            Some(content)
        }
        (None, None) => None,
    };

    Ok(ResolvedAgentConfig {
        name: agent.name.clone(),
        role: agent.role.clone(),
        description: agent.description.clone(),
        command: agent.command.clone(),
        prompt,
        ttl: agent.ttl,
    })
}

/// Resolve the main agent prompt_file if specified.
fn resolve_main_agent_prompt(
    main: &MainAgentConfig,
    project_root: &Path,
) -> Result<Option<String>, DispatchError> {
    match (&main.prompt, &main.prompt_file) {
        (Some(_), Some(_)) => Err(DispatchError::AgentConfigError {
            name: "main_agent".into(),
            reason: "cannot specify both 'prompt' and 'prompt_file'".into(),
        }),
        (Some(p), None) => Ok(Some(p.clone())),
        (None, Some(path)) => {
            let full_path = project_root.join(path);
            let content = std::fs::read_to_string(&full_path).map_err(|_| {
                DispatchError::PromptFileNotFound {
                    name: "main_agent".into(),
                    path: full_path,
                }
            })?;
            Ok(Some(content))
        }
        (None, None) => Ok(None),
    }
}

/// Search upward from `start_dir` for `dispatch.config.toml`.
/// Returns the path to the config file and the directory containing it.
pub fn find_config_file(start_dir: &Path) -> Option<(PathBuf, PathBuf)> {
    let mut current = start_dir.to_path_buf();
    loop {
        let candidate = current.join("dispatch.config.toml");
        if candidate.is_file() {
            return Some((candidate, current));
        }
        if !current.pop() {
            return None;
        }
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

# Monitor dashboard — starts an HTTP dashboard on serve
# [monitor]
# port = 8384
# launch = true  # auto-open dashboard in browser on serve

# Agent definitions — launched automatically by `dispatch serve`
# [[agents]]
# name = \"reviewer\"
# role = \"code-reviewer\"
# description = \"Reviews code changes\"
# command = \"claude --model sonnet\"
# prompt_file = \"prompts/reviewer.md\"
# ttl = 3600

# Main interactive agent — printed as a ready-to-paste command
# [main_agent]
# command = \"claude\"
# model = \"opus\"
# prompt = \"You are the lead agent for this project...\"

# Scheduled heartbeats — commands run on a timer while the broker is running
# [[heartbeats]]
# name = \"check-prs\"
# command = \"dispatch send --to $GITHUB_AGENT --body '{\\\"type\\\":\\\"check_prs\\\"}'\"
# every = 120
# after = 30  # optional: wait this long before the first execution

# Defaults for ad-hoc agents spawned from the monitor UI
# [agent_defaults]
# command = \"claude --model sonnet --dangerously-skip-permissions\"
# role = \"adhoc\"
# description = \"Ad-hoc agent spawned from monitor\"
# ttl = 3600
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

    // Check for config in a parent directory (skip cwd itself)
    if let Some(parent) = cwd.parent() {
        if let Some((parent_config, _)) = find_config_file(parent) {
            eprintln!(
                "Note: found existing config at {}, creating a new one in the current directory",
                parent_config.display()
            );
        }
    }

    std::fs::write(&config_path, CONFIG_TEMPLATE)?;
    Ok(config_path)
}

/// Resolve configuration with full precedence:
/// CLI flag > env var > config file > derived fallback.
pub fn resolve_config(
    cli_cell_id: Option<&str>,
    start_dir: &Path,
) -> Result<ResolvedConfig, DispatchError> {
    let env_cell_id = env::var("DISPATCH_CELL_ID").ok();
    resolve_config_inner(cli_cell_id, env_cell_id.as_deref(), start_dir)
}

fn resolve_config_inner(
    cli_cell_id: Option<&str>,
    env_cell_id: Option<&str>,
    start_dir: &Path,
) -> Result<ResolvedConfig, DispatchError> {
    // Try to find and load config file
    let (config_file, project_root) = if let Some((config_path, root)) = find_config_file(start_dir)
    {
        let config = load_config_file(&config_path)?;
        (Some(config), root)
    } else {
        (None, start_dir.to_path_buf())
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
    let (name, backend, monitor_config, raw_agents, main_agent_config, heartbeats, agent_defaults) =
        match config_file {
            Some(c) => (
                c.name,
                c.backend,
                c.monitor,
                c.agents,
                c.main_agent,
                c.heartbeats,
                c.agent_defaults,
            ),
            None => (None, None, None, vec![], None, vec![], None),
        };
    let monitor_launch = monitor_config.as_ref().is_some_and(|m| m.launch);
    let monitor_port = monitor_config.map(|m| m.port);

    // Resolve agent prompt files
    let agents: Vec<ResolvedAgentConfig> = raw_agents
        .iter()
        .map(|a| resolve_agent_config(a, &project_root))
        .collect::<Result<_, _>>()?;

    // Resolve main agent prompt file
    let main_agent = if let Some(ref ma) = main_agent_config {
        let resolved_prompt = resolve_main_agent_prompt(ma, &project_root)?;
        Some(MainAgentConfig {
            command: ma.command.clone(),
            model: ma.model.clone(),
            prompt: resolved_prompt,
            prompt_file: None,
        })
    } else {
        None
    };

    Ok(ResolvedConfig {
        name,
        cell_id,
        backend,
        project_root,
        monitor_port,
        monitor_launch,
        agents,
        main_agent,
        heartbeats,
        agent_defaults,
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
    fn test_find_config_in_parent_dir() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("dispatch.config.toml");
        fs::write(&config_path, "").unwrap();

        let child = tmp.path().join("subdir");
        fs::create_dir(&child).unwrap();

        let result = find_config_file(&child);
        assert!(result.is_some());
        let (path, root) = result.unwrap();
        assert_eq!(path, config_path);
        assert_eq!(root, tmp.path().to_path_buf());
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

        let resolved = resolve_config_inner(Some("from-cli"), None, tmp.path()).unwrap();
        assert_eq!(resolved.cell_id, "from-cli");
    }

    #[test]
    fn test_resolve_config_env_override() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("dispatch.config.toml");
        fs::write(&config_path, r#"cell_id = "from-config""#).unwrap();

        let resolved = resolve_config_inner(None, Some("from-env"), tmp.path()).unwrap();
        assert_eq!(resolved.cell_id, "from-env");
    }

    #[test]
    fn test_resolve_config_from_file() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("dispatch.config.toml");
        fs::write(&config_path, r#"cell_id = "from-config""#).unwrap();

        let resolved = resolve_config_inner(None, None, tmp.path()).unwrap();
        assert_eq!(resolved.cell_id, "from-config");
    }

    #[test]
    fn test_resolve_config_derived_fallback() {
        let tmp = TempDir::new().unwrap();

        let resolved = resolve_config_inner(None, None, tmp.path()).unwrap();
        assert!(resolved.cell_id.starts_with("cell-"));
    }

    #[test]
    fn test_resolve_config_precedence_cli_over_env() {
        let tmp = TempDir::new().unwrap();

        let resolved =
            resolve_config_inner(Some("from-cli"), Some("from-env"), tmp.path()).unwrap();
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
    fn test_init_config_with_parent_config() {
        let tmp = TempDir::new().unwrap();
        // Config in parent
        let parent_config = tmp.path().join("dispatch.config.toml");
        fs::write(&parent_config, "").unwrap();

        // Init in child — should succeed despite parent config
        let child = tmp.path().join("subdir");
        fs::create_dir(&child).unwrap();

        let result = init_config(&child);
        assert!(result.is_ok());
        let path = result.unwrap();
        assert_eq!(path, child.join("dispatch.config.toml"));
        assert!(path.is_file());
    }

    #[test]
    fn test_resolve_config_project_root_with_config() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("dispatch.config.toml");
        fs::write(&config_path, "").unwrap();

        let child = tmp.path().join("sub");
        fs::create_dir(&child).unwrap();

        let resolved = resolve_config_inner(None, None, &child).unwrap();
        assert_eq!(resolved.project_root, tmp.path().to_path_buf());
    }
}
