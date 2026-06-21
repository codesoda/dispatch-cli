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
fn test_find_config_does_not_walk_parents() {
    let tmp = TempDir::new().unwrap();
    let config_path = tmp.path().join("dispatch.config.toml");
    fs::write(&config_path, "").unwrap();

    let child = tmp.path().join("subdir");
    fs::create_dir(&child).unwrap();

    let result = find_config_file(&child);
    assert!(result.is_none());
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

    let resolved = resolve_config_inner(Some("from-cli"), None, None, tmp.path()).unwrap();
    assert_eq!(resolved.cell_id, "from-cli");
}

#[test]
fn test_resolve_config_env_override() {
    let tmp = TempDir::new().unwrap();
    let config_path = tmp.path().join("dispatch.config.toml");
    fs::write(&config_path, r#"cell_id = "from-config""#).unwrap();

    let resolved = resolve_config_inner(None, Some("from-env"), None, tmp.path()).unwrap();
    assert_eq!(resolved.cell_id, "from-env");
}

#[test]
fn test_resolve_config_from_file() {
    let tmp = TempDir::new().unwrap();
    let config_path = tmp.path().join("dispatch.config.toml");
    fs::write(&config_path, r#"cell_id = "from-config""#).unwrap();

    let resolved = resolve_config_inner(None, None, None, tmp.path()).unwrap();
    assert_eq!(resolved.cell_id, "from-config");
}

#[test]
fn test_resolve_config_derived_fallback() {
    let tmp = TempDir::new().unwrap();

    let resolved = resolve_config_inner(None, None, None, tmp.path()).unwrap();
    assert!(resolved.cell_id.starts_with("cell-"));
}

#[test]
fn test_resolve_config_precedence_cli_over_env() {
    let tmp = TempDir::new().unwrap();

    let resolved =
        resolve_config_inner(Some("from-cli"), Some("from-env"), None, tmp.path()).unwrap();
    assert_eq!(resolved.cell_id, "from-cli");
}

#[test]
fn test_init_config_creates_file() {
    let tmp = TempDir::new().unwrap();
    let result = init_config(tmp.path());
    assert!(result.is_ok());

    let path = result.unwrap();
    assert_eq!(path, tmp.path().join("dispatch.config.toml"));
    assert!(path.is_file());

    let contents = fs::read_to_string(&path).unwrap();
    assert!(contents.contains("# cell_id = \"my-project\""));

    // Template must be valid TOML (all active lines are comments)
    let parsed: Result<ConfigFile, _> = toml::from_str(&contents);
    assert!(parsed.is_ok());
}

#[test]
fn test_init_config_already_exists() {
    let tmp = TempDir::new().unwrap();
    let config_path = tmp.path().join("dispatch.config.toml");
    fs::write(&config_path, "").unwrap();

    let result = init_config(tmp.path());
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        err.to_string().contains("already exists"),
        "expected 'already exists' in error, got: {err}"
    );
}

#[test]
fn test_resolve_config_explicit_config_path() {
    let tmp = TempDir::new().unwrap();
    let config_dir = tmp.path().join("other");
    fs::create_dir(&config_dir).unwrap();
    let config_path = config_dir.join("dispatch.config.toml");
    fs::write(&config_path, r#"cell_id = "explicit""#).unwrap();

    // cwd is tmp root, config is in other/ — project_root should be other/
    let resolved = resolve_config_inner(None, None, Some(&config_path), tmp.path()).unwrap();
    assert_eq!(resolved.cell_id, "explicit");
    assert_eq!(resolved.project_root, config_dir);
}

#[test]
fn agent_config_parses_claude_adapter() {
    let tmp = TempDir::new().unwrap();
    let prompt_path = tmp.path().join("reviewer.md");
    fs::write(&prompt_path, "you are a reviewer").unwrap();
    let config_path = tmp.path().join("dispatch.config.toml");
    fs::write(
        &config_path,
        r#"
[[agents]]
name = "reviewer"
role = "reviewer"
description = "reviews"
adapter = "claude"
extra_args = ["--model", "sonnet"]
prompt_file = "reviewer.md"
launch = true
"#,
    )
    .unwrap();

    let resolved = resolve_config_inner(None, None, None, tmp.path()).unwrap();
    assert_eq!(resolved.agents.len(), 1);
    let a = &resolved.agents[0];
    assert_eq!(a.adapter, crate::adapter::Adapter::Claude);
    assert_eq!(a.extra_args, vec!["--model", "sonnet"]);
    assert!(a.launch);
    assert!(a.command.is_none());
    assert!(a.prompt_file_path.is_some());
    // Issue #43: stream_json defaults to false so existing configs see
    // no behavior change.
    assert!(!a.stream_json, "stream_json must default to false");
}

/// Issue #43: `stream_json = true` round-trips through TOML and lands
/// on the resolved config. Verified separately by the claude adapter
/// test that translates this flag into argv.
#[test]
fn agent_config_parses_stream_json_flag() {
    let tmp = TempDir::new().unwrap();
    let prompt_path = tmp.path().join("reviewer.md");
    fs::write(&prompt_path, "you are a reviewer").unwrap();
    let config_path = tmp.path().join("dispatch.config.toml");
    fs::write(
        &config_path,
        r#"
[[agents]]
name = "reviewer"
role = "reviewer"
description = "reviews"
adapter = "claude"
prompt_file = "reviewer.md"
stream_json = true
"#,
    )
    .unwrap();

    let resolved = resolve_config_inner(None, None, None, tmp.path()).unwrap();
    assert!(
        resolved.agents[0].stream_json,
        "stream_json = true in TOML must land on the resolved config",
    );
}

#[test]
fn agent_config_rejects_claude_adapter_with_inline_prompt() {
    let tmp = TempDir::new().unwrap();
    let config_path = tmp.path().join("dispatch.config.toml");
    fs::write(
        &config_path,
        r#"
[[agents]]
name = "reviewer"
role = "reviewer"
description = "reviews"
adapter = "claude"
prompt = "you are a reviewer"
"#,
    )
    .unwrap();

    let err = resolve_config_inner(None, None, None, tmp.path()).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("prompt_file") && msg.contains("claude"),
        "expected prompt_file-required error for claude, got: {msg}"
    );
}

#[test]
fn agent_config_rejects_codex_adapter_with_inline_prompt() {
    let tmp = TempDir::new().unwrap();
    let config_path = tmp.path().join("dispatch.config.toml");
    fs::write(
        &config_path,
        r#"
[[agents]]
name = "worker"
role = "worker"
description = "codex"
adapter = "codex"
prompt = "be helpful"
"#,
    )
    .unwrap();

    let err = resolve_config_inner(None, None, None, tmp.path()).unwrap_err();
    assert!(err.to_string().contains("prompt_file"));
}

#[test]
fn agent_config_parses_command_adapter() {
    let tmp = TempDir::new().unwrap();
    let config_path = tmp.path().join("dispatch.config.toml");
    fs::write(
        &config_path,
        r#"
[[agents]]
name = "worker"
role = "worker"
description = "bash worker"
adapter = "command"
command = "./worker.sh --verbose"
"#,
    )
    .unwrap();

    let resolved = resolve_config_inner(None, None, None, tmp.path()).unwrap();
    let a = &resolved.agents[0];
    assert_eq!(a.adapter, crate::adapter::Adapter::Command);
    assert_eq!(a.command.as_deref(), Some("./worker.sh --verbose"));
    assert!(!a.launch);
    assert!(a.extra_args.is_empty());
}

#[test]
fn agent_config_rejects_command_adapter_without_command() {
    let tmp = TempDir::new().unwrap();
    let config_path = tmp.path().join("dispatch.config.toml");
    fs::write(
        &config_path,
        r#"
[[agents]]
name = "broken"
role = "worker"
description = "no command"
adapter = "command"
"#,
    )
    .unwrap();

    let err = resolve_config_inner(None, None, None, tmp.path()).unwrap_err();
    assert!(
        err.to_string().contains("adapter = \"command\""),
        "expected command-required error, got: {err}"
    );
}

#[test]
fn agent_config_rejects_missing_adapter_field() {
    let tmp = TempDir::new().unwrap();
    let config_path = tmp.path().join("dispatch.config.toml");
    fs::write(
        &config_path,
        r#"
[[agents]]
name = "legacy"
role = "worker"
description = "old shape"
command = "./worker.sh"
"#,
    )
    .unwrap();

    let err = resolve_config_inner(None, None, None, tmp.path()).unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("adapter"),
        "expected adapter-related parse error, got: {err}"
    );
}

#[test]
fn agent_config_rejects_unknown_adapter_value() {
    let tmp = TempDir::new().unwrap();
    let config_path = tmp.path().join("dispatch.config.toml");
    fs::write(
        &config_path,
        r#"
[[agents]]
name = "x"
role = "r"
description = "d"
adapter = "gpt"
"#,
    )
    .unwrap();

    assert!(resolve_config_inner(None, None, None, tmp.path()).is_err());
}

/// Agent names must pass `is_safe_name` at resolve time so the
/// boot-prompt filename (derived via lossy `sanitize_name`) cannot
/// collide across distinct raw names under `dispatch serve`.
#[test]
fn agent_config_rejects_name_with_unsafe_characters() {
    let tmp = TempDir::new().unwrap();
    let config_path = tmp.path().join("dispatch.config.toml");
    fs::write(
        &config_path,
        r#"
[[agents]]
name = "alice/foo"
role = "worker"
description = "d"
adapter = "command"
command = "./run.sh"
"#,
    )
    .unwrap();

    let err = resolve_config_inner(None, None, None, tmp.path()).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("alice/foo") && msg.contains("ASCII"),
        "expected safe-name rejection for 'alice/foo', got: {msg}"
    );
}

/// `interactive = true` on a `launch = false` agent round-trips
/// through TOML unchanged. The adapter uses this to drop `-p` (claude)
/// / `exec` (codex) so the printed copy-paste command opens the vendor
/// CLI in its REPL instead of headless mode.
#[test]
fn agent_config_parses_interactive_flag() {
    let tmp = TempDir::new().unwrap();
    let prompt_path = tmp.path().join("coord.md");
    fs::write(&prompt_path, "you are the coordinator").unwrap();
    let config_path = tmp.path().join("dispatch.config.toml");
    fs::write(
        &config_path,
        r#"
[[agents]]
name = "coordinator"
role = "coordinator"
description = "the human-run coordinator"
adapter = "claude"
prompt_file = "coord.md"
launch = false
interactive = true
"#,
    )
    .unwrap();

    let resolved = resolve_config_inner(None, None, None, tmp.path()).unwrap();
    assert_eq!(resolved.agents.len(), 1);
    let a = &resolved.agents[0];
    assert!(a.interactive, "interactive = true must round-trip");
    assert!(!a.launch);
}

/// `interactive = true + launch = true` is contradictory (the
/// supervisor owns stdin/stdout, so there's no TTY for a REPL). We
/// warn and prefer `launch = true` (headless), keeping the config
/// loadable rather than forcing a hard failure that would strand a
/// user who typed the wrong combo.
#[test]
fn agent_config_warns_and_prefers_launch_when_both_set() {
    let tmp = TempDir::new().unwrap();
    let prompt_path = tmp.path().join("r.md");
    fs::write(&prompt_path, "role").unwrap();
    let config_path = tmp.path().join("dispatch.config.toml");
    fs::write(
        &config_path,
        r#"
[[agents]]
name = "reviewer"
role = "reviewer"
description = "reviews"
adapter = "claude"
prompt_file = "r.md"
launch = true
interactive = true
"#,
    )
    .unwrap();

    let resolved = resolve_config_inner(None, None, None, tmp.path()).unwrap();
    let a = &resolved.agents[0];
    assert!(a.launch, "launch must win when both are set");
    assert!(
        !a.interactive,
        "interactive must be forced off when launch is on",
    );
}

/// `interactive` defaults to false so existing configs see no
/// behavior change after the new field is introduced.
#[test]
fn agent_config_interactive_defaults_false() {
    let tmp = TempDir::new().unwrap();
    let config_path = tmp.path().join("dispatch.config.toml");
    fs::write(
        &config_path,
        r#"
[[agents]]
name = "worker"
role = "worker"
description = "d"
adapter = "command"
command = "./run.sh"
"#,
    )
    .unwrap();

    let resolved = resolve_config_inner(None, None, None, tmp.path()).unwrap();
    assert!(!resolved.agents[0].interactive);
}

/// Issue #45: `[main_agent]` has been removed in favor of a regular
/// `[[agents]] launch = false + prompt_file` entry. `ConfigFile`'s
/// `deny_unknown_fields` means a legacy config with `[main_agent]`
/// fails parse with a clear pointer rather than silently ignoring
/// the section.
#[test]
fn config_rejects_legacy_main_agent_table() {
    let tmp = TempDir::new().unwrap();
    let config_path = tmp.path().join("dispatch.config.toml");
    fs::write(
        &config_path,
        r#"
[main_agent]
command = "claude"
model = "opus"
"#,
    )
    .unwrap();

    let err = resolve_config_inner(None, None, None, tmp.path()).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("main_agent"),
        "expected error to reference main_agent, got: {msg}"
    );
}

/// `config_file_path` carries the discovered file path as an absolute
/// canonical path, so agents spawned by `dispatch serve` can propagate
/// it via `DISPATCH_CONFIG_PATH` regardless of their working directory.
#[test]
fn resolved_config_carries_path_when_discovered() {
    let tmp = TempDir::new().unwrap();
    let config_path = tmp.path().join("dispatch.config.toml");
    fs::write(&config_path, "").unwrap();

    let resolved = resolve_config_inner(None, None, None, tmp.path()).unwrap();
    let expected = config_path.canonicalize().unwrap();
    assert_eq!(
        resolved.config_file_path.as_deref(),
        Some(expected.as_path())
    );
}

/// Explicit `--config <path>` flow: the absolute path threads through
/// to `ResolvedConfig` so the injected env var points at the right file.
#[test]
fn resolved_config_carries_path_when_flag_given() {
    let tmp = TempDir::new().unwrap();
    let config_dir = tmp.path().join("other");
    fs::create_dir(&config_dir).unwrap();
    let config_path = config_dir.join("dispatch.config.toml");
    fs::write(&config_path, "").unwrap();

    let resolved = resolve_config_inner(None, None, Some(&config_path), tmp.path()).unwrap();
    let expected = config_path.canonicalize().unwrap();
    assert_eq!(
        resolved.config_file_path.as_deref(),
        Some(expected.as_path())
    );
}

/// Cwd without a config file: `config_file_path` stays `None` so the
/// orchestrator emits no `DISPATCH_CONFIG_PATH` env var (regression
/// guard — existing configs see byte-identical env).
#[test]
fn resolved_config_none_when_no_config_file() {
    let tmp = TempDir::new().unwrap();

    let resolved = resolve_config_inner(None, None, None, tmp.path()).unwrap();
    assert!(resolved.config_file_path.is_none());
}

/// `DISPATCH_CONFIG_PATH` acts as a fallback to `--config`, mirroring how
/// `DISPATCH_SOCKET_PATH` already works for the broker socket.
#[test]
fn resolve_config_with_env_honors_dispatch_config_path() {
    let tmp = TempDir::new().unwrap();
    let config_dir = tmp.path().join("elsewhere");
    fs::create_dir(&config_dir).unwrap();
    let config_path = config_dir.join("dispatch.config.toml");
    fs::write(&config_path, r#"cell_id = "from-env-path""#).unwrap();

    let resolved = resolve_config_with_env(
        None,
        None,
        None,
        Some(config_path.to_str().unwrap()),
        tmp.path(),
    )
    .unwrap();
    assert_eq!(resolved.cell_id, "from-env-path");
}

/// CLI flag beats env var — matches the stated precedence order
/// (CLI > env > discovery) from the config docs.
#[test]
fn resolve_config_with_env_prefers_cli_flag_over_env() {
    let tmp = TempDir::new().unwrap();
    let cli_dir = tmp.path().join("cli");
    fs::create_dir(&cli_dir).unwrap();
    let cli_config = cli_dir.join("dispatch.config.toml");
    fs::write(&cli_config, r#"cell_id = "from-cli""#).unwrap();

    let env_dir = tmp.path().join("env");
    fs::create_dir(&env_dir).unwrap();
    let env_config = env_dir.join("dispatch.config.toml");
    fs::write(&env_config, r#"cell_id = "from-env""#).unwrap();

    let resolved = resolve_config_with_env(
        None,
        None,
        Some(&cli_config),
        Some(env_config.to_str().unwrap()),
        tmp.path(),
    )
    .unwrap();
    assert_eq!(resolved.cell_id, "from-cli");
}

/// Empty `DISPATCH_CONFIG_PATH` is treated as unset — matches the
/// `resolve_socket_path_with_env` idiom and avoids a hard error when
/// a parent shell exports the var blank.
#[test]
fn resolve_config_with_env_treats_empty_string_as_unset() {
    let tmp = TempDir::new().unwrap();

    let resolved = resolve_config_with_env(None, None, None, Some(""), tmp.path()).unwrap();
    assert!(resolved.config_file_path.is_none());
}

/// `DISPATCH_CONFIG_PATH` pointing at a missing file errors out — same
/// failure mode as `--config nonexistent.toml`, documented as a
/// breaking-change surface.
#[test]
fn resolve_config_with_env_hard_errors_on_missing_file() {
    let tmp = TempDir::new().unwrap();
    let missing = tmp.path().join("does-not-exist.toml");

    let err = resolve_config_with_env(
        None,
        None,
        None,
        Some(missing.to_str().unwrap()),
        tmp.path(),
    )
    .unwrap_err();
    // ConfigNotFound — exactly what `--config <missing>` emits today.
    assert!(
        err.to_string().to_lowercase().contains("not found")
            || err.to_string().to_lowercase().contains("no such"),
        "expected not-found error, got: {err}"
    );
}

/// Regression guard for the `absolutize` fallback: when canonicalize
/// fails (e.g. permission denied on an ancestor dir), we must still
/// produce an absolute path, not echo the raw relative input. The
/// fallback is what stands between `DISPATCH_CONFIG_PATH` and a
/// worthless value in rare failure modes.
#[test]
fn absolutize_joins_relative_paths_against_cwd() {
    let tmp = TempDir::new().unwrap();
    let relative = Path::new("foo/bar.toml");
    let joined = absolutize(tmp.path(), relative);
    assert!(joined.is_absolute());
    assert_eq!(joined, tmp.path().join("foo/bar.toml"));
}

/// `absolutize` leaves already-absolute paths untouched.
#[test]
fn absolutize_passes_absolute_paths_through() {
    let tmp = TempDir::new().unwrap();
    let already_abs = tmp.path().join("x.toml");
    assert_eq!(absolutize(tmp.path(), &already_abs), already_abs);
}

/// Discovered path is always absolute — even a `TempDir` root (already
/// absolute) gets canonicalized to resolve `/tmp` → `/private/tmp` on
/// macOS, guaranteeing the stored path is what downstream code can
/// stat regardless of how the test env was configured.
#[test]
fn discovered_config_path_is_absolute() {
    let tmp = TempDir::new().unwrap();
    let config_path = tmp.path().join("dispatch.config.toml");
    fs::write(&config_path, "").unwrap();

    let resolved = resolve_config_inner(None, None, None, tmp.path()).unwrap();
    let stored = resolved.config_file_path.expect("path must be set");
    assert!(
        stored.is_absolute(),
        "discovered path must be absolute, got: {}",
        stored.display()
    );
}

/// With no `continue_instruction` anywhere, the resolver returns
/// the shipped built-in default — for a configured agent and for an
/// unknown / ad-hoc (`None`) name alike.
#[test]
fn continue_instruction_falls_back_to_shipped_default() {
    let tmp = TempDir::new().unwrap();
    let config_path = tmp.path().join("dispatch.config.toml");
    fs::write(
        &config_path,
        r#"
[[agents]]
name = "worker"
role = "worker"
description = "d"
adapter = "command"
command = "./run.sh"
"#,
    )
    .unwrap();

    let resolved = resolve_config_inner(None, None, None, tmp.path()).unwrap();
    assert_eq!(
        resolved.continue_instruction_for(Some("worker")),
        DEFAULT_CONTINUE_INSTRUCTION
    );
    assert_eq!(
        resolved.continue_instruction_for(None),
        DEFAULT_CONTINUE_INSTRUCTION
    );
}

/// A global `continue_instruction` overrides the shipped default
/// for every agent that doesn't set its own (and for `None`).
#[test]
fn continue_instruction_uses_global_override() {
    let tmp = TempDir::new().unwrap();
    let config_path = tmp.path().join("dispatch.config.toml");
    fs::write(
        &config_path,
        r#"
continue_instruction = "global: keep listening"

[[agents]]
name = "worker"
role = "worker"
description = "d"
adapter = "command"
command = "./run.sh"
"#,
    )
    .unwrap();

    let resolved = resolve_config_inner(None, None, None, tmp.path()).unwrap();
    assert_eq!(
        resolved.continue_instruction_for(Some("worker")),
        "global: keep listening"
    );
    assert_eq!(
        resolved.continue_instruction_for(None),
        "global: keep listening"
    );
}

/// A per-agent `continue_instruction` wins over the global one for
/// that agent; a sibling without its own override still gets the global.
#[test]
fn continue_instruction_per_agent_overrides_global() {
    let tmp = TempDir::new().unwrap();
    let config_path = tmp.path().join("dispatch.config.toml");
    fs::write(
        &config_path,
        r#"
continue_instruction = "global text"

[[agents]]
name = "special"
role = "worker"
description = "d"
adapter = "command"
command = "./run.sh"
continue_instruction = "special text"

[[agents]]
name = "plain"
role = "worker"
description = "d"
adapter = "command"
command = "./run.sh"
"#,
    )
    .unwrap();

    let resolved = resolve_config_inner(None, None, None, tmp.path()).unwrap();
    assert_eq!(
        resolved.continue_instruction_for(Some("special")),
        "special text"
    );
    assert_eq!(
        resolved.continue_instruction_for(Some("plain")),
        "global text"
    );
}

/// With no `boot_prompt` anywhere, the resolved agent carries `None` —
/// `write_boot_prompt` applies the shipped [`DEFAULT_BOOT_PROMPT`] at
/// launch time.
#[test]
fn boot_prompt_unset_resolves_none() {
    let tmp = TempDir::new().unwrap();
    let config_path = tmp.path().join("dispatch.config.toml");
    fs::write(
        &config_path,
        r#"
[[agents]]
name = "worker"
role = "worker"
description = "d"
adapter = "command"
command = "./run.sh"
"#,
    )
    .unwrap();

    let resolved = resolve_config_inner(None, None, None, tmp.path()).unwrap();
    let worker = resolved.agents.iter().find(|a| a.name == "worker").unwrap();
    assert_eq!(worker.boot_prompt, None);
}

/// A file-level `boot_prompt` is folded onto every agent that doesn't set
/// its own.
#[test]
fn boot_prompt_uses_file_level() {
    let tmp = TempDir::new().unwrap();
    let config_path = tmp.path().join("dispatch.config.toml");
    fs::write(
        &config_path,
        r#"
boot_prompt = "Use the dispatch skill, then: dispatch register --for-agent"

[[agents]]
name = "worker"
role = "worker"
description = "d"
adapter = "command"
command = "./run.sh"
"#,
    )
    .unwrap();

    let resolved = resolve_config_inner(None, None, None, tmp.path()).unwrap();
    let worker = resolved.agents.iter().find(|a| a.name == "worker").unwrap();
    assert_eq!(
        worker.boot_prompt.as_deref(),
        Some("Use the dispatch skill, then: dispatch register --for-agent")
    );
}

/// A per-agent `boot_prompt` wins over the file-level one for that agent;
/// a sibling without its own override still gets the file-level value.
#[test]
fn boot_prompt_per_agent_overrides_file_level() {
    let tmp = TempDir::new().unwrap();
    let config_path = tmp.path().join("dispatch.config.toml");
    fs::write(
        &config_path,
        r#"
boot_prompt = "file-level boot"

[[agents]]
name = "special"
role = "worker"
description = "d"
adapter = "command"
command = "./run.sh"
boot_prompt = "special boot"

[[agents]]
name = "plain"
role = "worker"
description = "d"
adapter = "command"
command = "./run.sh"
"#,
    )
    .unwrap();

    let resolved = resolve_config_inner(None, None, None, tmp.path()).unwrap();
    let special = resolved
        .agents
        .iter()
        .find(|a| a.name == "special")
        .unwrap();
    let plain = resolved.agents.iter().find(|a| a.name == "plain").unwrap();
    assert_eq!(special.boot_prompt.as_deref(), Some("special boot"));
    assert_eq!(plain.boot_prompt.as_deref(), Some("file-level boot"));
}
