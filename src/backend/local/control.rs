use std::sync::Arc;

use tokio::sync::{broadcast, Mutex};

use crate::backend::orchestrator::AgentOrchestrator;
use crate::protocol::ControlState;

use super::{BrokerEvent, BrokerState};

/// What a coordinator `agent stop`/`restart` marks `stopping`: a single worker
/// (the caller passed a worker id) or every worker registered under an agent
/// name (the caller passed a name). Stopping one replica by id must not take its
/// same-named siblings down with it.
pub(super) enum StopTarget {
    /// A single worker id — only this replica is marked.
    Worker(String),
    /// An agent name — every worker registered under it is marked.
    Name(String),
}

/// Resolve a coordinator stop/restart argument (a worker id or an agent name)
/// into the agent name that drives the supervisor plus the [`StopTarget`] that
/// decides how many workers are marked `stopping`. Precedence mirrors
/// [`resolve_agent_target`]: a configured agent name targets every worker under
/// it; otherwise a live worker id targets just that worker; an unknown value
/// falls back to a name target (the supervisor then surfaces "no such agent").
pub(super) async fn resolve_stop_target(
    arg: &str,
    state: &Arc<Mutex<BrokerState>>,
    orchestrator: &Arc<Mutex<AgentOrchestrator>>,
) -> (String, StopTarget) {
    {
        let orch = orchestrator.lock().await;
        if orch.has_config(arg) {
            return (arg.to_string(), StopTarget::Name(arg.to_string()));
        }
    }
    let s = state.lock().await;
    if let Some(name) = s.worker_name(arg) {
        return (name.to_string(), StopTarget::Worker(arg.to_string()));
    }
    (arg.to_string(), StopTarget::Name(arg.to_string()))
}

/// Mark a [`StopTarget`] `stopping` (stamping the drain clock) and emit a
/// lifecycle event per affected worker. Called before the process is killed so a
/// late stop-hook call from the dying agent sees `stopping` and is allowed to
/// exit, and so the record drains rather than vanishing instantly. A no-op when
/// the target matches no live worker (e.g. an unmanaged agent that never
/// attached).
pub(super) async fn mark_worker_stopping(
    state: &Arc<Mutex<BrokerState>>,
    event_tx: &broadcast::Sender<BrokerEvent>,
    target: &StopTarget,
) {
    let mut s = state.lock().await;
    let ids = match target {
        StopTarget::Worker(id) => {
            if s.set_control_state(id, ControlState::Stopping) {
                vec![id.clone()]
            } else {
                Vec::new()
            }
        }
        StopTarget::Name(name) => s.set_control_state_by_name(name, ControlState::Stopping),
    };
    for id in &ids {
        let name = s.worker_name(id).map(str::to_string);
        s.emit_and_record(
            event_tx,
            "lifecycle",
            id,
            name.as_deref(),
            "active -> stopping",
            Some(serde_json::json!({ "control_state": "stopping" })),
        );
    }
}

/// Resolve an `agent` subcommand target string (agent name or worker ID) to a
/// configured agent name. If the input already matches a configured agent
/// name, it's returned as-is. Otherwise, treat it as a worker ID and look up
/// the worker's name in the broker registry. Falls back to the input itself
/// when neither lookup succeeds — the orchestrator will then surface a
/// "no such agent in config" error.
pub(super) async fn resolve_agent_target(
    target: &str,
    state: &Arc<Mutex<BrokerState>>,
    orchestrator: &Arc<Mutex<AgentOrchestrator>>,
) -> String {
    {
        let orch = orchestrator.lock().await;
        if orch.has_config(target) {
            return target.to_string();
        }
    }
    let state = state.lock().await;
    if let Some(name) = state.worker_name(target) {
        return name.to_string();
    }
    target.to_string()
}
