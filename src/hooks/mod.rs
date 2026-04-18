//! Hook subcommands for Codex and Claude Code CLIs.
//!
//! The `codex-hook` and `claude-hook` subcommand families let an agent's
//! vendor CLI call back into dispatch on lifecycle events. The Stop hook is
//! the important one: by printing `{"decision":"block","reason":"..."}` on
//! stdout we tell the agent to stay alive and keep polling dispatch for new
//! messages instead of exiting at the end of a turn.

pub mod claude;
pub mod codex;

/// Reason string returned to the vendor CLI telling the agent to keep
/// listening for more dispatch messages instead of stopping.
///
/// Kept short and imperative so the LLM treats it as a continuation
/// instruction rather than conversational text.
pub const CONTINUE_REASON: &str = "The dispatch broker may still have work queued for you. Do not stop yet — call `dispatch listen` again with your registered worker_id and a timeout (e.g. 600) to wait for the next message. When a message arrives, process it and then return to this listen step.";

/// JSON body every stop hook emits on stdout. Both vendors accept this shape.
pub fn stop_decision_json() -> String {
    serde_json::json!({
        "decision": "block",
        "reason": CONTINUE_REASON,
    })
    .to_string()
}
