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
use crate::protocol::{
    BrokerRequest, BrokerResponse, Message, ResponsePayload, StatusEntry, Worker,
    STATUS_HISTORY_MAX,
};

/// Default worker TTL in seconds (1 hour).
const DEFAULT_WORKER_TTL_SECS: u64 = 3600;

/// Default listen timeout in seconds.
const DEFAULT_LISTEN_TIMEOUT_SECS: u64 = 30;

/// Default maximum number of events retained in history.
const DEFAULT_EVENT_HISTORY_MAX: usize = 10_000;

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

/// Record of a message acknowledgement.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AckRecord {
    pub message_id: String,
    pub worker_id: String,
    pub note: Option<String>,
    pub acked_at: u64,
}

/// In-memory broker state.
#[derive(Debug)]
pub struct BrokerState {
    /// Registered workers keyed by worker ID.
    pub workers: HashMap<String, Worker>,
    /// Per-worker message mailboxes keyed by worker ID.
    pub mailboxes: HashMap<String, VecDeque<Message>>,
    /// Per-worker notification channels for long-poll wakeup.
    pub notifiers: HashMap<String, Arc<Notify>>,
    /// Message acknowledgement log keyed by message ID.
    pub ack_log: HashMap<String, AckRecord>,
    /// Bounded event history for queries.
    pub event_history: VecDeque<BrokerEvent>,
    /// Maximum number of events to retain.
    pub event_history_max: usize,
    /// Bounded message history for queries (non-destructive inspection).
    pub message_history: VecDeque<Message>,
    /// Maximum number of messages to retain in history.
    pub message_history_max: usize,
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
            ack_log: HashMap::new(),
            event_history: VecDeque::new(),
            event_history_max: DEFAULT_EVENT_HISTORY_MAX,
            message_history: VecDeque::new(),
            message_history_max: DEFAULT_EVENT_HISTORY_MAX,
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
            last_status: None,
            last_status_at: None,
            status_history: VecDeque::new(),
        };
        self.workers.insert(id.clone(), worker);
        id
    }

    /// Remove workers whose TTL has expired, including their mailboxes and notifiers.
    /// Returns `(id, name)` pairs for the evicted workers so callers can populate
    /// expire events with the worker's name (which is otherwise lost on removal).
    pub fn evict_expired(&mut self) -> Vec<(String, String)> {
        let now = now_secs();
        let expired: Vec<(String, String)> = self
            .workers
            .iter()
            .filter(|(_, w)| w.expires_at <= now)
            .map(|(id, w)| (id.clone(), w.name.clone()))
            .collect();
        for (id, _) in &expired {
            self.workers.remove(id);
            self.mailboxes.remove(id);
            self.notifiers.remove(id);
        }
        expired
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

    /// Renew a worker's TTL and optionally update status.
    /// Returns the new expiry timestamp, or None if not found/expired.
    ///
    /// When `status` differs from the worker's existing `last_status`, the
    /// previous tagline (with its set time) is pushed onto `status_history`
    /// so the card UI can show the last few values. Identical re-sets are
    /// deduped so a steady heartbeat doesn't fill the buffer with copies.
    pub fn heartbeat_worker(&mut self, worker_id: &str, status: Option<String>) -> Option<u64> {
        self.evict_expired();
        if let Some(worker) = self.workers.get_mut(worker_id) {
            let now = now_secs();
            worker.expires_at = now + worker.ttl_secs;
            if let Some(s) = status {
                let unchanged = worker.last_status.as_deref() == Some(s.as_str());
                if !unchanged {
                    if let (Some(prev_status), Some(prev_at)) =
                        (worker.last_status.take(), worker.last_status_at)
                    {
                        push_status_history(
                            &mut worker.status_history,
                            StatusEntry {
                                status: prev_status,
                                set_at: prev_at,
                            },
                        );
                    }
                    worker.last_status = Some(s);
                    worker.last_status_at = Some(now);
                }
            }
            Some(worker.expires_at)
        } else {
            None
        }
    }

    /// Get status summaries for all active workers or a specific worker.
    pub fn get_status(&mut self, worker_id: Option<&str>) -> Vec<crate::protocol::WorkerStatus> {
        self.evict_expired();
        match worker_id {
            Some(id) => self
                .workers
                .get(id)
                .map(|w| {
                    vec![crate::protocol::WorkerStatus {
                        id: w.id.clone(),
                        name: w.name.clone(),
                        role: w.role.clone(),
                        last_status: w.last_status.clone(),
                        last_status_at: w.last_status_at,
                    }]
                })
                .unwrap_or_default(),
            None => self
                .workers
                .values()
                .map(|w| crate::protocol::WorkerStatus {
                    id: w.id.clone(),
                    name: w.name.clone(),
                    role: w.role.clone(),
                    last_status: w.last_status.clone(),
                    last_status_at: w.last_status_at,
                })
                .collect(),
        }
    }

    /// Emit an event: broadcast it, record in history, and print to stderr.
    ///
    /// `worker_name_override` wins when the worker has just been evicted (its
    /// name can't be looked up anymore) or when the caller already has the
    /// name cheaply. If `None`, falls back to `self.worker_name(worker_id)`.
    pub fn emit_and_record(
        &mut self,
        tx: &broadcast::Sender<BrokerEvent>,
        kind: &str,
        worker_id: &str,
        worker_name_override: Option<&str>,
        detail: &str,
        payload: Option<serde_json::Value>,
    ) {
        let worker_name = worker_name_override
            .map(|s| s.to_string())
            .or_else(|| self.worker_name(worker_id).map(|s| s.to_string()));
        let event = BrokerEvent {
            kind: kind.to_string(),
            worker_id: worker_id.to_string(),
            worker_name,
            detail: detail.to_string(),
            payload,
            timestamp: now_secs(),
        };
        let ts = chrono_ts();
        let display_name = event.worker_name.as_deref().unwrap_or(worker_id);
        if let Some(ref p) = event.payload {
            eprintln!(
                "[{ts}] {:>10}  {display_name}  {}  {p}",
                event.kind, event.detail
            );
        } else {
            eprintln!(
                "[{ts}] {:>10}  {display_name}  {}",
                event.kind, event.detail
            );
        }
        let _ = tx.send(event.clone());
        self.event_history.push_back(event);
        while self.event_history.len() > self.event_history_max {
            self.event_history.pop_front();
        }
    }

    /// Clear a worker's current status tagline. Does **not** touch
    /// `status_history` — clear is a display-level operation so the recent
    /// taglines stay visible on the agent card after the user resets the
    /// current state.
    pub fn clear_status(&mut self, worker_id: &str) -> Result<(), String> {
        self.evict_expired();
        if let Some(worker) = self.workers.get_mut(worker_id) {
            worker.last_status = None;
            worker.last_status_at = None;
            Ok(())
        } else {
            Err(format!("worker not found or expired: {worker_id}"))
        }
    }

    /// Record an acknowledgement for a message.
    ///
    /// Validates (in order) that the worker exists, the message exists in
    /// history, and the message was addressed to this worker. Without these
    /// checks a caller could record acks for arbitrary or nonexistent message
    /// IDs, corrupting the monitor's message state.
    pub fn ack_message(
        &mut self,
        worker_id: &str,
        message_id: &str,
        note: Option<String>,
    ) -> Result<(), String> {
        self.evict_expired();
        if !self.workers.contains_key(worker_id) {
            return Err(format!("worker not found or expired: {worker_id}"));
        }
        let recipient = self
            .message_history
            .iter()
            .find(|m| m.message_id == message_id)
            .map(|m| m.to.clone())
            .ok_or_else(|| format!("message not found: {message_id}"))?;
        if recipient != worker_id {
            return Err(format!(
                "message {message_id} was not addressed to worker {worker_id}"
            ));
        }
        let now = now_secs();
        self.ack_log.insert(
            message_id.to_string(),
            AckRecord {
                message_id: message_id.to_string(),
                worker_id: worker_id.to_string(),
                note,
                acked_at: now,
            },
        );
        if let Some(hist) = self
            .message_history
            .iter_mut()
            .find(|m| m.message_id == message_id)
        {
            hist.acked_at = Some(now);
        }
        Ok(())
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
        let now = now_secs();
        let message_id = uuid::Uuid::new_v4().to_string();
        let message = Message {
            message_id: message_id.clone(),
            from,
            to: to.clone(),
            body,
            sent_at: Some(now),
            delivered_at: None,
            acked_at: None,
        };
        // Record in history before moving into mailbox.
        self.message_history.push_back(message.clone());
        while self.message_history.len() > self.message_history_max {
            self.message_history.pop_front();
        }
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
    /// Also marks the message as delivered in the history.
    pub fn pop_message(&mut self, worker_id: &str) -> Option<Message> {
        let msg = self.mailboxes.get_mut(worker_id)?.pop_front()?;
        // Update delivered_at in history.
        let now = now_secs();
        if let Some(hist) = self
            .message_history
            .iter_mut()
            .find(|m| m.message_id == msg.message_id)
        {
            hist.delivered_at = Some(now);
        }
        Some(msg)
    }

    /// Query event history with optional filters.
    pub fn query_events(
        &self,
        since: Option<u64>,
        until: Option<u64>,
        event_type: Option<&str>,
        worker: Option<&str>,
        limit: Option<usize>,
    ) -> Vec<&BrokerEvent> {
        let limit = limit.unwrap_or(100);
        self.event_history
            .iter()
            .rev() // most recent first
            .filter(|e| since.is_none_or(|ts| e.timestamp >= ts))
            .filter(|e| until.is_none_or(|ts| e.timestamp <= ts))
            .filter(|e| event_type.is_none_or(|t| e.kind == t))
            .filter(|e| worker.is_none_or(|w| e.worker_id == w))
            .take(limit)
            .collect()
    }

    /// Query message history with optional filters.
    pub fn query_messages(
        &self,
        worker_id: &str,
        unacked: bool,
        sent: bool,
        since: Option<u64>,
        limit: Option<usize>,
        id: Option<&str>,
    ) -> Vec<&Message> {
        let limit = limit.unwrap_or(100);

        // Single message by ID
        if let Some(msg_id) = id {
            return self
                .message_history
                .iter()
                .filter(|m| m.message_id == msg_id)
                .collect();
        }

        self.message_history
            .iter()
            .rev() // most recent first
            .filter(|m| {
                if sent {
                    m.from.as_deref() == Some(worker_id)
                } else {
                    m.to == worker_id
                }
            })
            .filter(|m| {
                if unacked {
                    m.delivered_at.is_some() && m.acked_at.is_none()
                } else {
                    true
                }
            })
            .filter(|m| since.is_none_or(|ts| m.sent_at.unwrap_or(0) >= ts))
            .take(limit)
            .collect()
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

/// Push a status entry into a worker's history ring, deduping against the
/// most-recent entry and capping the ring at `STATUS_HISTORY_MAX`.
///
/// Dedupe protects the buffer from filling with duplicates when an upstream
/// re-emits the same tagline — only transitions show up in the history.
fn push_status_history(history: &mut VecDeque<StatusEntry>, entry: StatusEntry) {
    if history.back().map(|e| e.status.as_str()) == Some(entry.status.as_str()) {
        return;
    }
    history.push_back(entry);
    while history.len() > STATUS_HISTORY_MAX {
        history.pop_front();
    }
}

/// Derive the Unix domain socket path for a given cell identity.
///
/// Socket is placed in `/tmp/dispatch-cli/sockets/<cell_id>.sock`.
/// The cell_id already encodes the project identity (hashed canonical path),
/// so no additional path components are needed. Using `/tmp` avoids the
/// Unix domain socket `SUN_LEN` limit (104 bytes on macOS) that triggers
/// when project paths are deeply nested.
pub fn socket_path(_project_root: &Path, cell_id: &str) -> PathBuf {
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

    // Single source of truth for the log directory — used by both the
    // orchestrator (writes) and the monitor (reads via /api/logs/{agent}).
    let log_dir = config.project_root.join("logs");

    // Compute monitor URL up front so the orchestrator can pass it to agents
    // as DISPATCH_MONITOR_URL; the monitor server itself starts after the
    // orchestrator is constructed so MonitorState can share it.
    let monitor_url = monitor_port.map(|port| format!("http://localhost:{port}"));

    // Set up the orchestrator (manages agent process lifecycle).
    let orchestrator = Arc::new(Mutex::new(super::orchestrator::AgentOrchestrator::new(
        cell_id,
        &socket,
        monitor_url.clone(),
        &config.agent_cwd,
        log_dir.clone(),
        config.agents.clone(),
    )));

    // Optionally start the HTTP monitor dashboard.
    if let Some(port) = monitor_port {
        let url = monitor_url.clone().expect("monitor_url set when port set");
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
            log_dir: log_dir.clone(),
            monitor_url: Some(url.clone()),
            orchestrator: Arc::clone(&orchestrator),
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
    }

    if launch_agents {
        // Auto-launch only agents explicitly marked `launch = true`. Agents
        // with `launch = false` (the default) stay unmanaged and their
        // copy-paste launch commands are printed below instead.
        let mut orch = orchestrator.lock().await;
        orch.launch_all().await?;
        // Start configured heartbeat timers.
        if !config.heartbeats.is_empty() {
            orch.start_heartbeats(&config.heartbeats, &event_tx);
        }
    }

    // Print copy-paste launch commands for agents that were not auto-launched.
    // That means: every agent when `--launch` is not set, plus `launch = false`
    // agents when `--launch` is set.
    let manual: Vec<&crate::config::ResolvedAgentConfig> = config
        .agents
        .iter()
        .filter(|a| !launch_agents || !a.launch)
        .collect();
    if !manual.is_empty() {
        eprintln!("\ndispatch serve: ready. Unmanaged agents — run these in separate terminals:\n");
        for agent in &manual {
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
        res = accept_loop(&listener, state, event_tx, Arc::clone(&orchestrator)) => res,
        _ = shutdown_signal() => {
            tracing::info!("shutdown signal received");
            eprintln!("dispatch serve: shutting down agents...");
            orchestrator.lock().await.shutdown_all().await;
            eprintln!("dispatch serve: shutting down");
            Ok(())
        }
        _ = monitor_shutdown.notified() => {
            tracing::info!("shutdown requested via monitor");
            eprintln!("dispatch serve: shutdown requested from monitor");
            eprintln!("dispatch serve: shutting down agents...");
            orchestrator.lock().await.shutdown_all().await;
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
    orchestrator: Arc<Mutex<super::orchestrator::AgentOrchestrator>>,
) -> Result<(), DispatchError> {
    loop {
        let (stream, _addr) = listener.accept().await.map_err(DispatchError::Io)?;
        let state = Arc::clone(&state);
        let event_tx = event_tx.clone();
        let orchestrator = Arc::clone(&orchestrator);
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, state, event_tx, orchestrator).await {
                // Broken pipe is expected when clients disconnect before reading the response.
                let is_broken_pipe = matches!(&e, DispatchError::Io(io) if io.kind() == std::io::ErrorKind::BrokenPipe);
                if is_broken_pipe {
                    tracing::debug!(error = %e, "client disconnected before response");
                } else {
                    tracing::error!(error = %e, "connection handler error");
                }
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
    orchestrator: Arc<Mutex<super::orchestrator::AgentOrchestrator>>,
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
        Ok(request) => handle_request(request, state, &event_tx, orchestrator).await,
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
    orchestrator: Arc<Mutex<super::orchestrator::AgentOrchestrator>>,
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
            state.emit_and_record(
                event_tx,
                "register",
                &worker_id,
                Some(&name),
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
            let recipient_name = state.worker_name(&to).map(|s| s.to_string());
            match state.send_message(to.clone(), body, from) {
                Some(message_id) => {
                    state.messages_sent += 1;
                    tracing::info!(message_id = %message_id, to = %to, "message queued");
                    state.emit_and_record(
                        event_tx,
                        "send",
                        &to,
                        recipient_name.as_deref(),
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
            let (notifier, immediate_msg, listener_name) = {
                let mut s = state.lock().await;
                let expired = s.evict_expired();
                for (id, name) in &expired {
                    s.emit_and_record(event_tx, "expire", id, Some(name), "worker expired", None);
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
                let name = s.worker_name(&worker_id).map(|s| s.to_string());
                (notifier, msg, name)
            };

            // If a message was immediately available, return it.
            if let Some(msg) = immediate_msg {
                {
                    let mut s = state.lock().await;
                    s.messages_delivered += 1;
                    s.emit_and_record(
                        event_tx,
                        "deliver",
                        &worker_id,
                        listener_name.as_deref(),
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
                }
                tracing::info!(worker_id = %worker_id, message_id = %msg.message_id, "listen: immediate delivery");
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
                    let name = s.worker_name(&worker_id).map(|s| s.to_string());
                    s.emit_and_record(
                        event_tx,
                        "deliver",
                        &worker_id,
                        name.as_deref(),
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
        BrokerRequest::Heartbeat { worker_id, status } => {
            let mut state = state.lock().await;
            let has_status = status.is_some();
            let status_clone = status.clone();
            match state.heartbeat_worker(&worker_id, status) {
                Some(expires_at) => {
                    let detail = if has_status {
                        "TTL renewed + status updated"
                    } else {
                        "TTL renewed"
                    };
                    tracing::info!(worker_id = %worker_id, expires_at, "heartbeat renewed");
                    let name = state.worker_name(&worker_id).map(|s| s.to_string());
                    state.emit_and_record(
                        event_tx,
                        "heartbeat",
                        &worker_id,
                        name.as_deref(),
                        detail,
                        status_clone.map(|s| serde_json::json!({ "status": s })),
                    );
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
        BrokerRequest::Ack {
            worker_id,
            message_id,
            note,
        } => {
            let mut state = state.lock().await;
            let note_clone = note.clone();
            match state.ack_message(&worker_id, &message_id, note) {
                Ok(()) => {
                    tracing::info!(worker_id = %worker_id, message_id = %message_id, "message acked");
                    state.emit_and_record(
                        event_tx,
                        "ack",
                        &worker_id,
                        None,
                        &format!("acked {}", &message_id[..message_id.len().min(8)]),
                        Some(serde_json::json!({
                            "message_id": message_id,
                            "note": note_clone,
                        })),
                    );
                    BrokerResponse::Ok {
                        payload: ResponsePayload::AckConfirm {
                            message_id,
                            ack_confirmed: true,
                        },
                    }
                }
                Err(msg) => BrokerResponse::Error { message: msg },
            }
        }
        BrokerRequest::Status { worker_id, clear } => {
            let mut state = state.lock().await;
            if clear {
                match worker_id {
                    Some(id) => match state.clear_status(&id) {
                        Ok(()) => {
                            tracing::info!(worker_id = %id, "status cleared");
                            BrokerResponse::Ok {
                                payload: ResponsePayload::Ack {},
                            }
                        }
                        Err(msg) => BrokerResponse::Error { message: msg },
                    },
                    None => BrokerResponse::Error {
                        message: "--clear requires --worker-id".to_string(),
                    },
                }
            } else {
                let workers = state.get_status(worker_id.as_deref());
                BrokerResponse::Ok {
                    payload: ResponsePayload::StatusResult { workers },
                }
            }
        }
        BrokerRequest::Events {
            since,
            until,
            event_type,
            worker,
            limit,
        } => {
            let state = state.lock().await;
            let events = state.query_events(
                since,
                until,
                event_type.as_deref(),
                worker.as_deref(),
                limit,
            );
            let events_json: Vec<serde_json::Value> = events
                .into_iter()
                .map(|e| serde_json::to_value(e).unwrap_or_default())
                .collect();
            BrokerResponse::Ok {
                payload: ResponsePayload::EventList {
                    events: events_json,
                },
            }
        }
        BrokerRequest::Messages {
            worker_id,
            unacked,
            sent,
            since,
            limit,
            id,
        } => {
            let state = state.lock().await;
            let messages =
                state.query_messages(&worker_id, unacked, sent, since, limit, id.as_deref());
            let messages: Vec<Message> = messages.into_iter().cloned().collect();
            BrokerResponse::Ok {
                payload: ResponsePayload::MessageList { messages },
            }
        }
        BrokerRequest::AgentStart { name } => {
            let resolved = resolve_agent_target(&name, &state, &orchestrator).await;
            let mut orch = orchestrator.lock().await;
            match orch.start_by_name(&resolved).await {
                Ok(()) => BrokerResponse::Ok {
                    payload: ResponsePayload::Ack {},
                },
                Err(e) => BrokerResponse::Error {
                    message: format!("agent start failed: {e}"),
                },
            }
        }
        BrokerRequest::AgentStop { name } => {
            let resolved = resolve_agent_target(&name, &state, &orchestrator).await;
            // Release the orchestrator mutex before awaiting the supervisor's
            // shutdown so concurrent list_state / monitor polls don't stall
            // for 500ms+ per stop.
            let handle = {
                let mut orch = orchestrator.lock().await;
                orch.signal_stop_by_name(&resolved)
            };
            match handle {
                Some(h) => {
                    let _ = h.await;
                    BrokerResponse::Ok {
                        payload: ResponsePayload::Ack {},
                    }
                }
                None => BrokerResponse::Error {
                    message: format!("agent '{resolved}' is not running"),
                },
            }
        }
        BrokerRequest::AgentRestart { name } => {
            let resolved = resolve_agent_target(&name, &state, &orchestrator).await;
            // Split phases mirror api_agent_restart: lock → signal stop →
            // unlock → await → lock → start. Avoids pinning the orchestrator
            // mutex across the kill window.
            let handle = {
                let mut orch = orchestrator.lock().await;
                if !orch.has_config(&resolved) {
                    return BrokerResponse::Error {
                        message: format!(
                            "agent restart failed: failed to launch agent \"{resolved}\": no such agent in config"
                        ),
                    };
                }
                orch.signal_stop_by_name(&resolved)
            };
            if let Some(h) = handle {
                let _ = h.await;
            }
            let mut orch = orchestrator.lock().await;
            match orch.start_by_name(&resolved).await {
                Ok(()) => BrokerResponse::Ok {
                    payload: ResponsePayload::Ack {},
                },
                Err(e) => BrokerResponse::Error {
                    message: format!("agent restart failed: {e}"),
                },
            }
        }
    }
}

/// Resolve an `agent` subcommand target string (agent name or worker ID) to a
/// configured agent name. If the input already matches a configured agent
/// name, it's returned as-is. Otherwise, treat it as a worker ID and look up
/// the worker's name in the broker registry. Falls back to the input itself
/// when neither lookup succeeds — the orchestrator will then surface a
/// "no such agent in config" error.
async fn resolve_agent_target(
    target: &str,
    state: &Arc<Mutex<BrokerState>>,
    orchestrator: &Arc<Mutex<super::orchestrator::AgentOrchestrator>>,
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

        let new_expiry = state.heartbeat_worker(&id, None).unwrap();
        assert!(
            new_expiry >= original_expiry,
            "heartbeat should renew to at least the original TTL"
        );
    }

    #[test]
    fn test_heartbeat_worker_not_found() {
        let mut state = BrokerState::new();
        assert!(state.heartbeat_worker("nonexistent", None).is_none());
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
            state.heartbeat_worker(&id, None).is_none(),
            "heartbeat for expired worker should return None"
        );
    }

    /// Pushing 5 distinct statuses leaves the current one on `last_status`
    /// and the most recent `STATUS_HISTORY_MAX` priors in `status_history`,
    /// oldest first. The very first status (A) drops off when the ring caps.
    #[test]
    fn test_status_history_caps_at_max() {
        let mut state = BrokerState::new();
        let id = state.register_worker(
            "hist".into(),
            "role".into(),
            "desc".into(),
            vec![],
            None,
            false,
        );
        for s in ["A", "B", "C", "D", "E"] {
            state.heartbeat_worker(&id, Some(s.into())).unwrap();
        }
        let w = state.workers.get(&id).unwrap();
        assert_eq!(w.last_status.as_deref(), Some("E"));
        let history: Vec<&str> = w.status_history.iter().map(|e| e.status.as_str()).collect();
        assert_eq!(history, vec!["B", "C", "D"]);
        assert_eq!(w.status_history.len(), STATUS_HISTORY_MAX);
    }

    /// Re-setting an identical status is a no-op for both `last_status_at`
    /// and `status_history` — heartbeats that re-emit the same tagline must
    /// not pollute the buffer with copies.
    #[test]
    fn test_status_history_dedupes_consecutive_repeats() {
        let mut state = BrokerState::new();
        let id = state.register_worker(
            "dedup".into(),
            "role".into(),
            "desc".into(),
            vec![],
            None,
            false,
        );
        state.heartbeat_worker(&id, Some("running".into())).unwrap();
        let first_at = state.workers.get(&id).unwrap().last_status_at;
        // Same status again — should not push, should not bump last_status_at.
        state.heartbeat_worker(&id, Some("running".into())).unwrap();
        let w = state.workers.get(&id).unwrap();
        assert!(
            w.status_history.is_empty(),
            "no transition, no history push"
        );
        assert_eq!(
            w.last_status_at, first_at,
            "identical status must not bump last_status_at",
        );
    }

    /// `status --clear` is a display-level reset: it nulls the current
    /// tagline but leaves the historical buffer alone so the card still
    /// shows the recent timeline.
    #[test]
    fn test_clear_status_preserves_history() {
        let mut state = BrokerState::new();
        let id = state.register_worker(
            "clr".into(),
            "role".into(),
            "desc".into(),
            vec![],
            None,
            false,
        );
        state.heartbeat_worker(&id, Some("phase 1".into())).unwrap();
        state.heartbeat_worker(&id, Some("phase 2".into())).unwrap();
        state.clear_status(&id).unwrap();
        let w = state.workers.get(&id).unwrap();
        assert!(w.last_status.is_none());
        assert!(w.last_status_at.is_none());
        let history: Vec<&str> = w.status_history.iter().map(|e| e.status.as_str()).collect();
        assert_eq!(history, vec!["phase 1"]);
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

    #[test]
    fn test_ack_message_rejects_unknown_worker() {
        let mut state = BrokerState::new();
        let err = state
            .ack_message("missing-worker", "msg-1", None)
            .unwrap_err();
        assert!(err.contains("worker not found"), "got: {err}");
    }

    #[test]
    fn test_ack_message_rejects_unknown_message() {
        let mut state = BrokerState::new();
        let worker_id = state.register_worker(
            "recv".into(),
            "coder".into(),
            "desc".into(),
            vec![],
            None,
            false,
        );
        let err = state
            .ack_message(&worker_id, "nonexistent-message", None)
            .unwrap_err();
        assert!(err.contains("message not found"), "got: {err}");
    }

    #[test]
    fn test_ack_message_rejects_wrong_recipient() {
        let mut state = BrokerState::new();
        let alice =
            state.register_worker("alice".into(), "r".into(), "d".into(), vec![], None, false);
        let bob = state.register_worker("bob".into(), "r".into(), "d".into(), vec![], None, false);
        let message_id = state
            .send_message(alice.clone(), "for alice".into(), None)
            .expect("send");
        let err = state.ack_message(&bob, &message_id, None).unwrap_err();
        assert!(
            err.contains("not addressed to worker"),
            "expected recipient-mismatch error, got: {err}"
        );
    }

    #[test]
    fn test_ack_message_success_updates_history() {
        let mut state = BrokerState::new();
        let alice =
            state.register_worker("alice".into(), "r".into(), "d".into(), vec![], None, false);
        let message_id = state
            .send_message(alice.clone(), "hello".into(), None)
            .expect("send");
        state
            .ack_message(&alice, &message_id, Some("noted".into()))
            .expect("ack should succeed");
        let hist = state
            .message_history
            .iter()
            .find(|m| m.message_id == message_id)
            .expect("message in history");
        assert!(hist.acked_at.is_some(), "acked_at should be set");
        assert!(state.ack_log.contains_key(&message_id));
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
