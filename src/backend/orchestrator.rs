use std::collections::HashMap;
use std::path::{Path, PathBuf};

use tokio::process::{Child, Command};

use crate::config::ResolvedAgentConfig;
use crate::errors::DispatchError;

/// Kill an entire process group. Sends SIGTERM first, then SIGKILL after a timeout.
async fn kill_process_group(pid: u32) {
    let pgid = pid as i32;
    // Send SIGTERM to the process group.
    unsafe {
        libc::kill(-pgid, libc::SIGTERM);
    }
    // Give processes a moment to exit gracefully.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    // Force kill any remaining processes.
    unsafe {
        libc::kill(-pgid, libc::SIGKILL);
    }
}

/// Info about a running agent for external queries.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AgentInfo {
    pub name: String,
    pub role: String,
    pub pid: u32,
    pub running: bool,
}

/// A running agent subprocess.
struct RunningAgent {
    name: String,
    role: String,
    child: Child,
    pid: u32,
}

/// A running heartbeat timer task.
struct RunningHeartbeat {
    name: String,
    handle: tokio::task::JoinHandle<()>,
}

/// Manages the lifecycle of agent subprocesses and heartbeat timers.
pub struct AgentOrchestrator {
    agents: Vec<RunningAgent>,
    heartbeats: Vec<RunningHeartbeat>,
    cell_id: String,
    socket_path: PathBuf,
    monitor_url: Option<String>,
    /// Working directory for spawned agent subprocesses.
    agent_cwd: PathBuf,
    /// Where per-agent log files are written. Must match the directory the
    /// monitor's `/api/logs/{agent}` endpoint reads from.
    log_dir: PathBuf,
}

impl AgentOrchestrator {
    pub fn new(
        cell_id: &str,
        socket_path: &Path,
        monitor_url: Option<String>,
        agent_cwd: &Path,
        log_dir: PathBuf,
    ) -> Self {
        Self {
            agents: Vec::new(),
            heartbeats: Vec::new(),
            cell_id: cell_id.to_string(),
            socket_path: socket_path.to_path_buf(),
            monitor_url,
            agent_cwd: agent_cwd.to_path_buf(),
            log_dir,
        }
    }

    /// Build the environment variables injected into every agent subprocess.
    fn env_vars(&self, name: &str, role: &str) -> HashMap<String, String> {
        let mut vars = HashMap::new();
        vars.insert("DISPATCH_CELL_ID".into(), self.cell_id.clone());
        vars.insert(
            "DISPATCH_SOCKET_PATH".into(),
            self.socket_path.display().to_string(),
        );
        if let Some(ref url) = self.monitor_url {
            vars.insert("DISPATCH_MONITOR_URL".into(), url.clone());
        }
        vars.insert("DISPATCH_AGENT_NAME".into(), name.into());
        vars.insert("DISPATCH_AGENT_ROLE".into(), role.into());
        vars
    }

    /// Build the full shell command for an agent.
    ///
    /// Pure function with no side effects. If `tempfile_override` is given,
    /// it is used for `{prompt_file}` substitution (for spawn-time use); when
    /// `None`, the substitution falls back to `config.prompt_file_path` and
    /// finally leaves the literal `{prompt_file}` placeholder in the rendered
    /// string (so display callers like the monitor don't trigger filesystem
    /// writes).
    pub(super) fn build_command(
        config: &ResolvedAgentConfig,
        tempfile_override: Option<&Path>,
    ) -> String {
        let base = &config.command;

        if let Some(ref prompt) = config.prompt {
            if base.contains("{prompt}") {
                return base.replace("{prompt}", &shell_escape(prompt));
            }
            if base.contains("{prompt_file}") {
                let resolved_path: Option<String> = tempfile_override
                    .map(|p| p.display().to_string())
                    .or_else(|| {
                        config
                            .prompt_file_path
                            .as_ref()
                            .map(|p| p.display().to_string())
                    });
                if let Some(path) = resolved_path {
                    return base.replace("{prompt_file}", &shell_escape(&path));
                }
                // No file available — leave the placeholder so display callers
                // can show users where the prompt file would be substituted.
                return base.clone();
            }
            // Fallback: append prompt as a positional argument for LLM commands
            let is_llm = base.starts_with("claude") || base.starts_with("codex");
            if is_llm {
                format!("{base} {}", shell_escape(prompt))
            } else {
                base.clone()
            }
        } else {
            base.clone()
        }
    }

    /// Spawn a single agent as a subprocess.
    ///
    /// Stdout and stderr are redirected to `<log_dir>/<sanitized-agent-name>.log`
    /// so that agent output can be inspected after the fact.
    pub async fn spawn_agent(&mut self, config: &ResolvedAgentConfig) -> Result<(), DispatchError> {
        let env_vars = self.env_vars(&config.name, &config.role);

        // If the command uses `{prompt_file}` and the prompt is inline
        // (no prompt_file_path resolved at config time), write the prompt
        // to a tempfile here so build_command stays side-effect free.
        let tempfile_path: Option<PathBuf> =
            if config.command.contains("{prompt_file}") && config.prompt_file_path.is_none() {
                if let Some(ref prompt) = config.prompt {
                    Some(write_prompt_tempfile(prompt, &config.name).map_err(|e| {
                        DispatchError::AgentLaunchFailed {
                            name: config.name.clone(),
                            reason: format!("failed to write prompt tempfile: {e}"),
                        }
                    })?)
                } else {
                    None
                }
            } else {
                None
            };
        let full_command = Self::build_command(config, tempfile_path.as_deref());

        tracing::info!(
            agent = %config.name,
            role = %config.role,
            command = %full_command,
            "launching agent"
        );

        // Ensure log directory exists and open a per-agent log file.
        tokio::fs::create_dir_all(&self.log_dir)
            .await
            .map_err(|e| DispatchError::AgentLaunchFailed {
                name: config.name.clone(),
                reason: format!("failed to create log dir {}: {e}", self.log_dir.display()),
            })?;
        let safe_name = sanitize_name(&config.name);
        let log_path = self.log_dir.join(format!("{safe_name}.log"));
        let log_file = tokio::fs::File::create(&log_path).await.map_err(|e| {
            DispatchError::AgentLaunchFailed {
                name: config.name.clone(),
                reason: format!("failed to create log file {}: {e}", log_path.display()),
            }
        })?;
        // Clone the file handle so stdout and stderr write to the same file.
        let log_file_err =
            log_file
                .try_clone()
                .await
                .map_err(|e| DispatchError::AgentLaunchFailed {
                    name: config.name.clone(),
                    reason: format!("failed to clone log file handle: {e}"),
                })?;
        // Convert to std::fs::File for use as Stdio.
        let log_file_std = log_file.into_std().await;
        let log_file_err_std = log_file_err.into_std().await;

        let child = Command::new("sh")
            .arg("-c")
            .arg(&full_command)
            .envs(&env_vars)
            .current_dir(&self.agent_cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(log_file_std))
            .stderr(std::process::Stdio::from(log_file_err_std))
            .process_group(0) // Own process group so we can kill the whole tree.
            .spawn()
            .map_err(|e| DispatchError::AgentLaunchFailed {
                name: config.name.clone(),
                reason: e.to_string(),
            })?;

        let pid = child.id().unwrap_or(0);
        eprintln!(
            "dispatch serve: launched agent '{}' (role={}, pid={}, log={})",
            config.name,
            config.role,
            pid,
            log_path.display()
        );

        self.agents.push(RunningAgent {
            name: config.name.clone(),
            role: config.role.clone(),
            child,
            pid,
        });

        Ok(())
    }

    /// Launch all configured agents sequentially with a brief pause between.
    pub async fn launch_all(
        &mut self,
        configs: &[ResolvedAgentConfig],
    ) -> Result<(), DispatchError> {
        for config in configs {
            self.spawn_agent(config).await?;
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        Ok(())
    }

    /// Return info about all agents.
    pub fn list(&mut self) -> Vec<AgentInfo> {
        self.agents
            .iter_mut()
            .map(|a| {
                let running = a.child.try_wait().ok().flatten().is_none();
                AgentInfo {
                    name: a.name.clone(),
                    role: a.role.clone(),
                    pid: a.pid,
                    running,
                }
            })
            .collect()
    }

    /// Start all configured heartbeat timers.
    pub fn start_heartbeats(
        &mut self,
        configs: &[crate::config::HeartbeatConfig],
        event_tx: &tokio::sync::broadcast::Sender<super::local::BrokerEvent>,
    ) {
        for config in configs {
            let name = config.name.clone();
            let command = config.command.clone();
            let every = config.every;
            let after = config.after.unwrap_or(0);
            let cell_id = self.cell_id.clone();
            let socket_path = self.socket_path.display().to_string();
            let agent_cwd = self.agent_cwd.clone();
            let event_tx = event_tx.clone();

            if after > 0 {
                eprintln!(
                    "dispatch serve: heartbeat '{}' every {}s (after {}s): {}",
                    name, every, after, command
                );
            } else {
                eprintln!(
                    "dispatch serve: heartbeat '{}' every {}s: {}",
                    name, every, command
                );
            }

            let hb_name = name.clone();
            let handle = tokio::spawn(async move {
                // Initial delay: use `after` for first wait, then `every` thereafter.
                let first_delay = if after > 0 { after } else { every };
                tokio::time::sleep(std::time::Duration::from_secs(first_delay)).await;

                loop {
                    tracing::debug!(heartbeat = %hb_name, "firing heartbeat");

                    let result = tokio::process::Command::new("sh")
                        .arg("-c")
                        .arg(&command)
                        .env("DISPATCH_CELL_ID", &cell_id)
                        .env("DISPATCH_SOCKET_PATH", &socket_path)
                        .current_dir(&agent_cwd)
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status()
                        .await;

                    let detail = match result {
                        Ok(status) if status.success() => format!("{hb_name}: ok"),
                        Ok(status) => {
                            format!("{hb_name}: exit {}", status.code().unwrap_or(-1))
                        }
                        Err(e) => format!("{hb_name}: error: {e}"),
                    };

                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    let _ = event_tx.send(super::local::BrokerEvent {
                        kind: "heartbeat".into(),
                        worker_id: String::new(),
                        worker_name: None,
                        detail,
                        payload: Some(serde_json::json!({
                            "name": hb_name,
                            "command": command,
                            "every": every,
                        })),
                        timestamp: now,
                    });

                    tokio::time::sleep(std::time::Duration::from_secs(every)).await;
                }
            });

            self.heartbeats.push(RunningHeartbeat { name, handle });
        }
    }

    /// Kill all running agent processes, cancel heartbeats, and wait for cleanup.
    pub async fn shutdown_all(&mut self) {
        // Cancel heartbeat timers.
        for hb in &self.heartbeats {
            tracing::info!(heartbeat = %hb.name, "stopping heartbeat");
            hb.handle.abort();
        }
        self.heartbeats.clear();

        // Kill agent processes.
        for agent in &mut self.agents {
            if agent.child.try_wait().ok().flatten().is_none() {
                tracing::info!(agent = %agent.name, pid = agent.pid, "stopping agent");
                kill_process_group(agent.pid).await;
                // Reap the child to avoid zombies.
                let _ = agent.child.wait().await;
            }
        }
        self.agents.clear();
    }

    /// Stop a specific agent by name.
    pub async fn stop_agent(&mut self, name: &str) -> bool {
        if let Some(idx) = self.agents.iter().position(|a| a.name == name) {
            let agent = &mut self.agents[idx];
            if agent.child.try_wait().ok().flatten().is_none() {
                kill_process_group(agent.pid).await;
                let _ = agent.child.wait().await;
            }
            self.agents.remove(idx);
            true
        } else {
            false
        }
    }
}

/// Build an agent command string for the user to paste.
///
/// Includes the dispatch env vars (shell-escaped) and the resolved command
/// with prompt substitution. Side-effect free — does not write tempfiles
/// even when the command uses `{prompt_file}`; the placeholder is left
/// in place for inline prompts so the user knows where it'll be substituted.
pub fn build_agent_command(
    config: &ResolvedAgentConfig,
    cell_id: &str,
    monitor_url: Option<&str>,
) -> String {
    let mut parts = vec![
        format!("DISPATCH_CELL_ID={}", shell_escape(cell_id)),
        format!("DISPATCH_AGENT_NAME={}", shell_escape(&config.name)),
        format!("DISPATCH_AGENT_ROLE={}", shell_escape(&config.role)),
    ];

    if let Some(url) = monitor_url {
        parts.push(format!("DISPATCH_MONITOR_URL={}", shell_escape(url)));
    }

    parts.push(AgentOrchestrator::build_command(config, None));
    parts.join(" ")
}

/// Build the main agent command string for the user to paste.
pub fn build_main_agent_command(
    main: &crate::config::MainAgentConfig,
    cell_id: &str,
    monitor_url: Option<&str>,
) -> String {
    let mut parts = vec![format!("DISPATCH_CELL_ID={}", shell_escape(cell_id))];

    if let Some(url) = monitor_url {
        parts.push(format!("DISPATCH_MONITOR_URL={}", shell_escape(url)));
    }

    let mut cmd = main.command.clone();

    if let Some(ref model) = main.model {
        cmd.push_str(&format!(" --model {model}"));
    }

    if let Some(ref prompt_file) = main.prompt_file {
        // Tell the agent to read the prompt file rather than inlining it.
        let msg = format!("Read and follow the instructions in {prompt_file}");
        cmd.push_str(&format!(" {}", shell_escape(&msg)));
    } else if let Some(ref prompt) = main.prompt {
        cmd.push_str(&format!(" {}", shell_escape(prompt)));
    }

    parts.push(cmd);
    parts.join(" ")
}

/// Shell-escape a string for use in `sh -c` commands. POSIX single-quote escaping.
pub(super) fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Whether `s` is safe to use as a single filename component on disk and as
/// a path segment in a URL (no separators, no `..`, ASCII alphanumerics plus
/// `-` and `_`).
pub(super) fn is_safe_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Replace any character outside `[A-Za-z0-9_-]` with `_`. Used to derive a
/// filesystem-safe component from a user-supplied agent name.
pub(super) fn sanitize_name(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() {
        out.push('_');
    }
    out
}

/// Write a prompt to a temporary file and return the path. The file is placed
/// in the OS temp directory and named by sanitised agent name to aid debugging.
fn write_prompt_tempfile(prompt: &str, agent_name: &str) -> std::io::Result<PathBuf> {
    let safe_name = sanitize_name(agent_name);
    let path = std::env::temp_dir().join(format!("dispatch-prompt-{safe_name}.md"));
    std::fs::write(&path, prompt)?;
    Ok(path)
}
