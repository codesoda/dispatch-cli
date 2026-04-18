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
pub fn install(cwd: &Path) -> Result<PathBuf, DispatchError> {
    std::fs::create_dir_all(cwd.join(".claude")).map_err(DispatchError::Io)?;
    let path = settings_path(cwd);

    let mut root = load_json(&path)?;
    let hooks = ensure_object(&mut root, "hooks");
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

    std::fs::write(&path, serde_json::to_string_pretty(&root).unwrap() + "\n")
        .map_err(DispatchError::Io)?;
    Ok(path)
}

/// Remove our Stop hook entry. Leaves other hooks, matchers, and settings
/// untouched. Returns `Ok(None)` if no entry was found.
pub fn uninstall(cwd: &Path) -> Result<Option<PathBuf>, DispatchError> {
    let path = settings_path(cwd);
    if !path.exists() {
        return Ok(None);
    }
    let mut root = load_json(&path)?;
    let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) else {
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
    if hooks.is_empty() {
        root.as_object_mut().unwrap().remove("hooks");
    }

    if !removed_any {
        return Ok(None);
    }

    if root.as_object().map(|obj| obj.is_empty()).unwrap_or(false) {
        std::fs::remove_file(&path).map_err(DispatchError::Io)?;
    } else {
        std::fs::write(&path, serde_json::to_string_pretty(&root).unwrap() + "\n")
            .map_err(DispatchError::Io)?;
    }
    Ok(Some(path))
}

fn load_json(path: &Path) -> Result<Value, DispatchError> {
    if !path.exists() {
        return Ok(Value::Object(Map::new()));
    }
    let text = std::fs::read_to_string(path).map_err(DispatchError::Io)?;
    if text.trim().is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    serde_json::from_str(&text).map_err(DispatchError::Serialization)
}

fn ensure_object<'a>(root: &'a mut Value, key: &str) -> &'a mut Map<String, Value> {
    let obj = root.as_object_mut().expect("root must be a JSON object");
    obj.entry(key.to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    obj.get_mut(key).unwrap().as_object_mut().unwrap()
}

fn ensure_array<'a>(map: &'a mut Map<String, Value>, key: &str) -> &'a mut Vec<Value> {
    map.entry(key.to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    map.get_mut(key).unwrap().as_array_mut().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn install_creates_settings_local() {
        let dir = tempdir().unwrap();
        let path = install(dir.path()).unwrap();
        assert!(path.ends_with(".claude/settings.local.json"));
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("claude-hook"));
        assert!(content.contains("\"Stop\""));
    }

    #[test]
    fn install_merges_into_existing_settings() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".claude")).unwrap();
        let original =
            "{\n  \"model\": \"sonnet\",\n  \"permissions\": {\"defaultMode\": \"bypass\"}\n}\n";
        std::fs::write(dir.path().join(".claude/settings.json"), original).unwrap();
        let path = install(dir.path()).unwrap();
        assert!(path.ends_with(".claude/settings.json"));
        let value: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["model"], "sonnet");
        assert_eq!(value["permissions"]["defaultMode"], "bypass");
        assert!(value["hooks"]["Stop"].is_array());
    }

    #[test]
    fn install_is_idempotent() {
        let dir = tempdir().unwrap();
        install(dir.path()).unwrap();
        install(dir.path()).unwrap();
        let path = dir.path().join(".claude/settings.local.json");
        let value: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["hooks"]["Stop"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn uninstall_leaves_other_settings() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".claude")).unwrap();
        std::fs::write(
            dir.path().join(".claude/settings.json"),
            "{\n  \"model\": \"sonnet\"\n}\n",
        )
        .unwrap();
        install(dir.path()).unwrap();
        uninstall(dir.path()).unwrap();
        let path = dir.path().join(".claude/settings.json");
        let value: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["model"], "sonnet");
        assert!(value.get("hooks").is_none());
    }

    #[test]
    fn uninstall_removes_empty_file() {
        let dir = tempdir().unwrap();
        install(dir.path()).unwrap();
        uninstall(dir.path()).unwrap();
        assert!(!dir.path().join(".claude/settings.local.json").exists());
    }

    #[test]
    fn uninstall_noop_when_absent() {
        let dir = tempdir().unwrap();
        let result = uninstall(dir.path()).unwrap();
        assert!(result.is_none());
    }
}
