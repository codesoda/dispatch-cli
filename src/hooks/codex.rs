//! Codex CLI hook integration.
//!
//! Codex reads `<repo>/.codex/hooks.json` when `features.codex_hooks = true`
//! is set in `<repo>/.codex/config.toml`. The Stop hook we register runs
//! `dispatch codex-hook stop` which prints a block decision — keeping the
//! agent alive for the next dispatch message.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

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

/// Merge the dispatch Stop hook into `.codex/hooks.json` (preserving any
/// other Stop entries and any other top-level hook categories the user has
/// registered) and enable `features.codex_hooks = true` in
/// `.codex/config.toml`. Creates `.codex/` if it doesn't exist.
///
/// Idempotent: re-running does not duplicate our Stop entry and does not
/// duplicate the `codex_hooks = true` line.
pub async fn install(cwd: &Path) -> Result<PathBuf, DispatchError> {
    let codex_dir = cwd.join(".codex");
    tokio::fs::create_dir_all(&codex_dir).await?;

    let hooks_path = hooks_path(cwd);
    let mut root = load_json(&hooks_path).await?;
    let obj = root
        .as_object_mut()
        .ok_or_else(|| DispatchError::ConfigInvalid {
            path: hooks_path.clone(),
            reason: "expected a JSON object at the top level".into(),
        })?;

    let stop = obj
        .entry("Stop".to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    let stop_arr = stop
        .as_array_mut()
        .ok_or_else(|| DispatchError::ConfigInvalid {
            path: hooks_path.clone(),
            reason: "expected \"Stop\" to be an array".into(),
        })?;

    if !stop_arr.iter().any(entry_is_dispatch_hook) {
        stop_arr.push(serde_json::json!({ "command": HOOK_COMMAND }));
    }

    tokio::fs::write(&hooks_path, serde_json::to_string_pretty(&root)? + "\n").await?;

    let config_path = config_path(cwd);
    let existing = match tokio::fs::read_to_string(&config_path).await {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(DispatchError::Io(e)),
    };
    let updated = ensure_codex_hooks_feature(&existing);
    if updated != existing {
        tokio::fs::write(&config_path, updated).await?;
    }

    Ok(hooks_path)
}

/// Remove the dispatch Stop entry from `.codex/hooks.json`. If that leaves
/// the Stop array empty, drop the key; if the file is empty afterwards,
/// delete it. Leaves `config.toml` alone so the user can keep the feature
/// flag enabled for their own hooks. Returns `Ok(None)` when nothing
/// dispatch-owned was present.
pub async fn uninstall(cwd: &Path) -> Result<Option<PathBuf>, DispatchError> {
    let path = hooks_path(cwd);
    if !tokio_file_exists(&path).await {
        return Ok(None);
    }

    let mut root = load_json(&path).await?;
    let Some(obj) = root.as_object_mut() else {
        return Ok(None);
    };
    let Some(stop) = obj.get_mut("Stop").and_then(Value::as_array_mut) else {
        return Ok(None);
    };

    let before = stop.len();
    stop.retain(|entry| !entry_is_dispatch_hook(entry));
    let removed = stop.len() != before;

    if stop.is_empty() {
        obj.remove("Stop");
    }

    if !removed {
        return Ok(None);
    }

    if obj.is_empty() {
        tokio::fs::remove_file(&path).await?;
    } else {
        tokio::fs::write(&path, serde_json::to_string_pretty(&root)? + "\n").await?;
    }
    Ok(Some(path))
}

/// Whether a hook entry is the one dispatch owns. Tolerates alternate
/// representations: `command` as an array (the shape we write) or as the
/// single-string form some examples in codex docs use.
fn entry_is_dispatch_hook(entry: &Value) -> bool {
    let Some(cmd) = entry.get("command") else {
        return false;
    };
    if let Some(arr) = cmd.as_array() {
        if arr.len() != HOOK_COMMAND.len() {
            return false;
        }
        return arr
            .iter()
            .zip(HOOK_COMMAND.iter())
            .all(|(a, b)| a.as_str() == Some(*b));
    }
    if let Some(s) = cmd.as_str() {
        return s == HOOK_COMMAND.join(" ");
    }
    false
}

/// Read `path` into a JSON value. Missing files and empty files both parse
/// as an empty object so callers can treat the "no existing config" path
/// symmetrically with "existing config".
async fn load_json(path: &Path) -> Result<Value, DispatchError> {
    match tokio::fs::read_to_string(path).await {
        Ok(s) if s.trim().is_empty() => Ok(Value::Object(Map::new())),
        Ok(s) => serde_json::from_str(&s).map_err(DispatchError::Serialization),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Value::Object(Map::new())),
        Err(e) => Err(DispatchError::Io(e)),
    }
}

/// Non-throwing existence check — avoids a TOCTOU race with the caller's
/// subsequent read, which handles `NotFound` itself.
async fn tokio_file_exists(path: &Path) -> bool {
    tokio::fs::metadata(path).await.is_ok()
}

/// Append `[features] codex_hooks = true` to `config.toml` if the flag isn't
/// already present. Uses a simple string merge rather than round-tripping
/// the TOML AST — so comments, ordering, and formatting of unrelated config
/// are preserved. Line-ending agnostic: walks `split_inclusive('\n')` so
/// CRLF-terminated files aren't split mid-pair when we compute the
/// insertion offset.
fn ensure_codex_hooks_feature(existing: &str) -> String {
    if existing
        .lines()
        .any(|l| l.trim_start().starts_with("codex_hooks") && l.contains("true"))
    {
        return existing.to_string();
    }

    // Walk sections, preserving the original line terminators so byte
    // offsets stay correct on both LF and CRLF files.
    let mut in_features = false;
    let mut features_end: Option<usize> = None;
    let mut offset = 0usize;
    for segment in existing.split_inclusive('\n') {
        let trimmed = segment.trim_end_matches(['\r', '\n']).trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            if in_features {
                features_end = Some(offset);
                break;
            }
            if trimmed == "[features]" {
                in_features = true;
            }
        }
        offset += segment.len();
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

    #[tokio::test]
    async fn install_creates_both_files() {
        let dir = tempdir().unwrap();
        let hooks_path = install(dir.path()).await.unwrap();
        assert!(hooks_path.ends_with(".codex/hooks.json"));
        let content = tokio::fs::read_to_string(&hooks_path).await.unwrap();
        assert!(content.contains("codex-hook"));
        let cfg = tokio::fs::read_to_string(dir.path().join(".codex/config.toml"))
            .await
            .unwrap();
        assert!(cfg.contains("[features]"));
        assert!(cfg.contains("codex_hooks = true"));
    }

    #[tokio::test]
    async fn install_preserves_existing_config() {
        let dir = tempdir().unwrap();
        tokio::fs::create_dir_all(dir.path().join(".codex"))
            .await
            .unwrap();
        let original = "# user comment\n[profile]\nmodel = \"gpt-5\"\n";
        tokio::fs::write(dir.path().join(".codex/config.toml"), original)
            .await
            .unwrap();
        install(dir.path()).await.unwrap();
        let cfg = tokio::fs::read_to_string(dir.path().join(".codex/config.toml"))
            .await
            .unwrap();
        assert!(cfg.contains("# user comment"));
        assert!(cfg.contains("[profile]"));
        assert!(cfg.contains("codex_hooks = true"));
    }

    #[tokio::test]
    async fn install_is_idempotent_on_feature_flag() {
        let dir = tempdir().unwrap();
        tokio::fs::create_dir_all(dir.path().join(".codex"))
            .await
            .unwrap();
        let original = "[features]\ncodex_hooks = true\n";
        tokio::fs::write(dir.path().join(".codex/config.toml"), original)
            .await
            .unwrap();
        install(dir.path()).await.unwrap();
        let cfg = tokio::fs::read_to_string(dir.path().join(".codex/config.toml"))
            .await
            .unwrap();
        assert_eq!(cfg.matches("codex_hooks").count(), 1);
    }

    /// Pre-existing Stop entries (different command) and other top-level
    /// hook categories must survive a dispatch install.
    #[tokio::test]
    async fn install_preserves_unrelated_hook_entries() {
        let dir = tempdir().unwrap();
        tokio::fs::create_dir_all(dir.path().join(".codex"))
            .await
            .unwrap();
        let pre = serde_json::json!({
            "PreCompact": [
                { "command": ["echo", "user-hook"] }
            ],
            "Stop": [
                { "command": ["echo", "existing-stop"] }
            ]
        });
        tokio::fs::write(
            dir.path().join(".codex/hooks.json"),
            serde_json::to_string_pretty(&pre).unwrap(),
        )
        .await
        .unwrap();

        install(dir.path()).await.unwrap();
        let content = tokio::fs::read_to_string(dir.path().join(".codex/hooks.json"))
            .await
            .unwrap();
        let value: Value = serde_json::from_str(&content).unwrap();
        // User's PreCompact hook intact.
        assert_eq!(value["PreCompact"][0]["command"][0], "echo");
        // User's existing Stop entry still present, ours appended.
        let stops = value["Stop"].as_array().unwrap();
        assert_eq!(stops.len(), 2);
        assert!(stops.iter().any(entry_is_dispatch_hook));
    }

    #[tokio::test]
    async fn install_is_idempotent_on_stop_entry() {
        let dir = tempdir().unwrap();
        install(dir.path()).await.unwrap();
        install(dir.path()).await.unwrap();
        let content = tokio::fs::read_to_string(dir.path().join(".codex/hooks.json"))
            .await
            .unwrap();
        let value: Value = serde_json::from_str(&content).unwrap();
        assert_eq!(value["Stop"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn uninstall_removes_dispatch_entry_only() {
        let dir = tempdir().unwrap();
        tokio::fs::create_dir_all(dir.path().join(".codex"))
            .await
            .unwrap();
        let pre = serde_json::json!({
            "Stop": [
                { "command": ["echo", "user-stop"] }
            ]
        });
        tokio::fs::write(
            dir.path().join(".codex/hooks.json"),
            serde_json::to_string_pretty(&pre).unwrap(),
        )
        .await
        .unwrap();

        install(dir.path()).await.unwrap();
        uninstall(dir.path()).await.unwrap();

        let content = tokio::fs::read_to_string(dir.path().join(".codex/hooks.json"))
            .await
            .unwrap();
        let value: Value = serde_json::from_str(&content).unwrap();
        let stops = value["Stop"].as_array().unwrap();
        assert_eq!(stops.len(), 1);
        assert_eq!(stops[0]["command"][0], "echo");
    }

    #[tokio::test]
    async fn uninstall_removes_hooks_file_when_empty() {
        let dir = tempdir().unwrap();
        install(dir.path()).await.unwrap();
        let removed = uninstall(dir.path()).await.unwrap();
        assert!(removed.is_some());
        assert!(!dir.path().join(".codex/hooks.json").exists());
        // config.toml kept intentionally.
        assert!(dir.path().join(".codex/config.toml").exists());
    }

    #[tokio::test]
    async fn uninstall_noop_when_nothing_to_remove() {
        let dir = tempdir().unwrap();
        let removed = uninstall(dir.path()).await.unwrap();
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

    /// CRLF-terminated config.toml must produce valid TOML after the
    /// merge — the offset walk must consume `\r\n` as a single line
    /// terminator rather than splitting mid-pair.
    #[test]
    fn handles_crlf_line_endings() {
        let input = "[profile]\r\nmodel = \"gpt-5\"\r\n[features]\r\nother = true\r\n";
        let out = ensure_codex_hooks_feature(input);
        assert!(out.contains("codex_hooks = true"));
        // Round-trip the result through a TOML parser to prove the file
        // is still syntactically valid.
        let parsed: toml::Value = toml::from_str(&out).expect("CRLF merge must yield valid TOML");
        assert_eq!(
            parsed["features"]["codex_hooks"],
            toml::Value::Boolean(true)
        );
        assert_eq!(parsed["features"]["other"], toml::Value::Boolean(true));
    }

    /// Rejects a hooks.json whose top-level JSON is not an object (array,
    /// scalar, etc.) rather than panicking via unwrap.
    #[tokio::test]
    async fn install_rejects_non_object_root() {
        let dir = tempdir().unwrap();
        tokio::fs::create_dir_all(dir.path().join(".codex"))
            .await
            .unwrap();
        tokio::fs::write(dir.path().join(".codex/hooks.json"), b"[]")
            .await
            .unwrap();
        let err = install(dir.path()).await.unwrap_err();
        assert!(matches!(err, DispatchError::ConfigInvalid { .. }));
    }
}
