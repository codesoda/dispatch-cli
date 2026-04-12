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

/// Standard dispatch-comms instructions embedded at compile time.
const DISPATCH_COMMS: &str = include_str!("comms.md");

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
}

impl AgentOrchestrator {
    pub fn new(
        cell_id: &str,
        socket_path: &Path,
        monitor_url: Option<String>,
        project_root: &Path,
    ) -> Self {
        Self {
            agents: Vec::new(),
            heartbeats: Vec::new(),
            cell_id: cell_id.to_string(),
            socket_path: socket_path.to_path_buf(),
            monitor_url,
            project_root: project_root.to_path_buf(),
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
    fn build_command(config: &ResolvedAgentConfig) -> String {
        let base = &config.command;

        // For claude/codex commands, append the prompt with comms instructions
        let is_llm = base.starts_with("claude") || base.starts_with("codex");
        if is_llm {
            let full_prompt = if let Some(ref prompt) = config.prompt {
                format!("{DISPATCH_COMMS}\n\n---\n\n{prompt}")
            } else {
                DISPATCH_COMMS.to_string()
            };
            format!("{base} -p {}", shell_escape(&full_prompt))
        } else {
            // Shell commands run as-is with env vars
            base.clone()
        }
    }

    /// Spawn a single agent as a subprocess.
    pub async fn spawn_agent(&mut self, config: &ResolvedAgentConfig) -> Result<(), DispatchError> {
        let env_vars = self.env_vars(&config.name, &config.role);
        let full_command = Self::build_command(config);

        tracing::info!(
            agent = %config.name,
            role = %config.role,
            command = %config.command,
            "launching agent"
        );

        let child = Command::new("sh")
            .arg("-c")
            .arg(&full_command)
            .envs(&env_vars)
            .current_dir(&self.project_root)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit())
            .process_group(0) // Own process group so we can kill the whole tree.
            .spawn()
            .map_err(|e| DispatchError::AgentLaunchFailed {
                name: config.name.clone(),
                reason: e.to_string(),
            })?;

        let pid = child.id().unwrap_or(0);
        eprintln!(
            "dispatch serve: launched agent '{}' (role={}, pid={})",
            config.name, config.role, pid
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
                        kind: "heartbeat".into(),
                        worker_id: String::new(),
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

    parts.push(main.command.clone());

    if let Some(ref model) = main.model {
        parts.push(format!("--model {model}"));
    }

    if let Some(ref prompt) = main.prompt {
        let full_prompt = format!("{DISPATCH_COMMS}\n\n---\n\n{prompt}");
        parts.push(format!("\"{}\"", full_prompt.replace('"', "\\\"")));
    } else {
        parts.push(format!("\"{}\"", DISPATCH_COMMS.replace('"', "\\\"")));
    }

    parts.join(" ")
}

/// Shell-escape a string for use in `sh -c` commands.
fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
