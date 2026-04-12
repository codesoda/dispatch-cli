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
        .route("/api/shutdown", axum::routing::post(api_shutdown))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
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

/// Trigger a graceful server shutdown from the monitor UI.
async fn api_shutdown(State(state): State<MonitorState>) -> axum::http::StatusCode {
    tracing::info!("shutdown requested via monitor");
    state.shutdown.notify_one();
    axum::http::StatusCode::OK
}
