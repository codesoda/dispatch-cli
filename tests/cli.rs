use std::process::{Child, Command};
use std::thread;
use std::time::Duration;

use assert_cmd::cargo::CommandCargoExt;
use tempfile::TempDir;

/// Start a `dispatch serve` broker as a background process in the given
/// temp directory with the given cell ID. Returns the child process handle.
fn start_broker(dir: &TempDir, cell_id: &str) -> Child {
    // Remove any stale socket from a previous test run so the broker
    // can bind cleanly.
    let socket =
        std::path::PathBuf::from("/tmp/dispatch-cli/sockets").join(format!("{cell_id}.sock"));
    let _ = std::fs::remove_file(&socket);

    let mut child = Command::cargo_bin("dispatch")
        .unwrap()
        .arg("--cell-id")
        .arg(cell_id)
        .arg("serve")
        .current_dir(dir.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("failed to start broker");
    for _ in 0..50 {
        if socket.exists() {
            return child;
        }
        thread::sleep(Duration::from_millis(50));
    }
    child.kill().ok();
    child.wait().ok();
    panic!("broker socket did not appear within 2.5s");
}

/// Build an `assert_cmd::Command` for `dispatch` that runs in the given
/// temp directory with the given cell ID.
fn dispatch_cmd(dir: &TempDir, cell_id: &str) -> assert_cmd::Command {
    let mut cmd = assert_cmd::Command::cargo_bin("dispatch").unwrap();
    cmd.current_dir(dir.path()).arg("--cell-id").arg(cell_id);
    cmd
}

// ── Help & arg-parsing tests ──────────────────────────────────────────

#[test]
fn help_exits_zero_and_shows_usage() {
    let mut cmd = assert_cmd::Command::cargo_bin("dispatch").unwrap();
    cmd.arg("--help")
        .assert()
        .success()
        .stdout(predicates::str::contains("Usage"))
        .stdout(predicates::str::contains("dispatch"));
}

#[test]
fn no_args_exits_non_zero() {
    let mut cmd = assert_cmd::Command::cargo_bin("dispatch").unwrap();
    cmd.assert().failure();
}

// ── Broker lifecycle tests ────────────────────────────────────────────

#[test]
fn serve_creates_socket_file() {
    let dir = TempDir::new().unwrap();
    let cell_id = "test-serve-socket";

    let mut broker = start_broker(&dir, cell_id);
    let socket =
        std::path::PathBuf::from("/tmp/dispatch-cli/sockets").join(format!("{cell_id}.sock"));
    assert!(
        socket.exists(),
        "socket file should exist while broker runs"
    );

    broker.kill().ok();
    broker.wait().ok();
}

// ── Register + Team round-trip ────────────────────────────────────────

#[test]
fn register_returns_worker_id() {
    let dir = TempDir::new().unwrap();
    let cell_id = "test-register";
    let mut broker = start_broker(&dir, cell_id);

    let output = dispatch_cmd(&dir, cell_id)
        .args([
            "register",
            "--name",
            "alice",
            "--role",
            "builder",
            "--description",
            "test worker",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: serde_json::Value =
        serde_json::from_slice(&output).expect("stdout should be valid JSON");
    assert_eq!(json["status"], "ok");
    assert!(
        json["worker_id"].is_string(),
        "response should contain worker_id"
    );

    broker.kill().ok();
    broker.wait().ok();
}

#[test]
fn team_lists_registered_workers() {
    let dir = TempDir::new().unwrap();
    let cell_id = "test-team";
    let mut broker = start_broker(&dir, cell_id);

    // Register a worker first.
    dispatch_cmd(&dir, cell_id)
        .args([
            "register",
            "--name",
            "bob",
            "--role",
            "tester",
            "--description",
            "test worker",
            "--capability",
            "rust",
        ])
        .assert()
        .success();

    // Team should list the worker.
    let output = dispatch_cmd(&dir, cell_id)
        .arg("team")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: serde_json::Value =
        serde_json::from_slice(&output).expect("stdout should be valid JSON");
    assert_eq!(json["status"], "ok");
    let workers = json["workers"]
        .as_array()
        .expect("workers should be an array");
    assert_eq!(workers.len(), 1);
    assert_eq!(workers[0]["name"], "bob");
    assert_eq!(workers[0]["role"], "tester");
    assert_eq!(workers[0]["capabilities"][0], "rust");

    broker.kill().ok();
    broker.wait().ok();
}

// ── Send + Listen round-trip ──────────────────────────────────────────

#[test]
fn send_and_listen_round_trip() {
    let dir = TempDir::new().unwrap();
    let cell_id = "test-send-listen";
    let mut broker = start_broker(&dir, cell_id);

    // Register a worker.
    let reg_out = dispatch_cmd(&dir, cell_id)
        .args([
            "register",
            "--name",
            "carol",
            "--role",
            "runner",
            "--description",
            "receives messages",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let reg: serde_json::Value = serde_json::from_slice(&reg_out).unwrap();
    let worker_id = reg["worker_id"].as_str().unwrap().to_string();

    // Send a message to the worker.
    let send_out = dispatch_cmd(&dir, cell_id)
        .args([
            "send",
            "--to",
            &worker_id,
            "--body",
            "hello from test",
            "--from",
            "test-harness",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let send_json: serde_json::Value = serde_json::from_slice(&send_out).unwrap();
    assert_eq!(send_json["status"], "ok");
    assert!(send_json["message_id"].is_string());

    // Listen should return the message immediately.
    let listen_out = dispatch_cmd(&dir, cell_id)
        .args(["listen", "--worker-id", &worker_id, "--timeout", "5"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let listen_json: serde_json::Value = serde_json::from_slice(&listen_out).unwrap();
    assert_eq!(listen_json["status"], "ok");
    assert_eq!(listen_json["body"], "hello from test");
    assert_eq!(listen_json["from"], "test-harness");
    assert_eq!(listen_json["to"], worker_id);

    broker.kill().ok();
    broker.wait().ok();
}

// ── Error cases ───────────────────────────────────────────────────────

#[test]
fn send_to_invalid_worker_returns_error_response() {
    let dir = TempDir::new().unwrap();
    let cell_id = "test-send-invalid";
    let mut broker = start_broker(&dir, cell_id);

    // The broker returns a JSON error response (exit 0) when the
    // recipient worker does not exist. The CLI only exits non-zero
    // for transport-level failures, not broker-level errors.
    let output = dispatch_cmd(&dir, cell_id)
        .args([
            "send",
            "--to",
            "nonexistent-worker",
            "--body",
            "should fail",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: serde_json::Value =
        serde_json::from_slice(&output).expect("stdout should be valid JSON");
    assert_eq!(json["status"], "error");
    assert!(
        json["message"]
            .as_str()
            .unwrap()
            .contains("nonexistent-worker"),
        "error message should mention the missing worker"
    );

    broker.kill().ok();
    broker.wait().ok();
}

// ── Heartbeat ─────────────────────────────────────────────────────────

#[test]
fn heartbeat_renews_worker_ttl() {
    let dir = TempDir::new().unwrap();
    let cell_id = "test-heartbeat";
    let mut broker = start_broker(&dir, cell_id);

    // Register a worker.
    let reg_out = dispatch_cmd(&dir, cell_id)
        .args([
            "register",
            "--name",
            "dave",
            "--role",
            "worker",
            "--description",
            "heartbeat test",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let reg: serde_json::Value = serde_json::from_slice(&reg_out).unwrap();
    let worker_id = reg["worker_id"].as_str().unwrap().to_string();

    // Heartbeat should succeed and return an updated expires_at.
    let hb_out = dispatch_cmd(&dir, cell_id)
        .args(["heartbeat", "--worker-id", &worker_id])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let hb_json: serde_json::Value = serde_json::from_slice(&hb_out).unwrap();
    assert_eq!(hb_json["status"], "ok");
    assert_eq!(hb_json["worker_id"], worker_id);
    assert!(
        hb_json["expires_at"].is_number(),
        "should return expires_at timestamp"
    );

    broker.kill().ok();
    broker.wait().ok();
}

// ── Listen timeout ────────────────────────────────────────────────────

#[test]
fn listen_times_out_with_no_messages() {
    let dir = TempDir::new().unwrap();
    let cell_id = "test-listen-timeout";
    let mut broker = start_broker(&dir, cell_id);

    // Register a worker.
    let reg_out = dispatch_cmd(&dir, cell_id)
        .args([
            "register",
            "--name",
            "eve",
            "--role",
            "idle",
            "--description",
            "timeout test",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let reg: serde_json::Value = serde_json::from_slice(&reg_out).unwrap();
    let worker_id = reg["worker_id"].as_str().unwrap().to_string();

    // Listen with a very short timeout — no messages are queued.
    let listen_out = dispatch_cmd(&dir, cell_id)
        .args(["listen", "--worker-id", &worker_id, "--timeout", "1"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: serde_json::Value = serde_json::from_slice(&listen_out).unwrap();
    assert_eq!(json["status"], "ok");
    assert_eq!(json["worker_id"], worker_id);
    // Confirm this is a timeout response, not a message or heartbeat ack.
    assert!(
        json.get("body").is_none(),
        "timeout should not contain body"
    );
    assert!(
        json.get("message_id").is_none(),
        "timeout should not contain message_id"
    );
    assert!(
        json.get("expires_at").is_none(),
        "timeout should not contain expires_at"
    );

    broker.kill().ok();
    broker.wait().ok();
}

// ── stdout/stderr separation ──────────────────────────────────────────

#[test]
fn stdout_is_json_stderr_is_empty_on_success() {
    let dir = TempDir::new().unwrap();
    let cell_id = "test-stdio-sep";
    let mut broker = start_broker(&dir, cell_id);

    let output = dispatch_cmd(&dir, cell_id)
        .arg("team")
        .assert()
        .success()
        .get_output()
        .clone();

    // stdout must be valid JSON.
    let _: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout should be valid JSON");

    // stderr should be empty (no status messages on success).
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.is_empty(),
        "stderr should be empty on success, got: {stderr}"
    );

    broker.kill().ok();
    broker.wait().ok();
}
