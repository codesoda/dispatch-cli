//! Claude Code CLI hook integration.
//!
//! Claude Code reads `<repo>/.claude/settings.json` (or `settings.local.json`)
//! for hook registration. We install a Stop hook that runs
//! `dispatch claude-hook stop`, which prints a block decision to keep the
//! agent alive for the next dispatch message.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::errors::DispatchError;

const HOOK_COMMAND: &str = "dispatch claude-hook stop";

/// Prefer `settings.local.json` because Claude Code treats it as user-private
/// (not committed). If the user has an existing `settings.json`, we still
/// merge our Stop hook into that file.
fn settings_path(cwd: &Path) -> PathBuf {
    let shared = cwd.join(".claude").join("settings.json");
    if shared.exists() {
        shared
    } else {
        cwd.join(".claude").join("settings.local.json")
    }
}

/// Install the Stop hook into `.claude/settings*.json`. Existing keys outside
/// our hook entry are preserved. Idempotent: re-running does not duplicate
/// the entry.
///
/// Refuses to modify a settings file whose top-level JSON isn't an object —
/// the caller gets a `ConfigInvalid` error instead of a panic.
pub async fn install(cwd: &Path) -> Result<PathBuf, DispatchError> {
    tokio::fs::create_dir_all(cwd.join(".claude")).await?;
    let path = settings_path(cwd);

    let mut root = load_json(&path).await?;
    let obj = root
        .as_object_mut()
        .ok_or_else(|| DispatchError::ConfigInvalid {
            path: path.clone(),
            reason: "expected a JSON object at the top level of settings.json".into(),
        })?;
    let hooks = ensure_object(obj, "hooks");
    let stop = ensure_array(hooks, "Stop");

    // Skip if any existing matcher contains our command.
    let already = stop.iter().any(|entry| {
        entry
            .get("hooks")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .any(|h| h.get("command").and_then(Value::as_str) == Some(HOOK_COMMAND))
            })
            .unwrap_or(false)
    });

    if !already {
        stop.push(serde_json::json!({
            "matcher": "",
            "hooks": [{
                "type": "command",
                "command": HOOK_COMMAND,
            }],
        }));
    }

    tokio::fs::write(&path, serde_json::to_string_pretty(&root)? + "\n").await?;
    Ok(path)
}

/// Remove our Stop hook entry. Leaves other hooks, matchers, and settings
/// untouched. Returns `Ok(None)` if no entry was found.
pub async fn uninstall(cwd: &Path) -> Result<Option<PathBuf>, DispatchError> {
    let path = settings_path(cwd);
    if !tokio_file_exists(&path).await {
        return Ok(None);
    }
    let mut root = load_json(&path).await?;
    let Some(root_obj) = root.as_object_mut() else {
        return Ok(None);
    };
    let Some(hooks) = root_obj.get_mut("hooks").and_then(Value::as_object_mut) else {
        return Ok(None);
    };
    let Some(stop) = hooks.get_mut("Stop").and_then(Value::as_array_mut) else {
        return Ok(None);
    };

    let before = stop.len();
    stop.retain(|entry| {
        let has_ours = entry
            .get("hooks")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .any(|h| h.get("command").and_then(Value::as_str) == Some(HOOK_COMMAND))
            })
            .unwrap_or(false);
        !has_ours
    });
    let removed_any = stop.len() != before;

    // Clean up empty keys so we don't leave dangling "hooks": {} behind.
    if stop.is_empty() {
        hooks.remove("Stop");
    }
    let hooks_empty = hooks.is_empty();
    if hooks_empty {
        root_obj.remove("hooks");
    }

    if !removed_any {
        return Ok(None);
    }

    if root_obj.is_empty() {
        tokio::fs::remove_file(&path).await?;
    } else {
        tokio::fs::write(&path, serde_json::to_string_pretty(&root)? + "\n").await?;
    }
    Ok(Some(path))
}

async fn load_json(path: &Path) -> Result<Value, DispatchError> {
    match tokio::fs::read_to_string(path).await {
        Ok(s) if s.trim().is_empty() => Ok(Value::Object(Map::new())),
        Ok(s) => serde_json::from_str(&s).map_err(DispatchError::Serialization),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Value::Object(Map::new())),
        Err(e) => Err(DispatchError::Io(e)),
    }
}

async fn tokio_file_exists(path: &Path) -> bool {
    tokio::fs::metadata(path).await.is_ok()
}

/// Ensure `map[key]` is a JSON object and return a mutable reference to it.
/// Overwrites non-object values at `key` — callers should have validated the
/// surrounding shape before invoking this.
fn ensure_object<'a>(map: &'a mut Map<String, Value>, key: &str) -> &'a mut Map<String, Value> {
    let current = map.get(key);
    if !matches!(current, Some(Value::Object(_))) {
        map.insert(key.to_string(), Value::Object(Map::new()));
    }
    map.get_mut(key)
        .and_then(Value::as_object_mut)
        .expect("just inserted an object")
}

fn ensure_array<'a>(map: &'a mut Map<String, Value>, key: &str) -> &'a mut Vec<Value> {
    let current = map.get(key);
    if !matches!(current, Some(Value::Array(_))) {
        map.insert(key.to_string(), Value::Array(Vec::new()));
    }
    map.get_mut(key)
        .and_then(Value::as_array_mut)
        .expect("just inserted an array")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn install_creates_settings_local() {
        let dir = tempdir().unwrap();
        let path = install(dir.path()).await.unwrap();
        assert!(path.ends_with(".claude/settings.local.json"));
        let content = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(content.contains("claude-hook"));
        assert!(content.contains("\"Stop\""));
    }

    #[tokio::test]
    async fn install_merges_into_existing_settings() {
        let dir = tempdir().unwrap();
        tokio::fs::create_dir_all(dir.path().join(".claude"))
            .await
            .unwrap();
        let original =
            "{\n  \"model\": \"sonnet\",\n  \"permissions\": {\"defaultMode\": \"bypass\"}\n}\n";
        tokio::fs::write(dir.path().join(".claude/settings.json"), original)
            .await
            .unwrap();
        let path = install(dir.path()).await.unwrap();
        assert!(path.ends_with(".claude/settings.json"));
        let value: Value =
            serde_json::from_str(&tokio::fs::read_to_string(&path).await.unwrap()).unwrap();
        assert_eq!(value["model"], "sonnet");
        assert_eq!(value["permissions"]["defaultMode"], "bypass");
        assert!(value["hooks"]["Stop"].is_array());
    }

    #[tokio::test]
    async fn install_is_idempotent() {
        let dir = tempdir().unwrap();
        install(dir.path()).await.unwrap();
        install(dir.path()).await.unwrap();
        let path = dir.path().join(".claude/settings.local.json");
        let value: Value =
            serde_json::from_str(&tokio::fs::read_to_string(&path).await.unwrap()).unwrap();
        assert_eq!(value["hooks"]["Stop"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn uninstall_leaves_other_settings() {
        let dir = tempdir().unwrap();
        tokio::fs::create_dir_all(dir.path().join(".claude"))
            .await
            .unwrap();
        tokio::fs::write(
            dir.path().join(".claude/settings.json"),
            "{\n  \"model\": \"sonnet\"\n}\n",
        )
        .await
        .unwrap();
        install(dir.path()).await.unwrap();
        uninstall(dir.path()).await.unwrap();
        let path = dir.path().join(".claude/settings.json");
        let value: Value =
            serde_json::from_str(&tokio::fs::read_to_string(&path).await.unwrap()).unwrap();
        assert_eq!(value["model"], "sonnet");
        assert!(value.get("hooks").is_none());
    }

    #[tokio::test]
    async fn uninstall_removes_empty_file() {
        let dir = tempdir().unwrap();
        install(dir.path()).await.unwrap();
        uninstall(dir.path()).await.unwrap();
        assert!(!dir.path().join(".claude/settings.local.json").exists());
    }

    #[tokio::test]
    async fn uninstall_noop_when_absent() {
        let dir = tempdir().unwrap();
        let result = uninstall(dir.path()).await.unwrap();
        assert!(result.is_none());
    }

    /// A settings.json whose root is a JSON array (not an object) must
    /// surface as `ConfigInvalid` rather than panicking via unwrap.
    #[tokio::test]
    async fn install_rejects_non_object_root() {
        let dir = tempdir().unwrap();
        tokio::fs::create_dir_all(dir.path().join(".claude"))
            .await
            .unwrap();
        tokio::fs::write(dir.path().join(".claude/settings.json"), b"[]")
            .await
            .unwrap();
        let err = install(dir.path()).await.unwrap_err();
        assert!(matches!(err, DispatchError::ConfigInvalid { .. }));
    }
}
