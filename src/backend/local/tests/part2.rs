#[tokio::test]
async fn test_register_via_broker_returns_worker_id() {
    let tmp = TempDir::new().unwrap();
    let project_root = tmp.path().to_path_buf();
    let cell_id = "reg-test";

    let root = project_root.clone();
    let serve_handle = tokio::spawn(async move { serve(&test_config(&root, cell_id), None).await });
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
    let serve_handle = tokio::spawn(async move { serve(&test_config(&root, cell_id), None).await });
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
    let active_id = state
        .register_worker(
            "active".into(),
            "coder".into(),
            "desc".into(),
            vec![],
            None,
            false,
            None,
            None,
        )
        .expect("register");
    let expired_id = state
        .register_worker(
            "expired".into(),
            "coder".into(),
            "desc".into(),
            vec![],
            None,
            false,
            None,
            None,
        )
        .expect("register");
    // Expire one worker.
    state.workers.get_mut(&expired_id).unwrap().expires_at = 0;

    let workers = state.list_workers();
    assert_eq!(workers.len(), 1);
    assert_eq!(workers[0].id, active_id);
}

#[test]
fn test_heartbeat_worker_renews_ttl() {
    let mut state = BrokerState::new();
    let id = state
        .register_worker(
            "hb-worker".into(),
            "role".into(),
            "desc".into(),
            vec![],
            None,
            false,
            None,
            None,
        )
        .expect("register");
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
    let id = state
        .register_worker(
            "exp-worker".into(),
            "role".into(),
            "desc".into(),
            vec![],
            None,
            false,
            None,
            None,
        )
        .expect("register");
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
    let id = state
        .register_worker(
            "hist".into(),
            "role".into(),
            "desc".into(),
            vec![],
            None,
            false,
            None,
            None,
        )
        .expect("register");
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
    let id = state
        .register_worker(
            "dedup".into(),
            "role".into(),
            "desc".into(),
            vec![],
            None,
            false,
            None,
            None,
        )
        .expect("register");
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
    let id = state
        .register_worker(
            "clr".into(),
            "role".into(),
            "desc".into(),
            vec![],
            None,
            false,
            None,
            None,
        )
        .expect("register");
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
    let serve_handle = tokio::spawn(async move { serve(&test_config(&root, cell_id), None).await });
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
    let serve_handle = tokio::spawn(async move { serve(&test_config(&root, cell_id), None).await });
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
    let serve_handle = tokio::spawn(async move { serve(&test_config(&root, cell_id), None).await });
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

