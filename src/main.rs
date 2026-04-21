use std::process;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::Parser;

use dispatch::backend::create_backend;
use dispatch::cli::{AgentAction, Cli, Commands, HookAction};
use dispatch::config::resolve_config;
use dispatch::hooks;
use dispatch::logging::init_tracing;
use dispatch::protocol::{BrokerRequest, BrokerResponse, ResponsePayload};

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
    let (monitor_port, launch_agents) = if let Commands::Serve { monitor, launch } = &cli.command {
        (monitor.or(config.monitor_port), *launch)
    } else {
        (None, false)
    };

    let backend = create_backend(&config, monitor_port, launch_agents)?;

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
                    name,
                    role,
                    description,
                    capabilities,
                    ttl_secs: ttl,
                    evict,
                    worker_id,
                    role_prompt,
                },
                Commands::Team => BrokerRequest::Team {
                    from: cli.from.clone(),
                },
                Commands::Send { to, body } => BrokerRequest::Send {
                    to,
                    body,
                    from: cli.from.clone(),
                },
                Commands::Listen { worker_id, timeout } => BrokerRequest::Listen {
                    worker_id,
                    timeout_secs: timeout,
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
                        worker_id,
                        unacked,
                        sent,
                        since,
                        limit,
                        id,
                    }
                }
                Commands::Status { worker_id, clear } => BrokerRequest::Status { worker_id, clear },
                Commands::Ack {
                    worker_id,
                    message_id,
                    note,
                } => BrokerRequest::Ack {
                    worker_id,
                    message_id,
                    note,
                },
                Commands::Heartbeat { worker_id, status } => {
                    BrokerRequest::Heartbeat { worker_id, status }
                }
                Commands::Agent { action } => match action {
                    AgentAction::Start { name } => BrokerRequest::AgentStart { name },
                    AgentAction::Stop { name } => BrokerRequest::AgentStop { name },
                    AgentAction::Restart { name } => BrokerRequest::AgentRestart { name },
                },
                Commands::CodexHook { .. } | Commands::ClaudeHook { .. } => unreachable!(),
            };
            let response = backend.send_request(&request).await?;
            if for_agent {
                // Prompt body to stdout, JSON envelope to stderr. If the
                // broker has no prompt stored for this worker, exit nonzero
                // — the agent has nothing to do and the supervisor should
                // restart rather than have the model see empty stdout.
                match &response {
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
                }
            } else {
                let json = serde_json::to_string(&response)?;
                println!("{json}");
            }
        }
    }

    Ok(())
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
                "installed codex hook at {}\nensure features.codex_hooks = true is set in .codex/config.toml (already added if missing)",
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
}
