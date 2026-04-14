use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::Html;
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
    pub orchestrator: super::orchestrator::SharedOrchestrator,
    pub agent_defaults: Option<crate::config::AgentDefaultsConfig>,
    /// Buffered event log for replaying to new SSE subscribers.
    pub event_log: Arc<Mutex<Vec<BrokerEvent>>>,
}

/// Maximum number of events to keep in the replay buffer.
const EVENT_LOG_CAP: usize = 500;

/// Start the HTTP monitor dashboard on the given port.
pub async fn run_monitor(
    port: u16,
    state: MonitorState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Spawn a background task that accumulates broadcast events into the log buffer.
    {
        let mut rx = state.events.subscribe();
        let log = Arc::clone(&state.event_log);
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        let mut buf = log.lock().await;
                        buf.push(event);
                        if buf.len() > EVENT_LOG_CAP {
                            let excess = buf.len() - EVENT_LOG_CAP;
                            buf.drain(..excess);
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    let app = Router::new()
        .route("/", get(dashboard))
        .route("/api/team", get(api_team))
        .route("/api/events", get(api_events))
        .route("/api/health", get(api_health))
        .route("/api/agents", get(api_agents))
        .route("/api/agents/status", get(api_agents_status))
        .route("/api/agents/stop", axum::routing::post(api_agent_stop))
        .route("/api/agents/start", axum::routing::post(api_agent_start))
        .route("/api/agents/spawn", axum::routing::post(api_agent_spawn))
        .route("/api/shutdown", axum::routing::post(api_shutdown))
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
    for id in &expired {
        let _ = state.events.send(super::local::BrokerEvent {
            kind: "expire".to_string(),
            worker_id: id.clone(),
            detail: "worker expired".to_string(),
            payload: None,
            timestamp: super::local::now_secs(),
        });
    }
    let workers = broker.list_workers();
    axum::Json(workers)
}

/// Stream broker events via Server-Sent Events.
/// Replays buffered events first so new subscribers don't miss early events.
async fn api_events(
    State(state): State<MonitorState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    // Snapshot the current log and subscribe to live events.
    let replay = state.event_log.lock().await.clone();
    let rx = state.events.subscribe();

    // State machine: first replay buffered events, then stream live.
    enum Phase {
        Replay(Vec<BrokerEvent>, usize, broadcast::Receiver<BrokerEvent>),
        Live(broadcast::Receiver<BrokerEvent>),
    }

    let initial = Phase::Replay(replay, 0, rx);
    let stream = futures_util::stream::unfold(initial, |phase| async move {
        match phase {
            Phase::Replay(events, idx, rx) => {
                if idx < events.len() {
                    let event = &events[idx];
                    let sse_event = Event::default().event("broker").json_data(event).unwrap();
                    Some((Ok(sse_event), Phase::Replay(events, idx + 1, rx)))
                } else {
                    // Replay done, switch to live.
                    Some((
                        Ok(Event::default().comment("replay complete")),
                        Phase::Live(rx),
                    ))
                }
            }
            Phase::Live(mut rx) => loop {
                match rx.recv().await {
                    Ok(event) => {
                        let sse_event = Event::default().event("broker").json_data(&event).unwrap();
                        return Some((Ok(sse_event), Phase::Live(rx)));
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            },
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
            serde_json::json!({
                "name": a.name,
                "role": a.role,
                "description": a.description,
                "command": a.command,
                "prompt": a.prompt,
                "ttl": a.ttl,
            })
        })
        .collect();
    let main_agent = state.main_agent.as_ref().map(|m| {
        serde_json::json!({
            "command": m.command,
            "model": m.model,
            "prompt": m.prompt,
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

/// Return live orchestrator state merged with config.
async fn api_agents_status(State(state): State<MonitorState>) -> axum::Json<serde_json::Value> {
    let running = state.orchestrator.lock().await.list();
    let has_defaults = state.agent_defaults.is_some();
    axum::Json(serde_json::json!({
        "running": running,
        "configured": state.agents.iter().map(|a| serde_json::json!({
            "name": a.name,
            "role": a.role,
            "description": a.description,
            "command": a.command,
        })).collect::<Vec<_>>(),
        "agent_defaults_available": has_defaults,
    }))
}

/// Stop a running agent by name.
async fn api_agent_stop(
    State(state): State<MonitorState>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> axum::http::StatusCode {
    let name = body["name"].as_str().unwrap_or("");
    if name.is_empty() {
        return axum::http::StatusCode::BAD_REQUEST;
    }
    tracing::info!(agent = %name, "stop requested via monitor");
    let stopped = state.orchestrator.lock().await.stop_agent(name).await;
    if stopped {
        axum::http::StatusCode::OK
    } else {
        axum::http::StatusCode::NOT_FOUND
    }
}

/// Start (or restart) a config-defined agent by name.
async fn api_agent_start(
    State(state): State<MonitorState>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> axum::http::StatusCode {
    let name = body["name"].as_str().unwrap_or("");
    if name.is_empty() {
        return axum::http::StatusCode::BAD_REQUEST;
    }
    let config = state.agents.iter().find(|a| a.name == name);
    match config {
        Some(cfg) => {
            tracing::info!(agent = %name, "start requested via monitor");
            let result = state.orchestrator.lock().await.spawn_agent(cfg).await;
            match result {
                Ok(_) => axum::http::StatusCode::OK,
                Err(e) => {
                    tracing::error!(agent = %name, error = %e, "failed to start agent");
                    axum::http::StatusCode::INTERNAL_SERVER_ERROR
                }
            }
        }
        None => axum::http::StatusCode::NOT_FOUND,
    }
}

/// Spawn an ad-hoc agent using agent_defaults + a user-provided prompt.
async fn api_agent_spawn(
    State(state): State<MonitorState>,
    axum::Json(body): axum::Json<serde_json::Value>,
) -> (axum::http::StatusCode, axum::Json<serde_json::Value>) {
    let defaults = match &state.agent_defaults {
        Some(d) => d,
        None => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({"error": "no [agent_defaults] configured"})),
            );
        }
    };

    let prompt = body["prompt"].as_str().unwrap_or("").to_string();
    if prompt.is_empty() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({"error": "prompt is required"})),
        );
    }

    let name = if let Some(n) = body["name"].as_str() {
        n.to_string()
    } else {
        format!("adhoc-{}", &uuid::Uuid::new_v4().to_string()[..8])
    };

    let config = crate::config::ResolvedAgentConfig {
        name: name.clone(),
        role: defaults.role.clone().unwrap_or_else(|| "adhoc".to_string()),
        description: defaults
            .description
            .clone()
            .unwrap_or_else(|| "Ad-hoc agent".to_string()),
        command: defaults.command.clone(),
        prompt: Some(prompt),
        ttl: defaults.ttl,
    };

    tracing::info!(agent = %name, "spawn ad-hoc agent via monitor");
    let result = state.orchestrator.lock().await.spawn_agent(&config).await;
    match result {
        Ok(_) => (
            axum::http::StatusCode::OK,
            axum::Json(serde_json::json!({"name": name})),
        ),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({"error": e.to_string()})),
        ),
    }
}

/// Trigger a graceful server shutdown from the monitor UI.
async fn api_shutdown(State(state): State<MonitorState>) -> axum::http::StatusCode {
    tracing::info!("shutdown requested via monitor");
    state.shutdown.notify_one();
    axum::http::StatusCode::OK
}
