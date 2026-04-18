//! Hook subcommands for Codex and Claude Code CLIs.
//!
//! The `codex-hook` and `claude-hook` subcommand families let an agent's
//! vendor CLI call back into dispatch on lifecycle events. The Stop hook is
//! the important one: by printing `{"decision":"block","reason":"..."}` on
//! stdout we tell the agent to stay alive and keep polling dispatch for new
//! messages instead of exiting at the end of a turn.
//!
//! The stop handler is broker-liveness-aware: if the socket is unreachable
//! (dispatch is shutting down / was never started / the user exited serve),
//! the hook emits nothing and exits 0 so the vendor CLI is free to stop the
//! agent cleanly. Only a connectable broker earns the block decision.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::net::UnixStream;

pub mod claude;
pub mod codex;

/// Reason string returned to the vendor CLI telling the agent to keep
/// listening for more dispatch messages instead of stopping.
///
/// Kept short and imperative so the LLM treats it as a continuation
/// instruction rather than conversational text.
pub const CONTINUE_REASON: &str = "The dispatch broker may still have work queued for you. Do not stop yet — call `dispatch listen` again with your registered worker_id and a timeout (e.g. 600) to wait for the next message. When a message arrives, process it and then return to this listen step.";

/// Upper bound on how long the stop hook waits for a connect before giving
/// up. Short enough that a dead broker doesn't stall the vendor CLI, long
/// enough that an alive broker under light contention still answers.
const PROBE_TIMEOUT: Duration = Duration::from_millis(250);

/// JSON body every stop hook emits on stdout. Both vendors accept this shape.
pub fn stop_decision_json() -> String {
    serde_json::json!({
        "decision": "block",
        "reason": CONTINUE_REASON,
    })
    .to_string()
}

/// Resolve the broker socket path for the stop hook.
///
/// Precedence mirrors the regular CLI client:
/// 1. `DISPATCH_SOCKET_PATH` env var — set by the orchestrator when it
///    spawns managed agents, so hooks invoked from a managed vendor CLI
///    always land on the right socket.
/// 2. Config-derived default — find `dispatch.config.toml` from `cwd`,
///    derive the cell ID, and compute the standard socket path. Covers
///    hooks invoked from manually-launched (`launch = false`) agents that
///    inherit the shell's env but not the orchestrator's.
/// 3. `None` — caller treats as "broker unreachable" and allows the stop.
pub fn resolve_socket_path(cwd: &Path) -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("DISPATCH_SOCKET_PATH") {
        return Some(PathBuf::from(explicit));
    }
    let (_, project_root) = crate::config::find_config_file(cwd)?;
    let cell_id = crate::config::derive_cell_id(&project_root);
    Some(crate::backend::local::socket_path(&project_root, &cell_id))
}

/// Check whether a dispatch broker is accepting connections at `path`.
///
/// Returns `true` only if a connect lands within `PROBE_TIMEOUT`. Every
/// failure mode (`NotFound`, `ConnectionRefused`, timeout, permission
/// denied, etc.) collapses to `false` — the stop hook treats any inability
/// to reach the broker as "allow stop" so the vendor CLI isn't held hostage
/// by a dead socket.
pub async fn broker_is_alive(path: &Path) -> bool {
    matches!(
        tokio::time::timeout(PROBE_TIMEOUT, UnixStream::connect(path)).await,
        Ok(Ok(_))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tokio::net::UnixListener;

    #[tokio::test]
    async fn broker_is_alive_true_for_listening_socket() {
        let tmp = TempDir::new().unwrap();
        let sock = tmp.path().join("alive.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        assert!(broker_is_alive(&sock).await);
        drop(listener);
    }

    #[tokio::test]
    async fn broker_is_alive_false_for_missing_socket() {
        let tmp = TempDir::new().unwrap();
        let sock = tmp.path().join("nope.sock");
        assert!(!broker_is_alive(&sock).await);
    }

    #[tokio::test]
    async fn broker_is_alive_false_after_listener_drops() {
        let tmp = TempDir::new().unwrap();
        let sock = tmp.path().join("stale.sock");
        {
            let _listener = UnixListener::bind(&sock).unwrap();
            // listener drops here; kernel refuses subsequent connects
        }
        assert!(!broker_is_alive(&sock).await);
    }

    #[tokio::test]
    async fn broker_is_alive_false_for_non_socket_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("just-a-file.txt");
        std::fs::write(&path, b"not a socket").unwrap();
        assert!(!broker_is_alive(&path).await);
    }

    #[test]
    fn resolve_socket_path_prefers_env_var() {
        let tmp = TempDir::new().unwrap();
        let expected = tmp.path().join("override.sock");
        // Env tests run serialised via the ENV_LOCK to keep concurrent test
        // threads from clobbering each other's DISPATCH_SOCKET_PATH.
        let _guard = env_lock();
        std::env::set_var("DISPATCH_SOCKET_PATH", &expected);
        let resolved = resolve_socket_path(tmp.path());
        std::env::remove_var("DISPATCH_SOCKET_PATH");
        assert_eq!(resolved, Some(expected));
    }

    #[test]
    fn resolve_socket_path_returns_none_without_config_or_env() {
        let tmp = TempDir::new().unwrap();
        let _guard = env_lock();
        std::env::remove_var("DISPATCH_SOCKET_PATH");
        assert_eq!(resolve_socket_path(tmp.path()), None);
    }

    /// Serialise env-touching tests in this module so parallel threads
    /// don't clobber each other's `DISPATCH_SOCKET_PATH`.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }
}
