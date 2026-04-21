use std::collections::{HashMap, HashSet};
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
/// resets. A process that stayed up at least this long is treated as a
/// fresh first-attempt restart when it next exits — the counter is set
/// back to `1` (not `0`), so the agent gets the full `MAX_RESTART_ATTEMPTS`
/// budget from that point forward.
const STABLE_AFTER: Duration = Duration::from_secs(30);

/// Kill an entire process group. Sends SIGTERM first, then SIGKILL after a timeout.
///
/// Refuses to signal `pid == 0`: `libc::kill(-0, …)` signals the *caller's*
/// process group, which would tear down `dispatch serve` itself along with
/// every sibling supervisor. A missing/zero PID means the child already
/// exited or was never spawned, so there is nothing to kill.
async fn kill_process_group(pid: u32) {
    if pid == 0 {
        tracing::warn!("kill_process_group called with pid=0; refusing to signal caller's pgid");
        return;
    }
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

/// Snapshot of the orchestrator's immutable spawn-side state. Created by
/// `AgentOrchestrator::snapshot_spawn_context` so the heavy spawn work can
/// run with the orchestrator mutex released. See `monitor::api_agent_start`
/// for the canonical three-phase usage (check → build → register).
pub struct SpawnContext {
    broker: Arc<Mutex<super::local::BrokerState>>,
    cell_id: String,
    socket_path: PathBuf,
    monitor_url: Option<String>,
    agent_cwd: PathBuf,
    log_dir: PathBuf,
}

impl SpawnContext {
    fn env_vars(&self, name: &str, role: &str, worker_id: Option<&str>) -> HashMap<String, String> {
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
        if let Some(id) = worker_id {
            vars.insert("DISPATCH_WORKER_ID".into(), id.into());
        }
        vars
    }
}

/// Opaque handle to an agent that has been spawned but not yet registered
/// with the orchestrator. Returned by `build_pending_agent`; consumed by
/// `AgentOrchestrator::register_pending`. Wraps the private `ManagedAgent`
/// so callers outside this module can move spawn results across the lock
/// boundary without touching internals.
pub struct PendingAgent {
    inner: ManagedAgent,
}

/// Manages the lifecycle of agent subprocesses and heartbeat timers.
pub struct AgentOrchestrator {
    agents: Vec<ManagedAgent>,
    /// Names that have passed `check_can_start` but haven't yet reached
    /// `register_pending`. The 3-phase pattern releases the orchestrator
    /// mutex during `build_pending_agent`; without this reservation, two
    /// concurrent `AgentStart` calls for the same name would both pass
    /// phase 1's "not in agents" check and both push duplicate entries
    /// in phase 3. Phase 1 atomically inserts here; phase 3 (success) or
    /// `cancel_start` (build failure) removes.
    starting: HashSet<String>,
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
    /// Shared broker state — used by `spawn_agent` to pre-register managed
    /// agents server-side at spawn time (issue #43) and by the supervisor
    /// to re-register them on restart so the worker record + role prompt
    /// stay alive across respawns.
    broker: Arc<Mutex<super::local::BrokerState>>,
}

impl AgentOrchestrator {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cell_id: &str,
        socket_path: &Path,
        monitor_url: Option<String>,
        agent_cwd: &Path,
        log_dir: PathBuf,
        configs: Vec<ResolvedAgentConfig>,
        broker: Arc<Mutex<super::local::BrokerState>>,
    ) -> Self {
        Self {
            agents: Vec::new(),
            starting: HashSet::new(),
            heartbeats: Vec::new(),
            cell_id: cell_id.to_string(),
            socket_path: socket_path.to_path_buf(),
            monitor_url,
            agent_cwd: agent_cwd.to_path_buf(),
            log_dir,
            configs,
            broker,
        }
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
            stream_json: config.stream_json,
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
    ///
    /// **Issue #43 — managed-agent pre-register flow.** When the agent has
    /// `launch = true` AND a `prompt_file`, dispatch:
    /// 1. Reads the prompt file content into memory.
    /// 2. Writes a one-line boot prompt that forces the model's first
    ///    observable action to be `dispatch register --for-agent ...` —
    ///    the broker returns the role prompt body in that response.
    ///    Done BEFORE step 3 so a write failure can't leave a zombie
    ///    worker in the broker.
    /// 3. Generates a fresh worker id and registers the worker server-side
    ///    (eviction on, so a stale same-name worker from a crashed prior
    ///    session is replaced).
    /// 4. Injects `DISPATCH_WORKER_ID` so the boot command can name the id.
    /// 5. Hands the supervisor a re-register hook so the worker record +
    ///    role prompt stay alive across respawns.
    ///
    /// Agents with `launch = false` are external (user copy-pastes the
    /// command into a separate terminal) and stay on the legacy path:
    /// no pre-register, full prompt piped to stdin as before.
    ///
    /// **Mutex contention warning.** This holds `&mut self` for the entire
    /// duration, including the prompt file read, boot prompt write, broker
    /// `register_worker` call, and `spawn_child_process` await. HTTP / IPC
    /// handlers wrapping the orchestrator in `Arc<Mutex<…>>` should
    /// instead use the three-phase pattern: `check_can_start` (locked) →
    /// `build_pending_agent` (unlocked) → `register_pending` (locked).
    /// See `monitor::api_agent_start` for the canonical example.
    pub async fn spawn_agent(&mut self, config: &ResolvedAgentConfig) -> Result<(), DispatchError> {
        let snapshot = self.snapshot_spawn_context();
        let pending = build_pending_agent(snapshot, config).await?;
        self.register_pending(pending);
        Ok(())
    }

    /// Validate that an agent named `name` can be started right now and
    /// reserve the slot. Reaps any finished supervisors, rejects if an
    /// instance is already running OR another concurrent caller is
    /// already in the middle of starting one, looks up the config, and
    /// inserts `name` into `starting`. Pure synchronous validation —
    /// no IO, no broker calls — so callers can release the orchestrator
    /// mutex immediately after and run the heavy spawn work unlocked.
    ///
    /// On success the caller MUST eventually call either `register_pending`
    /// (build succeeded) or `cancel_start` (build failed). Otherwise the
    /// `starting` reservation lingers and future starts of the same name
    /// will be rejected with "already starting" forever.
    ///
    /// Companion to `build_pending_agent` + `register_pending` for the
    /// three-phase start pattern used by HTTP / IPC handlers.
    pub fn check_can_start(&mut self, name: &str) -> Result<ResolvedAgentConfig, DispatchError> {
        self.agents.retain(|a| !a.supervisor.is_finished());

        if self.agents.iter().any(|a| a.name == name) {
            return Err(DispatchError::AgentLaunchFailed {
                name: name.to_string(),
                reason: "already running".into(),
            });
        }
        if self.starting.contains(name) {
            return Err(DispatchError::AgentLaunchFailed {
                name: name.to_string(),
                reason: "already starting from a concurrent request".into(),
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
        self.starting.insert(name.to_string());
        Ok(config)
    }

    /// Release a `starting` reservation that was claimed by
    /// `check_can_start` but won't be completed. Call from the build
    /// error path of the three-phase pattern; without it the slot is
    /// permanently reserved and future starts of the same name fail.
    pub fn cancel_start(&mut self, name: &str) {
        self.starting.remove(name);
    }

    /// Snapshot the immutable orchestrator state needed to spawn an agent.
    /// Cheap clones (PathBuf / String / Arc) so the caller can release the
    /// orchestrator mutex before running the heavyweight async spawn.
    pub fn snapshot_spawn_context(&self) -> SpawnContext {
        SpawnContext {
            broker: Arc::clone(&self.broker),
            cell_id: self.cell_id.clone(),
            socket_path: self.socket_path.clone(),
            monitor_url: self.monitor_url.clone(),
            agent_cwd: self.agent_cwd.clone(),
            log_dir: self.log_dir.clone(),
        }
    }

    /// Commit a freshly-spawned agent to the orchestrator's tracked set.
    /// Phase 3 of the unlocked-spawn pattern — caller has already done
    /// `check_can_start` (phase 1, claimed the `starting` reservation)
    /// and `build_pending_agent` (phase 2, did the heavy IO work).
    /// Removes the reservation and pushes onto `agents`. No race-recheck
    /// needed: the phase 1 reservation guaranteed mutual exclusion against
    /// other concurrent starts of this name.
    ///
    /// Tolerant of being called without a prior reservation (e.g. via
    /// `spawn_agent`'s always-locked path) — the remove is a no-op when
    /// the name isn't present.
    pub fn register_pending(&mut self, pending: PendingAgent) {
        self.starting.remove(&pending.inner.name);
        self.agents.push(pending.inner);
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
        let config = self.check_can_start(name)?;
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

/// Spawn an agent process and assemble the supervisor task **without**
/// touching the orchestrator. Phase 2 of the unlocked-spawn pattern:
/// `AgentOrchestrator::check_can_start` (locked) → this (unlocked) →
/// `AgentOrchestrator::register_pending` (locked).
///
/// All the heavy waits live here: prompt file read, boot prompt write,
/// broker pre-register, and `spawn_child_process`. Running this with the
/// orchestrator mutex released keeps the dashboard's 2 s
/// `/api/agents/state` poll from stalling on a slow start. The caller is
/// responsible for calling `check_can_start` first to validate the name.
pub async fn build_pending_agent(
    ctx: SpawnContext,
    config: &ResolvedAgentConfig,
) -> Result<PendingAgent, DispatchError> {
    let pre_register = config.launch && config.prompt_file_path.is_some();

    let (env_vars, worker_id, role_prompt, spawn_config) =
        if pre_register {
            let prompt_path = config.prompt_file_path.as_ref().ok_or_else(|| {
                DispatchError::AgentLaunchFailed {
                    name: config.name.clone(),
                    reason: "pre_register set but prompt_file_path is None".into(),
                }
            })?;
            let prompt_content = tokio::fs::read_to_string(prompt_path).await.map_err(|_| {
                DispatchError::PromptFileNotFound {
                    name: config.name.clone(),
                    path: prompt_path.clone(),
                }
            })?;

            // Write the boot prompt BEFORE pre-registering so a write failure
            // (disk full, log_dir not writable, etc.) can't leave an orphan
            // worker in the broker. After register_worker runs, any subsequent
            // failure has to route through the cleanup guard below to maintain
            // the "no zombie state" invariant.
            let boot_path = write_boot_prompt(&ctx.log_dir, config).await.map_err(|e| {
                DispatchError::AgentLaunchFailed {
                    name: config.name.clone(),
                    reason: format!("write boot prompt: {e}"),
                }
            })?;

            let id = uuid::Uuid::new_v4().to_string();
            {
                let mut broker = ctx.broker.lock().await;
                broker
                    .register_worker(
                        config.name.clone(),
                        config.role.clone(),
                        config.description.clone(),
                        Vec::new(),
                        config.ttl,
                        true, // evict any stale same-name worker from a prior session
                        Some(id.clone()),
                        Some(prompt_content.clone()),
                    )
                    .map_err(|e| DispatchError::AgentLaunchFailed {
                        name: config.name.clone(),
                        reason: format!("pre-register failed: {e}"),
                    })?;
            }

            let mut sc = config.clone();
            sc.prompt_file_path = Some(boot_path);

            let env = ctx.env_vars(&config.name, &config.role, Some(&id));
            (env, Some(id), Some(prompt_content), sc)
        } else {
            // Legacy unmanaged path (launch=false, or no prompt_file).
            let env = ctx.env_vars(&config.name, &config.role, None);
            (env, None, None, config.clone())
        };

    // Initial spawn is synchronous so the caller sees config errors
    // (bad prompt file, missing binary) as a proper Err instead of
    // discovering them later via AgentState::Crashed.
    let child =
        match spawn_child_process(&spawn_config, &env_vars, &ctx.agent_cwd, &ctx.log_dir).await {
            Ok(c) => c,
            Err(e) => {
                // Clean up the pre-registered worker so we don't leave an
                // orphan in the broker that will never check in. Routes
                // through remove_worker so mailboxes/notifiers go with it.
                if let Some(ref id) = worker_id {
                    let mut b = ctx.broker.lock().await;
                    b.remove_worker(id);
                }
                return Err(e);
            }
        };
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

    // Re-register hook: passed to the supervisor so it can refresh the
    // worker record (and re-store the role prompt) on every respawn.
    // None for unmanaged agents — the supervisor stays on legacy behavior.
    let re_register = worker_id
        .zip(role_prompt)
        .map(|(id, prompt)| (Arc::clone(&ctx.broker), id, prompt));

    let supervisor = tokio::spawn(supervise_agent(
        spawn_config,
        env_vars,
        ctx.agent_cwd.clone(),
        ctx.log_dir.clone(),
        child,
        Arc::clone(&state),
        Arc::clone(&shutdown),
        re_register,
    ));

    Ok(PendingAgent {
        inner: ManagedAgent {
            name: config.name.clone(),
            role: config.role.clone(),
            state,
            shutdown,
            supervisor,
        },
    })
}

/// Write the issue-#43 one-line boot prompt to a stable per-agent file
/// under the log dir. The boot prompt forces the model's first observable
/// action to be a real `dispatch register --for-agent` tool call, which
/// returns the role prompt body in its tool result. Description is
/// substituted at write time (shell-escaped) so the dispatch CLI's
/// required `--description` flag is satisfied without an env var.
async fn write_boot_prompt(
    log_dir: &Path,
    config: &ResolvedAgentConfig,
) -> Result<PathBuf, std::io::Error> {
    tokio::fs::create_dir_all(log_dir).await?;
    let safe = sanitize_name(&config.name);
    let path = log_dir.join(format!("{safe}.boot.prompt"));
    let body = format!(
        "Run: dispatch register --worker-id \"$DISPATCH_WORKER_ID\" \
         --name \"$DISPATCH_AGENT_NAME\" --role \"$DISPATCH_AGENT_ROLE\" \
         --description {} --for-agent\n",
        shell_escape(&config.description),
    );
    tokio::fs::write(&path, body).await?;
    Ok(path)
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
#[allow(clippy::too_many_arguments)]
async fn supervise_agent(
    config: ResolvedAgentConfig,
    env_vars: HashMap<String, String>,
    agent_cwd: PathBuf,
    log_dir: PathBuf,
    initial_child: Child,
    state: Arc<Mutex<AgentState>>,
    shutdown: Arc<Notify>,
    // `Some((broker, worker_id, role_prompt))` for managed agents that need
    // the broker worker record + role prompt re-stored on every respawn
    // (issue #43). `None` for unmanaged agents on the legacy path.
    re_register: Option<(Arc<Mutex<super::local::BrokerState>>, String, String)>,
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

                // Issue #43: refresh the broker's worker record + role
                // prompt before the respawn so the agent's claim call can
                // get its prompt back even if TTL expired during downtime.
                // Three branches in `BrokerState::register_worker`:
                // - Supervisor's worker still alive (TTL not expired):
                //   idempotent-claim short-circuit matches name+role,
                //   renews TTL, refreshes description (and capabilities
                //   if non-empty). evict pass is BYPASSED — a same-name
                //   worker with a different id from a racing manual
                //   `dispatch register` would persist alongside us.
                // - Supervisor's worker GC'd: idempotent-claim misses,
                //   evict=true wipes any same-name worker with a
                //   different id, then a fresh entry is created using
                //   the supervisor's id.
                // - worker_id collision (different name+role under our
                //   id — essentially impossible with UUIDs but defended
                //   against): register_worker returns Err, handled below
                //   as terminal Crashed.
                //
                // Treat the Err case as terminal: if we can't restore
                // the broker state, the respawned child will fail its
                // `--for-agent` lookup and crash-loop until
                // MAX_RESTART_ATTEMPTS with only a generic "exited with
                // ..." reason. Surface the real cause immediately instead.
                if let Some((broker, worker_id, role_prompt)) = &re_register {
                    let register_result = {
                        let mut b = broker.lock().await;
                        b.register_worker(
                            config.name.clone(),
                            config.role.clone(),
                            config.description.clone(),
                            Vec::new(),
                            config.ttl,
                            true,
                            Some(worker_id.clone()),
                            Some(role_prompt.clone()),
                        )
                        // `b` drops here so `state.lock().await` below
                        // doesn't pin the broker mutex across the await.
                    };
                    if let Err(err) = register_result {
                        tracing::error!(
                            agent = %config.name,
                            %err,
                            "restart re-register failed; marking agent crashed",
                        );
                        *state.lock().await = AgentState::Crashed {
                            reason: format!("re-register failed: {err}"),
                            attempts: attempt,
                        };
                        return;
                    }
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
                        // Symmetric with the initial-spawn cleanup guard:
                        // the re-register above restored the worker record,
                        // but spawn_child_process failed (log dir gone,
                        // prompt file deleted, binary missing, ...). Drop
                        // the broker entry so it doesn't linger as a zombie
                        // for a process that will never run.
                        if let Some((broker, worker_id, _)) = &re_register {
                            let mut b = broker.lock().await;
                            b.remove_worker(worker_id);
                        }
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
/// launch string. `worker_id` is `Some` only for the managed-spawn print path
/// (issue #43); the unmanaged copy-paste path leaves it `None` so the agent
/// receives a broker-assigned id from its own `dispatch register` call.
pub fn build_agent_command(
    config: &ResolvedAgentConfig,
    cell_id: &str,
    monitor_url: Option<&str>,
    worker_id: Option<&str>,
) -> String {
    let mut parts = vec![
        format!("DISPATCH_CELL_ID={}", shell_escape(cell_id)),
        format!("DISPATCH_AGENT_NAME={}", shell_escape(&config.name)),
        format!("DISPATCH_AGENT_ROLE={}", shell_escape(&config.role)),
    ];

    if let Some(url) = monitor_url {
        parts.push(format!("DISPATCH_MONITOR_URL={}", shell_escape(url)));
    }

    if let Some(id) = worker_id {
        parts.push(format!("DISPATCH_WORKER_ID={}", shell_escape(id)));
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

    /// Constructing `SpawnContext::env_vars(_, _, Some(id))` injects
    /// `DISPATCH_WORKER_ID`; `None` omits the key entirely so legacy
    /// register-yourself agents see exactly the previous environment.
    #[test]
    fn env_vars_includes_worker_id_when_some() {
        let tmp = tempfile::tempdir().unwrap();
        let orch = AgentOrchestrator::new(
            "cell-x",
            &tmp.path().join("broker.sock"),
            None,
            tmp.path(),
            tmp.path().join("logs"),
            Vec::new(),
            Arc::new(Mutex::new(super::super::local::BrokerState::new())),
        );
        let ctx = orch.snapshot_spawn_context();

        let with_id = ctx.env_vars("alice", "test-runner", Some("w-123"));
        assert_eq!(
            with_id.get("DISPATCH_WORKER_ID").map(String::as_str),
            Some("w-123")
        );
        assert_eq!(
            with_id.get("DISPATCH_AGENT_NAME").map(String::as_str),
            Some("alice")
        );
        assert_eq!(
            with_id.get("DISPATCH_AGENT_ROLE").map(String::as_str),
            Some("test-runner")
        );

        let without_id = ctx.env_vars("alice", "test-runner", None);
        assert!(!without_id.contains_key("DISPATCH_WORKER_ID"));
        // The other vars must match exactly so the legacy code path is bit-for-bit unchanged.
        assert_eq!(
            without_id.get("DISPATCH_AGENT_NAME").map(String::as_str),
            Some("alice")
        );
        assert_eq!(
            without_id.get("DISPATCH_AGENT_ROLE").map(String::as_str),
            Some("test-runner")
        );
    }

    /// `build_agent_command` emits `DISPATCH_WORKER_ID=<id>` only when the id
    /// is supplied — the unmanaged copy-paste path keeps its previous output.
    #[test]
    fn build_agent_command_includes_worker_id_when_some() {
        let cfg = test_config("alice", "echo hi");
        let with_id = build_agent_command(&cfg, "cell-x", None, Some("w-123"));
        // shell_escape wraps the value in single quotes; assert on the
        // assignment as it would appear after escaping.
        assert!(
            with_id.contains("DISPATCH_WORKER_ID='w-123'"),
            "expected worker id in: {with_id}",
        );

        let without_id = build_agent_command(&cfg, "cell-x", None, None);
        assert!(
            !without_id.contains("DISPATCH_WORKER_ID"),
            "did not expect worker id in: {without_id}",
        );
    }

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
            stream_json: false,
            launch: true,
        }
    }

    /// Helper: build a managed test config (launch=true, with prompt_file)
    /// that exercises the issue-#43 pre-register flow. Uses the `command`
    /// adapter so we don't need `claude` on the test host — the prompt file
    /// is created but ignored by the adapter; what we're testing is whether
    /// the orchestrator correctly pre-registers the worker server-side.
    fn managed_test_config(name: &str, command: &str, prompt_path: PathBuf) -> ResolvedAgentConfig {
        ResolvedAgentConfig {
            name: name.into(),
            role: "test-runner".into(),
            description: "managed test agent".into(),
            adapter: Adapter::Command,
            command: Some(command.into()),
            extra_args: Vec::new(),
            prompt: None,
            prompt_file_path: Some(prompt_path),
            ttl: None,
            stream_json: false,
            launch: true,
        }
    }

    /// Issue #43: spawning a managed agent (launch=true with prompt_file)
    /// pre-registers the worker server-side BEFORE the child starts, with
    /// the prompt body stored under the assigned worker id.
    #[tokio::test]
    async fn spawn_managed_agent_pre_registers_with_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        let prompt_path = tmp.path().join("alice.md");
        let prompt_body = "Run: dispatch listen --timeout 270\nRole context here.";
        tokio::fs::write(&prompt_path, prompt_body).await.unwrap();

        let broker = Arc::new(Mutex::new(super::super::local::BrokerState::new()));
        let mut orch = AgentOrchestrator::new(
            "test-cell",
            &tmp.path().join("broker.sock"),
            None,
            tmp.path(),
            tmp.path().join("logs"),
            Vec::new(),
            Arc::clone(&broker),
        );
        let cfg = managed_test_config("alice", "sleep 30", prompt_path);
        orch.spawn_agent(&cfg).await.expect("spawn");

        // Worker is in the broker right away — the agent's later claim
        // call will idempotently match it without inventing an id.
        let b = broker.lock().await;
        assert_eq!(b.workers.len(), 1, "pre-register must create the worker");
        let (id, worker) = b.workers.iter().next().unwrap();
        assert_eq!(worker.name, "alice");
        assert_eq!(worker.role, "test-runner");
        assert_eq!(
            b.role_prompts.get(id).map(String::as_str),
            Some(prompt_body),
            "role prompt must be stored under the worker id",
        );

        drop(b);
        orch.shutdown_all().await;
    }

    /// Issue #43: launch=false agents stay on the legacy register-yourself
    /// path. No worker shows up in the broker after `spawn_agent`.
    #[tokio::test]
    async fn spawn_unmanaged_agent_does_not_pre_register() {
        let tmp = tempfile::tempdir().unwrap();
        let broker = Arc::new(Mutex::new(super::super::local::BrokerState::new()));
        let mut orch = AgentOrchestrator::new(
            "test-cell",
            &tmp.path().join("broker.sock"),
            None,
            tmp.path(),
            tmp.path().join("logs"),
            Vec::new(),
            Arc::clone(&broker),
        );
        let mut cfg = test_config("bob", "sleep 30");
        cfg.launch = false; // explicitly unmanaged
        orch.spawn_agent(&cfg).await.expect("spawn");

        let b = broker.lock().await;
        assert!(
            b.workers.is_empty(),
            "unmanaged agents must not be pre-registered: {:?}",
            b.workers,
        );
        drop(b);
        orch.shutdown_all().await;
    }

    /// Issue #43: a missing prompt file fails the spawn and leaves NO
    /// orphan worker in the broker. The read_to_string `?` returns before
    /// `register_worker` is reached, so nothing is ever created — the
    /// early return (not the cleanup guard) is what prevents the orphan.
    #[tokio::test]
    async fn spawn_managed_agent_missing_prompt_file_leaves_no_orphan() {
        let tmp = tempfile::tempdir().unwrap();
        let broker = Arc::new(Mutex::new(super::super::local::BrokerState::new()));
        let mut orch = AgentOrchestrator::new(
            "test-cell",
            &tmp.path().join("broker.sock"),
            None,
            tmp.path(),
            tmp.path().join("logs"),
            Vec::new(),
            Arc::clone(&broker),
        );
        let cfg = managed_test_config("alice", "sleep 30", tmp.path().join("does-not-exist.md"));
        let err = orch.spawn_agent(&cfg).await.expect_err("must fail");
        assert!(
            matches!(err, DispatchError::PromptFileNotFound { .. }),
            "expected PromptFileNotFound, got: {err:?}",
        );
        let b = broker.lock().await;
        assert!(
            b.workers.is_empty(),
            "failed pre-register must not leave an orphan worker",
        );
    }

    /// Issue #43: when `spawn_child_process` fails AFTER the pre-register
    /// succeeds, the cleanup guard must remove the worker record, role
    /// prompt, mailbox, and notifier so no zombie state survives in the
    /// broker. We trigger a spawn failure by pointing `agent_cwd` at a
    /// path that doesn't exist — `Command::current_dir` errors with ENOENT
    /// when the spawn syscall tries to chdir.
    #[tokio::test]
    async fn spawn_managed_agent_spawn_failure_triggers_cleanup_guard() {
        let tmp = tempfile::tempdir().unwrap();
        let prompt_path = tmp.path().join("alice.md");
        tokio::fs::write(&prompt_path, "role prompt body")
            .await
            .unwrap();

        let bad_cwd = tmp.path().join("does-not-exist-cwd");
        let broker = Arc::new(Mutex::new(super::super::local::BrokerState::new()));
        let mut orch = AgentOrchestrator::new(
            "test-cell",
            &tmp.path().join("broker.sock"),
            None,
            &bad_cwd,
            tmp.path().join("logs"),
            Vec::new(),
            Arc::clone(&broker),
        );

        // Force `spawn_agent` to fail and verify the broker is left with no
        // residual state. This covers cleanup of any entries created during
        // the failed spawn attempt across `workers`, `role_prompts`,
        // `mailboxes`, and `notifiers`.
        let cfg = managed_test_config("alice", "true", prompt_path);
        let err = orch.spawn_agent(&cfg).await.expect_err("must fail");
        assert!(
            matches!(err, DispatchError::AgentLaunchFailed { .. }),
            "expected AgentLaunchFailed, got: {err:?}",
        );

        let b = broker.lock().await;
        assert!(
            b.workers.is_empty(),
            "spawn failure must not leave an orphan worker: {:?}",
            b.workers,
        );
        assert!(
            b.role_prompts.is_empty(),
            "spawn failure must not leave an orphan role prompt: {:?}",
            b.role_prompts,
        );
        assert!(
            b.mailboxes.is_empty(),
            "spawn failure must not leave an orphan mailbox",
        );
        assert!(
            b.notifiers.is_empty(),
            "spawn failure must not leave an orphan notifier",
        );
    }

    /// `check_can_start` reserves the agent name in `starting` so a
    /// concurrent caller in the unlocked-spawn pattern can't pass phase
    /// 1 for the same name. `register_pending` and `cancel_start` both
    /// release the reservation. Without this, two parallel
    /// `BrokerRequest::AgentStart` calls would race past `check_can_start`
    /// and push duplicate `ManagedAgent` entries.
    #[tokio::test]
    async fn check_can_start_reserves_name_until_register_or_cancel() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_config("alice", "sleep 30");
        let mut orch = AgentOrchestrator::new(
            "test-cell",
            &tmp.path().join("broker.sock"),
            None,
            tmp.path(),
            tmp.path().join("logs"),
            vec![cfg.clone()],
            Arc::new(Mutex::new(super::super::local::BrokerState::new())),
        );

        // First reservation succeeds.
        orch.check_can_start("alice").expect("first reservation");

        // Second reservation while the first is still pending must fail
        // with "already starting" — this is the race-defense that makes
        // the 3-phase pattern safe.
        let err = orch
            .check_can_start("alice")
            .expect_err("second reservation must be rejected");
        match err {
            DispatchError::AgentLaunchFailed { name, reason } => {
                assert_eq!(name, "alice");
                assert!(
                    reason.contains("already starting"),
                    "expected 'already starting' rejection, got: {reason}"
                );
            }
            other => panic!("expected AgentLaunchFailed, got: {other:?}"),
        }

        // cancel_start releases the slot — a fresh check_can_start succeeds.
        orch.cancel_start("alice");
        orch.check_can_start("alice")
            .expect("post-cancel reservation");

        // Now exercise the success path via a real spawn. spawn_agent is
        // tolerant of the lingering reservation (register_pending removes
        // it) so the agent ends up in `agents` with no leftover slot.
        orch.spawn_agent(&cfg).await.expect("spawn");
        // After register_pending, "alice" is in agents and NOT in starting.
        // Starting a fresh "alice" now hits the "already running" guard,
        // not "already starting".
        let err = orch.check_can_start("alice").expect_err("alice is running");
        match err {
            DispatchError::AgentLaunchFailed { reason, .. } => {
                assert!(
                    reason.contains("already running"),
                    "expected 'already running', got: {reason}"
                );
            }
            other => panic!("expected AgentLaunchFailed, got: {other:?}"),
        }
        orch.shutdown_all().await;
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
            Arc::new(Mutex::new(super::super::local::BrokerState::new())),
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
            Arc::new(Mutex::new(super::super::local::BrokerState::new())),
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
