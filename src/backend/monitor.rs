use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::Html;
use axum::routing::get;
use axum::Router;
use futures_util::stream::Stream;
use tokio::sync::{broadcast, Mutex};

use super::local::{BrokerEvent, BrokerState};

/// Shared state for the monitor HTTP server.
#[derive(Clone)]
pub struct MonitorState {
    pub broker: Arc<Mutex<BrokerState>>,
    pub events: broadcast::Sender<BrokerEvent>,
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
async fn api_team(State(state): State<MonitorState>) -> axum::Json<Vec<crate::protocol::Worker>> {
    let mut broker = state.broker.lock().await;
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
