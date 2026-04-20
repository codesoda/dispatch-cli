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
///
/// Prefixed with an "if you are a dispatch agent" guard because both vendor
/// stop hooks fire for *every* claude/codex session inside a repo where the
/// hook is installed — including ad-hoc sessions the user runs by hand. A
/// non-dispatch session that reads this should treat it as a no-op.
pub const CONTINUE_REASON: &str = "If you are a dispatch agent (you have a registered worker_id from `dispatch register`): the dispatch broker may still have work queued for you. Do not stop yet — call `dispatch listen` again with your worker_id and a timeout (e.g. 600) to wait for the next message. When a message arrives, process it and then return to this listen step.\n\nIf you are NOT a dispatch agent (you have no worker_id and the user did not start you via dispatch), ignore this notice and stop normally.";

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
