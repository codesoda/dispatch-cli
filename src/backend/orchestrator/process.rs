use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::process::{Child, Command};
use tokio::sync::{Mutex, Notify};

use crate::config::ResolvedAgentConfig;
use crate::errors::DispatchError;

use super::{sanitize_name, AgentOrchestrator, AgentState};

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
pub(super) fn restart_backoff(attempt: u32) -> Duration {
    let secs = 1u64
        .checked_shl(attempt.saturating_sub(1))
        .unwrap_or(u64::MAX);
    Duration::from_secs(secs.min(30))
}

/// Spawn the agent process (single attempt, no supervision).
///
/// Used both for the initial launch and for respawns inside the supervisor.
/// Stdout/stderr append to `<log_dir>/<sanitized-name>.log`; restarts append
/// to the same file so the full history is preserved across respawns.
pub(super) async fn spawn_child_process(
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
pub(super) async fn supervise_agent(
    config: ResolvedAgentConfig,
    env_vars: HashMap<String, String>,
    agent_cwd: PathBuf,
    log_dir: PathBuf,
    initial_child: Child,
    state: Arc<Mutex<AgentState>>,
    shutdown: Arc<Notify>,
    // `Some((broker, worker_id, role_prompt))` for managed agents that need
    // the broker worker record + role prompt re-stored on every respawn.
    // `None` for unmanaged agents on the legacy path.
    re_register: Option<(
        Arc<Mutex<crate::backend::local::BrokerState>>,
        String,
        String,
    )>,
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
                            started_at: crate::backend::local::now_secs(),
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
