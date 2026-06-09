//! Hook subcommands for Codex and Claude Code CLIs.
//!
//! The `codex-hook` and `claude-hook` subcommand families let an agent's
//! vendor CLI call back into dispatch on lifecycle events. The Stop hook is
//! the important one: by printing `{"decision":"block","reason":"..."}` on
//! stdout we tell the agent to stay alive and keep polling dispatch for new
//! messages instead of exiting at the end of a turn.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::backend::socket_path;
use crate::config::resolve_config;
use crate::protocol::{BrokerRequest, BrokerResponse, ControlState, ResponsePayload};

pub mod claude;
pub mod codex;

/// How long the stop hook waits for the broker to answer the control-state
/// query before giving up and allowing the stop. Kept well under the codex
/// hook's ~10s budget — a `Status` query is answered immediately, so this is
/// purely a failsafe against a hung or vanishing broker.
const STOP_HOOK_QUERY_TIMEOUT: Duration = Duration::from_secs(2);

/// Read the dispatch worker identity from the environment. Empty is treated as
/// unset. Only a dispatch-launched agent has this set — it is the gate that
/// keeps the stop hook from hijacking unrelated ad-hoc vendor sessions.
fn env_worker_id() -> Option<String> {
    std::env::var("DISPATCH_WORKER_ID")
        .ok()
        .filter(|s| !s.is_empty())
}

/// Read the dispatch agent name from the environment, used to resolve the
/// per-agent `continue_instruction`. Empty is treated as unset.
fn env_agent_name() -> Option<String> {
    std::env::var("DISPATCH_AGENT_NAME")
        .ok()
        .filter(|s| !s.is_empty())
}

/// JSON body the stop hook emits on stdout to keep the agent alive. Both
/// vendors accept `{"decision":"block","reason":"..."}`. `reason` is the
/// resolved `continue_instruction`.
pub fn stop_decision_json(reason: &str) -> String {
    serde_json::json!({
        "decision": "block",
        "reason": reason,
    })
    .to_string()
}

/// Handler for both `dispatch codex-hook stop` and `dispatch claude-hook stop`.
/// Decides whether to keep the agent in its listen loop:
///
/// 1. No `DISPATCH_WORKER_ID` → **allow** (an ad-hoc vendor session in a
///    hooked repo, not a dispatch worker). The hook fires for *every* session,
///    so identity is the gate that stops it from hijacking unrelated agents.
/// 2. Worker `active` → **block**, returning the configured
///    `continue_instruction` so the agent runs `dispatch listen` again.
/// 3. Worker `stopping`/`stopped`/unknown, or the broker is
///    unreachable/slow/malformed → **allow**. The coordinator owns the stop
///    decision; a shutting-down or gone dispatch must never strand the agent.
///
/// Never returns an error: every failure maps to "allow stop" (no output).
pub async fn run_stop_hook(cwd: &Path) {
    let worker_id = match env_worker_id() {
        Some(id) => id,
        None => {
            tracing::debug!("no DISPATCH_WORKER_ID; allowing stop");
            return;
        }
    };

    let socket = match resolve_socket_path(cwd) {
        Some(p) => p,
        None => {
            tracing::debug!("no socket path resolvable; allowing stop");
            return;
        }
    };

    match query_control_state(&socket, &worker_id).await {
        Some(ControlState::Active) => {
            let reason = resolve_continue_instruction(cwd);
            println!("{}", stop_decision_json(&reason));
            tracing::debug!(worker = %worker_id, "worker active; blocking stop");
        }
        other => {
            tracing::debug!(
                worker = %worker_id,
                state = ?other,
                "worker not active; allowing stop",
            );
        }
    }
}

/// Resolve the `continue_instruction` for this agent from its config, falling
/// back to the shipped default when no config is resolvable. Runs in the
/// agent's own process, which carries `DISPATCH_CONFIG_PATH` +
/// `DISPATCH_AGENT_NAME`, so the per-agent override is visible.
fn resolve_continue_instruction(cwd: &Path) -> String {
    let agent = env_agent_name();
    match resolve_config(None, None, cwd) {
        Ok(cfg) => cfg.continue_instruction_for(agent.as_deref()),
        Err(e) => {
            tracing::debug!(error = %e, "config resolution failed; using default continue instruction");
            crate::config::DEFAULT_CONTINUE_INSTRUCTION.to_string()
        }
    }
}

/// Resolve the broker socket path using the same precedence the regular
/// client uses: `DISPATCH_SOCKET_PATH` env var wins, otherwise fall back
/// to the config-derived path. Returns `None` if neither source yields a
/// path (e.g. config resolution fails outside a project).
fn resolve_socket_path(cwd: &Path) -> Option<PathBuf> {
    let env = std::env::var("DISPATCH_SOCKET_PATH").ok();
    resolve_socket_path_with_env(env.as_deref(), cwd)
}

/// Internal helper that separates the process-global `DISPATCH_SOCKET_PATH`
/// read from the resolution logic so tests can exercise the env-precedence
/// branch without mutating `std::env` (which races across parallel tests).
fn resolve_socket_path_with_env(env_path: Option<&str>, cwd: &Path) -> Option<PathBuf> {
    if let Some(p) = env_path {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    match resolve_config(None, None, cwd) {
        Ok(cfg) => Some(socket_path(&cfg.project_root, &cfg.cell_id)),
        Err(e) => {
            tracing::debug!(error = %e, "config resolution failed during hook probe");
            None
        }
    }
}

/// Query the broker for `worker_id`'s control state via a `Status` request.
/// The `Status` request is reused deliberately rather than adding a new wire
/// variant (keeps the untagged response enum — and its delicate `Timeout`
/// disambiguation — untouched). Returns `None` on *any* failure — unreachable,
/// timeout, malformed response, or worker not found — so every failure maps to
/// "allow stop". Bounded by [`STOP_HOOK_QUERY_TIMEOUT`].
async fn query_control_state(socket: &Path, worker_id: &str) -> Option<ControlState> {
    let exchange = async {
        let stream = UnixStream::connect(socket).await.ok()?;
        let (reader, mut writer) = stream.into_split();

        let request = BrokerRequest::Status {
            worker_id: Some(worker_id.to_string()),
            clear: false,
            // Mark this as a stop-hook probe so the broker records the
            // block/allow decision as a `stop_decision` event.
            probe: Some("stop_hook".to_string()),
        };
        let mut bytes = serde_json::to_vec(&request).ok()?;
        bytes.push(b'\n');
        writer.write_all(&bytes).await.ok()?;

        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        if reader.read_line(&mut line).await.ok()? == 0 {
            return None;
        }

        match serde_json::from_str::<BrokerResponse>(line.trim()).ok()? {
            BrokerResponse::Ok {
                payload: ResponsePayload::StatusResult { workers },
            } => workers
                .into_iter()
                .find(|w| w.id == worker_id)
                .map(|w| w.control_state),
            _ => None,
        }
    };

    tokio::time::timeout(STOP_HOOK_QUERY_TIMEOUT, exchange)
        .await
        .ok()
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixListener;

    /// One-shot mock broker: binds `socket` synchronously (so the file exists
    /// before the caller connects), then accepts a single connection, drains
    /// the request line, and replies with `response_json`.
    fn spawn_mock_broker(socket: &Path, response_json: String) {
        let listener = UnixListener::bind(socket).unwrap();
        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let (reader, mut writer) = stream.into_split();
                let mut reader = BufReader::new(reader);
                let mut line = String::new();
                let _ = reader.read_line(&mut line).await;
                let mut body = response_json.into_bytes();
                body.push(b'\n');
                let _ = writer.write_all(&body).await;
            }
        });
    }

    /// Mirror of the real broker's `StatusResult` wire shape for one worker.
    /// A `WorkerStatus` serializes without `Worker`'s required `description` /
    /// `capabilities` / `expires_at`, so untagged dispatch skips `WorkerList`
    /// and lands on `StatusResult` — this test locks that behavior.
    fn status_response(worker_id: &str, control_state: &str) -> String {
        serde_json::json!({
            "status": "ok",
            "workers": [{
                "id": worker_id,
                "name": "alice",
                "role": "runner",
                "control_state": control_state,
            }],
        })
        .to_string()
    }

    #[tokio::test]
    async fn query_control_state_returns_active() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("active.sock");
        spawn_mock_broker(&sock, status_response("w1", "active"));
        assert_eq!(
            query_control_state(&sock, "w1").await,
            Some(ControlState::Active)
        );
    }

    #[tokio::test]
    async fn query_control_state_returns_stopping() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("stopping.sock");
        spawn_mock_broker(&sock, status_response("w1", "stopping"));
        assert_eq!(
            query_control_state(&sock, "w1").await,
            Some(ControlState::Stopping)
        );
    }

    /// An empty worker list (worker not registered / already finalized) maps to
    /// `None` → allow stop.
    #[tokio::test]
    async fn query_control_state_none_when_worker_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("empty.sock");
        let resp = serde_json::json!({ "status": "ok", "workers": [] }).to_string();
        spawn_mock_broker(&sock, resp);
        assert_eq!(query_control_state(&sock, "w1").await, None);
    }

    /// No broker listening on the socket → `None` (failsafe: allow stop).
    #[tokio::test]
    async fn query_control_state_none_when_unreachable() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("missing.sock");
        assert_eq!(query_control_state(&sock, "w1").await, None);
    }

    /// Env-var precedence: when DISPATCH_SOCKET_PATH is set, resolution
    /// returns it verbatim without touching the config file. Exercises
    /// `resolve_socket_path_with_env` so tests don't race on the
    /// process-global env (which `std::env::set_var` is documented to
    /// require serialising across threads).
    #[tokio::test]
    async fn resolve_socket_path_prefers_env_var() {
        let tmp = tempfile::tempdir().unwrap();
        let explicit = tmp.path().join("from-env.sock");
        let resolved =
            resolve_socket_path_with_env(Some(&explicit.display().to_string()), tmp.path());
        assert_eq!(resolved, Some(explicit));
    }

    /// Empty env value falls through to config-derived resolution, same as
    /// an unset variable.
    #[tokio::test]
    async fn resolve_socket_path_empty_env_falls_through() {
        let tmp = tempfile::tempdir().unwrap();
        let resolved = resolve_socket_path_with_env(Some(""), tmp.path());
        // No config in tmp → config resolver derives cell_id from the path,
        // so we still get *some* path back (not the empty-string one).
        match resolved {
            Some(p) => assert!(!p.as_os_str().is_empty()),
            None => panic!("expected a derived path, not None"),
        }
    }
}
