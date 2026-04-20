//! Adapter abstraction for launching dispatch agents.
//!
//! Each agent in `dispatch.config.toml` selects an adapter (`command`, `claude`,
//! `codex`). Adapters encapsulate vendor-specific launch concerns: which binary
//! to run, which args to inject, how the prompt is supplied.
//!
//! This module exposes only the launch-spec assembly. Env injection, cwd,
//! stdout/stderr wiring, and process supervision are handled by the caller
//! (typically the orchestrator).

use std::path::{Path, PathBuf};

use serde::Deserialize;

pub mod claude;
pub mod codex;
pub mod command;

/// Which adapter an agent uses to launch.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Adapter {
    Command,
    Claude,
    Codex,
}

impl std::fmt::Display for Adapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Adapter::Command => write!(f, "command"),
            Adapter::Claude => write!(f, "claude"),
            Adapter::Codex => write!(f, "codex"),
        }
    }
}

/// Inputs an adapter needs to assemble a launch specification.
pub struct BuildContext<'a> {
    /// User-supplied extra args, appended to the adapter-assembled argv.
    pub extra_args: &'a [String],
    /// Path to a prompt file, if the agent has one. Adapters that consume
    /// prompts via stdin (claude, codex) populate `Launch::stdin_file`.
    pub prompt_file: Option<&'a Path>,
    /// Inline prompt text — used by the `command` adapter for `{prompt}`
    /// substitution in the user's command string.
    pub prompt_inline: Option<&'a str>,
    /// Full shell command — only used by the `command` adapter.
    pub command_string: Option<&'a str>,
}

/// The result of an adapter building a launch specification.
///
/// Callers translate this into a concrete `tokio::process::Command` and add
/// env vars, cwd, and stdout/stderr wiring.
#[derive(Debug)]
pub struct Launch {
    /// Program to exec (e.g. `claude`, `codex`, or `sh` for the command adapter).
    pub program: String,
    /// Argv excluding `program`.
    pub args: Vec<String>,
    /// Authoritative: when true, `program == "sh"` and `args` is `["-c", <cmd>]`.
    /// Callers that render a shell-pasteable string rely on this flag to decide
    /// whether to emit the raw command (true) or quote each argv element (false).
    pub wrap_in_shell: bool,
    /// File to read as stdin for the spawned process. None = inherit/null
    /// (caller decides).
    pub stdin_file: Option<PathBuf>,
}

#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    #[error("command adapter requires `command = \"...\"` in agent config")]
    MissingCommandString,
    #[error("`{{prompt_file}}` token used in command but no `prompt_file` is configured")]
    MissingPromptFile,
    #[error("`{{prompt}}` token used in command but no `prompt` is configured")]
    MissingPromptInline,
    #[error("hook install not supported for the `command` adapter")]
    HookInstallNotSupported,
    #[error("hook install not yet implemented for adapter `{0}`")]
    HookInstallPending(Adapter),
    #[error("hook uninstall not supported for the `command` adapter")]
    HookUninstallNotSupported,
    #[error("hook uninstall not yet implemented for adapter `{0}`")]
    HookUninstallPending(Adapter),
}

/// POSIX single-quote shell escape. Always safe to paste into `sh -c`.
pub(crate) fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Quote a shell argument only when necessary. Plain alphanumerics, dashes,
/// dots, slashes, underscores, and equals signs pass through unquoted for
/// paste-friendly output.
pub(crate) fn shell_arg_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_/.=".contains(c))
    {
        s.to_string()
    } else {
        shell_escape(s)
    }
}

impl Adapter {
    /// Assemble a launch spec for this adapter.
    pub fn build(&self, ctx: &BuildContext<'_>) -> Result<Launch, AdapterError> {
        match self {
            Adapter::Command => command::build(ctx),
            Adapter::Claude => claude::build(ctx),
            Adapter::Codex => codex::build(ctx),
        }
    }

    /// Install the vendor stop-hook in the given repo root.
    /// (Deferred to a later step — returns a pending error for now.)
    pub fn hook_install(&self, _repo_root: &Path) -> Result<PathBuf, AdapterError> {
        match self {
            Adapter::Command => Err(AdapterError::HookInstallNotSupported),
            other => Err(AdapterError::HookInstallPending(*other)),
        }
    }

    /// Remove the vendor stop-hook from the given repo root.
    /// (Deferred to a later step.)
    pub fn hook_uninstall(&self, _repo_root: &Path) -> Result<(), AdapterError> {
        match self {
            Adapter::Command => Err(AdapterError::HookUninstallNotSupported),
            other => Err(AdapterError::HookUninstallPending(*other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, serde::Deserialize)]
    struct Wrapper {
        a: Adapter,
    }

    #[test]
    fn adapter_parses_from_toml_lowercase() {
        assert_eq!(
            toml::from_str::<Wrapper>("a = \"command\"").unwrap().a,
            Adapter::Command
        );
        assert_eq!(
            toml::from_str::<Wrapper>("a = \"claude\"").unwrap().a,
            Adapter::Claude
        );
        assert_eq!(
            toml::from_str::<Wrapper>("a = \"codex\"").unwrap().a,
            Adapter::Codex
        );
    }

    #[test]
    fn adapter_rejects_unknown_variant() {
        let err = toml::from_str::<Wrapper>("a = \"gpt\"").unwrap_err();
        assert!(err.to_string().to_lowercase().contains("expected"));
    }

    #[test]
    fn display_matches_serde_name() {
        assert_eq!(Adapter::Command.to_string(), "command");
        assert_eq!(Adapter::Claude.to_string(), "claude");
        assert_eq!(Adapter::Codex.to_string(), "codex");
    }

    #[test]
    fn hook_install_command_rejects() {
        let tmp = std::env::temp_dir();
        assert!(matches!(
            Adapter::Command.hook_install(&tmp),
            Err(AdapterError::HookInstallNotSupported)
        ));
    }

    #[test]
    fn hook_install_claude_pending() {
        let tmp = std::env::temp_dir();
        assert!(matches!(
            Adapter::Claude.hook_install(&tmp),
            Err(AdapterError::HookInstallPending(Adapter::Claude))
        ));
    }

    #[test]
    fn hook_install_codex_pending() {
        let tmp = std::env::temp_dir();
        assert!(matches!(
            Adapter::Codex.hook_install(&tmp),
            Err(AdapterError::HookInstallPending(Adapter::Codex))
        ));
    }

    #[test]
    fn hook_uninstall_command_rejects() {
        let tmp = std::env::temp_dir();
        assert!(matches!(
            Adapter::Command.hook_uninstall(&tmp),
            Err(AdapterError::HookUninstallNotSupported)
        ));
    }

    #[test]
    fn hook_uninstall_claude_pending() {
        let tmp = std::env::temp_dir();
        assert!(matches!(
            Adapter::Claude.hook_uninstall(&tmp),
            Err(AdapterError::HookUninstallPending(Adapter::Claude))
        ));
    }

    #[test]
    fn hook_uninstall_codex_pending() {
        let tmp = std::env::temp_dir();
        assert!(matches!(
            Adapter::Codex.hook_uninstall(&tmp),
            Err(AdapterError::HookUninstallPending(Adapter::Codex))
        ));
    }
}
