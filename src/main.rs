use std::process;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::Parser;

use dispatch::backend::create_backend;
use dispatch::cli::{AgentAction, Cli, Commands};
use dispatch::config::resolve_config;
use dispatch::logging::init_tracing;
use dispatch::protocol::BrokerRequest;

/// Parse a timestamp string as either a relative duration (e.g. "5m", "1h", "30s")
/// or an absolute Unix timestamp. Returns a Unix timestamp in seconds.
fn parse_timestamp(s: &str) -> Result<u64, String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    // Try relative duration: number + suffix (s/m/h/d)
    if let Some(num_str) = s.strip_suffix('s') {
        if let Ok(n) = num_str.parse::<u64>() {
            return Ok(now.saturating_sub(n));
        }
    }
    if let Some(num_str) = s.strip_suffix('m') {
        if let Ok(n) = num_str.parse::<u64>() {
            return Ok(now.saturating_sub(n * 60));
        }
    }
    if let Some(num_str) = s.strip_suffix('h') {
        if let Ok(n) = num_str.parse::<u64>() {
            return Ok(now.saturating_sub(n * 3600));
        }
    }
    if let Some(num_str) = s.strip_suffix('d') {
        if let Ok(n) = num_str.parse::<u64>() {
            return Ok(now.saturating_sub(n * 86400));
        }
    }

    // Try absolute Unix timestamp
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
                } => BrokerRequest::Register {
                    name,
                    role,
                    description,
                    capabilities,
                    ttl_secs: ttl,
                    evict,
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
            };
            let response = backend.send_request(&request).await?;
            let json = serde_json::to_string(&response)?;
            println!("{json}");
        }
    }

    Ok(())
}
