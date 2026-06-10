use std::process;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::Parser;

use dispatch::backend::create_backend;
use dispatch::backend::local::DEFAULT_LISTEN_TIMEOUT_SECS;
use dispatch::cli::{AgentAction, Cli, Commands, HookAction};
use dispatch::config::resolve_config;
use dispatch::errors::DispatchError;
use dispatch::hooks;
use dispatch::logging::init_tracing;
use dispatch::protocol::{BrokerRequest, BrokerResponse, ControlState, ResponsePayload};

/// Read `$DISPATCH_WORKER_ID`, treating an empty value as unset. A
/// dispatch-launched agent always has this set (the orchestrator injects it),
/// so the agent's commands can be bare (`dispatch listen`, `dispatch ack ...`).
fn env_worker_id() -> Option<String> {
    std::env::var("DISPATCH_WORKER_ID")
        .ok()
        .filter(|s| !s.is_empty())
}

/// Read `$DISPATCH_AGENT_NAME` (empty treated as unset). Used to resolve the
/// per-agent `continue_instruction` when `listen --for-agent` times out while
/// the worker is still active.
fn env_agent_name() -> Option<String> {
    std::env::var("DISPATCH_AGENT_NAME")
        .ok()
        .filter(|s| !s.is_empty())
}

/// Resolve worker identity for an agent command: explicit per-command flag
/// (`--worker-id`) **>** the global `--from` **>** `$DISPATCH_WORKER_ID`.
fn resolve_identity(specific: Option<String>, from: Option<String>) -> Option<String> {
    resolve_identity_with_env(specific, from, env_worker_id())
}

/// Pure core of [`resolve_identity`] — the process-global env read is lifted
/// out so the precedence logic is testable without mutating `std::env` (which
/// races across parallel tests; see `config::resolve_config_with_env`).
fn resolve_identity_with_env(
    specific: Option<String>,
    from: Option<String>,
    env: Option<String>,
) -> Option<String> {
    specific.or(from).or(env)
}

/// Like [`resolve_identity`] but errors when no identity can be found, for
/// commands that act *as* a worker (listen / ack / heartbeat / messages).
fn require_identity(
    specific: Option<String>,
    from: Option<String>,
) -> Result<String, DispatchError> {
    resolve_identity(specific, from).ok_or(DispatchError::MissingWorkerIdentity)
}

/// Resolve the `dispatch listen` timeout: explicit `--timeout` flag **>**
/// `$DISPATCH_LISTEN_TIMEOUT` (injected by the orchestrator from the agent's
/// resolved `listen_timeout`) **>** the built-in [`DEFAULT_LISTEN_TIMEOUT_SECS`].
fn resolve_listen_timeout(flag: Option<u64>) -> u64 {
    let env = std::env::var("DISPATCH_LISTEN_TIMEOUT").ok();
    resolve_listen_timeout_with_env(flag, env.as_deref())
}

/// Pure core of [`resolve_listen_timeout`]; env read lifted out for testing.
fn resolve_listen_timeout_with_env(flag: Option<u64>, env: Option<&str>) -> u64 {
    flag.or_else(|| env.and_then(|s| s.trim().parse::<u64>().ok()))
        .unwrap_or(DEFAULT_LISTEN_TIMEOUT_SECS)
}

/// Resolve a required `register` field: explicit flag **>** the orchestrator-
/// injected env var (`DISPATCH_AGENT_NAME` / `_ROLE` / `_DESCRIPTION`). Lets a
/// launched agent boot with a bare `dispatch register --for-agent` while a
/// hand-run register that supplies neither fails with a clear message.
fn require_register_field(
    flag: Option<String>,
    env: &'static str,
    field: &'static str,
) -> Result<String, DispatchError> {
    flag.or_else(|| std::env::var(env).ok().filter(|s| !s.is_empty()))
        .ok_or(DispatchError::MissingRegisterField { field, env })
}

/// Parse a timestamp string as either a relative duration (e.g. "5m", "1h", "30s")
/// or an absolute Unix timestamp. Returns a Unix timestamp in seconds.
fn parse_timestamp(s: &str) -> Result<u64, String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let relative = [('s', 1u64), ('m', 60), ('h', 3600), ('d', 86400)];
    for (suffix, factor) in relative {
        if let Some(num_str) = s.strip_suffix(suffix) {
            if let Ok(n) = num_str.parse::<u64>() {
                let secs = n.checked_mul(factor).ok_or_else(|| {
                    format!("invalid timestamp: {s} (relative duration overflows)")
                })?;
                return Ok(now.saturating_sub(secs));
            }
        }
    }

    if let Ok(ts) = s.parse::<u64>() {
        return Ok(ts);
    }

    Err(format!(
        "invalid timestamp: {s} (use relative like 5m/1h/30s or a Unix timestamp)"
    ))
}

#[tokio::main]
async fn main() {
    init_tracing();

    let cli = Cli::parse();

    if let Err(e) = run(cli).await {
        eprintln!("Error: {e}");
        let code = match e {
            dispatch::errors::DispatchError::ConfigAlreadyExists { .. }
            | dispatch::errors::DispatchError::ConfigNotFound { .. }
            | dispatch::errors::DispatchError::ConfigInvalid { .. } => 2,
            _ => 1,
        };
        process::exit(code);
    }
}

async fn run(cli: Cli) -> Result<(), dispatch::errors::DispatchError> {
    let cwd = std::env::current_dir().map_err(dispatch::errors::DispatchError::Io)?;

    // Handle init before config resolution — it doesn't need an existing config
    if let Commands::Init = cli.command {
        let path = dispatch::config::init_config(&cwd)?;
        println!("{}", path.display());
        eprintln!("Created dispatch.config.toml");
        return Ok(());
    }

    // Hook subcommands run without resolving a config up front — Stop probes
    // the broker itself, install/uninstall touch vendor files only.
    if let Commands::CodexHook { action } = &cli.command {
        return run_codex_hook(action, &cwd).await;
    }
    if let Commands::ClaudeHook { action } = &cli.command {
        return run_claude_hook(action, &cwd).await;
    }

    let config = resolve_config(cli.cell_id.as_deref(), cli.config.as_deref(), &cwd)?;

    tracing::debug!(cell_id = %config.cell_id, project_root = %config.project_root.display(), "resolved config");

    // Extract monitor port: CLI flag takes precedence over config.
    let monitor_port = if let Commands::Serve { monitor } = &cli.command {
        monitor.or(config.monitor_port)
    } else {
        None
    };

    let backend = create_backend(&config, monitor_port)?;

    match cli.command {
        Commands::Serve { .. } => {
            backend.serve().await?;
        }
        cmd => {
            // Issue #43: when `dispatch register --for-agent` is invoked,
            // route the prompt body to stdout (so it lands in the model's
            // tool result) and the structured envelope to stderr.
            let for_agent = matches!(
                &cmd,
                Commands::Register {
                    for_agent: true,
                    ..
                } | Commands::Listen {
                    for_agent: true,
                    ..
                } | Commands::Result {
                    for_agent: true,
                    ..
                }
            );
            let request = match cmd {
                Commands::Init => unreachable!(),
                Commands::Serve { .. } => unreachable!(),
                Commands::Register {
                    name,
                    role,
                    description,
                    capabilities,
                    ttl,
                    evict,
                    worker_id,
                    role_prompt,
                    for_agent: _,
                } => BrokerRequest::Register {
                    // name/role/description fall back to the orchestrator-
                    // injected env so the boot line is a bare
                    // `dispatch register --for-agent`.
                    name: require_register_field(name, "DISPATCH_AGENT_NAME", "name")?,
                    role: require_register_field(role, "DISPATCH_AGENT_ROLE", "role")?,
                    description: require_register_field(
                        description,
                        "DISPATCH_AGENT_DESCRIPTION",
                        "description",
                    )?,
                    capabilities,
                    ttl_secs: ttl,
                    evict,
                    // Identity from env when no explicit `--worker-id`: a
                    // dispatch-launched agent claims the pre-registered worker
                    // dispatch reserved for it.
                    worker_id: worker_id.or_else(env_worker_id),
                    role_prompt,
                },
                Commands::Team => BrokerRequest::Team {
                    from: resolve_identity(None, cli.from.clone()),
                },
                Commands::Send { to, body } => BrokerRequest::Send {
                    to,
                    body,
                    from: resolve_identity(None, cli.from.clone()),
                },
                Commands::Listen {
                    worker_id,
                    timeout,
                    for_agent: _,
                } => BrokerRequest::Listen {
                    worker_id: require_identity(worker_id, cli.from.clone())?,
                    timeout_secs: resolve_listen_timeout(timeout),
                },
                Commands::Events {
                    event_type,
                    worker,
                    since,
                    until,
                    limit,
                } => {
                    let since = since
                        .map(|s| parse_timestamp(&s))
                        .transpose()
                        .unwrap_or_else(|e| {
                            eprintln!("dispatch: {e}");
                            process::exit(2);
                        });
                    let until = until
                        .map(|s| parse_timestamp(&s))
                        .transpose()
                        .unwrap_or_else(|e| {
                            eprintln!("dispatch: {e}");
                            process::exit(2);
                        });
                    BrokerRequest::Events {
                        since,
                        until,
                        event_type,
                        worker,
                        limit,
                    }
                }
                Commands::Messages {
                    worker_id,
                    unacked,
                    sent,
                    since,
                    limit,
                    id,
                } => {
                    let since = since
                        .map(|s| parse_timestamp(&s))
                        .transpose()
                        .unwrap_or_else(|e| {
                            eprintln!("dispatch: {e}");
                            process::exit(2);
                        });
                    BrokerRequest::Messages {
                        worker_id: require_identity(worker_id, cli.from.clone())?,
                        unacked,
                        sent,
                        since,
                        limit,
                        id,
                    }
                }
                // `status` intentionally keeps its "no identity = all workers"
                // default — a coordinator's `dispatch status` should show the
                // whole team, so we don't fold env identity in here.
                Commands::Status { worker_id, clear } => BrokerRequest::Status {
                    worker_id,
                    clear,
                    probe: None,
                },
                Commands::Ack {
                    worker_id,
                    message_id,
                    note,
                } => BrokerRequest::Ack {
                    worker_id: require_identity(worker_id, cli.from.clone())?,
                    message_id,
                    note,
                    status: None,
                    summary: None,
                    artifacts: vec![],
                },
                // `result` rides the Ack request as a super-ack: same
                // identity resolution + broker-side validation, but `status`
                // present flips the recorded event to `result`.
                Commands::Result {
                    worker_id,
                    message_id,
                    status,
                    summary,
                    artifacts,
                    for_agent: _,
                } => BrokerRequest::Ack {
                    worker_id: require_identity(worker_id, cli.from.clone())?,
                    message_id,
                    note: None,
                    status: Some(status.as_str().to_string()),
                    summary,
                    artifacts,
                },
                Commands::Heartbeat { worker_id, status } => BrokerRequest::Heartbeat {
                    worker_id: require_identity(worker_id, cli.from.clone())?,
                    status,
                },
                Commands::Agent { action } => match action {
                    AgentAction::Start { name } => BrokerRequest::AgentStart { name },
                    AgentAction::Stop { name } => BrokerRequest::AgentStop { name },
                    AgentAction::Restart { name } => BrokerRequest::AgentRestart { name },
                },
                Commands::CodexHook { .. } | Commands::ClaudeHook { .. } => unreachable!(),
            };
            let response = backend.send_request(&request).await?;
            if for_agent {
                match &request {
                    // `listen --for-agent`: deliver a message body
                    // verbatim; render a timeout as the continue-instruction
                    // while the worker is active, else the neutral JSON timeout.
                    BrokerRequest::Listen { worker_id, .. } => {
                        render_listen_for_agent(backend.as_ref(), &config, worker_id, &response)
                            .await?;
                    }
                    // `result --for-agent`: terse completion
                    // confirmation. Only `result` sets `--for-agent` on an Ack
                    // request (plain `ack` has no such flag).
                    BrokerRequest::Ack {
                        message_id, status, ..
                    } => {
                        render_result_for_agent(&response, message_id, status.as_deref())?;
                    }
                    // `register --for-agent`: prompt body to stdout,
                    // JSON envelope to stderr. If the broker has no prompt stored
                    // for this worker, exit nonzero — the agent has nothing to do
                    // and the supervisor should restart rather than have the
                    // model see empty stdout.
                    _ => match &response {
                        BrokerResponse::Ok {
                            payload:
                                ResponsePayload::WorkerRegistered {
                                    worker_id,
                                    role_prompt: Some(prompt),
                                },
                        } => {
                            // Write the prompt body verbatim (no trailing newline
                            // added) so the agent receives byte-for-byte what the
                            // orchestrator stored.
                            use std::io::Write as _;
                            let mut stdout = std::io::stdout().lock();
                            stdout.write_all(prompt.as_bytes())?;
                            stdout.flush()?;

                            // Strip `role_prompt` from the stderr envelope so the
                            // prompt body isn't duplicated into agent logs.
                            let stripped = BrokerResponse::Ok {
                                payload: ResponsePayload::WorkerRegistered {
                                    worker_id: worker_id.clone(),
                                    role_prompt: None,
                                },
                            };
                            let json = serde_json::to_string(&stripped)?;
                            eprintln!("{json}");
                        }
                        // `send_request` returns `Ok(BrokerResponse::Error { .. })`
                        // for broker-side errors (e.g. worker_id collision when
                        // DISPATCH_AGENT_NAME drifted from what was pre-registered).
                        // Forward `message` verbatim via a typed error so the
                        // exit-code classifier in `main` stays authoritative.
                        BrokerResponse::Error { message } => {
                            return Err(dispatch::errors::DispatchError::RegisterForAgentFailed {
                                message: message.clone(),
                            });
                        }
                        _ => {
                            // Log the unexpected JSON envelope to stderr before
                            // bubbling the typed error — useful for debugging a
                            // response shape that shouldn't happen in practice.
                            let json = serde_json::to_string(&response)?;
                            eprintln!("{json}");
                            return Err(dispatch::errors::DispatchError::NoRolePromptReturned);
                        }
                    },
                }
            } else {
                let json = serde_json::to_string(&response)?;
                println!("{json}");
            }
        }
    }

    Ok(())
}

/// Render a `listen --for-agent` response for direct LLM tool-result
/// consumption.
///
/// - A delivered `Message` body is written to stdout **verbatim** (no JSON, no
///   added newline) — the coordinator authored exactly what the agent should
///   act on.
/// - A `Timeout` triggers a control-state re-query: if the worker is still
///   `active`, the configured `continue_instruction` is printed so the agent
///   loops back into `listen`; otherwise the neutral JSON timeout is emitted
///   unchanged so a stopping/stopped agent can exit.
/// - Anything else is forwarded as JSON (shouldn't normally happen for listen).
async fn render_listen_for_agent(
    backend: &dyn dispatch::backend::Backend,
    config: &dispatch::config::ResolvedConfig,
    worker_id: &str,
    response: &BrokerResponse,
) -> Result<(), DispatchError> {
    match response {
        BrokerResponse::Ok {
            payload: ResponsePayload::Message { body, .. },
        } => {
            use std::io::Write as _;
            let mut stdout = std::io::stdout().lock();
            stdout.write_all(body.as_bytes())?;
            stdout.flush()?;
        }
        BrokerResponse::Ok {
            payload: ResponsePayload::Timeout(_),
        } => {
            if worker_is_active(backend, worker_id).await {
                let agent = env_agent_name();
                println!("{}", config.continue_instruction_for(agent.as_deref()));
            } else {
                // Neutral timeout JSON — same shape as the non-for-agent path.
                println!("{}", serde_json::to_string(response)?);
            }
        }
        _ => {
            println!("{}", serde_json::to_string(response)?);
        }
    }
    Ok(())
}

/// Re-query the broker for `worker_id`'s control state via `Status` (additive,
/// no new wire variant). Returns `true` only when the worker is present and
/// `Active`; any error, miss, or non-active state → `false` (→ emit the neutral
/// timeout so the agent isn't told to keep looping a dead/stopping worker).
async fn worker_is_active(backend: &dyn dispatch::backend::Backend, worker_id: &str) -> bool {
    let request = BrokerRequest::Status {
        worker_id: Some(worker_id.to_string()),
        clear: false,
        probe: None,
    };
    matches!(
        backend.send_request(&request).await,
        Ok(BrokerResponse::Ok {
            payload: ResponsePayload::StatusResult { workers },
        }) if workers.iter().any(|w| w.id == worker_id && w.control_state == ControlState::Active)
    )
}

/// Render `result --for-agent`: a terse one-line confirmation on
/// success — the loop contract lives elsewhere (the stop hook re-arms `listen`
/// for hook-capable agents; SEAMS supplies any coda for the rest), so this adds
/// no loop logic. A broker-side failure becomes a typed error so the exit code
/// reflects it instead of a silent JSON dump.
fn render_result_for_agent(
    response: &BrokerResponse,
    message_id: &str,
    status: Option<&str>,
) -> Result<(), DispatchError> {
    match response {
        BrokerResponse::Ok {
            payload: ResponsePayload::AckConfirm { .. },
        } => {
            let short = &message_id[..message_id.len().min(8)];
            println!("result recorded: {} ({short})", status.unwrap_or("done"));
            Ok(())
        }
        BrokerResponse::Error { message } => Err(DispatchError::ResultForAgentFailed {
            message: message.clone(),
        }),
        _ => {
            println!("{}", serde_json::to_string(response)?);
            Ok(())
        }
    }
}

async fn run_codex_hook(
    action: &HookAction,
    cwd: &std::path::Path,
) -> Result<(), dispatch::errors::DispatchError> {
    match action {
        HookAction::Stop => {
            hooks::run_stop_hook(cwd).await;
        }
        HookAction::Install => {
            let path = hooks::codex::install(cwd).await?;
            eprintln!(
                "installed codex hook at {}\nensure features.hooks = true is set in .codex/config.toml (already added if missing)",
                path.display()
            );
        }
        HookAction::Uninstall => match hooks::codex::uninstall(cwd).await? {
            Some(path) => eprintln!("removed {}", path.display()),
            None => eprintln!("no codex hook installed"),
        },
    }
    Ok(())
}

async fn run_claude_hook(
    action: &HookAction,
    cwd: &std::path::Path,
) -> Result<(), dispatch::errors::DispatchError> {
    match action {
        HookAction::Stop => {
            hooks::run_stop_hook(cwd).await;
        }
        HookAction::Install => {
            let path = hooks::claude::install(cwd).await?;
            eprintln!("installed claude hook at {}", path.display());
        }
        HookAction::Uninstall => match hooks::claude::uninstall(cwd).await? {
            Some(path) => eprintln!("removed entry from {}", path.display()),
            None => eprintln!("no claude hook installed"),
        },
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_timestamp_accepts_unix_seconds() {
        assert_eq!(parse_timestamp("1700000000").unwrap(), 1700000000);
    }

    #[test]
    fn parse_timestamp_relative_does_not_overflow() {
        // u64::MAX followed by a suffix must not panic in debug or wrap in
        // release — it should return a readable error.
        let input = format!("{}m", u64::MAX);
        let err = parse_timestamp(&input).unwrap_err();
        assert!(err.contains("overflow"), "got: {err}");
    }

    #[test]
    fn parse_timestamp_rejects_garbage() {
        assert!(parse_timestamp("not-a-timestamp").is_err());
    }

    #[test]
    fn identity_prefers_specific_flag_over_from_and_env() {
        assert_eq!(
            resolve_identity_with_env(Some("flag".into()), Some("from".into()), Some("env".into())),
            Some("flag".into())
        );
    }

    #[test]
    fn identity_falls_back_from_then_env() {
        // No --worker-id → global --from wins over env.
        assert_eq!(
            resolve_identity_with_env(None, Some("from".into()), Some("env".into())),
            Some("from".into())
        );
        // Neither flag → env (the dispatch-launched-agent case).
        assert_eq!(
            resolve_identity_with_env(None, None, Some("env".into())),
            Some("env".into())
        );
    }

    #[test]
    fn identity_none_when_nothing_present() {
        assert_eq!(resolve_identity_with_env(None, None, None), None);
    }

    #[test]
    fn listen_timeout_precedence_flag_env_default() {
        // Flag wins.
        assert_eq!(resolve_listen_timeout_with_env(Some(5), Some("99")), 5);
        // No flag → env.
        assert_eq!(resolve_listen_timeout_with_env(None, Some("99")), 99);
        // Neither → built-in default (270).
        assert_eq!(
            resolve_listen_timeout_with_env(None, None),
            DEFAULT_LISTEN_TIMEOUT_SECS
        );
        // Unparseable env falls through to the default rather than erroring.
        assert_eq!(
            resolve_listen_timeout_with_env(None, Some("not-a-number")),
            DEFAULT_LISTEN_TIMEOUT_SECS
        );
    }
}
