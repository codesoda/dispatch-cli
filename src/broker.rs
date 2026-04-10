use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::Mutex;
use tracing::instrument;

use crate::errors::DispatchError;
use crate::protocol::{BrokerRequest, BrokerResponse, Message, ResponsePayload, Worker};

/// Default worker TTL in seconds (5 minutes).
const DEFAULT_WORKER_TTL_SECS: u64 = 300;

/// In-memory broker state.
#[derive(Debug, Default)]
pub struct BrokerState {
    /// Registered workers keyed by worker ID.
    pub workers: HashMap<String, Worker>,
    /// Per-worker message mailboxes keyed by worker ID.
    pub mailboxes: HashMap<String, VecDeque<Message>>,
}

impl BrokerState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a new worker and return its unique ID.
    pub fn register_worker(
        &mut self,
        name: String,
        role: String,
        description: String,
        capabilities: Vec<String>,
    ) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        let now = now_secs();
        let worker = Worker {
            id: id.clone(),
            name,
            role,
            description,
            capabilities,
            expires_at: now + DEFAULT_WORKER_TTL_SECS,
        };
        self.workers.insert(id.clone(), worker);
        id
    }

    /// Remove workers whose TTL has expired.
    pub fn evict_expired(&mut self) {
        let now = now_secs();
        self.workers.retain(|_, w| w.expires_at > now);
    }

    /// Return a list of all active (non-expired) workers.
    pub fn list_workers(&mut self) -> Vec<Worker> {
        self.evict_expired();
        self.workers.values().cloned().collect()
    }

    /// Renew a worker's TTL. Returns the new expiry timestamp, or None if not found/expired.
    pub fn heartbeat_worker(&mut self, worker_id: &str) -> Option<u64> {
        self.evict_expired();
        if let Some(worker) = self.workers.get_mut(worker_id) {
            let new_expiry = now_secs() + DEFAULT_WORKER_TTL_SECS;
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
        self.mailboxes.entry(to).or_default().push_back(message);
        Some(message_id)
    }
}

/// Get current Unix timestamp in seconds.
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_secs()
}

/// Derive the Unix domain socket path for a given cell identity.
///
/// Socket is placed in `<project_root>/.dispatch/<cell_id>.sock`.
pub fn socket_path(project_root: &Path, cell_id: &str) -> PathBuf {
    project_root
        .join(".dispatch")
        .join(format!("{cell_id}.sock"))
}

/// Check whether a broker is already running for this cell by testing
/// if the socket file exists and a connection can be made.
pub async fn check_no_existing_broker(socket: &Path, cell_id: &str) -> Result<(), DispatchError> {
    if !socket.exists() {
        return Ok(());
    }

    // Socket file exists — try to connect to see if a broker is actually listening.
    match tokio::net::UnixStream::connect(socket).await {
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

/// Start the embedded broker server.
///
/// Listens on a Unix domain socket and handles JSON-line requests.
/// Returns when a shutdown signal (SIGINT/SIGTERM) is received.
#[instrument(skip_all, fields(cell_id, socket_path))]
pub async fn serve(project_root: &Path, cell_id: &str) -> Result<(), DispatchError> {
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

    let state = Arc::new(Mutex::new(BrokerState::new()));

    // Run until shutdown signal.
    let result = tokio::select! {
        res = accept_loop(&listener, state) => res,
        _ = shutdown_signal() => {
            tracing::info!("shutdown signal received");
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
) -> Result<(), DispatchError> {
    loop {
        let (stream, _addr) = listener.accept().await.map_err(DispatchError::Io)?;
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, state).await {
                tracing::error!(error = %e, "connection handler error");
            }
        });
    }
}

/// Handle a single client connection.
///
/// Reads one JSON line, processes it, writes one JSON line response, then closes.
async fn handle_connection(
    stream: tokio::net::UnixStream,
    _state: Arc<Mutex<BrokerState>>,
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
        Ok(request) => handle_request(request, _state).await,
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
async fn handle_request(request: BrokerRequest, state: Arc<Mutex<BrokerState>>) -> BrokerResponse {
    match request {
        BrokerRequest::Register {
            name,
            role,
            description,
            capabilities,
        } => {
            let mut state = state.lock().await;
            let worker_id = state.register_worker(name, role, description, capabilities);
            tracing::info!(worker_id = %worker_id, "worker registered");
            BrokerResponse::Ok {
                payload: ResponsePayload::WorkerRegistered { worker_id },
            }
        }
        BrokerRequest::Team => {
            let mut state = state.lock().await;
            let workers = state.list_workers();
            tracing::info!(count = workers.len(), "team listing");
            BrokerResponse::Ok {
                payload: ResponsePayload::WorkerList { workers },
            }
        }
        BrokerRequest::Send { to, body, from } => {
            let mut state = state.lock().await;
            match state.send_message(to.clone(), body, from) {
                Some(message_id) => {
                    tracing::info!(message_id = %message_id, to = %to, "message queued");
                    BrokerResponse::Ok {
                        payload: ResponsePayload::MessageAck { message_id },
                    }
                }
                None => BrokerResponse::Error {
                    message: format!("recipient worker not found or expired: {to}"),
                },
            }
        }
        BrokerRequest::Listen { .. } => {
            // Will be implemented in US-008.
            BrokerResponse::Ok {
                payload: ResponsePayload::Ack {},
            }
        }
        BrokerRequest::Heartbeat { worker_id } => {
            let mut state = state.lock().await;
            match state.heartbeat_worker(&worker_id) {
                Some(expires_at) => {
                    tracing::info!(worker_id = %worker_id, expires_at, "heartbeat renewed");
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    #[test]
    fn test_socket_path_derivation() {
        let path = socket_path(Path::new("/home/user/project"), "cell-abc123");
        assert_eq!(
            path,
            PathBuf::from("/home/user/project/.dispatch/cell-abc123.sock")
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
        let serve_handle = tokio::spawn(async move { serve(&root, "test-cell").await });

        // Wait briefly for the server to bind.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Verify socket file exists.
        assert!(sock.exists(), "socket file should exist after startup");

        // Connect and send a request.
        let stream = tokio::net::UnixStream::connect(&sock).await.unwrap();
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
        let result = serve(&project_root, cell_id).await;
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
        std::fs::write(&sock, "stale").unwrap();

        // Starting serve should clean up the stale socket and bind fresh.
        let root = project_root.clone();
        let serve_handle = tokio::spawn(async move { serve(&root, "restart-cell").await });

        // Wait briefly for the server to bind.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Should be able to connect.
        let result = tokio::net::UnixStream::connect(&sock).await;
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
        );
        let id2 = state.register_worker(
            "worker-b".into(),
            "coder".into(),
            "Writes code".into(),
            vec![],
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
        let id = state.register_worker("ttl-worker".into(), "role".into(), "desc".into(), vec![]);
        let worker = state.workers.get(&id).unwrap();
        // Should expire roughly DEFAULT_WORKER_TTL_SECS from now.
        assert!(worker.expires_at >= now + DEFAULT_WORKER_TTL_SECS - 1);
        assert!(worker.expires_at <= now + DEFAULT_WORKER_TTL_SECS + 1);
    }

    #[test]
    fn test_evict_expired_workers() {
        let mut state = BrokerState::new();
        let id = state.register_worker("soon-expired".into(), "role".into(), "desc".into(), vec![]);
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
        let serve_handle = tokio::spawn(async move { serve(&root, "reg-test").await });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Send register request via raw socket.
        let sock = socket_path(&project_root, cell_id);
        let stream = tokio::net::UnixStream::connect(&sock).await.unwrap();
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

        let root = project_root.clone();
        let serve_handle = tokio::spawn(async move { serve(&root, "cap-test").await });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let sock = socket_path(&project_root, "cap-test");

        // Register with capabilities using name:description convention.
        let stream = tokio::net::UnixStream::connect(&sock).await.unwrap();
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
        let stream = tokio::net::UnixStream::connect(sock).await.unwrap();
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
        let active_id =
            state.register_worker("active".into(), "coder".into(), "desc".into(), vec![]);
        let expired_id =
            state.register_worker("expired".into(), "coder".into(), "desc".into(), vec![]);
        // Expire one worker.
        state.workers.get_mut(&expired_id).unwrap().expires_at = 0;

        let workers = state.list_workers();
        assert_eq!(workers.len(), 1);
        assert_eq!(workers[0].id, active_id);
    }

    #[test]
    fn test_heartbeat_worker_renews_ttl() {
        let mut state = BrokerState::new();
        let id = state.register_worker("hb-worker".into(), "role".into(), "desc".into(), vec![]);
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
        let id = state.register_worker("exp-worker".into(), "role".into(), "desc".into(), vec![]);
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
        let serve_handle = tokio::spawn(async move { serve(&root, "team-test").await });
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
        let serve_handle = tokio::spawn(async move { serve(&root, "hb-test").await });
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
        let serve_handle = tokio::spawn(async move { serve(&root, "hb-err-test").await });
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
        let worker_id = state.register_worker("recv".into(), "coder".into(), "desc".into(), vec![]);

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
        let worker_id =
            state.register_worker("expiring".into(), "role".into(), "desc".into(), vec![]);
        state.workers.get_mut(&worker_id).unwrap().expires_at = 0;

        let result = state.send_message(worker_id, "hello".into(), None);
        assert!(result.is_none(), "sending to expired worker should fail");
    }

    #[test]
    fn test_send_message_unique_ids() {
        let mut state = BrokerState::new();
        let worker_id = state.register_worker("recv".into(), "coder".into(), "desc".into(), vec![]);

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
        let worker_id = state.register_worker("recv".into(), "coder".into(), "desc".into(), vec![]);

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
        let serve_handle = tokio::spawn(async move { serve(&root, "send-test").await });
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
        let serve_handle = tokio::spawn(async move { serve(&root, "send-err-test").await });
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

    #[tokio::test]
    async fn test_send_multiple_messages_via_broker() {
        let tmp = TempDir::new().unwrap();
        let project_root = tmp.path().to_path_buf();
        let cell_id = "send-multi-test";

        let root = project_root.clone();
        let serve_handle = tokio::spawn(async move { serve(&root, "send-multi-test").await });
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
