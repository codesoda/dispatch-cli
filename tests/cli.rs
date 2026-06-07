use std::process::Command;
use std::thread;
use std::time::Duration;

use assert_cmd::cargo::CommandCargoExt;
use tempfile::TempDir;

/// RAII guard that kills the broker child process on drop.
/// Ensures cleanup even if a test panics.
struct BrokerGuard {
    child: std::process::Child,
}

impl Drop for BrokerGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Start a `dispatch serve` broker as a background process in the given
/// temp directory with the given cell ID. Returns a guard that kills
/// the broker when dropped.
fn start_broker(dir: &TempDir, cell_id: &str) -> BrokerGuard {
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
            return BrokerGuard { child };
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

    let _broker = start_broker(&dir, cell_id);
    let socket =
        std::path::PathBuf::from("/tmp/dispatch-cli/sockets").join(format!("{cell_id}.sock"));
    assert!(
        socket.exists(),
        "socket file should exist while broker runs"
    );
}

// ── Register + Team round-trip ────────────────────────────────────────

#[test]
fn register_returns_worker_id() {
    let dir = TempDir::new().unwrap();
    let cell_id = "test-register";
    let _broker = start_broker(&dir, cell_id);

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
}

/// Issue #43: registering with `--worker-id` uses the supplied id verbatim,
/// and re-registering with the same id+name+role is an idempotent claim
/// (returns the same id without creating a duplicate worker).
#[test]
fn register_with_worker_id_is_idempotent() {
    let dir = TempDir::new().unwrap();
    let cell_id = "test-register-claim";
    let _broker = start_broker(&dir, cell_id);

    let supplied_id = "w-fixed-id-for-test";

    let first = dispatch_cmd(&dir, cell_id)
        .args([
            "register",
            "--name",
            "alice",
            "--role",
            "test-runner",
            "--description",
            "test worker",
            "--worker-id",
            supplied_id,
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let first_json: serde_json::Value =
        serde_json::from_slice(&first).expect("stdout should be valid JSON");
    assert_eq!(first_json["worker_id"], supplied_id);

    // Re-register with the same id+name+role: should claim, not duplicate.
    let second = dispatch_cmd(&dir, cell_id)
        .args([
            "register",
            "--name",
            "alice",
            "--role",
            "test-runner",
            "--description",
            "test worker",
            "--worker-id",
            supplied_id,
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let second_json: serde_json::Value =
        serde_json::from_slice(&second).expect("stdout should be valid JSON");
    assert_eq!(second_json["worker_id"], supplied_id);

    // Team should report exactly one worker.
    let team = dispatch_cmd(&dir, cell_id)
        .arg("team")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let team_json: serde_json::Value =
        serde_json::from_slice(&team).expect("team stdout should be valid JSON");
    let workers = team_json["workers"]
        .as_array()
        .expect("workers should be an array");
    assert_eq!(
        workers.len(),
        1,
        "claim must not create duplicates: {team_json}"
    );
    assert_eq!(workers[0]["id"], supplied_id);
}

/// Issue #43: `dispatch register --role-prompt <body> --for-agent` routes
/// the prompt body to stdout and the JSON envelope to stderr — so when
/// the spawned agent runs this command as its first tool call, the prompt
/// body lands directly in the model's tool result.
#[test]
fn register_for_agent_routes_prompt_to_stdout() {
    let dir = TempDir::new().unwrap();
    let cell_id = "test-register-for-agent";
    let _broker = start_broker(&dir, cell_id);

    let prompt = "Run: dispatch listen --timeout 270";
    let output = dispatch_cmd(&dir, cell_id)
        .args([
            "register",
            "--name",
            "alice",
            "--role",
            "test-runner",
            "--description",
            "test worker",
            "--worker-id",
            "w-prompt-test",
            "--role-prompt",
            prompt,
            "--for-agent",
        ])
        .assert()
        .success()
        .get_output()
        .clone();

    let stdout = String::from_utf8(output.stdout).expect("stdout utf8");
    let stderr = String::from_utf8(output.stderr).expect("stderr utf8");

    // Byte-for-byte fidelity: the prompt body must land on stdout without
    // an appended newline, so agents that feed stdout into a tool-result
    // get exactly what the orchestrator stored.
    assert_eq!(
        stdout, prompt,
        "prompt body must be written verbatim (no trailing newline)",
    );
    assert!(
        stderr.contains("\"status\":\"ok\""),
        "JSON envelope must be on stderr: {stderr}",
    );
    assert!(
        stderr.contains("w-prompt-test"),
        "JSON envelope must contain worker_id: {stderr}",
    );
    // The stderr envelope must NOT duplicate the prompt body — stdout
    // already carries it, and the log would otherwise bloat with (and
    // potentially leak) the full role prompt on every --for-agent call.
    assert!(
        !stderr.contains(prompt),
        "stderr envelope must not duplicate the role prompt body: {stderr}",
    );
}

/// Issue #43: `--for-agent` without a stored prompt exits nonzero so the
/// supervisor can restart rather than have the model see empty stdout.
#[test]
fn register_for_agent_without_prompt_exits_nonzero() {
    let dir = TempDir::new().unwrap();
    let cell_id = "test-register-for-agent-nopr";
    let _broker = start_broker(&dir, cell_id);

    dispatch_cmd(&dir, cell_id)
        .args([
            "register",
            "--name",
            "alice",
            "--role",
            "test-runner",
            "--description",
            "no prompt here",
            "--for-agent",
        ])
        .assert()
        .failure();
}

/// Issue #43: an agent claim (re-register with same id) returns the prompt
/// originally stored at orchestrator pre-register time. The claim itself
/// passes no `--role-prompt`, so the broker must produce it from storage.
#[test]
fn register_claim_returns_originally_stored_prompt() {
    let dir = TempDir::new().unwrap();
    let cell_id = "test-claim-prompt";
    let _broker = start_broker(&dir, cell_id);

    let supplied_id = "w-claim-test";
    let prompt = "Run: dispatch listen --timeout 270";

    // Pre-register: orchestrator-style call carrying the prompt.
    dispatch_cmd(&dir, cell_id)
        .args([
            "register",
            "--name",
            "alice",
            "--role",
            "test-runner",
            "--description",
            "pre-register",
            "--worker-id",
            supplied_id,
            "--role-prompt",
            prompt,
        ])
        .assert()
        .success();

    // Agent claim: no --role-prompt, but --for-agent should return the stored one.
    let output = dispatch_cmd(&dir, cell_id)
        .args([
            "register",
            "--name",
            "alice",
            "--role",
            "test-runner",
            "--description",
            "agent claim",
            "--worker-id",
            supplied_id,
            "--for-agent",
        ])
        .assert()
        .success()
        .get_output()
        .clone();

    let stdout = String::from_utf8(output.stdout).expect("stdout utf8");
    assert_eq!(
        stdout, prompt,
        "claim must return exactly the prompt the orchestrator originally stored",
    );
}

#[test]
fn team_lists_registered_workers() {
    let dir = TempDir::new().unwrap();
    let cell_id = "test-team";
    let _broker = start_broker(&dir, cell_id);

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
}

// ── Send + Listen round-trip ──────────────────────────────────────────

#[test]
fn send_and_listen_round_trip() {
    let dir = TempDir::new().unwrap();
    let cell_id = "test-send-listen";
    let _broker = start_broker(&dir, cell_id);

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
}

// ── Error cases ───────────────────────────────────────────────────────

#[test]
fn send_to_invalid_worker_returns_error_response() {
    let dir = TempDir::new().unwrap();
    let cell_id = "test-send-invalid";
    let _broker = start_broker(&dir, cell_id);

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
}

// ── Heartbeat ─────────────────────────────────────────────────────────

#[test]
fn heartbeat_renews_worker_ttl() {
    let dir = TempDir::new().unwrap();
    let cell_id = "test-heartbeat";
    let _broker = start_broker(&dir, cell_id);

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
}

// ── Listen timeout ────────────────────────────────────────────────────

#[test]
fn listen_times_out_with_no_messages() {
    let dir = TempDir::new().unwrap();
    let cell_id = "test-listen-timeout";
    let _broker = start_broker(&dir, cell_id);

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
}

// ── Agent-facing listen rendering (US-006) ────────────────────────────

/// `listen --for-agent` writes a delivered message body to stdout verbatim —
/// no JSON envelope, no added newline — so it lands cleanly in the agent's
/// tool result.
#[test]
fn listen_for_agent_renders_message_body_verbatim() {
    let dir = TempDir::new().unwrap();
    let cell_id = "test-listen-for-agent-msg";
    let _broker = start_broker(&dir, cell_id);

    dispatch_cmd(&dir, cell_id)
        .args([
            "register",
            "--name",
            "recv",
            "--role",
            "worker",
            "--description",
            "d",
            "--worker-id",
            "w-recv",
        ])
        .assert()
        .success();

    dispatch_cmd(&dir, cell_id)
        .args(["send", "--to", "w-recv", "--body", "hello agent"])
        .assert()
        .success();

    let out = dispatch_cmd(&dir, cell_id)
        .args([
            "listen",
            "--for-agent",
            "--worker-id",
            "w-recv",
            "--timeout",
            "5",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(out).unwrap();
    assert_eq!(
        stdout, "hello agent",
        "message body must be rendered verbatim (no JSON, no trailing newline)"
    );
}

/// `listen --for-agent` that times out while the worker is `active` prints the
/// configured continue-instruction (plain text), not the neutral JSON timeout,
/// so the agent loops back into `listen`.
#[test]
fn listen_for_agent_timeout_while_active_prints_continue_instruction() {
    let dir = TempDir::new().unwrap();
    let cell_id = "test-listen-for-agent-active";
    let _broker = start_broker(&dir, cell_id);

    dispatch_cmd(&dir, cell_id)
        .args([
            "register",
            "--name",
            "act",
            "--role",
            "worker",
            "--description",
            "d",
            "--worker-id",
            "w-act",
        ])
        .assert()
        .success();

    let out = dispatch_cmd(&dir, cell_id)
        .args([
            "listen",
            "--for-agent",
            "--worker-id",
            "w-act",
            "--timeout",
            "1",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(out).unwrap();
    assert!(
        stdout.contains("dispatch listen"),
        "active timeout must print the continue-instruction, got: {stdout:?}"
    );
    assert!(
        serde_json::from_str::<serde_json::Value>(stdout.trim()).is_err(),
        "active timeout must be plain text, not the neutral JSON timeout, got: {stdout:?}"
    );
}

/// `listen --for-agent` that times out while the worker is `stopping` emits the
/// neutral JSON timeout (same shape as the non-`--for-agent` path), so a
/// stopping agent isn't told to keep looping.
#[test]
fn listen_for_agent_timeout_while_stopping_emits_neutral_json() {
    let dir = TempDir::new().unwrap();
    let cell_id = "test-listen-for-agent-stopping";
    let _broker = start_broker(&dir, cell_id);

    dispatch_cmd(&dir, cell_id)
        .args([
            "register",
            "--name",
            "s-stop2",
            "--role",
            "worker",
            "--description",
            "d",
            "--worker-id",
            "w-stop2",
        ])
        .assert()
        .success();

    // Transition the worker to `stopping`. `agent stop` marks the worker
    // stopping *before* attempting the (here nonexistent) process kill, so the
    // command exits non-zero but the control-state side effect lands. We then
    // confirm the precondition via `status` before relying on it.
    let _ = dispatch_cmd(&dir, cell_id)
        .args(["agent", "stop", "s-stop2"])
        .output();

    let mut is_stopping = false;
    for _ in 0..20 {
        let out = dispatch_cmd(&dir, cell_id).arg("status").output().unwrap();
        let s = String::from_utf8_lossy(&out.stdout);
        if s.contains("\"id\":\"w-stop2\"") && s.contains("\"control_state\":\"stopping\"") {
            is_stopping = true;
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(is_stopping, "precondition: w-stop2 must be marked stopping");

    let out = dispatch_cmd(&dir, cell_id)
        .args([
            "listen",
            "--for-agent",
            "--worker-id",
            "w-stop2",
            "--timeout",
            "1",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(out).unwrap();
    let json: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|_| panic!("stopping timeout must be neutral JSON, got: {stdout:?}"));
    assert_eq!(json["status"], "ok");
    assert_eq!(json["worker_id"], "w-stop2");
}

// ── Env-driven identity (US-001) ──────────────────────────────────────

/// A dispatch-launched agent runs a bare `dispatch listen` (no `--worker-id`);
/// identity comes from `$DISPATCH_WORKER_ID`, which the orchestrator injects.
#[test]
fn listen_resolves_worker_id_from_env() {
    let dir = TempDir::new().unwrap();
    let cell_id = "test-listen-env-id";
    let _broker = start_broker(&dir, cell_id);

    let worker_id = register_worker(&dir, cell_id, "env-listener", "idle");

    // No --worker-id flag; identity flows from DISPATCH_WORKER_ID.
    let listen_out = dispatch_cmd(&dir, cell_id)
        .env("DISPATCH_WORKER_ID", &worker_id)
        .args(["listen", "--timeout", "1"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: serde_json::Value = serde_json::from_slice(&listen_out).unwrap();
    assert_eq!(json["status"], "ok");
    assert_eq!(
        json["worker_id"], worker_id,
        "bare listen must resolve identity from $DISPATCH_WORKER_ID"
    );
}

/// With neither `--worker-id`/`--from` nor `$DISPATCH_WORKER_ID`, a command
/// that acts *as* a worker fails with a clear, actionable error.
#[test]
fn listen_without_identity_errors() {
    let dir = TempDir::new().unwrap();
    let cell_id = "test-listen-no-id";
    // No broker needed: identity is resolved before any broker round-trip.
    let assert = dispatch_cmd(&dir, cell_id)
        .env_remove("DISPATCH_WORKER_ID")
        .args(["listen", "--timeout", "1"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
    assert!(
        stderr.contains("no worker identity"),
        "expected a missing-identity error, got: {stderr}"
    );
}

/// `register --for-agent` with no `--worker-id` claims the pre-registered
/// worker named by `$DISPATCH_WORKER_ID` and returns its stored role prompt.
#[test]
fn register_for_agent_claims_worker_id_from_env() {
    let dir = TempDir::new().unwrap();
    let cell_id = "test-register-env-claim";
    let _broker = start_broker(&dir, cell_id);

    let prompt = "Run: dispatch listen";
    // Pre-register a worker with a fixed id + stored role prompt (the
    // orchestrator does this server-side at spawn time).
    dispatch_cmd(&dir, cell_id)
        .args([
            "register",
            "--worker-id",
            "w-env-claim",
            "--name",
            "claimer",
            "--role",
            "worker",
            "--description",
            "env claim test",
            "--role-prompt",
            prompt,
        ])
        .assert()
        .success();

    // Claim it via env identity (no --worker-id flag). The role prompt body
    // lands on stdout for the agent's next instruction.
    let out = dispatch_cmd(&dir, cell_id)
        .env("DISPATCH_WORKER_ID", "w-env-claim")
        .args([
            "register",
            "--name",
            "claimer",
            "--role",
            "worker",
            "--description",
            "env claim test",
            "--for-agent",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8_lossy(&out).to_string();
    assert_eq!(
        stdout, prompt,
        "register --for-agent must return the pre-registered worker's prompt via env identity"
    );
}

// ── Uniform minimal bootstrap (US-003) ────────────────────────────────

/// The whole point of US-003: the boot line is exactly
/// `dispatch register --for-agent` with no flags. name/role/description and
/// the worker id all resolve from the orchestrator-injected env.
#[test]
fn register_for_agent_bare_boot_line_resolves_all_from_env() {
    let dir = TempDir::new().unwrap();
    let cell_id = "test-bare-boot";
    let _broker = start_broker(&dir, cell_id);

    let prompt = "Run: dispatch listen";
    // Pre-register the worker (what the orchestrator does server-side).
    dispatch_cmd(&dir, cell_id)
        .args([
            "register",
            "--worker-id",
            "w-boot",
            "--name",
            "booty",
            "--role",
            "worker",
            "--description",
            "boot desc",
            "--role-prompt",
            prompt,
        ])
        .assert()
        .success();

    // The bare boot line — every field from env, nothing on the command line.
    let out = dispatch_cmd(&dir, cell_id)
        .env("DISPATCH_WORKER_ID", "w-boot")
        .env("DISPATCH_AGENT_NAME", "booty")
        .env("DISPATCH_AGENT_ROLE", "worker")
        .env("DISPATCH_AGENT_DESCRIPTION", "boot desc")
        .args(["register", "--for-agent"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    assert_eq!(
        String::from_utf8_lossy(&out),
        prompt,
        "bare `dispatch register --for-agent` must resolve all identity from env"
    );
}

/// A hand-run `dispatch register` with neither flags nor env for a required
/// field fails with a clear, actionable error rather than a clap usage wall.
#[test]
fn register_without_name_flag_or_env_errors() {
    let dir = TempDir::new().unwrap();
    let cell_id = "test-register-missing-name";
    let assert = dispatch_cmd(&dir, cell_id)
        .env_remove("DISPATCH_AGENT_NAME")
        .args(["register", "--role", "r", "--description", "d"])
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
    assert!(
        stderr.contains("register requires name"),
        "expected a missing-field error for name, got: {stderr}"
    );
}

// ── Stop-hook broker-liveness probe ───────────────────────────────────

/// With the broker unreachable (env points at a nonexistent socket, no
/// config in cwd), the stop hook must emit nothing and exit 0 — the
/// vendor CLI treats empty stdout as "allow stop" so a shutting-down
/// dispatch doesn't strand the agent in a listen loop.
#[test]
fn codex_hook_stop_is_silent_when_broker_unreachable() {
    let dir = TempDir::new().unwrap();
    let fake_socket = dir.path().join("does-not-exist.sock");
    let output = assert_cmd::Command::cargo_bin("dispatch")
        .unwrap()
        .arg("codex-hook")
        .arg("stop")
        .current_dir(dir.path())
        .env("DISPATCH_SOCKET_PATH", &fake_socket)
        .env_remove("DISPATCH_CELL_ID")
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.trim().is_empty(),
        "expected empty stdout when broker unreachable, got: {stdout:?}",
    );
}

#[test]
fn claude_hook_stop_is_silent_when_broker_unreachable() {
    let dir = TempDir::new().unwrap();
    let fake_socket = dir.path().join("does-not-exist.sock");
    let output = assert_cmd::Command::cargo_bin("dispatch")
        .unwrap()
        .arg("claude-hook")
        .arg("stop")
        .current_dir(dir.path())
        .env("DISPATCH_SOCKET_PATH", &fake_socket)
        .env_remove("DISPATCH_CELL_ID")
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.trim().is_empty(),
        "expected empty stdout when broker unreachable, got: {stdout:?}",
    );
}

/// Helper: extract the first worker id whose `name` matches from a
/// `dispatch status` JSON response.
fn worker_id_by_name(status_json: &str, name: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(status_json).ok()?;
    v["workers"]
        .as_array()?
        .iter()
        .find(|w| w["name"] == name)
        .and_then(|w| w["id"].as_str())
        .map(|s| s.to_string())
}

/// US-005: the stop hook is identity-gated. With a live broker but NO
/// `DISPATCH_WORKER_ID` (an ad-hoc vendor session in a hooked repo, not a
/// dispatch worker), it must emit nothing and allow the stop — even though the
/// broker is reachable. This inverts the pre-US-005 behavior where mere broker
/// reachability blocked the stop.
#[test]
fn codex_hook_stop_allows_without_identity() {
    let dir = TempDir::new().unwrap();
    let cell_id = "test-codex-hook-no-identity";
    let _broker = start_broker(&dir, cell_id);
    let socket =
        std::path::PathBuf::from("/tmp/dispatch-cli/sockets").join(format!("{cell_id}.sock"));

    let output = assert_cmd::Command::cargo_bin("dispatch")
        .unwrap()
        .arg("codex-hook")
        .arg("stop")
        .current_dir(dir.path())
        .env("DISPATCH_SOCKET_PATH", &socket)
        .env_remove("DISPATCH_WORKER_ID")
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.trim().is_empty(),
        "no identity must allow stop (empty stdout), got: {stdout:?}",
    );
}

/// US-005: with `DISPATCH_WORKER_ID` set and that worker `active`, the hook
/// blocks the stop and returns the continue-instruction as the reason, keeping
/// the agent in its listen loop.
#[test]
fn codex_hook_stop_blocks_when_worker_active() {
    let dir = TempDir::new().unwrap();
    let cell_id = "test-codex-hook-active";
    let _broker = start_broker(&dir, cell_id);
    let socket =
        std::path::PathBuf::from("/tmp/dispatch-cli/sockets").join(format!("{cell_id}.sock"));

    // Register an active worker with a known id.
    dispatch_cmd(&dir, cell_id)
        .args([
            "register",
            "--name",
            "worker-a",
            "--role",
            "runner",
            "--description",
            "d",
            "--worker-id",
            "w-active",
        ])
        .assert()
        .success();

    let output = assert_cmd::Command::cargo_bin("dispatch")
        .unwrap()
        .arg("codex-hook")
        .arg("stop")
        .current_dir(dir.path())
        .env("DISPATCH_SOCKET_PATH", &socket)
        .env("DISPATCH_WORKER_ID", "w-active")
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|_| panic!("stop hook stdout should be JSON: {stdout:?}"));
    assert_eq!(json["decision"], "block");
    assert!(
        json["reason"].as_str().is_some_and(|s| !s.is_empty()),
        "block decision must carry a non-empty continue-instruction reason",
    );
}

/// US-005 parity: the claude-hook stop handler shares `run_stop_hook`, so it
/// must also block an `active` worker.
#[test]
fn claude_hook_stop_blocks_when_worker_active() {
    let dir = TempDir::new().unwrap();
    let cell_id = "test-claude-hook-active";
    let _broker = start_broker(&dir, cell_id);
    let socket =
        std::path::PathBuf::from("/tmp/dispatch-cli/sockets").join(format!("{cell_id}.sock"));

    dispatch_cmd(&dir, cell_id)
        .args([
            "register",
            "--name",
            "worker-c",
            "--role",
            "runner",
            "--description",
            "d",
            "--worker-id",
            "w-claude-active",
        ])
        .assert()
        .success();

    let output = assert_cmd::Command::cargo_bin("dispatch")
        .unwrap()
        .arg("claude-hook")
        .arg("stop")
        .current_dir(dir.path())
        .env("DISPATCH_SOCKET_PATH", &socket)
        .env("DISPATCH_WORKER_ID", "w-claude-active")
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let json: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|_| panic!("stop hook stdout should be JSON: {stdout:?}"));
    assert_eq!(json["decision"], "block");
}

/// US-005 × US-004: once the coordinator marks a worker `stopping`, that
/// worker's own stop hook must allow the stop (so it can exit cleanly) instead
/// of fighting the shutdown. Uses the real coordinator path: a managed agent is
/// pre-registered, then `agent stop` transitions it to `stopping`.
#[test]
fn codex_hook_stop_allows_when_worker_stopping() {
    let dir = TempDir::new().unwrap();
    let cell_id = "test-codex-hook-stopping";

    let prompt = dir.path().join("sleeper.prompt.md");
    std::fs::write(&prompt, "you are a sleeper\n").unwrap();
    let config = format!(
        r#"
[[agents]]
name = "sleeper"
role = "worker"
description = "sleeps"
adapter = "command"
command = "sleep 60"
prompt_file = {:?}
launch = true
"#,
        prompt.display()
    );
    std::fs::write(dir.path().join("dispatch.config.toml"), config).unwrap();

    let _broker = start_broker(&dir, cell_id);
    let socket =
        std::path::PathBuf::from("/tmp/dispatch-cli/sockets").join(format!("{cell_id}.sock"));

    // Capture the pre-registered worker's id while it's still active.
    let mut worker_id = String::new();
    for _ in 0..100 {
        let out = dispatch_cmd(&dir, cell_id).arg("status").output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        if let Some(id) = worker_id_by_name(&stdout, "sleeper") {
            worker_id = id;
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(!worker_id.is_empty(), "sleeper worker never surfaced");

    // Coordinator stops it → control state becomes `stopping`.
    dispatch_cmd(&dir, cell_id)
        .args(["agent", "stop", "sleeper"])
        .assert()
        .success();

    // The dying agent's own stop hook now sees `stopping` → allow (no output).
    let output = assert_cmd::Command::cargo_bin("dispatch")
        .unwrap()
        .arg("codex-hook")
        .arg("stop")
        .current_dir(dir.path())
        .env("DISPATCH_SOCKET_PATH", &socket)
        .env("DISPATCH_WORKER_ID", &worker_id)
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.trim().is_empty(),
        "stopping worker must allow stop (empty stdout), got: {stdout:?}",
    );
}

// ── Introspection commands (events / messages / status / ack) ────────

/// Helper: register a worker and return its ID.
fn register_worker(dir: &TempDir, cell_id: &str, name: &str, role: &str) -> String {
    let out = dispatch_cmd(dir, cell_id)
        .args([
            "register",
            "--name",
            name,
            "--role",
            role,
            "--description",
            "integration test worker",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: serde_json::Value = serde_json::from_slice(&out).unwrap();
    json["worker_id"].as_str().unwrap().to_string()
}

#[test]
fn events_command_returns_event_history() {
    let dir = TempDir::new().unwrap();
    let cell_id = "test-events-history";
    let _broker = start_broker(&dir, cell_id);
    let _worker = register_worker(&dir, cell_id, "ev-worker", "tester");

    let output = dispatch_cmd(&dir, cell_id)
        .args(["events", "--limit", "10"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(json["status"], "ok");
    let events = json["events"].as_array().expect("events array");
    assert!(
        events.iter().any(|e| e["kind"] == "register"),
        "expected at least one register event, got: {json}"
    );
}

#[test]
fn events_command_filters_by_type() {
    let dir = TempDir::new().unwrap();
    let cell_id = "test-events-filter";
    let _broker = start_broker(&dir, cell_id);
    let worker_id = register_worker(&dir, cell_id, "ev-filter", "tester");
    dispatch_cmd(&dir, cell_id)
        .args([
            "send", "--to", &worker_id, "--body", "ping", "--from", "harness",
        ])
        .assert()
        .success();

    let output = dispatch_cmd(&dir, cell_id)
        .args(["events", "--type", "send"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: serde_json::Value = serde_json::from_slice(&output).unwrap();
    let events = json["events"].as_array().unwrap();
    assert!(!events.is_empty(), "expected at least one send event");
    assert!(events.iter().all(|e| e["kind"] == "send"));
}

/// Default `dispatch messages --worker-id <id>` (no `--sent` flag) queries
/// the worker's *inbox* — messages delivered **to** that worker. Named
/// accordingly so the assertion (`to == worker_id`) and the test intent
/// line up.
#[test]
fn messages_command_returns_worker_inbox() {
    let dir = TempDir::new().unwrap();
    let cell_id = "test-messages-inbox";
    let _broker = start_broker(&dir, cell_id);
    let worker_id = register_worker(&dir, cell_id, "msg-worker", "tester");
    dispatch_cmd(&dir, cell_id)
        .args([
            "send",
            "--to",
            &worker_id,
            "--body",
            "payload-body",
            "--from",
            "harness",
        ])
        .assert()
        .success();

    let output = dispatch_cmd(&dir, cell_id)
        .args(["messages", "--worker-id", &worker_id])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(json["status"], "ok");
    let messages = json["messages"].as_array().expect("messages array");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["body"], "payload-body");
    assert_eq!(messages[0]["to"], worker_id);
}

/// `--sent` flips the query to the worker's *outbox* — messages the worker
/// sent to others. Covers the flag branch the inbox test above does not.
#[test]
fn messages_command_with_sent_flag_returns_outbox() {
    let dir = TempDir::new().unwrap();
    let cell_id = "test-messages-sent";
    let _broker = start_broker(&dir, cell_id);
    let sender_id = register_worker(&dir, cell_id, "sender-worker", "sender");
    let recipient_id = register_worker(&dir, cell_id, "recipient-worker", "recipient");
    dispatch_cmd(&dir, cell_id)
        .args([
            "send",
            "--to",
            &recipient_id,
            "--body",
            "outbound-payload",
            "--from",
            &sender_id,
        ])
        .assert()
        .success();

    let output = dispatch_cmd(&dir, cell_id)
        .args(["messages", "--worker-id", &sender_id, "--sent"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(json["status"], "ok");
    let messages = json["messages"].as_array().expect("messages array");
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["body"], "outbound-payload");
    assert_eq!(messages[0]["from"], sender_id);
    assert_eq!(messages[0]["to"], recipient_id);
}

#[test]
fn status_command_returns_worker_status() {
    let dir = TempDir::new().unwrap();
    let cell_id = "test-status";
    let _broker = start_broker(&dir, cell_id);
    let worker_id = register_worker(&dir, cell_id, "status-worker", "tester");
    dispatch_cmd(&dir, cell_id)
        .args([
            "heartbeat",
            "--worker-id",
            &worker_id,
            "--status",
            "running e2e tests 3/10",
        ])
        .assert()
        .success();

    let output = dispatch_cmd(&dir, cell_id)
        .args(["status", "--worker-id", &worker_id])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(json["status"], "ok");
    let workers = json["workers"].as_array().expect("workers array");
    assert_eq!(workers.len(), 1);
    assert_eq!(workers[0]["last_status"], "running e2e tests 3/10");
}

#[test]
fn ack_command_records_acknowledgement() {
    let dir = TempDir::new().unwrap();
    let cell_id = "test-ack";
    let _broker = start_broker(&dir, cell_id);
    let worker_id = register_worker(&dir, cell_id, "ack-worker", "tester");

    // Send a message so there's a real message_id in history.
    let send_out = dispatch_cmd(&dir, cell_id)
        .args([
            "send", "--to", &worker_id, "--body", "ack me", "--from", "harness",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let send_json: serde_json::Value = serde_json::from_slice(&send_out).unwrap();
    let message_id = send_json["message_id"].as_str().unwrap().to_string();

    let ack_out = dispatch_cmd(&dir, cell_id)
        .args([
            "ack",
            "--worker-id",
            &worker_id,
            "--message-id",
            &message_id,
            "--note",
            "starting impl",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: serde_json::Value = serde_json::from_slice(&ack_out).unwrap();
    assert_eq!(json["status"], "ok");
    assert_eq!(json["message_id"], message_id);
    assert_eq!(json["ack_confirmed"], true);
}

#[test]
fn ack_command_rejects_unknown_message() {
    let dir = TempDir::new().unwrap();
    let cell_id = "test-ack-unknown";
    let _broker = start_broker(&dir, cell_id);
    let worker_id = register_worker(&dir, cell_id, "ack-unknown", "tester");

    let output = dispatch_cmd(&dir, cell_id)
        .args([
            "ack",
            "--worker-id",
            &worker_id,
            "--message-id",
            "fabricated-message-id",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let json: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(json["status"], "error");
    assert!(
        json["message"]
            .as_str()
            .unwrap()
            .contains("message not found"),
        "expected message-not-found error, got: {json}"
    );
}

// ── result super-ack (US-007) ─────────────────────────────────────────

/// Helper: register a worker and queue one message addressed to it, returning
/// `(worker_id, message_id)`.
fn worker_with_message(dir: &TempDir, cell_id: &str, name: &str) -> (String, String) {
    let worker_id = register_worker(dir, cell_id, name, "tester");
    let send_out = dispatch_cmd(dir, cell_id)
        .args([
            "send", "--to", &worker_id, "--body", "task", "--from", "harness",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let send: serde_json::Value = serde_json::from_slice(&send_out).unwrap();
    let message_id = send["message_id"].as_str().unwrap().to_string();
    (worker_id, message_id)
}

/// US-007: `dispatch result` records completion (status + summary + artifacts)
/// on the ack substrate without a prior `ack`, and surfaces as a `result`
/// event distinct from `ack`/`deliver`.
#[test]
fn result_records_completion() {
    let dir = TempDir::new().unwrap();
    let cell_id = "test-result-records";
    let _broker = start_broker(&dir, cell_id);
    let (worker_id, message_id) = worker_with_message(&dir, cell_id, "res-worker");

    let out = dispatch_cmd(&dir, cell_id)
        .args([
            "result",
            "--worker-id",
            &worker_id,
            "--message-id",
            &message_id,
            "--status",
            "done",
            "--summary",
            "did the task",
            "--artifact",
            "out/report.md",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(json["status"], "ok");
    assert_eq!(json["ack_confirmed"], true);
    assert_eq!(json["message_id"], message_id);

    let ev = dispatch_cmd(&dir, cell_id)
        .args(["events", "--type", "result"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let ev_s = String::from_utf8(ev).unwrap();
    assert!(
        ev_s.contains("\"status\":\"done\""),
        "result event must carry status, got: {ev_s}"
    );
    assert!(
        ev_s.contains("did the task"),
        "result event must carry summary, got: {ev_s}"
    );
    assert!(
        ev_s.contains("out/report.md"),
        "result event must carry artifact, got: {ev_s}"
    );
}

/// US-007: an invalid `--status` is rejected at parse time (clap value-enum),
/// before any broker request.
#[test]
fn result_rejects_invalid_status() {
    let dir = TempDir::new().unwrap();
    let cell_id = "test-result-bad-status";
    dispatch_cmd(&dir, cell_id)
        .args([
            "result",
            "--worker-id",
            "w",
            "--message-id",
            "m",
            "--status",
            "bogus",
        ])
        .assert()
        .failure();
}

/// US-007: results are idempotent at the verb level — a duplicate result on
/// the same message succeeds (the latest status wins in the ack log). Also
/// exercises the `failed` status.
#[test]
fn result_duplicate_succeeds() {
    let dir = TempDir::new().unwrap();
    let cell_id = "test-result-dup";
    let _broker = start_broker(&dir, cell_id);
    let (worker_id, message_id) = worker_with_message(&dir, cell_id, "dup-worker");

    dispatch_cmd(&dir, cell_id)
        .args([
            "result",
            "--worker-id",
            &worker_id,
            "--message-id",
            &message_id,
            "--status",
            "done",
        ])
        .assert()
        .success();

    let out = dispatch_cmd(&dir, cell_id)
        .args([
            "result",
            "--worker-id",
            &worker_id,
            "--message-id",
            &message_id,
            "--status",
            "failed",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let json: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(json["ack_confirmed"], true);
}

/// US-007: `result --for-agent` prints a terse confirmation (not JSON) for
/// direct tool-result consumption.
#[test]
fn result_for_agent_terse_confirmation() {
    let dir = TempDir::new().unwrap();
    let cell_id = "test-result-for-agent";
    let _broker = start_broker(&dir, cell_id);
    let (worker_id, message_id) = worker_with_message(&dir, cell_id, "fa-worker");

    let out = dispatch_cmd(&dir, cell_id)
        .args([
            "result",
            "--for-agent",
            "--worker-id",
            &worker_id,
            "--message-id",
            &message_id,
            "--status",
            "done",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let s = String::from_utf8(out).unwrap();
    assert!(
        s.contains("result recorded"),
        "terse confirmation expected, got: {s:?}"
    );
    assert!(
        s.contains("done"),
        "confirmation should mention status, got: {s:?}"
    );
    assert!(
        serde_json::from_str::<serde_json::Value>(s.trim()).is_err(),
        "for-agent confirmation must be plain text, not JSON, got: {s:?}"
    );
}

// ── stdout/stderr separation ──────────────────────────────────────────

#[test]
fn stdout_is_json_stderr_is_empty_on_success() {
    let dir = TempDir::new().unwrap();
    let cell_id = "test-stdio-sep";
    let _broker = start_broker(&dir, cell_id);

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
}

// ── Spawned-agent env propagation ─────────────────────────────────────

/// Regression guard for the `DISPATCH_CONFIG_PATH` injection wire: a
/// `launch = true` command-adapter agent must receive the env var pointing
/// at the canonicalized config file path, so child `dispatch` calls from
/// any cwd in the agent tree resolve the orchestrator's config rather than
/// falling back to a cwd-derived `cell-<hash>`.
#[test]
fn serve_spawns_agent_with_dispatch_config_path_in_env() {
    let dir = TempDir::new().unwrap();
    let cell_id = "test-dispatch-config-path-env";

    // Dump the DISPATCH_* env seen by the spawned agent to a sibling file,
    // then sleep so the supervisor doesn't enter its restart loop and
    // stomp on the file before we can read it.
    let envfile = dir.path().join("agent.env");
    let command = format!("env | grep ^DISPATCH_ > {} && sleep 60", envfile.display());
    let config_path = dir.path().join("dispatch.config.toml");
    let config = format!(
        r#"
[[agents]]
name = "dumper"
role = "worker"
description = "dumps env"
adapter = "command"
command = {command:?}
launch = true
"#
    );
    std::fs::write(&config_path, config).unwrap();

    let _broker = start_broker(&dir, cell_id);

    // Poll up to ~5s for the env dump. The supervisor launches agents
    // sequentially with a 500ms pause, so even on a cold machine the dump
    // should land well under the budget.
    let mut contents = String::new();
    for _ in 0..100 {
        if envfile.exists() {
            contents = std::fs::read_to_string(&envfile).unwrap_or_default();
            if !contents.is_empty() {
                break;
            }
        }
        thread::sleep(Duration::from_millis(50));
    }

    assert!(
        !contents.is_empty(),
        "agent env dump did not appear within 5s; file: {}",
        envfile.display()
    );

    let expected = config_path.canonicalize().unwrap();
    let needle = format!("DISPATCH_CONFIG_PATH={}", expected.display());
    assert!(
        contents.contains(&needle),
        "env dump missing {needle:?}; got:\n{contents}"
    );
}

// ── Coordinator-driven lifecycle (US-004) ─────────────────────────────

/// `dispatch agent stop <name>` marks the worker `stopping` (stamping the
/// drain clock) BEFORE killing the process, so the record lingers in the
/// drain window — a late stop-hook call from the dying agent then sees
/// `stopping` and is allowed to exit instead of racing the record away.
///
/// A managed `launch = true` + `prompt_file` agent is pre-registered
/// server-side, so a worker exists to transition. `sleep 60` keeps the
/// process alive long enough for `agent stop` to find a live supervisor
/// handle to signal.
#[test]
fn agent_stop_marks_worker_stopping() {
    let dir = TempDir::new().unwrap();
    let cell_id = "test-agent-stop-stopping";

    let prompt = dir.path().join("sleeper.prompt.md");
    std::fs::write(&prompt, "you are a sleeper\n").unwrap();
    let config = format!(
        r#"
[[agents]]
name = "sleeper"
role = "worker"
description = "sleeps"
adapter = "command"
command = "sleep 60"
prompt_file = {:?}
launch = true
"#,
        prompt.display()
    );
    std::fs::write(dir.path().join("dispatch.config.toml"), config).unwrap();

    let _broker = start_broker(&dir, cell_id);

    // Wait for the pre-registered worker to surface as `active`. The broker
    // serves client requests only after `launch_all` completes, so once
    // `status` reports the worker its supervisor handle is registered too.
    let mut saw_active = false;
    for _ in 0..100 {
        let out = dispatch_cmd(&dir, cell_id).arg("status").output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        if stdout.contains("\"name\":\"sleeper\"")
            && stdout.contains("\"control_state\":\"active\"")
        {
            saw_active = true;
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        saw_active,
        "pre-registered sleeper never surfaced as active"
    );

    // Stop it: marks `stopping`, then signals the supervisor to kill.
    dispatch_cmd(&dir, cell_id)
        .args(["agent", "stop", "sleeper"])
        .assert()
        .success();

    // Within the drain window the worker lingers as `stopping`.
    let out = dispatch_cmd(&dir, cell_id).arg("status").output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("\"control_state\":\"stopping\""),
        "worker should be `stopping` after agent stop; got: {stdout}"
    );

    // The transition is observable as a `lifecycle` event.
    let out = dispatch_cmd(&dir, cell_id)
        .args(["events", "--type", "lifecycle"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("active -> stopping"),
        "expected a lifecycle event for the stop transition; got: {stdout}"
    );
}
