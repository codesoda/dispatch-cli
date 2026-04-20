use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::Router;
use futures_util::stream::Stream;
use tokio::sync::{broadcast, Mutex, Notify};

use super::local::{BrokerEvent, BrokerState};

/// Shared state for the monitor HTTP server.
#[derive(Clone)]
pub struct MonitorState {
    pub broker: Arc<Mutex<BrokerState>>,
    pub events: broadcast::Sender<BrokerEvent>,
    pub shutdown: Arc<Notify>,
    pub name: Option<String>,
    pub cell_id: String,
    /// Unix timestamp (seconds) when the server started.
    pub started_at: u64,
    pub agents: Vec<crate::config::ResolvedAgentConfig>,
    pub main_agent: Option<crate::config::MainAgentConfig>,
    pub heartbeats: Vec<crate::config::HeartbeatConfig>,
    pub log_dir: PathBuf,
    pub monitor_url: Option<String>,
    /// Orchestrator handle used to read live supervisor state for each
    /// managed agent. Shared with the broker's request handler.
    pub orchestrator: Arc<Mutex<super::orchestrator::AgentOrchestrator>>,
}

/// Start the HTTP monitor dashboard on the given port.
pub async fn run_monitor(
    port: u16,
    state: MonitorState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let app = Router::new()
        .route("/", get(dashboard))
        .route("/api/team", get(api_team))
        .route("/api/events", get(api_events))
        .route("/api/health", get(api_health))
        .route("/api/agents", get(api_agents))
        .route("/api/agents/state", get(api_agents_state))
        .route("/api/logs/{agent}", get(api_logs))
        .route("/api/events/history", get(api_events_history))
        .route("/api/messages/{worker}", get(api_messages))
        .route("/api/shutdown", axum::routing::post(api_shutdown))
        .route(
            "/api/agents/{name}/start",
            axum::routing::post(api_agent_start),
        )
        .route(
            "/api/agents/{name}/stop",
            axum::routing::post(api_agent_stop),
        )
        .route(
            "/api/agents/{name}/restart",
            axum::routing::post(api_agent_restart),
        )
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    tracing::info!(port, "monitor dashboard listening");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Serve the embedded HTML dashboard.
async fn dashboard() -> Html<&'static str> {
    Html(include_str!("monitor.html"))
}

/// Return the current team as JSON.
/// Evicts expired workers first and emits expire events so the dashboard
/// stays consistent even when no other request triggers eviction.
async fn api_team(State(state): State<MonitorState>) -> axum::Json<Vec<crate::protocol::Worker>> {
    let mut broker = state.broker.lock().await;
    let expired = broker.evict_expired();
    for (id, name) in &expired {
        let _ = state.events.send(super::local::BrokerEvent {
            kind: "expire".to_string(),
            worker_id: id.clone(),
            worker_name: Some(name.clone()),
            detail: "worker expired".to_string(),
            payload: None,
            timestamp: super::local::now_secs(),
        });
    }
    let workers = broker.list_workers();
    axum::Json(workers)
}

/// Stream broker events via Server-Sent Events.
async fn api_events(
    State(state): State<MonitorState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let rx = state.events.subscribe();

    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let sse_event = Event::default().event("broker").json_data(&event).unwrap();
                    return Some((Ok(sse_event), rx));
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    });

    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// Health check endpoint — returns server timestamps and stats.
/// Sends absolute UTC timestamps so the UI can tick locally between polls.
async fn api_health(State(state): State<MonitorState>) -> axum::Json<serde_json::Value> {
    let mut broker = state.broker.lock().await;
    let workers = broker.list_workers();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    axum::Json(serde_json::json!({
        "status": "ok",
        "name": state.name,
        "cell_id": state.cell_id,
        "started_at": state.started_at,
        "server_time": now,
        "workers": workers.len(),
        "messages_sent": broker.messages_sent,
        "messages_delivered": broker.messages_delivered,
        "requests_handled": broker.requests_handled,
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

/// Return configured agent definitions (for the sidebar agent detail view).
async fn api_agents(State(state): State<MonitorState>) -> axum::Json<serde_json::Value> {
    let agents: Vec<serde_json::Value> = state
        .agents
        .iter()
        .map(|a| {
            let launch_cmd = super::orchestrator::build_agent_command(
                a,
                &state.cell_id,
                state.monitor_url.as_deref(),
                None,
            );
            serde_json::json!({
                "name": a.name,
                "role": a.role,
                "description": a.description,
                "adapter": a.adapter.to_string(),
                "command": a.command,
                "extra_args": a.extra_args,
                "prompt": a.prompt,
                "ttl": a.ttl,
                "launch": a.launch,
                "launch_command": launch_cmd,
            })
        })
        .collect();
    let main_agent = state.main_agent.as_ref().map(|m| {
        let launch_cmd = super::orchestrator::build_main_agent_command(
            m,
            &state.cell_id,
            state.monitor_url.as_deref(),
        );
        serde_json::json!({
            "command": m.command,
            "model": m.model,
            "prompt": m.prompt,
            "launch_command": launch_cmd,
        })
    });
    let heartbeats: Vec<serde_json::Value> = state
        .heartbeats
        .iter()
        .map(|h| {
            serde_json::json!({
                "name": h.name,
                "command": h.command,
                "every": h.every,
            })
        })
        .collect();
    axum::Json(serde_json::json!({
        "agents": agents,
        "main_agent": main_agent,
        "heartbeats": heartbeats,
    }))
}

/// Live supervisor state for every managed agent.
///
/// The dashboard polls this to drive the agent cards view. Agents the
/// orchestrator hasn't spawned (launch = false, or explicitly stopped) are
/// simply absent from the response — the UI merges this with `/api/agents`
/// configs to show unmanaged agents with a "not running" placeholder.
async fn api_agents_state(State(state): State<MonitorState>) -> axum::Json<serde_json::Value> {
    let orch = state.orchestrator.lock().await;
    let entries = orch.list_state().await;
    let states: Vec<serde_json::Value> = entries
        .into_iter()
        .map(|(name, role, agent_state)| {
            serde_json::json!({
                "name": name,
                "role": role,
                "state": agent_state,
            })
        })
        .collect();
    axum::Json(serde_json::json!({ "agents": states }))
}

/// Query params for the logs endpoint.
#[derive(serde::Deserialize)]
struct LogQuery {
    /// Number of lines to return from the tail of the file.
    #[serde(default = "default_log_lines")]
    lines: usize,
}
fn default_log_lines() -> usize {
    200
}

/// Hard cap on `lines` to bound memory usage on a large log file.
const MAX_LOG_LINES: usize = 5_000;

/// Return the tail of an agent's log file.
async fn api_logs(
    State(state): State<MonitorState>,
    Path(agent): Path<String>,
    Query(query): Query<LogQuery>,
) -> axum::response::Response {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;

    // Allowlist agent name: must be a single non-empty filename component
    // composed only of `[A-Za-z0-9_-]` so it cannot escape `log_dir`.
    if !super::orchestrator::is_safe_name(&agent) {
        return (StatusCode::BAD_REQUEST, "invalid agent name").into_response();
    }

    let log_path = state.log_dir.join(format!("{agent}.log"));
    let content = match tokio::fs::read_to_string(&log_path).await {
        Ok(c) => c,
        Err(_) => return (StatusCode::NOT_FOUND, "log file not found").into_response(),
    };

    // Return the last N lines, capped to MAX_LOG_LINES.
    let requested = query.lines.min(MAX_LOG_LINES);
    let lines: Vec<&str> = content.lines().collect();
    let start = lines.len().saturating_sub(requested);
    let tail: String = lines[start..].join("\n");

    (
        StatusCode::OK,
        [("content-type", "text/plain; charset=utf-8")],
        tail,
    )
        .into_response()
}

/// Query params for the events history endpoint.
#[derive(serde::Deserialize)]
struct EventsQuery {
    since: Option<u64>,
    until: Option<u64>,
    #[serde(rename = "type")]
    event_type: Option<String>,
    worker: Option<String>,
    limit: Option<usize>,
}

/// Return filtered event history as JSON.
async fn api_events_history(
    State(state): State<MonitorState>,
    Query(query): Query<EventsQuery>,
) -> axum::Json<serde_json::Value> {
    let broker = state.broker.lock().await;
    let events = broker.query_events(
        query.since,
        query.until,
        query.event_type.as_deref(),
        query.worker.as_deref(),
        query.limit,
    );
    let events_json: Vec<serde_json::Value> = events
        .into_iter()
        .map(|e| serde_json::to_value(e).unwrap_or_default())
        .collect();
    axum::Json(serde_json::json!({ "events": events_json }))
}

/// Query params for the messages endpoint.
#[derive(serde::Deserialize)]
struct MessagesQuery {
    #[serde(default)]
    unacked: bool,
    #[serde(default)]
    sent: bool,
    since: Option<u64>,
    limit: Option<usize>,
}

/// Return message history for a worker as JSON.
async fn api_messages(
    State(state): State<MonitorState>,
    Path(worker): Path<String>,
    Query(query): Query<MessagesQuery>,
) -> axum::Json<serde_json::Value> {
    let broker = state.broker.lock().await;
    let messages = broker.query_messages(
        &worker,
        query.unacked,
        query.sent,
        query.since,
        query.limit,
        None,
    );
    let messages: Vec<serde_json::Value> = messages
        .into_iter()
        .map(|m| serde_json::to_value(m).unwrap_or_default())
        .collect();
    axum::Json(serde_json::json!({ "messages": messages }))
}

/// Trigger a graceful server shutdown from the monitor UI.
async fn api_shutdown(State(state): State<MonitorState>) -> StatusCode {
    tracing::info!("shutdown requested via monitor");
    state.shutdown.notify_one();
    StatusCode::OK
}

/// Start a configured agent by name. Same authz posture as `/api/shutdown`:
/// unauthenticated, intended for local-loopback use only.
async fn api_agent_start(
    State(state): State<MonitorState>,
    Path(name): Path<String>,
) -> axum::response::Response {
    if !super::orchestrator::is_safe_name(&name) {
        return error_response(StatusCode::BAD_REQUEST, "invalid agent name");
    }
    let mut orch = state.orchestrator.lock().await;
    match orch.start_by_name(&name).await {
        Ok(()) => ok_response(),
        Err(e) => error_response(classify_agent_error(&e), &e.to_string()),
    }
}

/// Stop a running managed agent by name. Releases the orchestrator mutex
/// before awaiting the supervisor's shutdown so the 500ms SIGTERM→SIGKILL
/// window doesn't stall concurrent `/api/agents/state` polls.
async fn api_agent_stop(
    State(state): State<MonitorState>,
    Path(name): Path<String>,
) -> axum::response::Response {
    if !super::orchestrator::is_safe_name(&name) {
        return error_response(StatusCode::BAD_REQUEST, "invalid agent name");
    }

    // Phase 1 (locked): validate + signal the supervisor to stop.
    let handle = {
        let mut orch = state.orchestrator.lock().await;
        if !orch.has_config(&name) {
            return error_response(StatusCode::NOT_FOUND, "no such agent in config");
        }
        orch.signal_stop_by_name(&name)
    };

    // Phase 2 (unlocked): wait for the supervisor to exit without pinning
    // the mutex that list_state / restart / poll all contend for.
    match handle {
        Some(h) => {
            let _ = h.await;
            ok_response()
        }
        None => error_response(StatusCode::CONFLICT, "agent is not running"),
    }
}

/// Stop-then-start an agent by name. Does not hold the orchestrator mutex
/// across the stop's await; the respawn re-acquires the lock afterwards.
async fn api_agent_restart(
    State(state): State<MonitorState>,
    Path(name): Path<String>,
) -> axum::response::Response {
    if !super::orchestrator::is_safe_name(&name) {
        return error_response(StatusCode::BAD_REQUEST, "invalid agent name");
    }

    // Phase 1 (locked): validate name + signal any running instance to stop.
    let handle = {
        let mut orch = state.orchestrator.lock().await;
        if !orch.has_config(&name) {
            return error_response(StatusCode::NOT_FOUND, "no such agent in config");
        }
        orch.signal_stop_by_name(&name)
    };

    // Phase 2 (unlocked): await the old supervisor's exit.
    if let Some(h) = handle {
        let _ = h.await;
    }

    // Phase 3 (locked): respawn. `start_by_name` reaps any finished
    // supervisor entry in case one slipped through between phases.
    let mut orch = state.orchestrator.lock().await;
    match orch.start_by_name(&name).await {
        Ok(()) => ok_response(),
        Err(e) => error_response(classify_agent_error(&e), &e.to_string()),
    }
}

/// Map orchestrator launch errors to HTTP statuses. "No such agent" is 404,
/// "already running" is 409 (resource state conflict), anything else is 400.
fn classify_agent_error(err: &crate::errors::DispatchError) -> StatusCode {
    let msg = err.to_string();
    if msg.contains("no such agent in config") {
        StatusCode::NOT_FOUND
    } else if msg.contains("already running") {
        StatusCode::CONFLICT
    } else {
        StatusCode::BAD_REQUEST
    }
}

fn ok_response() -> axum::response::Response {
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({"status": "ok"})),
    )
        .into_response()
}

fn error_response(code: StatusCode, message: &str) -> axum::response::Response {
    (
        code,
        axum::Json(serde_json::json!({"status": "error", "message": message})),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::Adapter;
    use crate::backend::orchestrator::AgentOrchestrator;
    use crate::config::ResolvedAgentConfig;

    /// Minimal ResolvedAgentConfig that uses the `command` adapter so tests
    /// don't need `claude` or `codex` binaries on the host.
    fn test_agent(name: &str, command: &str) -> ResolvedAgentConfig {
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
            launch: false,
        }
    }

    /// Build a MonitorState backed by a real orchestrator loaded with
    /// `configs` and a disposable log dir under `tmp`.
    fn make_state(tmp: &std::path::Path, configs: Vec<ResolvedAgentConfig>) -> MonitorState {
        let broker = Arc::new(Mutex::new(BrokerState::new()));
        let (events, _) = broadcast::channel(16);
        let shutdown = Arc::new(Notify::new());
        let orchestrator = Arc::new(Mutex::new(AgentOrchestrator::new(
            "test-cell",
            &tmp.join("broker.sock"),
            None,
            tmp,
            tmp.join("logs"),
            configs,
            Arc::clone(&broker),
        )));
        MonitorState {
            broker,
            events,
            shutdown,
            name: None,
            cell_id: "test-cell".into(),
            started_at: 0,
            agents: vec![],
            main_agent: None,
            heartbeats: vec![],
            log_dir: tmp.join("logs"),
            monitor_url: None,
            orchestrator,
        }
    }

    #[tokio::test]
    async fn start_stop_restart_happy_path() {
        let tmp = tempfile::tempdir().unwrap();
        let state = make_state(tmp.path(), vec![test_agent("alice", "sleep 30")]);

        let resp = api_agent_start(State(state.clone()), Path("alice".into())).await;
        assert_eq!(resp.status(), StatusCode::OK);

        // Let the supervisor publish its Running state before we act again.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let resp = api_agent_restart(State(state.clone()), Path("alice".into())).await;
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = api_agent_stop(State(state.clone()), Path("alice".into())).await;
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn start_unknown_agent_returns_404() {
        let tmp = tempfile::tempdir().unwrap();
        let state = make_state(tmp.path(), vec![]);
        let resp = api_agent_start(State(state), Path("nope".into())).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn start_already_running_returns_409() {
        let tmp = tempfile::tempdir().unwrap();
        let state = make_state(tmp.path(), vec![test_agent("busy", "sleep 30")]);

        assert_eq!(
            api_agent_start(State(state.clone()), Path("busy".into()))
                .await
                .status(),
            StatusCode::OK
        );
        let resp = api_agent_start(State(state), Path("busy".into())).await;
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn stop_not_running_returns_409() {
        let tmp = tempfile::tempdir().unwrap();
        let state = make_state(tmp.path(), vec![test_agent("idle", "sleep 30")]);
        let resp = api_agent_stop(State(state), Path("idle".into())).await;
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn stop_unknown_agent_returns_404() {
        let tmp = tempfile::tempdir().unwrap();
        let state = make_state(tmp.path(), vec![]);
        let resp = api_agent_stop(State(state), Path("ghost".into())).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn invalid_agent_name_returns_400() {
        let tmp = tempfile::tempdir().unwrap();
        let state = make_state(tmp.path(), vec![]);
        let resp = api_agent_start(State(state), Path("../evil".into())).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}
