use std::collections::HashMap;
use std::path::{Path, PathBuf};

use tokio::process::{Child, Command};

use crate::adapter::{BuildContext, Launch};
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
    project_root: PathBuf,
    log_dir: PathBuf,
}

impl AgentOrchestrator {
    pub fn new(
        cell_id: &str,
        socket_path: &Path,
        monitor_url: Option<String>,
        project_root: &Path,
    ) -> Self {
        let log_dir = project_root.join("logs");
        Self {
            agents: Vec::new(),
            heartbeats: Vec::new(),
            cell_id: cell_id.to_string(),
            socket_path: socket_path.to_path_buf(),
            monitor_url,
            project_root: project_root.to_path_buf(),
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

    /// Build the adapter's Launch spec for this agent.
    ///
    /// The Launch describes the program, argv, shell-wrap hint, and optional
    /// stdin source. Callers translate it into a concrete `Command` (for
    /// spawning) or a shell-pasteable string (for display).
    pub(super) fn build_launch(config: &ResolvedAgentConfig) -> Result<Launch, DispatchError> {
        let ctx = BuildContext {
            extra_args: &config.extra_args,
            prompt_file: config.prompt_file_path.as_deref(),
            prompt_inline: config.prompt.as_deref(),
            command_string: config.command.as_deref(),
        };
        config
            .adapter
            .build(&ctx)
            .map_err(|e| DispatchError::AgentLaunchFailed {
                name: config.name.clone(),
                reason: e.to_string(),
            })
    }

    /// Spawn a single agent as a subprocess.
    ///
    /// Stdout and stderr are redirected to `logs/<agent-name>.log` relative to
    /// the project root so that agent output can be inspected after the fact.
    /// Stdin comes from the adapter's `stdin_file` (typically the prompt file)
    /// or is `/dev/null` if the adapter doesn't need one.
    pub async fn spawn_agent(&mut self, config: &ResolvedAgentConfig) -> Result<(), DispatchError> {
        let env_vars = self.env_vars(&config.name, &config.role);
        let launch = Self::build_launch(config)?;

        tracing::info!(
            agent = %config.name,
            role = %config.role,
            adapter = %config.adapter,
            program = %launch.program,
            args = ?launch.args,
            cwd = %self.project_root.display(),
            "launching agent"
        );

        // Ensure log directory exists and open a per-agent log file.
        std::fs::create_dir_all(&self.log_dir).map_err(|e| DispatchError::AgentLaunchFailed {
            name: config.name.clone(),
            reason: format!("failed to create log dir {}: {e}", self.log_dir.display()),
        })?;
        let log_path = self.log_dir.join(format!("{}.log", config.name));
        let log_file =
            std::fs::File::create(&log_path).map_err(|e| DispatchError::AgentLaunchFailed {
                name: config.name.clone(),
                reason: format!("failed to create log file {}: {e}", log_path.display()),
            })?;
        let log_file_err = log_file
            .try_clone()
            .map_err(|e| DispatchError::AgentLaunchFailed {
                name: config.name.clone(),
                reason: format!("failed to clone log file handle: {e}"),
            })?;

        let stdin = match &launch.stdin_file {
            Some(path) => {
                let f =
                    std::fs::File::open(path).map_err(|e| DispatchError::AgentLaunchFailed {
                        name: config.name.clone(),
                        reason: format!("failed to open prompt file {}: {e}", path.display()),
                    })?;
                std::process::Stdio::from(f)
            }
            None => std::process::Stdio::null(),
        };

        let child = Command::new(&launch.program)
            .args(&launch.args)
            .envs(&env_vars)
            .current_dir(&self.project_root)
            .stdin(stdin)
            .stdout(std::process::Stdio::from(log_file))
            .stderr(std::process::Stdio::from(log_file_err))
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
            let project_root = self.project_root.clone();
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
                        .current_dir(&project_root)
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
                        id: uuid::Uuid::new_v4().to_string(),
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
/// Includes the dispatch env vars and the adapter-assembled launch string.
pub fn build_agent_command(
    config: &ResolvedAgentConfig,
    cell_id: &str,
    monitor_url: Option<&str>,
) -> String {
    let mut parts = vec![
        format!("DISPATCH_CELL_ID={cell_id}"),
        format!("DISPATCH_AGENT_NAME={}", config.name),
        format!("DISPATCH_AGENT_ROLE={}", config.role),
    ];

    if let Some(url) = monitor_url {
        parts.push(format!("DISPATCH_MONITOR_URL={url}"));
    }

    let cmd_str = AgentOrchestrator::build_launch(config)
        .map(|launch| launch_to_shell_string(&launch))
        .unwrap_or_else(|e| format!("# adapter error: {e}"));
    parts.push(cmd_str);
    parts.join(" ")
}

/// Translate a Launch into a shell-pasteable command string.
///
/// For the `command` adapter, returns the user's original shell string
/// verbatim (not wrapped in `sh -c '...'`). For claude/codex, quotes each
/// argument as needed and appends `< <prompt_file>` if stdin is redirected.
fn launch_to_shell_string(launch: &Launch) -> String {
    if launch.wrap_in_shell {
        return launch.args.get(1).cloned().unwrap_or_default();
    }

    let mut parts: Vec<String> = vec![shell_arg_quote(&launch.program)];
    for arg in &launch.args {
        parts.push(shell_arg_quote(arg));
    }
    let mut s = parts.join(" ");
    if let Some(path) = &launch.stdin_file {
        s.push_str(&format!(
            " < {}",
            shell_arg_quote(&path.display().to_string())
        ));
    }
    s
}

/// Quote a shell argument only when necessary. Plain alphanumerics, dashes,
/// dots, slashes, underscores, and equals signs pass through unquoted for
/// paste-friendly output.
fn shell_arg_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_/.=".contains(c))
    {
        s.to_string()
    } else {
        shell_escape(s)
    }
}

/// Build the main agent command string for the user to paste.
pub fn build_main_agent_command(
    main: &crate::config::MainAgentConfig,
    cell_id: &str,
    monitor_url: Option<&str>,
) -> String {
    let mut parts = vec![format!("DISPATCH_CELL_ID={cell_id}")];

    if let Some(url) = monitor_url {
        parts.push(format!("DISPATCH_MONITOR_URL={url}"));
    }

    let mut cmd = main.command.clone();

    if let Some(ref model) = main.model {
        cmd.push_str(&format!(" --model {model}"));
    }

    if let Some(ref prompt_file) = main.prompt_file {
        // Tell the agent to read the prompt file rather than inlining it.
        let msg = format!("Read and follow the instructions in {prompt_file}");
        cmd.push_str(&format!(" \"{}\"", msg.replace('"', "\\\"")));
    } else if let Some(ref prompt) = main.prompt {
        cmd.push_str(&format!(" \"{}\"", prompt.replace('"', "\\\"")));
    }

    parts.push(cmd);
    parts.join(" ")
}

/// Shell-escape a string for use in `sh -c` commands.
fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
