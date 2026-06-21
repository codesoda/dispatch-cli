/// Create a minimal ResolvedConfig for testing.
fn test_config(project_root: &Path, cell_id: &str) -> ResolvedConfig {
    ResolvedConfig {
        name: None,
        cell_id: cell_id.to_string(),
        backend: None,
        project_root: project_root.to_path_buf(),
        config_file_path: None,
        agent_cwd: project_root.to_path_buf(),
        monitor_port: None,
        monitor_open: false,
        default_ttl: None,
        stopping_drain_secs: None,
        continue_instruction: None,
        log_prompt_bodies: false,
        agents: vec![],
        heartbeats: vec![],
    }
}

#[tokio::test]
async fn test_client_broker_not_running() {
    let tmp = TempDir::new().unwrap();
    let config = test_config(tmp.path(), "nonexistent-cell");
    let backend = LocalBackend::new(&config, None);

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

    let root = project_root.clone();
    let serve_handle = tokio::spawn(async move { serve(&test_config(&root, cell_id), None).await });

    // Wait for broker to start.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let cfg = test_config(&project_root, cell_id);
    let backend = LocalBackend::new(&cfg, None);
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
    std::fs::write(&sock, "").unwrap();
    let result = check_no_existing_broker(&sock, "test-cell").await;
    assert!(result.is_ok());
    assert!(!sock.exists(), "stale socket should be removed");
}

#[tokio::test]
async fn test_check_no_existing_broker_active_broker() {
    let tmp = TempDir::new().unwrap();
    let sock = tmp.path().join("active.sock");

    let listener = UnixListener::bind(&sock).unwrap();

    // Spawn a task to accept one connection so the connect test works.
    let sock_clone = sock.clone();
    let accept_handle = tokio::spawn(async move {
        let _ = listener.accept().await;
        // Keep listener alive until we drop it.
        drop(listener);
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

    let root = project_root.clone();
    let serve_handle = tokio::spawn(async move { serve(&test_config(&root, cell_id), None).await });

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

    let listener = UnixListener::bind(&sock).unwrap();
    let accept_handle = tokio::spawn(async move {
        let _ = listener.accept().await;
    });

    // Try to start second broker — should fail.
    let result = serve(&test_config(&project_root, cell_id), None).await;
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

    std::fs::create_dir_all(sock.parent().unwrap()).unwrap();
    // Remove any leftover real socket from a previous run before
    // creating a fake stale file.
    let _ = std::fs::remove_file(&sock);
    std::fs::write(&sock, "stale").unwrap();

    // Starting serve should clean up the stale socket and bind fresh.
    let root = project_root.clone();
    let serve_handle = tokio::spawn(async move { serve(&test_config(&root, cell_id), None).await });

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

/// Issue #43: when an id is supplied, the broker uses it verbatim.
#[test]
fn test_register_worker_uses_supplied_id() {
    let mut state = BrokerState::new();
    let id = state
        .register_worker(
            "alice".into(),
            "test-runner".into(),
            "desc".into(),
            vec![],
            None,
            false,
            Some("w-fixed".into()),
            None,
        )
        .expect("register");
    assert_eq!(id, "w-fixed");
    assert!(state.workers.contains_key("w-fixed"));
}

/// Issue #43: re-registering an existing id with the same name+role is an
/// idempotent claim — same id returned, no duplicate worker, TTL renewed.
#[test]
fn test_register_worker_idempotent_claim_renews_ttl() {
    let mut state = BrokerState::new();
    let id = state
        .register_worker(
            "alice".into(),
            "test-runner".into(),
            "desc".into(),
            vec![],
            Some(60),
            false,
            Some("w-fixed".into()),
            None,
        )
        .expect("first register");
    // Force the expiry into the past so we can detect the renewal.
    state.workers.get_mut(&id).unwrap().expires_at = 0;

    let id2 = state
        .register_worker(
            "alice".into(),
            "test-runner".into(),
            "desc".into(),
            vec![],
            Some(60),
            false,
            Some("w-fixed".into()),
            None,
        )
        .expect("idempotent claim");
    assert_eq!(id, id2);
    assert_eq!(state.workers.len(), 1, "no duplicate worker created");
    assert!(
        state.workers.get(&id).unwrap().expires_at > 0,
        "claim should renew TTL",
    );
}

/// Issue #43: an idempotent claim with updated `description` /
/// `capabilities` must refresh the stored worker record so `dispatch
/// team` reflects current config — stale metadata from the initial
/// pre-register otherwise lingers. Empty capabilities (agent-side
/// claim shape) must NOT clobber the stored list.
#[test]
fn test_register_worker_idempotent_claim_updates_metadata() {
    let mut state = BrokerState::new();
    let id = state
        .register_worker(
            "alice".into(),
            "test-runner".into(),
            "original description".into(),
            vec!["cap-a".into(), "cap-b".into()],
            None,
            false,
            Some("w-fixed".into()),
            None,
        )
        .expect("first register");

    // Re-register with a new description and fresh capabilities —
    // both must flow through to the stored worker.
    state
        .register_worker(
            "alice".into(),
            "test-runner".into(),
            "updated description".into(),
            vec!["cap-c".into()],
            None,
            false,
            Some("w-fixed".into()),
            None,
        )
        .expect("claim with updated metadata");
    let w = state.workers.get(&id).unwrap();
    assert_eq!(w.description, "updated description");
    assert_eq!(w.capabilities, vec!["cap-c".to_string()]);

    // Agent-style claim with empty capabilities must preserve the
    // existing list rather than blanking it.
    state
        .register_worker(
            "alice".into(),
            "test-runner".into(),
            "third description".into(),
            vec![],
            None,
            false,
            Some("w-fixed".into()),
            None,
        )
        .expect("agent-style claim");
    let w = state.workers.get(&id).unwrap();
    assert_eq!(w.description, "third description");
    assert_eq!(
        w.capabilities,
        vec!["cap-c".to_string()],
        "empty capabilities must not wipe stored list",
    );
}

/// Issue #43: re-registering an existing id with a *different* name or
/// role is rejected — silent overwriting would mask config drift.
#[test]
fn test_register_worker_collision_rejected() {
    let mut state = BrokerState::new();
    state
        .register_worker(
            "alice".into(),
            "test-runner".into(),
            "desc".into(),
            vec![],
            None,
            false,
            Some("w-fixed".into()),
            None,
        )
        .expect("first register");
    let err = state
        .register_worker(
            "bob".into(),
            "reviewer".into(),
            "desc".into(),
            vec![],
            None,
            false,
            Some("w-fixed".into()),
            None,
        )
        .expect_err("collision must be rejected");
    assert!(
        matches!(
            err,
            BrokerError::WorkerIdCollision { ref supplied, ref existing_name, .. }
                if supplied == "w-fixed" && existing_name == "alice"
        ),
        "expected WorkerIdCollision for w-fixed/alice, got: {err:?}"
    );
    let msg = err.to_string();
    assert!(
        msg.contains("w-fixed"),
        "error display should name the colliding id: {msg}"
    );
    assert!(
        msg.contains("alice"),
        "error display should name the existing worker: {msg}"
    );
    // The original worker is still there, untouched.
    assert_eq!(state.workers.len(), 1);
    assert_eq!(state.workers.get("w-fixed").unwrap().name, "alice");
}

/// Issue #43: a fresh registration with `role_prompt` stores the prompt
/// keyed by worker id; subsequent claims with `role_prompt: None` see
/// the stored prompt back via `role_prompts`.
#[test]
fn test_register_worker_stores_and_returns_role_prompt() {
    let mut state = BrokerState::new();
    let id = state
        .register_worker(
            "alice".into(),
            "test-runner".into(),
            "desc".into(),
            vec![],
            None,
            false,
            Some("w-fixed".into()),
            Some("Run: dispatch listen --timeout 270".into()),
        )
        .expect("pre-register");
    assert_eq!(
        state.role_prompts.get(&id).map(String::as_str),
        Some("Run: dispatch listen --timeout 270"),
    );

    // Agent claim with role_prompt=None must NOT erase the stored prompt.
    let claimed = state
        .register_worker(
            "alice".into(),
            "test-runner".into(),
            "desc".into(),
            vec![],
            None,
            false,
            Some("w-fixed".into()),
            None,
        )
        .expect("claim");
    assert_eq!(claimed, id);
    assert_eq!(
        state.role_prompts.get(&id).map(String::as_str),
        Some("Run: dispatch listen --timeout 270"),
        "claim must not erase the stored prompt",
    );
}

/// A pre-registered worker (caller supplied `worker_id`) is born with
/// `claimed = false` so the monitor can show it as "reserved, waiting
/// for the agent" rather than solid green. When the agent then
/// registers with the same id (idempotent-claim path) OR sends a
/// heartbeat, `claimed` flips to true. Self-registered workers
/// (no supplied id) skip the reserved state entirely — they're
/// always a real process registering itself.
#[test]
fn test_register_worker_claimed_flips_on_attach() {
    let mut state = BrokerState::new();

    // Pre-register via supplied id — should start unclaimed.
    let pre = state
        .register_worker(
            "alice".into(),
            "runner".into(),
            "d".into(),
            vec![],
            None,
            false,
            Some("w-fixed".into()),
            Some("prompt".into()),
        )
        .expect("pre-register");
    assert!(
        !state.workers.get(&pre).unwrap().claimed,
        "pre-register must leave worker unclaimed",
    );

    // Agent-side claim (supplied id matches existing record).
    let claimed = state
        .register_worker(
            "alice".into(),
            "runner".into(),
            "d".into(),
            vec![],
            None,
            false,
            Some("w-fixed".into()),
            None,
        )
        .expect("claim");
    assert_eq!(claimed, pre);
    assert!(
        state.workers.get(&pre).unwrap().claimed,
        "idempotent claim must flip claimed = true",
    );

    // Self-register (no supplied id) should be claimed immediately.
    let self_reg = state
        .register_worker(
            "bob".into(),
            "worker".into(),
            "d".into(),
            vec![],
            None,
            false,
            None,
            None,
        )
        .expect("self-register");
    assert!(
        state.workers.get(&self_reg).unwrap().claimed,
        "self-register (no supplied id) must be claimed immediately",
    );

    // Heartbeat alone is also proof of life — flips claimed on a
    // pre-registered worker even without a register-claim step.
    let hb_pre = state
        .register_worker(
            "carol".into(),
            "worker".into(),
            "d".into(),
            vec![],
            None,
            false,
            Some("w-carol".into()),
            None,
        )
        .expect("pre-register carol");
    assert!(!state.workers.get(&hb_pre).unwrap().claimed);
    state
        .heartbeat_worker(&hb_pre, None)
        .expect("heartbeat on pre-registered worker");
    assert!(
        state.workers.get(&hb_pre).unwrap().claimed,
        "heartbeat must flip claimed = true",
    );
}

/// Issue #43: evicting a worker (via `evict=true` or TTL expiry) drops
/// its stored prompt too — no stale prompts left behind.
#[test]
fn test_register_worker_evict_clears_role_prompt() {
    let mut state = BrokerState::new();
    let first = state
        .register_worker(
            "alice".into(),
            "test-runner".into(),
            "desc".into(),
            vec![],
            None,
            false,
            None,
            Some("first prompt".into()),
        )
        .expect("first");
    // Evict and re-register with a different id. Old prompt must be gone.
    let second = state
        .register_worker(
            "alice".into(),
            "test-runner".into(),
            "desc".into(),
            vec![],
            None,
            true,
            None,
            Some("second prompt".into()),
        )
        .expect("evict + re-register");
    assert_ne!(first, second);
    assert!(
        !state.role_prompts.contains_key(&first),
        "evicted worker's prompt must be cleared",
    );
    assert_eq!(
        state.role_prompts.get(&second).map(String::as_str),
        Some("second prompt"),
    );
}

/// Idempotent claim runs BEFORE evict, so a same-name evict cannot wipe a
/// pre-registered worker that an agent is about to claim.
#[test]
fn test_register_worker_claim_beats_evict() {
    let mut state = BrokerState::new();
    let id = state
        .register_worker(
            "alice".into(),
            "test-runner".into(),
            "desc".into(),
            vec![],
            None,
            false,
            Some("w-fixed".into()),
            None,
        )
        .expect("pre-register");
    let claimed = state
        .register_worker(
            "alice".into(),
            "test-runner".into(),
            "desc".into(),
            vec![],
            None,
            true, // evict
            Some("w-fixed".into()),
            None,
        )
        .expect("claim should win over evict");
    assert_eq!(id, claimed);
    assert_eq!(state.workers.len(), 1);
}

#[test]
fn test_register_worker_returns_unique_ids() {
    let mut state = BrokerState::new();
    let id1 = state
        .register_worker(
            "worker-a".into(),
            "planner".into(),
            "Plans things".into(),
            vec!["plan:create plans".into()],
            None,
            false,
            None,
            None,
        )
        .expect("register");
    let id2 = state
        .register_worker(
            "worker-b".into(),
            "coder".into(),
            "Writes code".into(),
            vec![],
            None,
            false,
            None,
            None,
        )
        .expect("register");
    assert_ne!(id1, id2, "each registration must produce a unique ID");
    assert_eq!(state.workers.len(), 2);
}

#[test]
fn test_register_worker_stores_fields() {
    let mut state = BrokerState::new();
    let id = state
        .register_worker(
            "my-worker".into(),
            "reviewer".into(),
            "Reviews pull requests".into(),
            vec!["review:code".into(), "review:docs".into()],
            None,
            false,
            None,
            None,
        )
        .expect("register");
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
    let id = state
        .register_worker(
            "ttl-worker".into(),
            "role".into(),
            "desc".into(),
            vec![],
            None,
            false,
            None,
            None,
        )
        .expect("register");
    let worker = state.workers.get(&id).unwrap();
    // Should expire roughly DEFAULT_WORKER_TTL_SECS from now.
    assert!(worker.expires_at >= now + DEFAULT_WORKER_TTL_SECS - 1);
    assert!(worker.expires_at <= now + DEFAULT_WORKER_TTL_SECS + 1);
}

#[test]
fn test_evict_expired_workers() {
    let mut state = BrokerState::new();
    let id = state
        .register_worker(
            "soon-expired".into(),
            "role".into(),
            "desc".into(),
            vec![],
            None,
            false,
            None,
            None,
        )
        .expect("register");
    // Manually set expiry to the past.
    state.workers.get_mut(&id).unwrap().expires_at = 0;
    state.evict_expired();
    assert!(state.workers.is_empty(), "expired worker should be evicted");
}

/// Helper: register a plain active worker and return its id.
fn register_active(state: &mut BrokerState, name: &str) -> String {
    state
        .register_worker(
            name.into(),
            "role".into(),
            "desc".into(),
            vec![],
            None,
            false,
            None,
            None,
        )
        .expect("register")
}

/// Freshly registered workers are `Active` with no `stopping_since`.
#[test]
fn register_defaults_to_active_control_state() {
    let mut state = BrokerState::new();
    let id = register_active(&mut state, "alice");
    let w = state.workers.get(&id).unwrap();
    assert_eq!(w.control_state, ControlState::Active);
    assert!(w.stopping_since.is_none());
}

/// `set_control_state` stamps `stopping_since` entering `Stopping` and
/// clears it on the way back out; unknown ids return false.
#[test]
fn set_control_state_manages_stopping_since() {
    let mut state = BrokerState::new();
    let id = register_active(&mut state, "alice");

    assert!(state.set_control_state(&id, ControlState::Stopping));
    let w = state.workers.get(&id).unwrap();
    assert_eq!(w.control_state, ControlState::Stopping);
    assert!(
        w.stopping_since.is_some(),
        "entering Stopping stamps the clock"
    );

    // Re-asserting Stopping must not reset the original stamp.
    let stamp = state.workers.get(&id).unwrap().stopping_since;
    assert!(state.set_control_state(&id, ControlState::Stopping));
    assert_eq!(state.workers.get(&id).unwrap().stopping_since, stamp);

    // Leaving Stopping clears the stamp.
    assert!(state.set_control_state(&id, ControlState::Active));
    let w = state.workers.get(&id).unwrap();
    assert_eq!(w.control_state, ControlState::Active);
    assert!(w.stopping_since.is_none());

    assert!(!state.set_control_state("nonexistent", ControlState::Stopping));
}

/// Control state is orthogonal to TTL: a `stopping` worker is finalized
/// only once its drain window elapses, with reason `StopDrained`. A
/// freshly-stopping worker (still inside the window) is retained.
#[test]
fn evict_finalizes_drained_stopping_worker() {
    let mut state = BrokerState::new();
    state.stopping_drain_secs = 10;
    let id = register_active(&mut state, "alice");
    state.set_control_state(&id, ControlState::Stopping);

    // Still inside the drain window → retained, no removal reported.
    let removed = state.evict_expired();
    assert!(
        removed.is_empty(),
        "worker within drain window must survive"
    );
    assert!(state.workers.contains_key(&id));

    // Push the stop time past the window → finalized as StopDrained.
    state.workers.get_mut(&id).unwrap().stopping_since = Some(now_secs() - 11);
    let removed = state.evict_expired();
    assert_eq!(removed.len(), 1);
    assert_eq!(removed[0].0, id);
    assert_eq!(removed[0].2, EvictReason::StopDrained);
    assert!(
        state.workers.is_empty(),
        "drained stopping worker must be removed"
    );
}

/// TTL expiry of an active worker reports `TtlExpired`, distinct from the
/// drain path — so callers emit `expire` vs `lifecycle` correctly.
#[test]
fn evict_reports_ttl_expiry_reason() {
    let mut state = BrokerState::new();
    let id = register_active(&mut state, "alice");
    state.workers.get_mut(&id).unwrap().expires_at = 0;
    let removed = state.evict_expired();
    assert_eq!(removed.len(), 1);
    assert_eq!(removed[0].2, EvictReason::TtlExpired);
}

/// A claim (idempotent re-register) of a `stopping` worker must NOT revive
/// it to `active` — only the coordinator owns that transition.
#[test]
fn claim_does_not_revive_stopping_worker() {
    let mut state = BrokerState::new();
    let id = state
        .register_worker(
            "alice".into(),
            "runner".into(),
            "desc".into(),
            vec![],
            None,
            false,
            Some("w-fixed".into()),
            None,
        )
        .expect("pre-register");
    state.set_control_state(&id, ControlState::Stopping);

    // The agent process re-registers (claims) its id.
    state
        .register_worker(
            "alice".into(),
            "runner".into(),
            "desc".into(),
            vec![],
            None,
            false,
            Some("w-fixed".into()),
            None,
        )
        .expect("claim");
    assert_eq!(
        state.workers.get(&id).unwrap().control_state,
        ControlState::Stopping,
        "claim must not silently revive a stopping worker",
    );
}

/// `get_status` surfaces the worker's control state so the stop hook /
/// listen renderer can read it via a `Status` query.
#[test]
fn get_status_exposes_control_state() {
    let mut state = BrokerState::new();
    let id = register_active(&mut state, "alice");
    state.set_control_state(&id, ControlState::Stopping);
    let statuses = state.get_status(Some(&id));
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].control_state, ControlState::Stopping);
}

/// `set_control_state_by_name` marks every worker registered under a name
/// (and only those) `stopping`, returning the affected ids — the broker-side
/// primitive the coordinator's `agent stop`/`restart` calls before the kill.
#[test]
fn set_control_state_by_name_marks_matching_workers() {
    let mut state = BrokerState::new();
    let alice = register_active(&mut state, "alice");
    let bob = register_active(&mut state, "bob");

    let affected = state.set_control_state_by_name("alice", ControlState::Stopping);
    assert_eq!(affected, vec![alice.clone()]);
    let w = state.workers.get(&alice).unwrap();
    assert_eq!(w.control_state, ControlState::Stopping);
    assert!(w.stopping_since.is_some(), "marks the drain clock");

    // A worker under a different name is left untouched.
    assert_eq!(
        state.workers.get(&bob).unwrap().control_state,
        ControlState::Active,
    );

    // No worker by that name → empty result, no panic.
    assert!(state
        .set_control_state_by_name("nobody", ControlState::Stopping)
        .is_empty());
}

/// `mark_worker_stopping` honors its `StopTarget`: by worker id it marks only
/// that replica (same-named siblings keep running); by name it marks every
/// worker under the name. Lets a coordinator stop one replica without taking
/// the others down with it.
#[tokio::test]
async fn mark_worker_stopping_targets_id_or_name() {
    let state = Arc::new(Mutex::new(BrokerState::new()));
    let (event_tx, _rx) = broadcast::channel::<BrokerEvent>(16);
    let (twin_a, twin_b) = {
        let mut s = state.lock().await;
        (
            register_active(&mut s, "twin"),
            register_active(&mut s, "twin"),
        )
    };

    // By id: only twin_a flips to stopping.
    mark_worker_stopping(&state, &event_tx, &StopTarget::Worker(twin_a.clone())).await;
    {
        let s = state.lock().await;
        assert_eq!(
            s.workers.get(&twin_a).unwrap().control_state,
            ControlState::Stopping,
        );
        assert_eq!(
            s.workers.get(&twin_b).unwrap().control_state,
            ControlState::Active,
            "a same-named sibling must keep running",
        );
    }

    // By name: every worker under "twin" flips to stopping.
    mark_worker_stopping(&state, &event_tx, &StopTarget::Name("twin".into())).await;
    {
        let s = state.lock().await;
        assert_eq!(
            s.workers.get(&twin_b).unwrap().control_state,
            ControlState::Stopping,
        );
    }
}

