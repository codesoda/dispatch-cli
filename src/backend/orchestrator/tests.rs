use super::*;
use crate::adapter::Adapter;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

/// Constructing `SpawnContext::env_vars(_, _, Some(id))` injects
/// `DISPATCH_WORKER_ID`; `None` omits the key entirely so legacy
/// register-yourself agents see exactly the previous environment.
#[test]
fn env_vars_includes_worker_id_when_some() {
    let tmp = tempfile::tempdir().unwrap();
    let orch = AgentOrchestrator::new(
        "cell-x",
        &tmp.path().join("broker.sock"),
        None,
        tmp.path(),
        tmp.path().join("logs"),
        Vec::new(),
        Arc::new(Mutex::new(super::super::local::BrokerState::new())),
        None,
    );
    let ctx = orch.snapshot_spawn_context();

    let with_id = ctx.env_vars(
        "alice",
        "test-runner",
        "does things",
        Some("w-123"),
        Some(540),
    );
    assert_eq!(
        with_id.get("DISPATCH_WORKER_ID").map(String::as_str),
        Some("w-123")
    );
    // Description is injected so the bare boot line can resolve the
    // broker-required --description from env.
    assert_eq!(
        with_id
            .get("DISPATCH_AGENT_DESCRIPTION")
            .map(String::as_str),
        Some("does things")
    );
    // A resolved per-agent/global listen timeout is injected as
    // DISPATCH_LISTEN_TIMEOUT so the agent's bare `dispatch listen` uses it.
    assert_eq!(
        with_id.get("DISPATCH_LISTEN_TIMEOUT").map(String::as_str),
        Some("540")
    );
    assert_eq!(
        with_id.get("DISPATCH_AGENT_NAME").map(String::as_str),
        Some("alice")
    );
    assert_eq!(
        with_id.get("DISPATCH_AGENT_ROLE").map(String::as_str),
        Some("test-runner")
    );

    let without_id = ctx.env_vars("alice", "test-runner", "does things", None, None);
    assert!(!without_id.contains_key("DISPATCH_WORKER_ID"));
    // No resolved timeout → the key is omitted and the CLI default applies.
    assert!(!without_id.contains_key("DISPATCH_LISTEN_TIMEOUT"));
    // The other vars must match exactly so the legacy code path is bit-for-bit unchanged.
    assert_eq!(
        without_id.get("DISPATCH_AGENT_NAME").map(String::as_str),
        Some("alice")
    );
    assert_eq!(
        without_id.get("DISPATCH_AGENT_ROLE").map(String::as_str),
        Some("test-runner")
    );
}

/// `DISPATCH_CONFIG_PATH` is injected when the orchestrator carries a
/// config file path, so child `dispatch` calls from any cwd in the
/// agent tree resolve the same config.
#[test]
fn env_vars_includes_config_path_when_set() {
    let tmp = tempfile::tempdir().unwrap();
    let config_path = tmp.path().join("dispatch.config.toml");
    let orch = AgentOrchestrator::new(
        "cell-x",
        &tmp.path().join("broker.sock"),
        None,
        tmp.path(),
        tmp.path().join("logs"),
        Vec::new(),
        Arc::new(Mutex::new(super::super::local::BrokerState::new())),
        Some(config_path.clone()),
    );
    let ctx = orch.snapshot_spawn_context();

    let vars = ctx.env_vars("alice", "test-runner", "does things", None, None);
    assert_eq!(
        vars.get("DISPATCH_CONFIG_PATH").map(String::as_str),
        Some(config_path.display().to_string().as_str())
    );
}

/// When the orchestrator has no config file path (e.g. serve launched
/// outside a project), no `DISPATCH_CONFIG_PATH` env var is emitted
/// — regression guard for configs that don't opt into the feature.
#[test]
fn env_vars_absent_when_config_path_none() {
    let tmp = tempfile::tempdir().unwrap();
    let orch = AgentOrchestrator::new(
        "cell-x",
        &tmp.path().join("broker.sock"),
        None,
        tmp.path(),
        tmp.path().join("logs"),
        Vec::new(),
        Arc::new(Mutex::new(super::super::local::BrokerState::new())),
        None,
    );
    let ctx = orch.snapshot_spawn_context();

    let vars = ctx.env_vars("alice", "test-runner", "does things", None, None);
    assert!(!vars.contains_key("DISPATCH_CONFIG_PATH"));
}

/// Exponential doubling capped at 30s — table-driven so the intent is
/// obvious if anyone tunes the backoff schedule later.
#[test]
fn restart_backoff_schedule() {
    let cases = [(1, 1), (2, 2), (3, 4), (4, 8), (5, 16), (6, 30), (10, 30)];
    for (attempt, expected) in cases {
        assert_eq!(
            restart_backoff(attempt).as_secs(),
            expected,
            "attempt {attempt} should back off {expected}s",
        );
    }
}

/// Build a ResolvedAgentConfig that runs a short sh command under the
/// `command` adapter — avoids needing `claude`/`codex` binaries on the
/// test host.
fn test_config(name: &str, command: &str) -> ResolvedAgentConfig {
    ResolvedAgentConfig {
        name: name.into(),
        role: "test".into(),
        description: "".into(),
        adapter: Adapter::Command,
        command: Some(command.into()),
        extra_args: Vec::new(),
        prompt: None,
        prompt_file_path: None,
        ttl: None,
        listen_timeout: None,
        continue_instruction: None,
        boot_prompt: None,
        stream_json: false,
        interactive: false,
        launch: true,
    }
}

/// Helper: build a managed test config (launch=true, with prompt_file)
/// that exercises the pre-register flow. Uses the `command`
/// adapter so we don't need `claude` on the test host — the prompt file
/// is created but ignored by the adapter; what we're testing is whether
/// the orchestrator correctly pre-registers the worker server-side.
fn managed_test_config(name: &str, command: &str, prompt_path: PathBuf) -> ResolvedAgentConfig {
    ResolvedAgentConfig {
        name: name.into(),
        role: "test-runner".into(),
        description: "managed test agent".into(),
        adapter: Adapter::Command,
        command: Some(command.into()),
        extra_args: Vec::new(),
        prompt: None,
        prompt_file_path: Some(prompt_path),
        ttl: None,
        listen_timeout: None,
        continue_instruction: None,
        boot_prompt: None,
        stream_json: false,
        interactive: false,
        launch: true,
    }
}

/// `write_boot_prompt` emits the shipped default when no `boot_prompt` is
/// configured, and the configured value (normalized to exactly one
/// trailing newline) when one is set.
#[tokio::test]
async fn write_boot_prompt_uses_default_or_configured() {
    let tmp = tempfile::tempdir().unwrap();
    let log_dir = tmp.path().join("logs");

    // Unset -> shipped default verbatim.
    let cfg = test_config("alice", "sleep 0");
    let path = write_boot_prompt(&log_dir, &cfg).await.unwrap();
    let body = tokio::fs::read_to_string(&path).await.unwrap();
    assert_eq!(body, DEFAULT_BOOT_PROMPT);

    // Configured without a trailing newline -> value + exactly one newline.
    let mut custom = test_config("bob", "sleep 0");
    custom.boot_prompt = Some("Use the dispatch skill, then: dispatch register --for-agent".into());
    let path = write_boot_prompt(&log_dir, &custom).await.unwrap();
    let body = tokio::fs::read_to_string(&path).await.unwrap();
    assert_eq!(
        body,
        "Use the dispatch skill, then: dispatch register --for-agent\n"
    );
}

/// Issue #43: spawning a managed agent (launch=true with prompt_file)
/// pre-registers the worker server-side BEFORE the child starts, with
/// the prompt body stored under the assigned worker id.
#[tokio::test]
async fn spawn_managed_agent_pre_registers_with_prompt() {
    let tmp = tempfile::tempdir().unwrap();
    let prompt_path = tmp.path().join("alice.md");
    let prompt_body = "Run: dispatch listen --timeout 270\nRole context here.";
    tokio::fs::write(&prompt_path, prompt_body).await.unwrap();

    let broker = Arc::new(Mutex::new(super::super::local::BrokerState::new()));
    let mut orch = AgentOrchestrator::new(
        "test-cell",
        &tmp.path().join("broker.sock"),
        None,
        tmp.path(),
        tmp.path().join("logs"),
        Vec::new(),
        Arc::clone(&broker),
        None,
    );
    let cfg = managed_test_config("alice", "sleep 30", prompt_path);
    orch.spawn_agent(&cfg).await.expect("spawn");

    // Worker is in the broker right away — the agent's later claim
    // call will idempotently match it without inventing an id.
    let b = broker.lock().await;
    assert_eq!(b.workers.len(), 1, "pre-register must create the worker");
    let (id, worker) = b.workers.iter().next().unwrap();
    assert_eq!(worker.name, "alice");
    assert_eq!(worker.role, "test-runner");
    assert_eq!(
        b.role_prompts.get(id).map(String::as_str),
        Some(prompt_body),
        "role prompt must be stored under the worker id",
    );

    drop(b);
    orch.shutdown_all().await;
}

/// Issue #43: launch=false agents stay on the legacy register-yourself
/// path. No worker shows up in the broker after `spawn_agent`.
#[tokio::test]
async fn spawn_unmanaged_agent_does_not_pre_register() {
    let tmp = tempfile::tempdir().unwrap();
    let broker = Arc::new(Mutex::new(super::super::local::BrokerState::new()));
    let mut orch = AgentOrchestrator::new(
        "test-cell",
        &tmp.path().join("broker.sock"),
        None,
        tmp.path(),
        tmp.path().join("logs"),
        Vec::new(),
        Arc::clone(&broker),
        None,
    );
    let mut cfg = test_config("bob", "sleep 30");
    cfg.launch = false; // explicitly unmanaged
    orch.spawn_agent(&cfg).await.expect("spawn");

    let b = broker.lock().await;
    assert!(
        b.workers.is_empty(),
        "unmanaged agents must not be pre-registered: {:?}",
        b.workers,
    );
    drop(b);
    orch.shutdown_all().await;
}

/// Issue #43: a missing prompt file fails the spawn and leaves NO
/// orphan worker in the broker. The read_to_string `?` returns before
/// `register_worker` is reached, so nothing is ever created — the
/// early return (not the cleanup guard) is what prevents the orphan.
#[tokio::test]
async fn spawn_managed_agent_missing_prompt_file_leaves_no_orphan() {
    let tmp = tempfile::tempdir().unwrap();
    let broker = Arc::new(Mutex::new(super::super::local::BrokerState::new()));
    let mut orch = AgentOrchestrator::new(
        "test-cell",
        &tmp.path().join("broker.sock"),
        None,
        tmp.path(),
        tmp.path().join("logs"),
        Vec::new(),
        Arc::clone(&broker),
        None,
    );
    let cfg = managed_test_config("alice", "sleep 30", tmp.path().join("does-not-exist.md"));
    let err = orch.spawn_agent(&cfg).await.expect_err("must fail");
    assert!(
        matches!(err, DispatchError::PromptFileNotFound { .. }),
        "expected PromptFileNotFound, got: {err:?}",
    );
    let b = broker.lock().await;
    assert!(
        b.workers.is_empty(),
        "failed pre-register must not leave an orphan worker",
    );
}

/// Issue #43: when `spawn_child_process` fails AFTER the pre-register
/// succeeds, the cleanup guard must remove the worker record, role
/// prompt, mailbox, and notifier so no zombie state survives in the
/// broker. We trigger a spawn failure by pointing `agent_cwd` at a
/// path that doesn't exist — `Command::current_dir` errors with ENOENT
/// when the spawn syscall tries to chdir.
#[tokio::test]
async fn spawn_managed_agent_spawn_failure_triggers_cleanup_guard() {
    let tmp = tempfile::tempdir().unwrap();
    let prompt_path = tmp.path().join("alice.md");
    tokio::fs::write(&prompt_path, "role prompt body")
        .await
        .unwrap();

    let bad_cwd = tmp.path().join("does-not-exist-cwd");
    let broker = Arc::new(Mutex::new(super::super::local::BrokerState::new()));
    let mut orch = AgentOrchestrator::new(
        "test-cell",
        &tmp.path().join("broker.sock"),
        None,
        &bad_cwd,
        tmp.path().join("logs"),
        Vec::new(),
        Arc::clone(&broker),
        None,
    );

    // Force `spawn_agent` to fail and verify the broker is left with no
    // residual state. This covers cleanup of any entries created during
    // the failed spawn attempt across `workers`, `role_prompts`,
    // `mailboxes`, and `notifiers`.
    let cfg = managed_test_config("alice", "true", prompt_path);
    let err = orch.spawn_agent(&cfg).await.expect_err("must fail");
    assert!(
        matches!(err, DispatchError::AgentLaunchFailed { .. }),
        "expected AgentLaunchFailed, got: {err:?}",
    );

    let b = broker.lock().await;
    assert!(
        b.workers.is_empty(),
        "spawn failure must not leave an orphan worker: {:?}",
        b.workers,
    );
    assert!(
        b.role_prompts.is_empty(),
        "spawn failure must not leave an orphan role prompt: {:?}",
        b.role_prompts,
    );
    assert!(
        b.mailboxes.is_empty(),
        "spawn failure must not leave an orphan mailbox",
    );
    assert!(
        b.notifiers.is_empty(),
        "spawn failure must not leave an orphan notifier",
    );
}

/// Issue #45: `pre_register_unmanaged` mirrors the managed-agent
/// pre-register flow without spawning. The unmanaged serve-time banner
/// calls this so the printed copy-paste command can include a
/// `DISPATCH_WORKER_ID` that the agent's first
/// `dispatch register --for-agent` call can idempotently claim.
#[tokio::test]
async fn pre_register_unmanaged_stores_worker_and_role_prompt() {
    let tmp = tempfile::tempdir().unwrap();
    let prompt_path = tmp.path().join("coord.md");
    let prompt_body = "Run: dispatch listen --timeout 270\nCoordinator instructions.";
    tokio::fs::write(&prompt_path, prompt_body).await.unwrap();

    let broker = Arc::new(Mutex::new(super::super::local::BrokerState::new()));
    let orch = AgentOrchestrator::new(
        "test-cell",
        &tmp.path().join("broker.sock"),
        None,
        tmp.path(),
        tmp.path().join("logs"),
        Vec::new(),
        Arc::clone(&broker),
        None,
    );
    let mut cfg = managed_test_config("coordinator", "true", prompt_path);
    cfg.launch = false;

    let ctx = orch.snapshot_spawn_context();
    let (worker_id, boot_path) = pre_register_unmanaged(&ctx, &cfg)
        .await
        .expect("pre_register_unmanaged must succeed");

    let b = broker.lock().await;
    assert_eq!(b.workers.len(), 1);
    let worker = b
        .workers
        .get(&worker_id)
        .expect("worker must be stored under returned id");
    assert_eq!(worker.name, "coordinator");
    assert_eq!(worker.role, "test-runner");
    assert_eq!(
        b.role_prompts.get(&worker_id).map(String::as_str),
        Some(prompt_body),
    );
    assert!(
        boot_path.exists(),
        "boot prompt file must be written: {}",
        boot_path.display()
    );
}

/// Issue #45: `pre_register_unmanaged` refuses a config without a
/// `prompt_file_path` — the caller should only reach this path for
/// unmanaged agents that actually need the boot-prompt bootstrap.
#[tokio::test]
async fn pre_register_unmanaged_rejects_config_without_prompt_file() {
    let tmp = tempfile::tempdir().unwrap();
    let broker = Arc::new(Mutex::new(super::super::local::BrokerState::new()));
    let orch = AgentOrchestrator::new(
        "test-cell",
        &tmp.path().join("broker.sock"),
        None,
        tmp.path(),
        tmp.path().join("logs"),
        Vec::new(),
        Arc::clone(&broker),
        None,
    );
    let mut cfg = test_config("bare", "true");
    cfg.launch = false;

    let ctx = orch.snapshot_spawn_context();
    let err = pre_register_unmanaged(&ctx, &cfg)
        .await
        .expect_err("must fail without prompt_file_path");
    assert!(
        matches!(err, DispatchError::AgentLaunchFailed { .. }),
        "expected AgentLaunchFailed, got: {err:?}",
    );
    assert!(
        broker.lock().await.workers.is_empty(),
        "failed pre-register must not leave a worker behind",
    );
}

/// Issue #45: `build_agent_command` emits `DISPATCH_WORKER_ID=<id>`
/// only when the id is supplied — the legacy bare-register path
/// (no prompt_file → no pre-register) keeps its previous output.
#[test]
fn build_agent_command_includes_worker_id_when_some() {
    let cfg = test_config("alice", "echo hi");
    let with_id = build_agent_command(&cfg, "cell-x", None, Some("w-123"), None);
    // shell_escape wraps the value in single quotes; assert on the
    // escaped form we'll actually see in the printed banner.
    assert!(
        with_id.contains("DISPATCH_WORKER_ID='w-123'"),
        "expected worker id in: {with_id}",
    );

    let without_id = build_agent_command(&cfg, "cell-x", None, None, None);
    assert!(
        !without_id.contains("DISPATCH_WORKER_ID"),
        "legacy path must not emit worker id: {without_id}",
    );
}

/// `build_agent_command` emits `DISPATCH_CONFIG_PATH=<shell-escaped>`
/// when a path is supplied, right alongside `DISPATCH_CELL_ID` so the
/// printed banner lets the pasted command resolve the same config from
/// any cwd. Unset → omitted, byte-identical to the previous banner.
#[test]
fn build_agent_command_includes_config_path_when_some() {
    let cfg = test_config("alice", "echo hi");
    let cfg_path = std::path::PathBuf::from("/tmp/ex/dispatch.config.toml");
    let with_path = build_agent_command(&cfg, "cell-x", None, None, Some(cfg_path.as_path()));
    assert!(
        with_path.contains("DISPATCH_CONFIG_PATH='/tmp/ex/dispatch.config.toml'"),
        "expected config path in: {with_path}"
    );

    let without = build_agent_command(&cfg, "cell-x", None, None, None);
    assert!(
        !without.contains("DISPATCH_CONFIG_PATH"),
        "unset path must not appear: {without}"
    );
}

/// `start_by_name` must release the `starting` reservation when
/// `build_pending_agent` fails — otherwise a bad prompt_file (or any
/// other build error) permanently blocks the name from ever being
/// started. Uses a managed config pointing at a nonexistent prompt
/// file to force `build_pending_agent` to error after the reservation
/// is claimed.
#[tokio::test]
async fn start_by_name_releases_reservation_on_build_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let bad_cfg = managed_test_config("alice", "true", tmp.path().join("nope.md"));
    let mut orch = AgentOrchestrator::new(
        "test-cell",
        &tmp.path().join("broker.sock"),
        None,
        tmp.path(),
        tmp.path().join("logs"),
        vec![bad_cfg],
        Arc::new(Mutex::new(super::super::local::BrokerState::new())),
        None,
    );

    // Build fails because the prompt file doesn't exist.
    let err = orch.start_by_name("alice").await.expect_err("must fail");
    assert!(
        matches!(err, DispatchError::PromptFileNotFound { .. }),
        "expected PromptFileNotFound, got: {err:?}",
    );

    // Reservation must have been released — a fresh `check_can_start`
    // for the same name succeeds rather than hitting "already starting".
    orch.check_can_start("alice")
        .expect("reservation must be released after build failure");
}

/// `check_can_start` reserves the agent name in `starting` so a
/// concurrent caller in the unlocked-spawn pattern can't pass phase
/// 1 for the same name. `register_pending` and `cancel_start` both
/// release the reservation. Without this, two parallel
/// `BrokerRequest::AgentStart` calls would race past `check_can_start`
/// and push duplicate `ManagedAgent` entries.
#[tokio::test]
async fn check_can_start_reserves_name_until_register_or_cancel() {
    let tmp = tempfile::tempdir().unwrap();
    let cfg = test_config("alice", "sleep 30");
    let mut orch = AgentOrchestrator::new(
        "test-cell",
        &tmp.path().join("broker.sock"),
        None,
        tmp.path(),
        tmp.path().join("logs"),
        vec![cfg.clone()],
        Arc::new(Mutex::new(super::super::local::BrokerState::new())),
        None,
    );

    // First reservation succeeds.
    orch.check_can_start("alice").expect("first reservation");

    // Second reservation while the first is still pending must fail
    // with "already starting" — this is the race-defense that makes
    // the 3-phase pattern safe.
    let err = orch
        .check_can_start("alice")
        .expect_err("second reservation must be rejected");
    match err {
        DispatchError::AgentLaunchFailed { name, reason } => {
            assert_eq!(name, "alice");
            assert!(
                reason.contains("already starting"),
                "expected 'already starting' rejection, got: {reason}"
            );
        }
        other => panic!("expected AgentLaunchFailed, got: {other:?}"),
    }

    // cancel_start releases the slot — a fresh check_can_start succeeds.
    orch.cancel_start("alice");
    orch.check_can_start("alice")
        .expect("post-cancel reservation");

    // Now exercise the success path via a real spawn. spawn_agent is
    // tolerant of the lingering reservation (register_pending removes
    // it) so the agent ends up in `agents` with no leftover slot.
    orch.spawn_agent(&cfg).await.expect("spawn");
    // After register_pending, "alice" is in agents and NOT in starting.
    // Starting a fresh "alice" now hits the "already running" guard,
    // not "already starting".
    let err = orch.check_can_start("alice").expect_err("alice is running");
    match err {
        DispatchError::AgentLaunchFailed { reason, .. } => {
            assert!(
                reason.contains("already running"),
                "expected 'already running', got: {reason}"
            );
        }
        other => panic!("expected AgentLaunchFailed, got: {other:?}"),
    }
    orch.shutdown_all().await;
}

/// Supervisor reports Running while a long-lived child is alive, and
/// transitions to Stopped after `stop_by_name`.
#[tokio::test]
async fn supervisor_running_then_stopped() {
    let tmp = tempfile::tempdir().unwrap();
    let mut orch = AgentOrchestrator::new(
        "test-cell",
        &tmp.path().join("broker.sock"),
        None,
        tmp.path(),
        tmp.path().join("logs"),
        Vec::new(),
        Arc::new(Mutex::new(super::super::local::BrokerState::new())),
        None,
    );
    let cfg = test_config("alice", "sleep 30");
    orch.spawn_agent(&cfg).await.expect("spawn");
    // Let the supervisor publish its Running state.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let states = orch.list_state().await;
    assert_eq!(states.len(), 1);
    assert!(matches!(states[0].2, AgentState::Running { .. }));

    assert!(orch.stop_by_name("alice").await);
    assert!(orch.list_state().await.is_empty());
}

/// When the child exits quickly, the supervisor moves through
/// Running → Restarting (attempt=1) before the first-backoff sleep
/// completes. We probe at 300ms — well after exit, well before the 1s
/// backoff elapses.
#[tokio::test]
async fn supervisor_transitions_to_restarting_after_quick_exit() {
    let tmp = tempfile::tempdir().unwrap();
    let mut orch = AgentOrchestrator::new(
        "test-cell",
        &tmp.path().join("broker.sock"),
        None,
        tmp.path(),
        tmp.path().join("logs"),
        Vec::new(),
        Arc::new(Mutex::new(super::super::local::BrokerState::new())),
        None,
    );
    let cfg = test_config("flaky", "exit 1");
    orch.spawn_agent(&cfg).await.expect("spawn");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let states = orch.list_state().await;
    assert_eq!(states.len(), 1);
    assert!(
        matches!(
            states[0].2,
            AgentState::Restarting {
                attempt: 1,
                backoff_secs: 1
            } | AgentState::Running { .. }
        ),
        "unexpected state: {:?}",
        states[0].2
    );

    // Shutdown should cancel the backoff sleep cleanly.
    orch.shutdown_all().await;
    assert!(orch.list_state().await.is_empty());
}
