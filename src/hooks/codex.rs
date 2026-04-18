//! Codex CLI hook integration.
//!
//! Codex reads `<repo>/.codex/hooks.json` when `features.codex_hooks = true`
//! is set in `<repo>/.codex/config.toml`. The Stop hook we register runs
//! `dispatch codex-hook stop` which prints a block decision — keeping the
//! agent alive for the next dispatch message.

use std::path::{Path, PathBuf};

use crate::errors::DispatchError;

const HOOK_COMMAND: [&str; 3] = ["dispatch", "codex-hook", "stop"];

/// Location of the hooks manifest relative to the project root.
fn hooks_path(cwd: &Path) -> PathBuf {
    cwd.join(".codex").join("hooks.json")
}

/// Location of the codex config file where the hooks feature flag lives.
fn config_path(cwd: &Path) -> PathBuf {
    cwd.join(".codex").join("config.toml")
}

/// Write `.codex/hooks.json` and enable `features.codex_hooks = true` in
/// `.codex/config.toml`. Creates `.codex/` if it doesn't exist. Existing
/// config.toml content is preserved; we only insert/update the feature flag.
pub fn install(cwd: &Path) -> Result<PathBuf, DispatchError> {
    let codex_dir = cwd.join(".codex");
    std::fs::create_dir_all(&codex_dir).map_err(DispatchError::Io)?;

    let hooks = serde_json::json!({
        "Stop": [
            { "command": HOOK_COMMAND }
        ],
    });
    let hooks_path = hooks_path(cwd);
    std::fs::write(
        &hooks_path,
        serde_json::to_string_pretty(&hooks).unwrap() + "\n",
    )
    .map_err(DispatchError::Io)?;

    let config_path = config_path(cwd);
    let existing = std::fs::read_to_string(&config_path).unwrap_or_default();
    let updated = ensure_codex_hooks_feature(&existing);
    if updated != existing {
        std::fs::write(&config_path, updated).map_err(DispatchError::Io)?;
    }

    Ok(hooks_path)
}

/// Remove `.codex/hooks.json` if it exists. Leaves `config.toml` alone so the
/// user can keep the feature flag enabled for their own hooks. Returns
/// `Ok(None)` if nothing was there to remove.
pub fn uninstall(cwd: &Path) -> Result<Option<PathBuf>, DispatchError> {
    let path = hooks_path(cwd);
    if path.exists() {
        std::fs::remove_file(&path).map_err(DispatchError::Io)?;
        Ok(Some(path))
    } else {
        Ok(None)
    }
}

/// Append `[features] codex_hooks = true` to `config.toml` if the flag isn't
/// already present. Uses a simple string merge rather than round-tripping the
/// TOML AST — so comments, ordering, and formatting of unrelated config are
/// preserved.
fn ensure_codex_hooks_feature(existing: &str) -> String {
    if existing
        .lines()
        .any(|l| l.trim_start().starts_with("codex_hooks") && l.contains("true"))
    {
        return existing.to_string();
    }

    // Walk sections to see if `[features]` already exists.
    let mut in_features = false;
    let mut features_end: Option<usize> = None; // byte offset
    let mut offset = 0usize;
    for line in existing.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            if in_features {
                features_end = Some(offset);
                break;
            }
            if trimmed == "[features]" {
                in_features = true;
            }
        }
        offset += line.len() + 1; // +1 for the \n we'll re-add
    }
    if in_features && features_end.is_none() {
        features_end = Some(existing.len());
    }

    match (in_features, features_end) {
        (true, Some(end)) => {
            let mut out = String::with_capacity(existing.len() + 24);
            out.push_str(&existing[..end]);
            if !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str("codex_hooks = true\n");
            out.push_str(&existing[end..]);
            out
        }
        _ => {
            let mut out = existing.to_string();
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str("[features]\ncodex_hooks = true\n");
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn install_creates_both_files() {
        let dir = tempdir().unwrap();
        let hooks_path = install(dir.path()).unwrap();
        assert!(hooks_path.ends_with(".codex/hooks.json"));
        let content = std::fs::read_to_string(&hooks_path).unwrap();
        assert!(content.contains("codex-hook"));
        let cfg = std::fs::read_to_string(dir.path().join(".codex/config.toml")).unwrap();
        assert!(cfg.contains("[features]"));
        assert!(cfg.contains("codex_hooks = true"));
    }

    #[test]
    fn install_preserves_existing_config() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".codex")).unwrap();
        let original = "# user comment\n[profile]\nmodel = \"gpt-5\"\n";
        std::fs::write(dir.path().join(".codex/config.toml"), original).unwrap();
        install(dir.path()).unwrap();
        let cfg = std::fs::read_to_string(dir.path().join(".codex/config.toml")).unwrap();
        assert!(cfg.contains("# user comment"));
        assert!(cfg.contains("[profile]"));
        assert!(cfg.contains("codex_hooks = true"));
    }

    #[test]
    fn install_is_idempotent_on_feature_flag() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".codex")).unwrap();
        let original = "[features]\ncodex_hooks = true\n";
        std::fs::write(dir.path().join(".codex/config.toml"), original).unwrap();
        install(dir.path()).unwrap();
        let cfg = std::fs::read_to_string(dir.path().join(".codex/config.toml")).unwrap();
        // Should contain only one occurrence, not duplicated.
        assert_eq!(cfg.matches("codex_hooks").count(), 1);
    }

    #[test]
    fn uninstall_removes_hooks_file_only() {
        let dir = tempdir().unwrap();
        install(dir.path()).unwrap();
        let removed = uninstall(dir.path()).unwrap();
        assert!(removed.is_some());
        assert!(!dir.path().join(".codex/hooks.json").exists());
        // config.toml kept intentionally.
        assert!(dir.path().join(".codex/config.toml").exists());
    }

    #[test]
    fn uninstall_noop_when_nothing_to_remove() {
        let dir = tempdir().unwrap();
        let removed = uninstall(dir.path()).unwrap();
        assert!(removed.is_none());
    }

    #[test]
    fn inserts_flag_into_existing_features_section() {
        let input = "[features]\nother = true\n";
        let out = ensure_codex_hooks_feature(input);
        assert!(out.contains("[features]"));
        assert!(out.contains("other = true"));
        assert!(out.contains("codex_hooks = true"));
    }
}
