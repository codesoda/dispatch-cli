use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast;
use tokio::sync::{Mutex, Notify};
use tracing::instrument;

use crate::errors::DispatchError;
use crate::protocol::{BrokerRequest, BrokerResponse, Message, ResponsePayload, Worker};

/// Default worker TTL in seconds (1 hour).
const DEFAULT_WORKER_TTL_SECS: u64 = 3600;

/// Default listen timeout in seconds.
const DEFAULT_LISTEN_TIMEOUT_SECS: u64 = 30;

/// Event emitted by the broker for the monitor dashboard.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BrokerEvent {
    pub kind: String,
    pub worker_id: String,
    /// Human-readable worker name (for display in UI).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_name: Option<String>,
    pub detail: String,
    /// Full structured payload for the event (shown in web UI and console).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
    pub timestamp: u64,
}

/// Local backend that uses a Unix domain socket for IPC.
///
/// The broker runs in-process with in-memory state. Clients connect
/// over a UDS, send one JSON-line request, and receive one JSON-line
/// response before the connection is closed.
pub struct LocalBackend {
    config: crate::config::ResolvedConfig,
    monitor_port: Option<u16>,
    launch_agents: bool,
}

impl LocalBackend {
    pub fn new(
        config: &crate::config::ResolvedConfig,
        monitor_port: Option<u16>,
        launch_agents: bool,
    ) -> Self {
        Self {
            config: config.clone(),
            monitor_port,
            launch_agents,
        }
    }
}

#[async_trait]
impl super::Backend for LocalBackend {
    /// Start the broker server on a Unix domain socket, blocking until
    /// a shutdown signal (SIGINT/SIGTERM) is received.
    async fn serve(&self) -> Result<(), DispatchError> {
        serve(&self.config, self.monitor_port, self.launch_agents).await
    }

    /// Send a request to the broker over a Unix domain socket and
    /// return the response.
    #[instrument(skip(self, request), fields(cell_id = %self.config.cell_id))]
    async fn send_request(&self, request: &BrokerRequest) -> Result<BrokerResponse, DispatchError> {
        let sock = socket_path(&self.config.project_root, &self.config.cell_id);

        let stream = UnixStream::connect(&sock).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound
                || e.kind() == std::io::ErrorKind::ConnectionRefused
            {
                DispatchError::BrokerNotRunning {
                    cell_id: self.config.cell_id.clone(),
                }
            } else {
                DispatchError::ConnectionFailed {
                    reason: e.to_string(),
                }
            }
        })?;

        let (reader, mut writer) = stream.into_split();

        // Serialize and send the request as a single JSON line.
        let mut request_bytes = serde_json::to_vec(request)?;
        request_bytes.push(b'\n');
        writer
            .write_all(&request_bytes)
            .await
            .map_err(DispatchError::Io)?;

        // Read the response line.
        let mut reader = BufReader::new(reader);
        let mut response_line = String::new();
        let n = reader
            .read_line(&mut response_line)
            .await
            .map_err(DispatchError::Io)?;

        if n == 0 {
            return Err(DispatchError::ConnectionFailed {
                reason: "broker closed connection without responding".to_string(),
            });
        }

        let response: BrokerResponse = serde_json::from_str(response_line.trim())?;
        Ok(response)
    }
}

// ---------------------------------------------------------------------------
// Broker state
// ---------------------------------------------------------------------------

/// In-memory broker state.
#[derive(Debug)]
pub struct BrokerState {
    /// Registered workers keyed by worker ID.
    pub workers: HashMap<String, Worker>,
    /// Per-worker message mailboxes keyed by worker ID.
    pub mailboxes: HashMap<String, VecDeque<Message>>,
    /// Per-worker notification channels for long-poll wakeup.
    pub notifiers: HashMap<String, Arc<Notify>>,
    /// Default TTL for workers that don't specify one.
    pub default_ttl: u64,
    /// Total number of messages sent through the broker.
    pub messages_sent: u64,
    /// Total number of messages delivered to listeners.
    pub messages_delivered: u64,
    /// Total number of requests handled.
    pub requests_handled: u64,
}

impl Default for BrokerState {
    fn default() -> Self {
        Self::with_default_ttl(DEFAULT_WORKER_TTL_SECS)
    }
}

impl BrokerState {
    pub fn new() -> Self {
        Self::with_default_ttl(DEFAULT_WORKER_TTL_SECS)
    }

    pub fn with_default_ttl(default_ttl: u64) -> Self {
        Self {
            workers: HashMap::new(),
            mailboxes: HashMap::new(),
            notifiers: HashMap::new(),
            default_ttl,
            messages_sent: 0,
            messages_delivered: 0,
            requests_handled: 0,
        }
    }

    /// Register a new worker and return its unique ID.
    ///
    /// If `evict` is true and a worker with the same name already exists, the
    /// old registration is removed (including its mailbox and notifier) before
    /// the new one is created.
    pub fn register_worker(
        &mut self,
        name: String,
        role: String,
        description: String,
        capabilities: Vec<String>,
        ttl_secs: Option<u64>,
        evict: bool,
    ) -> String {
        if evict {
            let old_ids: Vec<String> = self
                .workers
                .iter()
                .filter(|(_, w)| w.name == name)
                .map(|(id, _)| id.clone())
                .collect();
            for old_id in &old_ids {
                self.workers.remove(old_id);
                self.mailboxes.remove(old_id);
                self.notifiers.remove(old_id);
            }
        }

        let id = uuid::Uuid::new_v4().to_string();
        let now = now_secs();
        let ttl = ttl_secs.unwrap_or(self.default_ttl);
        let worker = Worker {
            id: id.clone(),
            name,
            role,
            description,
            capabilities,
            ttl_secs: ttl,
            expires_at: now + ttl,
        };
        self.workers.insert(id.clone(), worker);
        id
    }

    /// Remove workers whose TTL has expired, including their mailboxes and notifiers.
    /// Returns the IDs of workers that were evicted.
    pub fn evict_expired(&mut self) -> Vec<String> {
        let now = now_secs();
        let expired_ids: Vec<String> = self
            .workers
            .iter()
            .filter(|(_, w)| w.expires_at <= now)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &expired_ids {
            self.workers.remove(id);
            self.mailboxes.remove(id);
            self.notifiers.remove(id);
        }
        expired_ids
    }

    /// Return a list of all active (non-expired) workers.
    pub fn list_workers(&mut self) -> Vec<Worker> {
        self.evict_expired();
        self.workers.values().cloned().collect()
    }

    /// Look up a worker's name by ID.
    pub fn worker_name(&self, worker_id: &str) -> Option<&str> {
        self.workers.get(worker_id).map(|w| w.name.as_str())
    }

    /// Renew a worker's TTL. Returns the new expiry timestamp, or None if not found/expired.
    pub fn heartbeat_worker(&mut self, worker_id: &str) -> Option<u64> {
        self.evict_expired();
        if let Some(worker) = self.workers.get_mut(worker_id) {
            let new_expiry = now_secs() + worker.ttl_secs;
            worker.expires_at = new_expiry;
            Some(new_expiry)
        } else {
            None
        }
    }

    /// Queue a message in a worker's mailbox. Returns the message ID, or None if the
    /// recipient worker is not found or expired.
    pub fn send_message(
        &mut self,
        to: String,
        body: String,
        from: Option<String>,
    ) -> Option<String> {
        self.evict_expired();
        if !self.workers.contains_key(&to) {
            return None;
        }
        let message_id = uuid::Uuid::new_v4().to_string();
        let message = Message {
            message_id: message_id.clone(),
            from,
            to: to.clone(),
            body,
        };
        self.mailboxes
            .entry(to.clone())
            .or_default()
            .push_back(message);
        // Wake any long-polling listener for this worker.
        if let Some(notify) = self.notifiers.get(&to) {
            notify.notify_one();
        }
        Some(message_id)
    }

    /// Pop the next message from a worker's mailbox, if any.
    pub fn pop_message(&mut self, worker_id: &str) -> Option<Message> {
        self.mailboxes.get_mut(worker_id)?.pop_front()
    }

    /// Get or create the Notify handle for a worker's mailbox.
    pub fn get_notifier(&mut self, worker_id: &str) -> Arc<Notify> {
        self.notifiers
            .entry(worker_id.to_string())
            .or_insert_with(|| Arc::new(Notify::new()))
            .clone()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Get current Unix timestamp in seconds.
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_secs()
}

/// Derive the Unix domain socket path for a given cell identity.
///
/// Socket is placed in `/tmp/dispatch-cli/sockets/<cell_id>.sock`.
/// The cell_id already encodes the project identity (hashed canonical path),
/// so no additional path components are needed. Using `/tmp` avoids the
/// Unix domain socket `SUN_LEN` limit (104 bytes on macOS) that triggers
/// when project paths are deeply nested.
fn socket_path(_project_root: &Path, cell_id: &str) -> PathBuf {
    PathBuf::from("/tmp/dispatch-cli/sockets").join(format!("{cell_id}.sock"))
}

/// Check whether a broker is already running for this cell by testing
/// if the socket file exists and a connection can be made.
async fn check_no_existing_broker(socket: &Path, cell_id: &str) -> Result<(), DispatchError> {
    if !socket.exists() {
        return Ok(());
    }

    // Socket file exists — try to connect to see if a broker is actually listening.
    match UnixStream::connect(socket).await {
        Ok(_) => Err(DispatchError::BrokerAlreadyRunning {
            cell_id: cell_id.to_string(),
            socket_path: socket.to_path_buf(),
        }),
        Err(_) => {
            // Stale socket file from a previous crashed run — remove it.
            tracing::warn!(path = %socket.display(), "removing stale socket file");
            std::fs::remove_file(socket).map_err(DispatchError::Io)?;
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// Start the embedded broker server.
///
/// Listens on a Unix domain socket and handles JSON-line requests.
/// Returns when a shutdown signal (SIGINT/SIGTERM) is received.
#[instrument(skip_all, fields(cell_id, socket_path))]
pub async fn serve(
    config: &crate::config::ResolvedConfig,
    monitor_port: Option<u16>,
    launch_agents: bool,
) -> Result<(), DispatchError> {
    let cell_id = &config.cell_id;
    let project_root = &config.project_root;
    let socket = socket_path(project_root, cell_id);

    // Ensure parent directory exists.
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent).map_err(DispatchError::Io)?;
    }

    // Check for duplicate broker.
    check_no_existing_broker(&socket, cell_id).await?;

    // Bind the listener.
    let listener = UnixListener::bind(&socket).map_err(DispatchError::Io)?;

    tracing::info!(cell_id, socket_path = %socket.display(), "broker listening");
    eprintln!(
        "dispatch serve: broker listening on {} (cell={})",
        socket.display(),
        cell_id
    );

    let state = Arc::new(Mutex::new(if let Some(ttl) = config.default_ttl {
        BrokerState::with_default_ttl(ttl)
    } else {
        BrokerState::new()
    }));
    let (event_tx, _) = broadcast::channel::<BrokerEvent>(256);

    // Shutdown signal shared with the monitor dashboard.
    let monitor_shutdown = Arc::new(Notify::new());

    // Optionally start the HTTP monitor dashboard.
    let monitor_url = if let Some(port) = monitor_port {
        let url = format!("http://localhost:{port}");
        let monitor_state = super::monitor::MonitorState {
            broker: Arc::clone(&state),
            events: event_tx.clone(),
            shutdown: Arc::clone(&monitor_shutdown),
            name: config.name.clone(),
            cell_id: cell_id.clone(),
            started_at: now_secs(),
            agents: config.agents.clone(),
            main_agent: config.main_agent.clone(),
            heartbeats: config.heartbeats.clone(),
            log_dir: config.project_root.join("logs"),
            monitor_url: Some(url.clone()),
        };
        tokio::spawn(async move {
            if let Err(e) = super::monitor::run_monitor(port, monitor_state).await {
                tracing::error!(error = %e, "monitor server error");
            }
        });
        eprintln!("dispatch serve: monitor dashboard at {url}");
        if config.monitor_open {
            if let Err(e) = open::that(&url) {
                tracing::warn!(error = %e, "failed to open monitor in browser");
            }
        }
        Some(url)
    } else {
        None
    };

    // Set up the orchestrator (manages agent process lifecycle).
    let mut orchestrator = super::orchestrator::AgentOrchestrator::new(
        cell_id,
        &socket,
        monitor_url.clone(),
        &config.agent_cwd,
    );

    if launch_agents {
        // Auto-launch configured agents.
        if !config.agents.is_empty() {
            orchestrator.launch_all(&config.agents).await?;
        }
        // Start configured heartbeat timers.
        if !config.heartbeats.is_empty() {
            orchestrator.start_heartbeats(&config.heartbeats, &event_tx);
        }
    }

    // Print agent commands for the user to run manually.
    if !config.agents.is_empty() && !launch_agents {
        eprintln!("\ndispatch serve: ready. Start agents in separate terminals:\n");
        for agent in &config.agents {
            let cmd =
                super::orchestrator::build_agent_command(agent, cell_id, monitor_url.as_deref());
            eprintln!("  # {} ({})", agent.name, agent.role);
            eprintln!("  {cmd}\n");
        }
    }

    // Print the main agent launch command.
    if let Some(ref main_agent) = config.main_agent {
        let cmd = super::orchestrator::build_main_agent_command(
            main_agent,
            cell_id,
            monitor_url.as_deref(),
        );
        eprintln!("dispatch serve: main session:\n");
        eprintln!("  {cmd}\n");
    }

    // Run until shutdown signal (OS signal or monitor UI).
    let result = tokio::select! {
        res = accept_loop(&listener, state, event_tx) => res,
        _ = shutdown_signal() => {
            tracing::info!("shutdown signal received");
            eprintln!("dispatch serve: shutting down agents...");
            orchestrator.shutdown_all().await;
            eprintln!("dispatch serve: shutting down");
            Ok(())
        }
        _ = monitor_shutdown.notified() => {
            tracing::info!("shutdown requested via monitor");
            eprintln!("dispatch serve: shutdown requested from monitor");
            eprintln!("dispatch serve: shutting down agents...");
            orchestrator.shutdown_all().await;
            eprintln!("dispatch serve: shutting down");
            Ok(())
        }
    };

    // Clean up socket file on exit.
    if socket.exists() {
        if let Err(e) = std::fs::remove_file(&socket) {
            tracing::warn!(error = %e, "failed to remove socket file on shutdown");
        }
    }

    result
}

/// Accept connections in a loop and spawn a handler for each.
async fn accept_loop(
    listener: &UnixListener,
    state: Arc<Mutex<BrokerState>>,
    event_tx: broadcast::Sender<BrokerEvent>,
) -> Result<(), DispatchError> {
    loop {
        let (stream, _addr) = listener.accept().await.map_err(DispatchError::Io)?;
        let state = Arc::clone(&state);
        let event_tx = event_tx.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, state, event_tx).await {
                tracing::error!(error = %e, "connection handler error");
            }
        });
    }
}

/// Handle a single client connection.
///
/// Reads one JSON line, processes it, writes one JSON line response, then closes.
async fn handle_connection(
    stream: UnixStream,
    state: Arc<Mutex<BrokerState>>,
    event_tx: broadcast::Sender<BrokerEvent>,
) -> Result<(), DispatchError> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    let n = reader
        .read_line(&mut line)
        .await
        .map_err(DispatchError::Io)?;
    if n == 0 {
        return Ok(()); // Client disconnected.
    }

    let line = line.trim();
    tracing::debug!(request = %line, "received request");

    let response = match serde_json::from_str::<BrokerRequest>(line) {
        Ok(request) => handle_request(request, state, &event_tx).await,
        Err(e) => BrokerResponse::Error {
            message: format!("invalid request: {e}"),
        },
    };

    let mut response_bytes = serde_json::to_vec(&response)?;
    response_bytes.push(b'\n');
    writer
        .write_all(&response_bytes)
        .await
        .map_err(DispatchError::Io)?;

    Ok(())
}

/// Emit a broker event (best-effort, ignores send failures).
/// Also prints a human-readable line to stderr for the serve console.
fn emit_event(
    tx: &broadcast::Sender<BrokerEvent>,
    kind: &str,
    worker_id: &str,
    detail: &str,
    payload: Option<serde_json::Value>,
) {
    let ts = chrono_ts();
    if let Some(ref p) = payload {
        eprintln!("[{ts}] {kind:>10}  {worker_id}  {detail}  {p}");
    } else {
        eprintln!("[{ts}] {kind:>10}  {worker_id}  {detail}");
    }
    let _ = tx.send(BrokerEvent {
        kind: kind.to_string(),
        worker_id: worker_id.to_string(),
        worker_name: None,
        detail: detail.to_string(),
        payload,
        timestamp: now_secs(),
    });
}

/// Format current time as HH:MM:SS for console output.
fn chrono_ts() -> String {
    let secs = now_secs();
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    format!("{h:02}:{m:02}:{s:02}")
}

/// Route a parsed request to the appropriate handler.
async fn handle_request(
    request: BrokerRequest,
    state: Arc<Mutex<BrokerState>>,
    event_tx: &broadcast::Sender<BrokerEvent>,
) -> BrokerResponse {
    {
        let mut s = state.lock().await;
        s.requests_handled += 1;
    }
    match request {
        BrokerRequest::Register {
            name,
            role,
            description,
            capabilities,
            ttl_secs,
            evict,
        } => {
            let mut state = state.lock().await;
            let worker_id = state.register_worker(
                name.clone(),
                role.clone(),
                description,
                capabilities,
                ttl_secs,
                evict,
            );
            tracing::info!(worker_id = %worker_id, "worker registered");
            emit_event(
                event_tx,
                "register",
                &worker_id,
                &format!("{name} ({role})"),
                Some(serde_json::json!({
                    "name": name,
                    "role": role,
                })),
            );
            BrokerResponse::Ok {
                payload: ResponsePayload::WorkerRegistered { worker_id },
            }
        }
        BrokerRequest::Team { from } => {
            let mut state = state.lock().await;
            // Renew caller's TTL if identified.
            if let Some(ref caller_id) = from {
                if let Some(w) = state.workers.get_mut(caller_id) {
                    w.expires_at = now_secs() + w.ttl_secs;
                }
            }
            let workers = state.list_workers();
            tracing::info!(count = workers.len(), "team listing");
            BrokerResponse::Ok {
                payload: ResponsePayload::WorkerList { workers },
            }
        }
        BrokerRequest::Send { to, body, from } => {
            let mut state = state.lock().await;
            // Renew sender's TTL if they're a registered worker.
            if let Some(ref sender_id) = from {
                if let Some(w) = state.workers.get_mut(sender_id) {
                    w.expires_at = now_secs() + w.ttl_secs;
                }
            }
            let body_clone = body.clone();
            let from_clone = from.clone();
            match state.send_message(to.clone(), body, from) {
                Some(message_id) => {
                    state.messages_sent += 1;
                    tracing::info!(message_id = %message_id, to = %to, "message queued");
                    emit_event(
                        event_tx,
                        "send",
                        &to,
                        &format!(
                            "from {} → {}",
                            from_clone.as_deref().unwrap_or("anonymous"),
                            &to
                        ),
                        Some(serde_json::json!({
                            "from": from_clone,
                            "to": to,
                            "body": body_clone,
                            "message_id": &message_id[..8],
                        })),
                    );
                    BrokerResponse::Ok {
                        payload: ResponsePayload::MessageAck { message_id },
                    }
                }
                None => BrokerResponse::Error {
                    message: format!("recipient worker not found or expired: {to}"),
                },
            }
        }
        BrokerRequest::Listen {
            worker_id,
            timeout_secs,
        } => {
            let timeout = if timeout_secs == 0 {
                DEFAULT_LISTEN_TIMEOUT_SECS
            } else {
                timeout_secs
            };

            // Check worker exists and renew TTL; get notifier and try immediate pop.
            let (notifier, immediate_msg) = {
                let mut s = state.lock().await;
                let expired = s.evict_expired();
                for id in &expired {
                    emit_event(event_tx, "expire", id, "worker expired", None);
                }
                if !s.workers.contains_key(&worker_id) {
                    return BrokerResponse::Error {
                        message: format!("worker not found or expired: {worker_id}"),
                    };
                }
                // Renew TTL on listen.
                if let Some(w) = s.workers.get_mut(&worker_id) {
                    w.expires_at = now_secs() + w.ttl_secs;
                }
                let notifier = s.get_notifier(&worker_id);
                let msg = s.pop_message(&worker_id);
                (notifier, msg)
            };

            // If a message was immediately available, return it.
            if let Some(msg) = immediate_msg {
                {
                    state.lock().await.messages_delivered += 1;
                }
                tracing::info!(worker_id = %worker_id, message_id = %msg.message_id, "listen: immediate delivery");
                emit_event(
                    event_tx,
                    "deliver",
                    &worker_id,
                    &format!(
                        "from {} → {}",
                        msg.from.as_deref().unwrap_or("anonymous"),
                        &worker_id
                    ),
                    Some(serde_json::json!({
                        "from": msg.from,
                        "to": msg.to,
                        "body": msg.body,
                        "message_id": &msg.message_id[..8],
                    })),
                );
                return BrokerResponse::Ok {
                    payload: ResponsePayload::Message {
                        message_id: msg.message_id,
                        from: msg.from,
                        to: msg.to,
                        body: msg.body,
                    },
                };
            }

            // Long-poll: wait for a notification or timeout.
            let result =
                tokio::time::timeout(Duration::from_secs(timeout), notifier.notified()).await;

            if result.is_ok() {
                // Notified — try to pop a message.
                let mut s = state.lock().await;
                if let Some(msg) = s.pop_message(&worker_id) {
                    s.messages_delivered += 1;
                    tracing::info!(worker_id = %worker_id, message_id = %msg.message_id, "listen: delivered after wait");
                    emit_event(
                        event_tx,
                        "deliver",
                        &worker_id,
                        &format!(
                            "from {} → {}",
                            msg.from.as_deref().unwrap_or("anonymous"),
                            &worker_id
                        ),
                        Some(serde_json::json!({
                            "from": msg.from,
                            "to": msg.to,
                            "body": msg.body,
                            "message_id": &msg.message_id[..8],
                        })),
                    );
                    BrokerResponse::Ok {
                        payload: ResponsePayload::Message {
                            message_id: msg.message_id,
                            from: msg.from,
                            to: msg.to,
                            body: msg.body,
                        },
                    }
                } else {
                    // Spurious wake — treat as timeout.
                    tracing::debug!(worker_id = %worker_id, "listen: spurious wake, returning timeout");
                    BrokerResponse::Ok {
                        payload: ResponsePayload::Timeout { worker_id },
                    }
                }
            } else {
                // Timed out.
                tracing::debug!(worker_id = %worker_id, timeout, "listen: timed out");
                BrokerResponse::Ok {
                    payload: ResponsePayload::Timeout { worker_id },
                }
            }
        }
        BrokerRequest::Heartbeat { worker_id } => {
            let mut state = state.lock().await;
            match state.heartbeat_worker(&worker_id) {
                Some(expires_at) => {
                    tracing::info!(worker_id = %worker_id, expires_at, "heartbeat renewed");
                    emit_event(event_tx, "heartbeat", &worker_id, "TTL renewed", None);
                    BrokerResponse::Ok {
                        payload: ResponsePayload::HeartbeatAck {
                            worker_id,
                            expires_at,
                        },
                    }
                }
                None => BrokerResponse::Error {
                    message: format!("worker not found or expired: {worker_id}"),
                },
            }
        }
    }
}

/// Wait for a shutdown signal (SIGINT or SIGTERM).
async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};

    let mut sigint = signal(SignalKind::interrupt()).expect("failed to register SIGINT handler");
    let mut sigterm = signal(SignalKind::terminate()).expect("failed to register SIGTERM handler");

    tokio::select! {
        _ = sigint.recv() => {}
        _ = sigterm.recv() => {}
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::Backend;
    use crate::config::ResolvedConfig;
    use tempfile::TempDir;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    /// Create a minimal ResolvedConfig for testing.
    fn test_config(project_root: &Path, cell_id: &str) -> ResolvedConfig {
        ResolvedConfig {
            name: None,
            cell_id: cell_id.to_string(),
            backend: None,
            project_root: project_root.to_path_buf(),
            agent_cwd: project_root.to_path_buf(),
            monitor_port: None,
            monitor_open: false,
            default_ttl: None,
            agents: vec![],
            main_agent: None,
            heartbeats: vec![],
        }
    }

    // ---- Client-side tests (formerly in client.rs) ----

    #[tokio::test]
    async fn test_client_broker_not_running() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path(), "nonexistent-cell");
        let backend = LocalBackend::new(&config, None, false);

        let result = backend
            .send_request(&BrokerRequest::Team { from: None })
            .await;
        assert!(result.is_err());

        match result.unwrap_err() {
            DispatchError::BrokerNotRunning { cell_id } => {
                assert_eq!(cell_id, "nonexistent-cell");
            }
            other => panic!("expected BrokerNotRunning, got: {other}"),
        }
    }

    #[tokio::test]
    async fn test_client_send_and_receive() {
        let tmp = TempDir::new().unwrap();
        let project_root = tmp.path().to_path_buf();
        let cell_id = "client-test";

        // Start broker in background.
        let root = project_root.clone();
        let serve_handle =
            tokio::spawn(async move { serve(&test_config(&root, cell_id), None, false).await });

        // Wait for broker to start.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let cfg = test_config(&project_root, cell_id);
        let backend = LocalBackend::new(&cfg, None, false);
        let response = backend
            .send_request(&BrokerRequest::Team { from: None })
            .await;
        assert!(response.is_ok(), "expected Ok response, got: {response:?}");

        let resp = response.unwrap();
        match resp {
            BrokerResponse::Ok { .. } => {} // Expected
            BrokerResponse::Error { message } => {
                panic!("expected Ok response, got error: {message}");
            }
        }

        serve_handle.abort();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // ---- Broker-side tests (formerly in broker.rs) ----

    #[test]
    fn test_socket_path_derivation() {
        let path = socket_path(Path::new("/home/user/project"), "cell-abc123");
        assert_eq!(
            path,
            PathBuf::from("/tmp/dispatch-cli/sockets/cell-abc123.sock")
        );
    }

    #[tokio::test]
    async fn test_check_no_existing_broker_no_socket() {
        let tmp = TempDir::new().unwrap();
        let sock = tmp.path().join("test.sock");
        let result = check_no_existing_broker(&sock, "test-cell").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_check_no_existing_broker_stale_socket() {
        let tmp = TempDir::new().unwrap();
        let sock = tmp.path().join("test.sock");
        // Create a regular file pretending to be a stale socket.
        std::fs::write(&sock, "").unwrap();
        let result = check_no_existing_broker(&sock, "test-cell").await;
        assert!(result.is_ok());
        assert!(!sock.exists(), "stale socket should be removed");
    }

    #[tokio::test]
    async fn test_check_no_existing_broker_active_broker() {
        let tmp = TempDir::new().unwrap();
        let sock = tmp.path().join("active.sock");

        // Start a real listener to simulate an active broker.
        let listener = UnixListener::bind(&sock).unwrap();

        // Spawn a task to accept one connection so the connect test works.
        let sock_clone = sock.clone();
        let accept_handle = tokio::spawn(async move {
            let _ = listener.accept().await;
            // Keep listener alive until we drop it.
            drop(listener);
            // Clean up.
            let _ = std::fs::remove_file(&sock_clone);
        });

        let result = check_no_existing_broker(&sock, "test-cell").await;
        assert!(result.is_err());

        let err = result.unwrap_err();
        match err {
            DispatchError::BrokerAlreadyRunning { cell_id, .. } => {
                assert_eq!(cell_id, "test-cell");
            }
            other => panic!("expected BrokerAlreadyRunning, got: {other}"),
        }

        accept_handle.abort();
    }

    #[tokio::test]
    async fn test_server_startup_and_connection() {
        let tmp = TempDir::new().unwrap();
        let project_root = tmp.path().to_path_buf();
        let cell_id = "test-cell";
        let sock = socket_path(&project_root, cell_id);

        // Start broker in background.
        let root = project_root.clone();
        let serve_handle =
            tokio::spawn(async move { serve(&test_config(&root, cell_id), None, false).await });

        // Wait briefly for the server to bind.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Verify socket file exists.
        assert!(sock.exists(), "socket file should exist after startup");

        // Connect and send a request.
        let stream = UnixStream::connect(&sock).await.unwrap();
        let (reader, mut writer) = stream.into_split();

        writer.write_all(b"{\"type\":\"ping\"}\n").await.unwrap();

        let mut reader = BufReader::new(reader);
        let mut response = String::new();
        reader.read_line(&mut response).await.unwrap();

        let parsed: serde_json::Value = serde_json::from_str(&response).unwrap();
        // Unrecognized request type returns an error.
        assert_eq!(parsed["status"], "error");

        // Clean up: abort the server.
        serve_handle.abort();

        // Give it a moment to clean up.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    #[tokio::test]
    async fn test_duplicate_broker_detection() {
        let tmp = TempDir::new().unwrap();
        let project_root = tmp.path().to_path_buf();
        let cell_id = "dup-cell";
        let sock = socket_path(&project_root, cell_id);

        // Ensure parent dir exists.
        std::fs::create_dir_all(sock.parent().unwrap()).unwrap();

        // Start first broker.
        let listener = UnixListener::bind(&sock).unwrap();
        let accept_handle = tokio::spawn(async move {
            let _ = listener.accept().await;
        });

        // Try to start second broker — should fail.
        let result = serve(&test_config(&project_root, cell_id), None, false).await;
        assert!(result.is_err());

        match result.unwrap_err() {
            DispatchError::BrokerAlreadyRunning {
                cell_id: id,
                socket_path: path,
            } => {
                assert_eq!(id, "dup-cell");
                assert_eq!(path, sock);
            }
            other => panic!("expected BrokerAlreadyRunning, got: {other}"),
        }

        accept_handle.abort();
        let _ = std::fs::remove_file(&sock);
    }

    #[tokio::test]
    async fn test_restart_clears_stale_socket() {
        let tmp = TempDir::new().unwrap();
        let project_root = tmp.path().to_path_buf();
        let cell_id = "restart-cell";
        let sock = socket_path(&project_root, cell_id);

        // Create socket directory and a stale socket file.
        std::fs::create_dir_all(sock.parent().unwrap()).unwrap();
        // Remove any leftover real socket from a previous run before
        // creating a fake stale file.
        let _ = std::fs::remove_file(&sock);
        std::fs::write(&sock, "stale").unwrap();

        // Starting serve should clean up the stale socket and bind fresh.
        let root = project_root.clone();
        let serve_handle =
            tokio::spawn(async move { serve(&test_config(&root, cell_id), None, false).await });

        // Wait briefly for the server to bind.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Should be able to connect.
        let result = UnixStream::connect(&sock).await;
        assert!(
            result.is_ok(),
            "should connect to fresh broker after stale cleanup"
        );

        serve_handle.abort();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    #[test]
    fn test_register_worker_returns_unique_ids() {
        let mut state = BrokerState::new();
        let id1 = state.register_worker(
            "worker-a".into(),
            "planner".into(),
            "Plans things".into(),
            vec!["plan:create plans".into()],
            None,
            false,
        );
        let id2 = state.register_worker(
            "worker-b".into(),
            "coder".into(),
            "Writes code".into(),
            vec![],
            None,
            false,
        );
        assert_ne!(id1, id2, "each registration must produce a unique ID");
        assert_eq!(state.workers.len(), 2);
    }

    #[test]
    fn test_register_worker_stores_fields() {
        let mut state = BrokerState::new();
        let id = state.register_worker(
            "my-worker".into(),
            "reviewer".into(),
            "Reviews pull requests".into(),
            vec!["review:code".into(), "review:docs".into()],
            None,
            false,
        );
        let worker = state.workers.get(&id).unwrap();
        assert_eq!(worker.name, "my-worker");
        assert_eq!(worker.role, "reviewer");
        assert_eq!(worker.description, "Reviews pull requests");
        assert_eq!(worker.capabilities, vec!["review:code", "review:docs"]);
        assert!(worker.expires_at > 0, "worker should have a TTL expiry");
    }

    #[test]
    fn test_register_worker_ttl_set() {
        let mut state = BrokerState::new();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let id = state.register_worker(
            "ttl-worker".into(),
            "role".into(),
            "desc".into(),
            vec![],
            None,
            false,
        );
        let worker = state.workers.get(&id).unwrap();
        // Should expire roughly DEFAULT_WORKER_TTL_SECS from now.
        assert!(worker.expires_at >= now + DEFAULT_WORKER_TTL_SECS - 1);
        assert!(worker.expires_at <= now + DEFAULT_WORKER_TTL_SECS + 1);
    }

    #[test]
    fn test_evict_expired_workers() {
        let mut state = BrokerState::new();
        let id = state.register_worker(
            "soon-expired".into(),
            "role".into(),
            "desc".into(),
            vec![],
            None,
            false,
        );
        // Manually set expiry to the past.
        state.workers.get_mut(&id).unwrap().expires_at = 0;
        state.evict_expired();
        assert!(state.workers.is_empty(), "expired worker should be evicted");
    }

    #[tokio::test]
    async fn test_register_via_broker_returns_worker_id() {
        let tmp = TempDir::new().unwrap();
        let project_root = tmp.path().to_path_buf();
        let cell_id = "reg-test";

        // Start broker.
        let root = project_root.clone();
        let serve_handle =
            tokio::spawn(async move { serve(&test_config(&root, cell_id), None, false).await });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Send register request via raw socket.
        let sock = socket_path(&project_root, cell_id);
        let stream = UnixStream::connect(&sock).await.unwrap();
        let (reader, mut writer) = stream.into_split();

        let req = serde_json::json!({
            "type": "register",
            "name": "test-agent",
            "role": "coder",
            "description": "Writes code",
            "capabilities": ["rust", "python"]
        });
        let mut req_bytes = serde_json::to_vec(&req).unwrap();
        req_bytes.push(b'\n');
        writer.write_all(&req_bytes).await.unwrap();

        let mut reader = BufReader::new(reader);
        let mut response = String::new();
        reader.read_line(&mut response).await.unwrap();

        let parsed: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(parsed["status"], "ok");
        assert!(
            parsed["worker_id"].is_string(),
            "response should contain worker_id"
        );
        // Worker ID should be a valid UUID.
        let worker_id = parsed["worker_id"].as_str().unwrap();
        assert!(
            uuid::Uuid::parse_str(worker_id).is_ok(),
            "worker_id should be a valid UUID"
        );

        serve_handle.abort();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    #[tokio::test]
    async fn test_register_capability_storage_via_broker() {
        let tmp = TempDir::new().unwrap();
        let project_root = tmp.path().to_path_buf();
        let cell_id = "cap-test";

        let root = project_root.clone();
        let serve_handle =
            tokio::spawn(async move { serve(&test_config(&root, cell_id), None, false).await });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let sock = socket_path(&project_root, "cap-test");

        // Register with capabilities using name:description convention.
        let stream = UnixStream::connect(&sock).await.unwrap();
        let (reader, mut writer) = stream.into_split();

        let req = serde_json::json!({
            "type": "register",
            "name": "cap-worker",
            "role": "tester",
            "description": "Runs tests",
            "capabilities": ["test:unit", "test:integration"]
        });
        let mut req_bytes = serde_json::to_vec(&req).unwrap();
        req_bytes.push(b'\n');
        writer.write_all(&req_bytes).await.unwrap();

        let mut reader = BufReader::new(reader);
        let mut response = String::new();
        reader.read_line(&mut response).await.unwrap();

        let parsed: serde_json::Value = serde_json::from_str(&response).unwrap();
        assert_eq!(parsed["status"], "ok");
        assert!(parsed["worker_id"].is_string());

        serve_handle.abort();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    /// Helper: send a JSON request to a broker socket and return the parsed response.
    async fn send_json_request(sock: &Path, request: &serde_json::Value) -> serde_json::Value {
        let stream = UnixStream::connect(sock).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut req_bytes = serde_json::to_vec(request).unwrap();
        req_bytes.push(b'\n');
        writer.write_all(&req_bytes).await.unwrap();
        let mut reader = BufReader::new(reader);
        let mut response = String::new();
        reader.read_line(&mut response).await.unwrap();
        serde_json::from_str(&response).unwrap()
    }

    /// Helper: register a worker via broker and return its worker_id.
    async fn register_worker(sock: &Path, name: &str, role: &str) -> String {
        let req = serde_json::json!({
            "type": "register",
            "name": name,
            "role": role,
            "description": format!("{name} worker"),
            "capabilities": []
        });
        let resp = send_json_request(sock, &req).await;
        resp["worker_id"].as_str().unwrap().to_string()
    }

    #[test]
    fn test_list_workers_excludes_expired() {
        let mut state = BrokerState::new();
        let active_id = state.register_worker(
            "active".into(),
            "coder".into(),
            "desc".into(),
            vec![],
            None,
            false,
        );
        let expired_id = state.register_worker(
            "expired".into(),
            "coder".into(),
            "desc".into(),
            vec![],
            None,
            false,
        );
        // Expire one worker.
        state.workers.get_mut(&expired_id).unwrap().expires_at = 0;

        let workers = state.list_workers();
        assert_eq!(workers.len(), 1);
        assert_eq!(workers[0].id, active_id);
    }

    #[test]
    fn test_heartbeat_worker_renews_ttl() {
        let mut state = BrokerState::new();
        let id = state.register_worker(
            "hb-worker".into(),
            "role".into(),
            "desc".into(),
            vec![],
            None,
            false,
        );
        let original_expiry = state.workers.get(&id).unwrap().expires_at;

        // Manually lower the expiry to simulate time passing.
        state.workers.get_mut(&id).unwrap().expires_at = now_secs() + 10;

        let new_expiry = state.heartbeat_worker(&id).unwrap();
        assert!(
            new_expiry >= original_expiry,
            "heartbeat should renew to at least the original TTL"
        );
    }

    #[test]
    fn test_heartbeat_worker_not_found() {
        let mut state = BrokerState::new();
        assert!(state.heartbeat_worker("nonexistent").is_none());
    }

    #[test]
    fn test_heartbeat_worker_expired() {
        let mut state = BrokerState::new();
        let id = state.register_worker(
            "exp-worker".into(),
            "role".into(),
            "desc".into(),
            vec![],
            None,
            false,
        );
        state.workers.get_mut(&id).unwrap().expires_at = 0;

        assert!(
            state.heartbeat_worker(&id).is_none(),
            "heartbeat for expired worker should return None"
        );
    }

    #[tokio::test]
    async fn test_team_via_broker() {
        let tmp = TempDir::new().unwrap();
        let project_root = tmp.path().to_path_buf();
        let cell_id = "team-test";

        let root = project_root.clone();
        let serve_handle =
            tokio::spawn(async move { serve(&test_config(&root, cell_id), None, false).await });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let sock = socket_path(&project_root, cell_id);

        // Register two workers.
        let id1 = register_worker(&sock, "worker-a", "planner").await;
        let id2 = register_worker(&sock, "worker-b", "coder").await;

        // List team.
        let resp = send_json_request(&sock, &serde_json::json!({"type": "team"})).await;
        assert_eq!(resp["status"], "ok");

        let workers = resp["workers"].as_array().unwrap();
        assert_eq!(workers.len(), 2);
        let ids: Vec<&str> = workers.iter().map(|w| w["id"].as_str().unwrap()).collect();
        assert!(ids.contains(&id1.as_str()));
        assert!(ids.contains(&id2.as_str()));

        // Verify worker fields are present.
        for w in workers {
            assert!(w["name"].is_string());
            assert!(w["role"].is_string());
            assert!(w["description"].is_string());
            assert!(w["capabilities"].is_array());
            assert!(w["id"].is_string());
        }

        serve_handle.abort();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    #[tokio::test]
    async fn test_heartbeat_via_broker() {
        let tmp = TempDir::new().unwrap();
        let project_root = tmp.path().to_path_buf();
        let cell_id = "hb-test";

        let root = project_root.clone();
        let serve_handle =
            tokio::spawn(async move { serve(&test_config(&root, cell_id), None, false).await });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let sock = socket_path(&project_root, cell_id);

        // Register a worker.
        let worker_id = register_worker(&sock, "hb-agent", "coder").await;

        // Send heartbeat.
        let resp = send_json_request(
            &sock,
            &serde_json::json!({"type": "heartbeat", "worker_id": worker_id}),
        )
        .await;
        assert_eq!(resp["status"], "ok");
        assert_eq!(resp["worker_id"], worker_id);
        assert!(
            resp["expires_at"].is_number(),
            "should return new expires_at"
        );

        serve_handle.abort();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    #[tokio::test]
    async fn test_heartbeat_unknown_worker_via_broker() {
        let tmp = TempDir::new().unwrap();
        let project_root = tmp.path().to_path_buf();
        let cell_id = "hb-err-test";

        let root = project_root.clone();
        let serve_handle =
            tokio::spawn(async move { serve(&test_config(&root, cell_id), None, false).await });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let sock = socket_path(&project_root, cell_id);

        // Heartbeat for a non-existent worker.
        let resp = send_json_request(
            &sock,
            &serde_json::json!({"type": "heartbeat", "worker_id": "nonexistent-id"}),
        )
        .await;
        assert_eq!(resp["status"], "error");
        assert!(
            resp["message"].as_str().unwrap().contains("not found"),
            "error message should mention worker not found"
        );

        serve_handle.abort();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    #[test]
    fn test_send_message_queues_in_mailbox() {
        let mut state = BrokerState::new();
        let worker_id = state.register_worker(
            "recv".into(),
            "coder".into(),
            "desc".into(),
            vec![],
            None,
            false,
        );

        let msg_id = state.send_message(worker_id.clone(), "hello".into(), Some("sender-1".into()));
        assert!(msg_id.is_some(), "send_message should return a message ID");

        let mailbox = state.mailboxes.get(&worker_id).unwrap();
        assert_eq!(mailbox.len(), 1);
        let msg = &mailbox[0];
        assert_eq!(msg.message_id, msg_id.unwrap());
        assert_eq!(msg.to, worker_id);
        assert_eq!(msg.body, "hello");
        assert_eq!(msg.from.as_deref(), Some("sender-1"));
    }

    #[test]
    fn test_send_message_unknown_recipient() {
        let mut state = BrokerState::new();
        let result = state.send_message("nonexistent".into(), "hello".into(), None);
        assert!(result.is_none(), "sending to unknown worker should fail");
    }

    #[test]
    fn test_send_message_expired_recipient() {
        let mut state = BrokerState::new();
        let worker_id = state.register_worker(
            "expiring".into(),
            "role".into(),
            "desc".into(),
            vec![],
            None,
            false,
        );
        state.workers.get_mut(&worker_id).unwrap().expires_at = 0;

        let result = state.send_message(worker_id, "hello".into(), None);
        assert!(result.is_none(), "sending to expired worker should fail");
    }

    #[test]
    fn test_send_message_unique_ids() {
        let mut state = BrokerState::new();
        let worker_id = state.register_worker(
            "recv".into(),
            "coder".into(),
            "desc".into(),
            vec![],
            None,
            false,
        );

        let id1 = state
            .send_message(worker_id.clone(), "msg1".into(), None)
            .unwrap();
        let id2 = state
            .send_message(worker_id.clone(), "msg2".into(), None)
            .unwrap();
        assert_ne!(id1, id2, "each message should have a unique ID");
        assert_eq!(state.mailboxes.get(&worker_id).unwrap().len(), 2);
    }

    #[test]
    fn test_send_message_without_from() {
        let mut state = BrokerState::new();
        let worker_id = state.register_worker(
            "recv".into(),
            "coder".into(),
            "desc".into(),
            vec![],
            None,
            false,
        );

        let msg_id = state
            .send_message(worker_id.clone(), "anon msg".into(), None)
            .unwrap();
        let msg = &state.mailboxes.get(&worker_id).unwrap()[0];
        assert_eq!(msg.message_id, msg_id);
        assert!(msg.from.is_none(), "from should be None when not provided");
    }

    #[tokio::test]
    async fn test_send_via_broker() {
        let tmp = TempDir::new().unwrap();
        let project_root = tmp.path().to_path_buf();
        let cell_id = "send-test";

        let root = project_root.clone();
        let serve_handle =
            tokio::spawn(async move { serve(&test_config(&root, cell_id), None, false).await });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let sock = socket_path(&project_root, cell_id);

        // Register a worker to receive the message.
        let worker_id = register_worker(&sock, "receiver", "coder").await;

        // Send a message.
        let resp = send_json_request(
            &sock,
            &serde_json::json!({
                "type": "send",
                "to": worker_id,
                "body": "build the feature",
                "from": "planner-1"
            }),
        )
        .await;
        assert_eq!(resp["status"], "ok");
        assert!(
            resp["message_id"].is_string(),
            "response should contain message_id"
        );
        let message_id = resp["message_id"].as_str().unwrap();
        assert!(
            uuid::Uuid::parse_str(message_id).is_ok(),
            "message_id should be a valid UUID"
        );

        serve_handle.abort();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    #[tokio::test]
    async fn test_send_unknown_recipient_via_broker() {
        let tmp = TempDir::new().unwrap();
        let project_root = tmp.path().to_path_buf();
        let cell_id = "send-err-test";

        let root = project_root.clone();
        let serve_handle =
            tokio::spawn(async move { serve(&test_config(&root, cell_id), None, false).await });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let sock = socket_path(&project_root, cell_id);

        // Send to a non-existent worker.
        let resp = send_json_request(
            &sock,
            &serde_json::json!({
                "type": "send",
                "to": "nonexistent-worker-id",
                "body": "hello"
            }),
        )
        .await;
        assert_eq!(resp["status"], "error");
        assert!(
            resp["message"].as_str().unwrap().contains("not found"),
            "error message should mention recipient not found"
        );

        serve_handle.abort();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // ---- Listen tests ----

    #[test]
    fn test_pop_message_returns_fifo_order() {
        let mut state = BrokerState::new();
        let worker_id = state.register_worker(
            "recv".into(),
            "coder".into(),
            "desc".into(),
            vec![],
            None,
            false,
        );
        state.send_message(worker_id.clone(), "first".into(), None);
        state.send_message(worker_id.clone(), "second".into(), None);

        let msg1 = state.pop_message(&worker_id).unwrap();
        assert_eq!(msg1.body, "first");
        let msg2 = state.pop_message(&worker_id).unwrap();
        assert_eq!(msg2.body, "second");
        assert!(state.pop_message(&worker_id).is_none());
    }

    #[test]
    fn test_pop_message_empty_mailbox() {
        let mut state = BrokerState::new();
        let worker_id = state.register_worker(
            "recv".into(),
            "coder".into(),
            "desc".into(),
            vec![],
            None,
            false,
        );
        assert!(state.pop_message(&worker_id).is_none());
    }

    #[test]
    fn test_pop_message_unknown_worker() {
        let mut state = BrokerState::new();
        assert!(state.pop_message("nonexistent").is_none());
    }

    #[test]
    fn test_get_notifier_returns_same_instance() {
        let mut state = BrokerState::new();
        let worker_id = state.register_worker(
            "recv".into(),
            "coder".into(),
            "desc".into(),
            vec![],
            None,
            false,
        );
        let n1 = state.get_notifier(&worker_id);
        let n2 = state.get_notifier(&worker_id);
        assert!(Arc::ptr_eq(&n1, &n2), "same worker should get same Notify");
    }

    #[tokio::test]
    async fn test_listen_immediate_delivery_via_broker() {
        let tmp = TempDir::new().unwrap();
        let project_root = tmp.path().to_path_buf();
        let cell_id = "listen-imm-test";

        let root = project_root.clone();
        let serve_handle =
            tokio::spawn(async move { serve(&test_config(&root, cell_id), None, false).await });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let sock = socket_path(&project_root, cell_id);

        // Register a worker and send a message before listening.
        let worker_id = register_worker(&sock, "listener", "coder").await;
        let send_resp = send_json_request(
            &sock,
            &serde_json::json!({
                "type": "send",
                "to": worker_id,
                "body": "immediate msg",
                "from": "sender-1"
            }),
        )
        .await;
        assert_eq!(send_resp["status"], "ok");

        // Listen should immediately return the queued message.
        let listen_resp = send_json_request(
            &sock,
            &serde_json::json!({
                "type": "listen",
                "worker_id": worker_id,
                "timeout_secs": 5
            }),
        )
        .await;
        assert_eq!(listen_resp["status"], "ok");
        assert_eq!(listen_resp["body"], "immediate msg");
        assert_eq!(listen_resp["from"], "sender-1");
        assert_eq!(listen_resp["to"], worker_id);
        assert!(listen_resp["message_id"].is_string());

        serve_handle.abort();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    #[tokio::test]
    async fn test_listen_timeout_via_broker() {
        let tmp = TempDir::new().unwrap();
        let project_root = tmp.path().to_path_buf();
        let cell_id = "listen-to-test";

        let root = project_root.clone();
        let serve_handle =
            tokio::spawn(async move { serve(&test_config(&root, cell_id), None, false).await });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let sock = socket_path(&project_root, cell_id);

        let worker_id = register_worker(&sock, "waiter", "coder").await;

        // Listen with a very short timeout and no messages queued.
        let listen_resp = send_json_request(
            &sock,
            &serde_json::json!({
                "type": "listen",
                "worker_id": worker_id,
                "timeout_secs": 1
            }),
        )
        .await;
        assert_eq!(listen_resp["status"], "ok");
        assert_eq!(
            listen_resp["worker_id"], worker_id,
            "timeout response should contain worker_id"
        );
        // Timeout response should NOT have a message_id or body.
        assert!(
            listen_resp.get("message_id").is_none() || listen_resp["message_id"].is_null(),
            "timeout should not have message_id"
        );

        serve_handle.abort();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    #[tokio::test]
    async fn test_listen_long_poll_delivery_via_broker() {
        let tmp = TempDir::new().unwrap();
        let project_root = tmp.path().to_path_buf();
        let cell_id = "listen-lp-test";

        let root = project_root.clone();
        let serve_handle =
            tokio::spawn(async move { serve(&test_config(&root, cell_id), None, false).await });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let sock = socket_path(&project_root, cell_id);

        let worker_id = register_worker(&sock, "poller", "coder").await;

        // Start listening in a background task — message arrives after a delay.
        let sock_clone = sock.clone();
        let wid = worker_id.clone();
        let listen_handle = tokio::spawn(async move {
            send_json_request(
                &sock_clone,
                &serde_json::json!({
                    "type": "listen",
                    "worker_id": wid,
                    "timeout_secs": 10
                }),
            )
            .await
        });

        // Wait a bit, then send a message.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let send_resp = send_json_request(
            &sock,
            &serde_json::json!({
                "type": "send",
                "to": worker_id,
                "body": "delayed message"
            }),
        )
        .await;
        assert_eq!(send_resp["status"], "ok");

        // The listen should return the message.
        let listen_resp = listen_handle.await.unwrap();
        assert_eq!(listen_resp["status"], "ok");
        assert_eq!(listen_resp["body"], "delayed message");
        assert!(listen_resp["message_id"].is_string());

        serve_handle.abort();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    #[tokio::test]
    async fn test_listen_renews_worker_ttl_via_broker() {
        let tmp = TempDir::new().unwrap();
        let project_root = tmp.path().to_path_buf();
        let cell_id = "listen-ttl-test";

        let root = project_root.clone();
        let serve_handle =
            tokio::spawn(async move { serve(&test_config(&root, cell_id), None, false).await });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let sock = socket_path(&project_root, cell_id);

        let worker_id = register_worker(&sock, "ttl-worker", "coder").await;

        // Send a message so listen returns immediately.
        send_json_request(
            &sock,
            &serde_json::json!({
                "type": "send",
                "to": worker_id,
                "body": "ttl test"
            }),
        )
        .await;

        // Listen (which should renew TTL).
        let listen_resp = send_json_request(
            &sock,
            &serde_json::json!({
                "type": "listen",
                "worker_id": worker_id,
                "timeout_secs": 5
            }),
        )
        .await;
        assert_eq!(listen_resp["status"], "ok");
        assert_eq!(listen_resp["body"], "ttl test");

        // Worker should still be active in team listing.
        let team_resp = send_json_request(&sock, &serde_json::json!({"type": "team"})).await;
        let workers = team_resp["workers"].as_array().unwrap();
        let found = workers.iter().any(|w| w["id"] == worker_id);
        assert!(found, "worker should still be active after listen");

        serve_handle.abort();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    #[tokio::test]
    async fn test_listen_unknown_worker_via_broker() {
        let tmp = TempDir::new().unwrap();
        let project_root = tmp.path().to_path_buf();
        let cell_id = "listen-err-test";

        let root = project_root.clone();
        let serve_handle =
            tokio::spawn(async move { serve(&test_config(&root, cell_id), None, false).await });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let sock = socket_path(&project_root, cell_id);

        let resp = send_json_request(
            &sock,
            &serde_json::json!({
                "type": "listen",
                "worker_id": "nonexistent-id",
                "timeout_secs": 1
            }),
        )
        .await;
        assert_eq!(resp["status"], "error");
        assert!(resp["message"].as_str().unwrap().contains("not found"));

        serve_handle.abort();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    #[tokio::test]
    async fn test_listen_fifo_ordering_via_broker() {
        let tmp = TempDir::new().unwrap();
        let project_root = tmp.path().to_path_buf();
        let cell_id = "listen-fifo-test";

        let root = project_root.clone();
        let serve_handle =
            tokio::spawn(async move { serve(&test_config(&root, cell_id), None, false).await });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let sock = socket_path(&project_root, cell_id);

        let worker_id = register_worker(&sock, "fifo-worker", "coder").await;

        // Send two messages.
        send_json_request(
            &sock,
            &serde_json::json!({"type": "send", "to": worker_id, "body": "first"}),
        )
        .await;
        send_json_request(
            &sock,
            &serde_json::json!({"type": "send", "to": worker_id, "body": "second"}),
        )
        .await;

        // Listen should return them in FIFO order.
        let r1 = send_json_request(
            &sock,
            &serde_json::json!({"type": "listen", "worker_id": worker_id, "timeout_secs": 1}),
        )
        .await;
        let r2 = send_json_request(
            &sock,
            &serde_json::json!({"type": "listen", "worker_id": worker_id, "timeout_secs": 1}),
        )
        .await;

        assert_eq!(r1["body"], "first");
        assert_eq!(r2["body"], "second");

        serve_handle.abort();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    #[tokio::test]
    async fn test_send_multiple_messages_via_broker() {
        let tmp = TempDir::new().unwrap();
        let project_root = tmp.path().to_path_buf();
        let cell_id = "send-multi-test";

        let root = project_root.clone();
        let serve_handle =
            tokio::spawn(async move { serve(&test_config(&root, cell_id), None, false).await });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let sock = socket_path(&project_root, cell_id);

        let worker_id = register_worker(&sock, "multi-recv", "coder").await;

        // Send two messages.
        let resp1 = send_json_request(
            &sock,
            &serde_json::json!({
                "type": "send",
                "to": worker_id,
                "body": "first message"
            }),
        )
        .await;
        let resp2 = send_json_request(
            &sock,
            &serde_json::json!({
                "type": "send",
                "to": worker_id,
                "body": "second message"
            }),
        )
        .await;

        assert_eq!(resp1["status"], "ok");
        assert_eq!(resp2["status"], "ok");
        let id1 = resp1["message_id"].as_str().unwrap();
        let id2 = resp2["message_id"].as_str().unwrap();
        assert_ne!(id1, id2, "each message should have a unique ID");

        serve_handle.abort();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}
