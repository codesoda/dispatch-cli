#[test]
fn test_send_message_queues_in_mailbox() {
    let mut state = BrokerState::new();
    let worker_id = state
        .register_worker(
            "recv".into(),
            "coder".into(),
            "desc".into(),
            vec![],
            None,
            false,
            None,
            None,
        )
        .expect("register");

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
    let worker_id = state
        .register_worker(
            "expiring".into(),
            "role".into(),
            "desc".into(),
            vec![],
            None,
            false,
            None,
            None,
        )
        .expect("register");
    state.workers.get_mut(&worker_id).unwrap().expires_at = 0;

    let result = state.send_message(worker_id, "hello".into(), None);
    assert!(result.is_none(), "sending to expired worker should fail");
}

#[test]
fn test_send_message_unique_ids() {
    let mut state = BrokerState::new();
    let worker_id = state
        .register_worker(
            "recv".into(),
            "coder".into(),
            "desc".into(),
            vec![],
            None,
            false,
            None,
            None,
        )
        .expect("register");

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
    let worker_id = state
        .register_worker(
            "recv".into(),
            "coder".into(),
            "desc".into(),
            vec![],
            None,
            false,
            None,
            None,
        )
        .expect("register");

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
    let serve_handle = tokio::spawn(async move { serve(&test_config(&root, cell_id), None).await });
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
    let serve_handle = tokio::spawn(async move { serve(&test_config(&root, cell_id), None).await });
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

#[test]
fn test_pop_message_returns_fifo_order() {
    let mut state = BrokerState::new();
    let worker_id = state
        .register_worker(
            "recv".into(),
            "coder".into(),
            "desc".into(),
            vec![],
            None,
            false,
            None,
            None,
        )
        .expect("register");
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
    let worker_id = state
        .register_worker(
            "recv".into(),
            "coder".into(),
            "desc".into(),
            vec![],
            None,
            false,
            None,
            None,
        )
        .expect("register");
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
    let worker_id = state
        .register_worker(
            "recv".into(),
            "coder".into(),
            "desc".into(),
            vec![],
            None,
            false,
            None,
            None,
        )
        .expect("register");
    let n1 = state.get_notifier(&worker_id);
    let n2 = state.get_notifier(&worker_id);
    assert!(Arc::ptr_eq(&n1, &n2), "same worker should get same Notify");
}

#[test]
fn test_ack_message_rejects_unknown_worker() {
    let mut state = BrokerState::new();
    let err = state
        .ack_message("missing-worker", "msg-1", None, None, None, vec![])
        .unwrap_err();
    assert!(err.contains("worker not found"), "got: {err}");
}

#[test]
fn test_ack_message_rejects_unknown_message() {
    let mut state = BrokerState::new();
    let worker_id = state
        .register_worker(
            "recv".into(),
            "coder".into(),
            "desc".into(),
            vec![],
            None,
            false,
            None,
            None,
        )
        .expect("register");
    let err = state
        .ack_message(&worker_id, "nonexistent-message", None, None, None, vec![])
        .unwrap_err();
    assert!(err.contains("message not found"), "got: {err}");
}

#[test]
fn test_ack_message_rejects_wrong_recipient() {
    let mut state = BrokerState::new();
    let alice = state
        .register_worker(
            "alice".into(),
            "r".into(),
            "d".into(),
            vec![],
            None,
            false,
            None,
            None,
        )
        .expect("register");
    let bob = state
        .register_worker(
            "bob".into(),
            "r".into(),
            "d".into(),
            vec![],
            None,
            false,
            None,
            None,
        )
        .expect("register");
    let message_id = state
        .send_message(alice.clone(), "for alice".into(), None)
        .expect("send");
    let err = state
        .ack_message(&bob, &message_id, None, None, None, vec![])
        .unwrap_err();
    assert!(
        err.contains("not addressed to worker"),
        "expected recipient-mismatch error, got: {err}"
    );
}

#[test]
fn test_ack_message_success_updates_history() {
    let mut state = BrokerState::new();
    let alice = state
        .register_worker(
            "alice".into(),
            "r".into(),
            "d".into(),
            vec![],
            None,
            false,
            None,
            None,
        )
        .expect("register");
    let message_id = state
        .send_message(alice.clone(), "hello".into(), None)
        .expect("send");
    state
        .ack_message(
            &alice,
            &message_id,
            Some("noted".into()),
            None,
            None,
            vec![],
        )
        .expect("ack should succeed");
    let hist = state
        .message_history
        .iter()
        .find(|m| m.message_id == message_id)
        .expect("message in history");
    assert!(hist.acked_at.is_some(), "acked_at should be set");
    assert!(state.ack_log.contains_key(&message_id));
}

/// `ack_message` carrying completion fields (the `result`
/// super-ack) records them on the `AckRecord` and still marks the message
/// acked — no prior plain ack required.
#[test]
fn ack_message_with_status_records_result() {
    let mut state = BrokerState::new();
    let alice = register_active(&mut state, "alice");
    let message_id = state
        .send_message(alice.clone(), "do the thing".into(), None)
        .expect("send");
    state
        .ack_message(
            &alice,
            &message_id,
            None,
            Some("done".into()),
            Some("did the thing".into()),
            vec!["out/report.md".into()],
        )
        .expect("result should succeed");

    let rec = state.ack_log.get(&message_id).expect("ack record");
    assert_eq!(rec.status.as_deref(), Some("done"));
    assert_eq!(rec.summary.as_deref(), Some("did the thing"));
    assert_eq!(rec.artifacts, vec!["out/report.md".to_string()]);

    let hist = state
        .message_history
        .iter()
        .find(|m| m.message_id == message_id)
        .expect("message in history");
    assert!(hist.acked_at.is_some(), "result must ack the message");
}

/// `body_fingerprint` is deterministic, returns a 16-hex-char hash
/// plus the byte length, and distinguishes different bodies.
#[test]
fn body_fingerprint_is_stable_and_sized() {
    let (h1, n1) = body_fingerprint("hello world");
    let (h2, n2) = body_fingerprint("hello world");
    assert_eq!(h1, h2, "same body must hash identically");
    assert_eq!(n1, 11);
    assert_eq!(n2, 11);
    assert_eq!(h1.len(), 16, "hash is 16 hex chars");
    let (h3, _) = body_fingerprint("a different body");
    assert_ne!(h1, h3, "different bodies must differ");
}

#[tokio::test]
async fn test_listen_immediate_delivery_via_broker() {
    let tmp = TempDir::new().unwrap();
    let project_root = tmp.path().to_path_buf();
    let cell_id = "listen-imm-test";

    let root = project_root.clone();
    let serve_handle = tokio::spawn(async move { serve(&test_config(&root, cell_id), None).await });
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
    let serve_handle = tokio::spawn(async move { serve(&test_config(&root, cell_id), None).await });
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
    let serve_handle = tokio::spawn(async move { serve(&test_config(&root, cell_id), None).await });
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
    let serve_handle = tokio::spawn(async move { serve(&test_config(&root, cell_id), None).await });
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
    let serve_handle = tokio::spawn(async move { serve(&test_config(&root, cell_id), None).await });
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
    let serve_handle = tokio::spawn(async move { serve(&test_config(&root, cell_id), None).await });
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
    let serve_handle = tokio::spawn(async move { serve(&test_config(&root, cell_id), None).await });
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
