use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast;
use tokio::sync::{Mutex, Notify};
use tracing::instrument;

use crate::errors::DispatchError;
use crate::protocol::{BrokerRequest, BrokerResponse, ControlState, Message, ResponsePayload};

mod control;
mod socket;
mod state;

use control::{mark_worker_stopping, resolve_agent_target, resolve_stop_target};
use socket::check_no_existing_broker;
pub use socket::socket_path;
pub use state::{
    body_fingerprint, now_secs, AckRecord, BrokerError, BrokerEvent, BrokerState, EvictReason,
    DEFAULT_STOPPING_DRAIN_SECS,
};

/// Default listen timeout in seconds. Single source of truth shared with the
/// CLI (`main.rs` resolves `--timeout` flag > `$DISPATCH_LISTEN_TIMEOUT` env >
/// this default) and the broker's own 0-means-default fallback below.
pub const DEFAULT_LISTEN_TIMEOUT_SECS: u64 = 270;

/// Local backend that uses a Unix domain socket for IPC.
///
/// The broker runs in-process with in-memory state. Clients connect
/// over a UDS, send one JSON-line request, and receive one JSON-line
/// response before the connection is closed.
pub struct LocalBackend {
    config: crate::config::ResolvedConfig,
    monitor_port: Option<u16>,
}

impl LocalBackend {
    pub fn new(config: &crate::config::ResolvedConfig, monitor_port: Option<u16>) -> Self {
        Self {
            config: config.clone(),
            monitor_port,
        }
    }
}

#[async_trait]
impl super::Backend for LocalBackend {
    /// Start the broker server on a Unix domain socket, blocking until
    /// a shutdown signal (SIGINT/SIGTERM) is received.
    async fn serve(&self) -> Result<(), DispatchError> {
        serve(&self.config, self.monitor_port).await
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

/// Start the embedded broker server.
///
/// Listens on a Unix domain socket and handles JSON-line requests.
/// Returns when a shutdown signal (SIGINT/SIGTERM) is received.
#[instrument(skip_all, fields(cell_id, socket_path))]
pub async fn serve(
    config: &crate::config::ResolvedConfig,
    monitor_port: Option<u16>,
) -> Result<(), DispatchError> {
    let cell_id = &config.cell_id;
    let project_root = &config.project_root;
    let socket = socket_path(project_root, cell_id);

    // Ensure parent directory exists.
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent).map_err(DispatchError::Io)?;
    }

    check_no_existing_broker(&socket, cell_id).await?;

    let listener = UnixListener::bind(&socket).map_err(DispatchError::Io)?;

    tracing::info!(cell_id, socket_path = %socket.display(), "broker listening");
    eprintln!(
        "dispatch serve: broker listening on {} (cell={})",
        socket.display(),
        cell_id
    );

    let mut broker_state = if let Some(ttl) = config.default_ttl {
        BrokerState::with_default_ttl(ttl)
    } else {
        BrokerState::new()
    };
    if let Some(drain) = config.stopping_drain_secs {
        broker_state.stopping_drain_secs = drain;
    }
    broker_state.log_prompt_bodies = config.log_prompt_bodies;
    let state = Arc::new(Mutex::new(broker_state));
    let (event_tx, _) = broadcast::channel::<BrokerEvent>(256);

    // Shutdown signal shared with the monitor dashboard.
    let monitor_shutdown = Arc::new(Notify::new());

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
        Arc::clone(&state),
        config.config_file_path.clone(),
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

    // Auto-launch agents marked `launch = true`. Agents with `launch = false`
    // (the default) stay unmanaged and their copy-paste launch commands are
    // printed below instead. `launch_all` already filters by `launch = true`,
    // so it's safe to always call.
    {
        let mut orch = orchestrator.lock().await;
        orch.launch_all().await?;
        if !config.heartbeats.is_empty() {
            orch.start_heartbeats(&config.heartbeats, &event_tx);
        }
    }

    // Print copy-paste launch commands for every `launch = false` agent.
    //
    // For every unmanaged agent with a `prompt_file`, pre-register a worker
    // server-side and wire the printed command to the boot-prompt
    // bootstrap: `DISPATCH_WORKER_ID=<uuid>` in the env + `< <name>.boot.prompt`
    // on stdin. When the user pastes + runs, the agent's first tool call
    // (`dispatch register --for-agent`) idempotently claims the pre-registered
    // worker and retrieves the role prompt — same mechanism as supervised
    // agents, just with the user launching the process instead of the
    // orchestrator.
    let manual: Vec<&crate::config::ResolvedAgentConfig> =
        config.agents.iter().filter(|a| !a.launch).collect();
    if !manual.is_empty() {
        eprintln!("\ndispatch serve: ready. Unmanaged agents — run these in separate terminals:\n");
        let ctx = orchestrator.lock().await.snapshot_spawn_context();
        for agent in &manual {
            // Agents with a prompt_file get the boot-prompt bootstrap; the
            // legacy bare-register path is reserved for configs with no prompt.
            // Pre-register failures log + fall back to the legacy path so one
            // misconfigured agent doesn't block the serve banner.
            let (worker_id, render_config): (Option<String>, crate::config::ResolvedAgentConfig) =
                if agent.prompt_file_path.is_some() {
                    match super::orchestrator::pre_register_unmanaged(&ctx, agent).await {
                        Ok((id, boot_path)) => {
                            let mut cloned = (*agent).clone();
                            cloned.prompt_file_path = Some(boot_path);
                            (Some(id), cloned)
                        }
                        Err(e) => {
                            eprintln!(
                                "  # warning: pre-register failed for '{}': {e} — falling back to legacy copy-paste",
                                agent.name
                            );
                            (None, (*agent).clone())
                        }
                    }
                } else {
                    (None, (*agent).clone())
                };
            let cmd = super::orchestrator::build_agent_command(
                &render_config,
                cell_id,
                monitor_url.as_deref(),
                worker_id.as_deref(),
                config.config_file_path.as_deref(),
            );
            eprintln!("  # {} ({})", agent.name, agent.role);
            eprintln!("  {cmd}\n");
        }
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
            worker_id,
            role_prompt,
        } => {
            let mut state = state.lock().await;
            // A claim (the agent fetching its own prompt) passes no
            // `role_prompt` and receives the stored body back — that's the
            // prompt-delivery moment. The orchestrator's pre-register
            // passes `role_prompt = Some(body)` (storing, not delivering), so
            // it is not counted as a delivery.
            let is_claim = role_prompt.is_none();
            let worker_id = match state.register_worker(
                name.clone(),
                role.clone(),
                description,
                capabilities,
                ttl_secs,
                evict,
                worker_id,
                role_prompt,
            ) {
                Ok(id) => id,
                Err(err) => {
                    let message = err.to_string();
                    tracing::warn!(%message, "register rejected");
                    return BrokerResponse::Error { message };
                }
            };
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
            // Issue #43: include the stored role prompt (if any) so the
            // spawned agent receives its first instructions as the response
            // body of its own `dispatch register` claim.
            let role_prompt = state.role_prompts.get(&worker_id).cloned();
            // Record the delivery by fingerprint (hash + byte size),
            // never the full body unless `log_prompt_bodies` is set, so prompt
            // cost/compliance is auditable via `dispatch events --type prompt`.
            if is_claim {
                if let Some(ref prompt) = role_prompt {
                    let log_bodies = state.log_prompt_bodies;
                    let (hash, bytes) = body_fingerprint(prompt);
                    let mut payload = serde_json::json!({ "hash": hash, "bytes": bytes });
                    if log_bodies {
                        payload["body"] = serde_json::Value::String(prompt.clone());
                    }
                    state.emit_and_record(
                        event_tx,
                        "prompt",
                        &worker_id,
                        Some(&name),
                        &format!("delivered role prompt ({bytes} bytes, {hash})"),
                        Some(payload),
                    );
                }
            }
            BrokerResponse::Ok {
                payload: ResponsePayload::WorkerRegistered {
                    worker_id,
                    role_prompt,
                },
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
                for (id, name, reason) in &expired {
                    match reason {
                        EvictReason::TtlExpired => s.emit_and_record(
                            event_tx,
                            "expire",
                            id,
                            Some(name),
                            "worker expired",
                            None,
                        ),
                        EvictReason::StopDrained => s.emit_and_record(
                            event_tx,
                            "lifecycle",
                            id,
                            Some(name),
                            "worker stopped (drain window elapsed)",
                            Some(serde_json::json!({ "control_state": "stopped" })),
                        ),
                    }
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
                        payload: ResponsePayload::Timeout(crate::protocol::TimeoutPayload {
                            worker_id,
                        }),
                    }
                }
            } else {
                // Timed out.
                tracing::debug!(worker_id = %worker_id, timeout, "listen: timed out");
                BrokerResponse::Ok {
                    payload: ResponsePayload::Timeout(crate::protocol::TimeoutPayload {
                        worker_id,
                    }),
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
            status,
            summary,
            artifacts,
        } => {
            let mut state = state.lock().await;
            // `dispatch result` rides this request as a super-ack: when
            // `status` is present, emit a `result` event with the completion
            // payload; otherwise a plain `ack` with its original shape. Capture
            // what we need for the event before the fields move into ack_message.
            let is_result = status.is_some();
            let note_clone = note.clone();
            let status_clone = status.clone();
            let summary_clone = summary.clone();
            let artifacts_clone = artifacts.clone();
            match state.ack_message(&worker_id, &message_id, note, status, summary, artifacts) {
                Ok(()) => {
                    let short = &message_id[..message_id.len().min(8)];
                    let (kind, detail, payload) = if is_result {
                        (
                            "result",
                            format!(
                                "result {} {}",
                                status_clone.as_deref().unwrap_or("done"),
                                short
                            ),
                            serde_json::json!({
                                "message_id": message_id,
                                "note": note_clone,
                                "status": status_clone,
                                "summary": summary_clone,
                                "artifacts": artifacts_clone,
                            }),
                        )
                    } else {
                        (
                            "ack",
                            format!("acked {short}"),
                            serde_json::json!({
                                "message_id": message_id,
                                "note": note_clone,
                            }),
                        )
                    };
                    tracing::info!(worker_id = %worker_id, message_id = %message_id, kind, "message acked");
                    state.emit_and_record(event_tx, kind, &worker_id, None, &detail, Some(payload));
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
        BrokerRequest::Status {
            worker_id,
            clear,
            probe,
        } => {
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
                // A stop-hook probe records the block/allow decision it
                // implies, derived from the worker's control state, as a
                // `stop_decision` event (the hook itself runs in the agent's
                // process and can't write to the broker's history directly).
                if probe.as_deref() == Some("stop_hook") {
                    if let Some(id) = worker_id.as_deref() {
                        let observed = workers.iter().find(|w| w.id == id).map(|w| w.control_state);
                        let (state_label, decision) = match observed {
                            Some(ControlState::Active) => ("active", "block"),
                            Some(ControlState::Stopping) => ("stopping", "allow"),
                            Some(ControlState::Stopped) => ("stopped", "allow"),
                            None => ("unknown", "allow"),
                        };
                        state.emit_and_record(
                            event_tx,
                            "stop_decision",
                            id,
                            None,
                            &format!("{state_label} -> {decision}"),
                            Some(serde_json::json!({
                                "control_state": state_label,
                                "decision": decision,
                            })),
                        );
                    }
                }
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
            // Three-phase pattern (PR #28 / #43): release the orchestrator
            // mutex during the heavy spawn await so concurrent dashboard
            // polls and other broker requests don't stall on file IO +
            // pre-register + spawn_child_process.
            let (config, ctx) = {
                let mut orch = orchestrator.lock().await;
                let config = match orch.check_can_start(&resolved) {
                    Ok(c) => c,
                    Err(e) => {
                        return BrokerResponse::Error {
                            message: format!("agent start failed: {e}"),
                        };
                    }
                };
                (config, orch.snapshot_spawn_context())
            };
            let pending = match super::orchestrator::build_pending_agent(ctx, &config).await {
                Ok(p) => p,
                Err(e) => {
                    // Release the phase 1 reservation so future starts
                    // of this name aren't blocked by a stale slot.
                    orchestrator.lock().await.cancel_start(&resolved);
                    return BrokerResponse::Error {
                        message: format!("agent start failed: {e}"),
                    };
                }
            };
            orchestrator.lock().await.register_pending(pending);
            BrokerResponse::Ok {
                payload: ResponsePayload::Ack {},
            }
        }
        BrokerRequest::AgentStop { name } => {
            let (resolved, target) = resolve_stop_target(&name, &state, &orchestrator).await;
            // Mark the target `stopping` BEFORE the kill so a late stop-hook
            // call from the dying agent sees `stopping` (and is allowed to
            // exit) instead of racing the record away. A worker-id argument
            // marks only that worker; an agent-name argument marks every worker
            // registered under it.
            mark_worker_stopping(&state, event_tx, &target).await;
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
            let (resolved, target) = resolve_stop_target(&name, &state, &orchestrator).await;
            // Validate the config up front so a bad name doesn't mark a worker
            // stopping for an agent we can't restart.
            {
                let orch = orchestrator.lock().await;
                if !orch.has_config(&resolved) {
                    return BrokerResponse::Error {
                        message: format!(
                            "agent restart failed: failed to launch agent \"{resolved}\": no such agent in config"
                        ),
                    };
                }
            }
            // Stopping semantics for the OLD worker before the respawn:
            // mark it stopping, then kill. A worker-id argument marks only that
            // worker; a name marks every worker under it. The fresh spawn
            // re-registers an active worker (its evict pass wipes the old id).
            mark_worker_stopping(&state, event_tx, &target).await;
            // Split phases mirror api_agent_restart: lock → signal stop →
            // unlock → await → lock → start. Avoids pinning the orchestrator
            // mutex across the kill window.
            let handle = {
                let mut orch = orchestrator.lock().await;
                orch.signal_stop_by_name(&resolved)
            };
            if let Some(h) = handle {
                let _ = h.await;
            }
            // Same three-phase pattern as AgentStart for the respawn.
            let (config, ctx) = {
                let mut orch = orchestrator.lock().await;
                let config = match orch.check_can_start(&resolved) {
                    Ok(c) => c,
                    Err(e) => {
                        return BrokerResponse::Error {
                            message: format!("agent restart failed: {e}"),
                        };
                    }
                };
                (config, orch.snapshot_spawn_context())
            };
            let pending = match super::orchestrator::build_pending_agent(ctx, &config).await {
                Ok(p) => p,
                Err(e) => {
                    // Release the phase 3 reservation on build failure.
                    orchestrator.lock().await.cancel_start(&resolved);
                    return BrokerResponse::Error {
                        message: format!("agent restart failed: {e}"),
                    };
                }
            };
            orchestrator.lock().await.register_pending(pending);
            BrokerResponse::Ok {
                payload: ResponsePayload::Ack {},
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

#[cfg(test)]
mod tests;
