use std::collections::hash_map::DefaultHasher;
use std::env;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::errors::DispatchError;

/// Runtime configuration for Dispatch, resolved from multiple sources.
#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    /// The cell identity for this project.
    pub cell_id: String,
    /// Backend URL (if configured).
    pub backend: Option<String>,
    /// The project root (directory containing dispatch.config.toml, or cwd).
    pub project_root: PathBuf,
}

/// On-disk config file shape.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    /// Explicit cell identity override.
    pub cell_id: Option<String>,
    /// Backend URL.
    pub backend: Option<String>,
}

/// Search upward from `start_dir` for `dispatch.config.toml`.
/// Returns the path to the config file and the directory containing it.
pub fn find_config_file(start_dir: &Path) -> Option<(PathBuf, PathBuf)> {
    let mut current = start_dir.to_path_buf();
    loop {
        let candidate = current.join("dispatch.config.toml");
        if candidate.is_file() {
            return Some((candidate, current));
        }
        if !current.pop() {
            return None;
        }
    }
}

/// Load and parse a config file from disk.
pub fn load_config_file(path: &Path) -> Result<ConfigFile, DispatchError> {
    let contents = std::fs::read_to_string(path).map_err(|_| DispatchError::ConfigNotFound {
        path: path.to_path_buf(),
    })?;
    toml::from_str(&contents).map_err(|e| DispatchError::ConfigInvalid {
        path: path.to_path_buf(),
        reason: e.to_string(),
    })
}

/// Derive a stable cell ID by hashing the canonical project root path.
pub fn derive_cell_id(project_root: &Path) -> String {
    let canonical = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());
    let mut hasher = DefaultHasher::new();
    canonical.to_string_lossy().hash(&mut hasher);
    let hash = hasher.finish();
    format!("cell-{hash:016x}")
}

/// Resolve configuration with full precedence:
/// CLI flag > env var > config file > derived fallback.
pub fn resolve_config(
    cli_cell_id: Option<&str>,
    start_dir: &Path,
) -> Result<ResolvedConfig, DispatchError> {
    let env_cell_id = env::var("DISPATCH_CELL_ID").ok();
    resolve_config_inner(cli_cell_id, env_cell_id.as_deref(), start_dir)
}

fn resolve_config_inner(
    cli_cell_id: Option<&str>,
    env_cell_id: Option<&str>,
    start_dir: &Path,
) -> Result<ResolvedConfig, DispatchError> {
    // Try to find and load config file
    let (config_file, project_root) = if let Some((config_path, root)) = find_config_file(start_dir)
    {
        let config = load_config_file(&config_path)?;
        (Some(config), root)
    } else {
        (None, start_dir.to_path_buf())
    };

    // Resolve cell_id with precedence: CLI > env > config > derived
    let cell_id = if let Some(id) = cli_cell_id {
        id.to_string()
    } else if let Some(id) = env_cell_id {
        id.to_string()
    } else if let Some(ref config) = config_file {
        if let Some(ref id) = config.cell_id {
            id.clone()
        } else {
            derive_cell_id(&project_root)
        }
    } else {
        derive_cell_id(&project_root)
    };

    let backend = config_file.and_then(|c| c.backend);

    Ok(ResolvedConfig {
        cell_id,
        backend,
        project_root,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_find_config_in_current_dir() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("dispatch.config.toml");
        fs::write(&config_path, "").unwrap();

        let result = find_config_file(tmp.path());
        assert!(result.is_some());
        let (path, root) = result.unwrap();
        assert_eq!(path, config_path);
        assert_eq!(root, tmp.path());
    }

    #[test]
    fn test_find_config_in_parent_dir() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("dispatch.config.toml");
        fs::write(&config_path, "").unwrap();

        let child = tmp.path().join("subdir");
        fs::create_dir(&child).unwrap();

        let result = find_config_file(&child);
        assert!(result.is_some());
        let (path, root) = result.unwrap();
        assert_eq!(path, config_path);
        assert_eq!(root, tmp.path().to_path_buf());
    }

    #[test]
    fn test_find_config_not_found() {
        let tmp = TempDir::new().unwrap();
        let result = find_config_file(tmp.path());
        assert!(result.is_none());
    }

    #[test]
    fn test_load_config_file_valid() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("dispatch.config.toml");
        fs::write(
            &config_path,
            r#"
cell_id = "my-cell"
backend = "https://example.com"
"#,
        )
        .unwrap();

        let config = load_config_file(&config_path).unwrap();
        assert_eq!(config.cell_id, Some("my-cell".to_string()));
        assert_eq!(config.backend, Some("https://example.com".to_string()));
    }

    #[test]
    fn test_load_config_file_denies_unknown_fields() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("dispatch.config.toml");
        fs::write(&config_path, r#"unknown_field = "oops""#).unwrap();

        let result = load_config_file(&config_path);
        assert!(result.is_err());
    }

    #[test]
    fn test_derive_cell_id_deterministic() {
        let tmp = TempDir::new().unwrap();
        let id1 = derive_cell_id(tmp.path());
        let id2 = derive_cell_id(tmp.path());
        assert_eq!(id1, id2);
        assert!(id1.starts_with("cell-"));
    }

    #[test]
    fn test_derive_cell_id_different_paths() {
        let tmp1 = TempDir::new().unwrap();
        let tmp2 = TempDir::new().unwrap();
        let id1 = derive_cell_id(tmp1.path());
        let id2 = derive_cell_id(tmp2.path());
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_resolve_config_cli_override() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("dispatch.config.toml");
        fs::write(&config_path, r#"cell_id = "from-config""#).unwrap();

        let resolved = resolve_config_inner(Some("from-cli"), None, tmp.path()).unwrap();
        assert_eq!(resolved.cell_id, "from-cli");
    }

    #[test]
    fn test_resolve_config_env_override() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("dispatch.config.toml");
        fs::write(&config_path, r#"cell_id = "from-config""#).unwrap();

        let resolved = resolve_config_inner(None, Some("from-env"), tmp.path()).unwrap();
        assert_eq!(resolved.cell_id, "from-env");
    }

    #[test]
    fn test_resolve_config_from_file() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("dispatch.config.toml");
        fs::write(&config_path, r#"cell_id = "from-config""#).unwrap();

        let resolved = resolve_config_inner(None, None, tmp.path()).unwrap();
        assert_eq!(resolved.cell_id, "from-config");
    }

    #[test]
    fn test_resolve_config_derived_fallback() {
        let tmp = TempDir::new().unwrap();

        let resolved = resolve_config_inner(None, None, tmp.path()).unwrap();
        assert!(resolved.cell_id.starts_with("cell-"));
    }

    #[test]
    fn test_resolve_config_precedence_cli_over_env() {
        let tmp = TempDir::new().unwrap();

        let resolved =
            resolve_config_inner(Some("from-cli"), Some("from-env"), tmp.path()).unwrap();
        assert_eq!(resolved.cell_id, "from-cli");
    }

    #[test]
    fn test_resolve_config_project_root_with_config() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("dispatch.config.toml");
        fs::write(&config_path, "").unwrap();

        let child = tmp.path().join("sub");
        fs::create_dir(&child).unwrap();

        let resolved = resolve_config_inner(None, None, &child).unwrap();
        assert_eq!(resolved.project_root, tmp.path().to_path_buf());
    }
}
