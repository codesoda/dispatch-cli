use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::process::{Child, Command};
use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;

use crate::adapter::{shell_arg_quote, shell_escape, BuildContext, Launch};
use crate::config::ResolvedAgentConfig;
use crate::errors::DispatchError;

/// Maximum consecutive restart attempts before marking an agent as crashed.
/// Counter resets when the agent runs for at least `STABLE_AFTER` seconds.
const MAX_RESTART_ATTEMPTS: u32 = 5;

/// Duration an agent must stay running before its restart attempt counter
/// resets. A process that crashes after this threshold gets a fresh budget.
const STABLE_AFTER: Duration = Duration::from_secs(30);

/// Kill an entire process group. Sends SIGTERM first, then SIGKILL after a timeout.
async fn kill_process_group(pid: u32) {
    let pgid = pid as i32;
    // Send SIGTERM to the process group.
    unsafe {
        libc::kill(-pgid, libc::SIGTERM);
    }
    // Give processes a moment to exit gracefully.
    tokio::time::sleep(Duration::from_millis(500)).await;
    // Force kill any remaining processes.
    unsafe {
        libc::kill(-pgid, libc::SIGKILL);
    }
}

/// Exponential backoff (capped at 30s) for consecutive restart attempts.
/// attempt=1 → 1s, 2 → 2s, 3 → 4s, 4 → 8s, 5 → 16s, 6+ → 30s.
fn restart_backoff(attempt: u32) -> Duration {
    let secs = 1u64
        .checked_shl(attempt.saturating_sub(1))
        .unwrap_or(u64::MAX);
    Duration::from_secs(secs.min(30))
}

/// Runtime state of a supervised agent. Consumed by the monitor UI and tests.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum AgentState {
    /// Supervisor has been created but the first spawn hasn't landed yet.
    Starting,
    /// A child process is alive. `started_at` is a Unix-seconds timestamp of
    /// the most recent spawn — the UI renders uptime as `now - started_at`.
    Running { pid: u32, started_at: u64 },
    /// Process exited; supervisor is waiting before respawning.
    Restarting { attempt: u32, backoff_secs: u64 },
    /// Restart budget exhausted — supervisor has given up.
    Crashed { reason: String, attempts: u32 },
    /// Supervisor was asked to stop (either by the user or on shutdown).
    Stopped,
}

/// An agent under supervisor control. The supervisor owns the `Child` handle
/// and respawns on exit; the orchestrator only holds signaling primitives and
/// observable state.
struct ManagedAgent {
    name: String,
    role: String,
    state: Arc<Mutex<AgentState>>,
    /// Notified when the supervisor should stop restarting. Using `notify_one`
    /// is sufficient because each supervisor has at most one pending wait.
    shutdown: Arc<Notify>,
    supervisor: JoinHandle<()>,
}

/// A running heartbeat timer task.
struct RunningHeartbeat {
    name: String,
    handle: tokio::task::JoinHandle<()>,
}

/// Manages the lifecycle of agent subprocesses and heartbeat timers.
pub struct AgentOrchestrator {
    agents: Vec<ManagedAgent>,
    heartbeats: Vec<RunningHeartbeat>,
    cell_id: String,
    socket_path: PathBuf,
    monitor_url: Option<String>,
    /// Working directory for spawned agent subprocesses.
    agent_cwd: PathBuf,
    /// Where per-agent log files are written. Must match the directory the
    /// monitor's `/api/logs/{agent}` endpoint reads from.
    log_dir: PathBuf,
    /// All configured agents, used to look up an agent's config by name
    /// when responding to `dispatch agent start/restart <name>` requests.
    configs: Vec<ResolvedAgentConfig>,
}

impl AgentOrchestrator {
    pub fn new(
        cell_id: &str,
        socket_path: &Path,
        monitor_url: Option<String>,
        agent_cwd: &Path,
        log_dir: PathBuf,
        configs: Vec<ResolvedAgentConfig>,
    ) -> Self {
        Self {
            agents: Vec::new(),
            heartbeats: Vec::new(),
            cell_id: cell_id.to_string(),
            socket_path: socket_path.to_path_buf(),
            monitor_url,
            agent_cwd: agent_cwd.to_path_buf(),
            log_dir,
            configs,
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

    /// Spawn an agent and attach a supervisor task that restarts it on exit
    /// (with exponential backoff) until the restart budget is exhausted.
    ///
    /// Stdout/stderr go to `<log_dir>/<sanitized-agent-name>.log`. Stdin
    /// comes from the adapter's `stdin_file` (typically the prompt file) or
    /// is `/dev/null` if the adapter doesn't need one.
    pub async fn spawn_agent(&mut self, config: &ResolvedAgentConfig) -> Result<(), DispatchError> {
        let env_vars = self.env_vars(&config.name, &config.role);

        // Initial spawn is synchronous so the caller sees config errors
        // (bad prompt file, missing binary) as a proper Err instead of
        // discovering them later via AgentState::Crashed.
        let child = spawn_child_process(config, &env_vars, &self.agent_cwd, &self.log_dir).await?;
        let pid = child.id().unwrap_or(0);
        eprintln!(
            "dispatch serve: launched agent '{}' (role={}, pid={})",
            config.name, config.role, pid
        );

        let state = Arc::new(Mutex::new(AgentState::Running {
            pid,
            started_at: super::local::now_secs(),
        }));
        let shutdown = Arc::new(Notify::new());

        let supervisor = tokio::spawn(supervise_agent(
            config.clone(),
            env_vars,
            self.agent_cwd.clone(),
            self.log_dir.clone(),
            child,
            Arc::clone(&state),
            Arc::clone(&shutdown),
        ));

        self.agents.push(ManagedAgent {
            name: config.name.clone(),
            role: config.role.clone(),
            state,
            shutdown,
            supervisor,
        });

        Ok(())
    }

    /// Snapshot of all managed agents' current runtime state.
    pub async fn list_state(&self) -> Vec<(String, String, AgentState)> {
        let mut out = Vec::with_capacity(self.agents.len());
        for a in &self.agents {
            out.push((a.name.clone(), a.role.clone(), a.state.lock().await.clone()));
        }
        out
    }

    /// Launch every agent marked `launch = true` in the stored configs,
    /// sequentially with a brief pause between.
    pub async fn launch_all(&mut self) -> Result<(), DispatchError> {
        let launchable: Vec<ResolvedAgentConfig> =
            self.configs.iter().filter(|a| a.launch).cloned().collect();
        for config in &launchable {
            self.spawn_agent(config).await?;
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        Ok(())
    }

    /// Start a configured agent by name. Errors if the name is unknown or
    /// an instance is already running.
    ///
    /// Before rejecting on `already running`, this reaps any ManagedAgent
    /// whose supervisor task has already finished (Crashed or Stopped), so a
    /// previously-failed agent can be started again without needing `restart`.
    pub async fn start_by_name(&mut self, name: &str) -> Result<(), DispatchError> {
        self.agents.retain(|a| !a.supervisor.is_finished());

        if self.agents.iter().any(|a| a.name == name) {
            return Err(DispatchError::AgentLaunchFailed {
                name: name.to_string(),
                reason: "already running".into(),
            });
        }
        let config = self
            .configs
            .iter()
            .find(|a| a.name == name)
            .cloned()
            .ok_or_else(|| DispatchError::AgentLaunchFailed {
                name: name.to_string(),
                reason: "no such agent in config".into(),
            })?;
        self.spawn_agent(&config).await
    }

    /// Restart a running agent: stop it (if running) then spawn it again.
    /// Errors if the agent name isn't in config.
    pub async fn restart_by_name(&mut self, name: &str) -> Result<(), DispatchError> {
        let config = self
            .configs
            .iter()
            .find(|a| a.name == name)
            .cloned()
            .ok_or_else(|| DispatchError::AgentLaunchFailed {
                name: name.to_string(),
                reason: "no such agent in config".into(),
            })?;
        let _ = self.stop_by_name(name).await;
        self.spawn_agent(&config).await
    }

    /// Whether an agent with this name exists in the config (running or not).
    pub fn has_config(&self, name: &str) -> bool {
        self.configs.iter().any(|a| a.name == name)
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

    /// Signal every supervisor to stop, cancel heartbeats, and wait for
    /// supervisors to finish reaping their children.
    pub async fn shutdown_all(&mut self) {
        // Cancel heartbeat timers.
        for hb in &self.heartbeats {
            tracing::info!(heartbeat = %hb.name, "stopping heartbeat");
            hb.handle.abort();
        }
        self.heartbeats.clear();

        // Signal every supervisor to stop (no respawn), then await them.
        for agent in &self.agents {
            tracing::info!(agent = %agent.name, "stopping agent");
            agent.shutdown.notify_one();
        }
        for agent in self.agents.drain(..) {
            let _ = agent.supervisor.await;
        }
    }

    /// Signal a running agent to shut down and return its supervisor's
    /// `JoinHandle`. Does **not** await the handle — the caller is expected
    /// to `.await` it after releasing the orchestrator mutex. This avoids
    /// holding `Arc<Mutex<AgentOrchestrator>>` across the 500ms
    /// SIGTERM→SIGKILL window inside `kill_process_group`, which would
    /// otherwise block every concurrent read of `list_state` (e.g. the
    /// dashboard's 2s `/api/agents/state` poll).
    ///
    /// Returns `None` if no agent by that name is currently supervised.
    pub fn signal_stop_by_name(&mut self, name: &str) -> Option<JoinHandle<()>> {
        let idx = self.agents.iter().position(|a| a.name == name)?;
        let agent = self.agents.remove(idx);
        agent.shutdown.notify_one();
        Some(agent.supervisor)
    }

    /// Stop a running agent by name. Convenience wrapper around
    /// `signal_stop_by_name` for test / non-HTTP callers that are fine
    /// awaiting inside the critical section (e.g. the full `shutdown_all`
    /// path). Returns `true` if an agent was found and stopped.
    pub async fn stop_by_name(&mut self, name: &str) -> bool {
        match self.signal_stop_by_name(name) {
            Some(handle) => {
                let _ = handle.await;
                true
            }
            None => false,
        }
    }
}

/// Spawn the agent process (single attempt, no supervision).
///
/// Used both for the initial launch and for respawns inside the supervisor.
/// Stdout/stderr append to `<log_dir>/<sanitized-name>.log`; restarts append
/// to the same file so the full history is preserved across respawns.
async fn spawn_child_process(
    config: &ResolvedAgentConfig,
    env_vars: &HashMap<String, String>,
    agent_cwd: &Path,
    log_dir: &Path,
) -> Result<Child, DispatchError> {
    let launch =
        AgentOrchestrator::build_launch(config).map_err(|e| DispatchError::AgentLaunchFailed {
            name: config.name.clone(),
            reason: e.to_string(),
        })?;

    tracing::info!(
        agent = %config.name,
        role = %config.role,
        adapter = %config.adapter,
        program = %launch.program,
        args = ?launch.args,
        cwd = %agent_cwd.display(),
        "launching agent"
    );

    tokio::fs::create_dir_all(&log_dir)
        .await
        .map_err(|e| DispatchError::AgentLaunchFailed {
            name: config.name.clone(),
            reason: format!("failed to create log dir {}: {e}", log_dir.display()),
        })?;
    let safe_name = sanitize_name(&config.name);
    let log_path = log_dir.join(format!("{safe_name}.log"));
    // Append rather than truncate so restart logs are retained.
    let log_file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .await
        .map_err(|e| DispatchError::AgentLaunchFailed {
            name: config.name.clone(),
            reason: format!("failed to open log file {}: {e}", log_path.display()),
        })?;
    let log_file_err =
        log_file
            .try_clone()
            .await
            .map_err(|e| DispatchError::AgentLaunchFailed {
                name: config.name.clone(),
                reason: format!("failed to clone log file handle: {e}"),
            })?;
    let log_file_std = log_file.into_std().await;
    let log_file_err_std = log_file_err.into_std().await;

    let stdin = match &launch.stdin_file {
        Some(path) => {
            let f = tokio::fs::File::open(path).await.map_err(|e| {
                DispatchError::AgentLaunchFailed {
                    name: config.name.clone(),
                    reason: format!("failed to open prompt file {}: {e}", path.display()),
                }
            })?;
            std::process::Stdio::from(f.into_std().await)
        }
        None => std::process::Stdio::null(),
    };

    Command::new(&launch.program)
        .args(&launch.args)
        .envs(env_vars)
        .current_dir(agent_cwd)
        .stdin(stdin)
        .stdout(std::process::Stdio::from(log_file_std))
        .stderr(std::process::Stdio::from(log_file_err_std))
        .process_group(0)
        .spawn()
        .map_err(|e| DispatchError::AgentLaunchFailed {
            name: config.name.clone(),
            reason: e.to_string(),
        })
}

/// Supervisor loop for a single agent.
///
/// - Waits for the initial `child` to exit or for a shutdown signal.
/// - On exit: if the agent ran for at least `STABLE_AFTER`, reset the attempt
///   counter (a long-lived process that dies once shouldn't exhaust the
///   budget). Otherwise increment.
/// - Applies `restart_backoff(attempt)` before respawning.
/// - Gives up after `MAX_RESTART_ATTEMPTS` consecutive unstable failures and
///   leaves `AgentState::Crashed` in place.
async fn supervise_agent(
    config: ResolvedAgentConfig,
    env_vars: HashMap<String, String>,
    agent_cwd: PathBuf,
    log_dir: PathBuf,
    initial_child: Child,
    state: Arc<Mutex<AgentState>>,
    shutdown: Arc<Notify>,
) {
    let mut child = initial_child;
    let mut attempt: u32 = 0;
    let mut started_at = Instant::now();

    loop {
        let pid = child.id().unwrap_or(0);
        tokio::select! {
            _ = shutdown.notified() => {
                tracing::info!(agent = %config.name, pid, "shutdown requested");
                kill_process_group(pid).await;
                let _ = child.wait().await;
                *state.lock().await = AgentState::Stopped;
                return;
            }
            status = child.wait() => {
                let ran_for = started_at.elapsed();
                tracing::info!(
                    agent = %config.name,
                    ?status,
                    ran_secs = ran_for.as_secs(),
                    "agent exited",
                );

                if ran_for >= STABLE_AFTER {
                    attempt = 1;
                } else {
                    attempt = attempt.saturating_add(1);
                }

                if attempt > MAX_RESTART_ATTEMPTS {
                    let reason = match status {
                        Ok(s) => format!("exited with {s}"),
                        Err(e) => format!("wait error: {e}"),
                    };
                    tracing::warn!(
                        agent = %config.name,
                        attempts = attempt - 1,
                        %reason,
                        "restart budget exhausted; marking crashed",
                    );
                    *state.lock().await = AgentState::Crashed {
                        reason,
                        attempts: attempt - 1,
                    };
                    return;
                }

                let backoff = restart_backoff(attempt);
                tracing::info!(
                    agent = %config.name,
                    attempt,
                    backoff_secs = backoff.as_secs(),
                    "restarting after backoff",
                );
                *state.lock().await = AgentState::Restarting {
                    attempt,
                    backoff_secs: backoff.as_secs(),
                };

                // Sleep with shutdown cancellation.
                tokio::select! {
                    _ = shutdown.notified() => {
                        *state.lock().await = AgentState::Stopped;
                        return;
                    }
                    _ = tokio::time::sleep(backoff) => {}
                }

                match spawn_child_process(&config, &env_vars, &agent_cwd, &log_dir).await {
                    Ok(new_child) => {
                        child = new_child;
                        started_at = Instant::now();
                        let new_pid = child.id().unwrap_or(0);
                        *state.lock().await = AgentState::Running {
                            pid: new_pid,
                            started_at: super::local::now_secs(),
                        };
                    }
                    Err(e) => {
                        tracing::warn!(agent = %config.name, error = %e, "respawn failed");
                        *state.lock().await = AgentState::Crashed {
                            reason: format!("respawn failed: {e}"),
                            attempts: attempt,
                        };
                        return;
                    }
                }
            }
        }
    }
}

/// Build an agent command string for the user to paste.
///
/// Includes the dispatch env vars (shell-escaped) and the adapter-assembled
/// launch string.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::Adapter;

    /// Exponential doubling capped at 30s — table-driven so the intent is
    /// obvious if anyone tunes the backoff schedule later.
    #[test]
    fn restart_backoff_schedule() {
        let cases = [(1, 1), (2, 2), (3, 4), (4, 8), (5, 16), (6, 30), (10, 30)];
        for (attempt, expected) in cases {
            assert_eq!(
                restart_backoff(attempt).as_secs(),
                expected,
                "attempt {attempt} should back off {expected}s",
            );
        }
    }

    /// Build a ResolvedAgentConfig that runs a short sh command under the
    /// `command` adapter — avoids needing `claude`/`codex` binaries on the
    /// test host.
    fn test_config(name: &str, command: &str) -> ResolvedAgentConfig {
        ResolvedAgentConfig {
            name: name.into(),
            role: "test".into(),
            description: "".into(),
            adapter: Adapter::Command,
            command: Some(command.into()),
            extra_args: Vec::new(),
            prompt: None,
            prompt_file_path: None,
            ttl: None,
            launch: true,
        }
    }

    /// Supervisor reports Running while a long-lived child is alive, and
    /// transitions to Stopped after `stop_by_name`.
    #[tokio::test]
    async fn supervisor_running_then_stopped() {
        let tmp = tempfile::tempdir().unwrap();
        let mut orch = AgentOrchestrator::new(
            "test-cell",
            &tmp.path().join("broker.sock"),
            None,
            tmp.path(),
            tmp.path().join("logs"),
            Vec::new(),
        );
        let cfg = test_config("alice", "sleep 30");
        orch.spawn_agent(&cfg).await.expect("spawn");
        // Let the supervisor publish its Running state.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let states = orch.list_state().await;
        assert_eq!(states.len(), 1);
        assert!(matches!(states[0].2, AgentState::Running { .. }));

        assert!(orch.stop_by_name("alice").await);
        assert!(orch.list_state().await.is_empty());
    }

    /// When the child exits quickly, the supervisor moves through
    /// Running → Restarting (attempt=1) before the first-backoff sleep
    /// completes. We probe at 300ms — well after exit, well before the 1s
    /// backoff elapses.
    #[tokio::test]
    async fn supervisor_transitions_to_restarting_after_quick_exit() {
        let tmp = tempfile::tempdir().unwrap();
        let mut orch = AgentOrchestrator::new(
            "test-cell",
            &tmp.path().join("broker.sock"),
            None,
            tmp.path(),
            tmp.path().join("logs"),
            Vec::new(),
        );
        let cfg = test_config("flaky", "exit 1");
        orch.spawn_agent(&cfg).await.expect("spawn");
        tokio::time::sleep(Duration::from_millis(300)).await;
        let states = orch.list_state().await;
        assert_eq!(states.len(), 1);
        assert!(
            matches!(
                states[0].2,
                AgentState::Restarting {
                    attempt: 1,
                    backoff_secs: 1
                } | AgentState::Running { .. }
            ),
            "unexpected state: {:?}",
            states[0].2
        );

        // Shutdown should cancel the backoff sleep cleanly.
        orch.shutdown_all().await;
        assert!(orch.list_state().await.is_empty());
    }
}
