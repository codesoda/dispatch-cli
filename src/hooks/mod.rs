//! Hook subcommands for Codex and Claude Code CLIs.
//!
//! The `codex-hook` and `claude-hook` subcommand families let an agent's
//! vendor CLI call back into dispatch on lifecycle events. The Stop hook is
//! the important one: by printing `{"decision":"block","reason":"..."}` on
//! stdout we tell the agent to stay alive and keep polling dispatch for new
//! messages instead of exiting at the end of a turn.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::net::UnixStream;

use crate::backend::socket_path;
use crate::config::resolve_config;

pub mod claude;
pub mod codex;

/// Reason string returned to the vendor CLI telling the agent to keep
/// listening for more dispatch messages instead of stopping.
///
/// Kept short and imperative so the LLM treats it as a continuation
/// instruction rather than conversational text.
pub const CONTINUE_REASON: &str = "The dispatch broker may still have work queued for you. Do not stop yet — call `dispatch listen` again with your registered worker_id and a timeout (e.g. 600) to wait for the next message. When a message arrives, process it and then return to this listen step.";

/// How long the stop hook waits for a connection to the broker socket
/// before treating it as unreachable. Kept short because the hook is in
/// the critical path of every agent turn.
const PROBE_TIMEOUT: Duration = Duration::from_millis(250);

/// JSON body every stop hook emits on stdout. Both vendors accept this shape.
pub fn stop_decision_json() -> String {
    serde_json::json!({
        "decision": "block",
        "reason": CONTINUE_REASON,
    })
    .to_string()
}

/// Handler for both `dispatch codex-hook stop` and `dispatch claude-hook stop`.
///
/// Probes the broker socket; if a broker is reachable, prints the block
/// decision so the vendor keeps the agent alive. If the broker is
/// unreachable (dispatch shutdown, never started, config missing) we
/// print nothing and exit 0 — the vendor treats that as "allow stop".
///
/// Never returns an error: probe failures map to "allow stop".
pub async fn run_stop_hook(cwd: &Path) {
    match resolve_socket_path(cwd) {
        Some(path) => {
            if probe_broker(&path).await {
                tracing::debug!(path = %path.display(), "broker reachable; blocking stop");
                println!("{}", stop_decision_json());
            } else {
                tracing::debug!(path = %path.display(), "broker unreachable; allowing stop");
            }
        }
        None => {
            tracing::debug!("no socket path resolvable; allowing stop");
        }
    }
}

/// Resolve the broker socket path using the same precedence the regular
/// client uses: `DISPATCH_SOCKET_PATH` env var wins, otherwise fall back
/// to the config-derived path. Returns `None` if neither source yields a
/// path (e.g. config resolution fails outside a project).
fn resolve_socket_path(cwd: &Path) -> Option<PathBuf> {
    if let Ok(env_path) = std::env::var("DISPATCH_SOCKET_PATH") {
        if !env_path.is_empty() {
            return Some(PathBuf::from(env_path));
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

/// Attempt a short-timeout connect to the broker's Unix socket.
/// Returns `true` only if a connection succeeds within `PROBE_TIMEOUT`.
async fn probe_broker(path: &Path) -> bool {
    matches!(
        tokio::time::timeout(PROBE_TIMEOUT, UnixStream::connect(path)).await,
        Ok(Ok(_))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixListener;

    #[tokio::test]
    async fn probe_returns_true_when_broker_listens() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("live.sock");
        let _listener = UnixListener::bind(&sock).unwrap();
        assert!(probe_broker(&sock).await);
    }

    #[tokio::test]
    async fn probe_returns_false_when_socket_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("missing.sock");
        assert!(!probe_broker(&sock).await);
    }

    #[tokio::test]
    async fn probe_returns_false_when_socket_stale() {
        // File exists but nothing is bound — UnixStream::connect fails with
        // ConnectionRefused. We treat that as "broker unreachable".
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("stale.sock");
        std::fs::write(&sock, b"").unwrap();
        assert!(!probe_broker(&sock).await);
    }

    #[tokio::test]
    async fn resolve_socket_path_prefers_env_var() {
        let tmp = tempfile::tempdir().unwrap();
        let explicit = tmp.path().join("from-env.sock");
        // Use a test-local env mutation guard; std::env is process-global so
        // we set and then unset within the same test. Serial in practice
        // because tokio::test uses the current_thread runtime.
        // SAFETY: tests are single-threaded within a single `#[tokio::test]`.
        unsafe {
            std::env::set_var("DISPATCH_SOCKET_PATH", &explicit);
        }
        let resolved = resolve_socket_path(tmp.path());
        unsafe {
            std::env::remove_var("DISPATCH_SOCKET_PATH");
        }
        assert_eq!(resolved, Some(explicit));
    }
}
