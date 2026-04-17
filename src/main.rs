use std::process;

use clap::Parser;

use dispatch::backend::create_backend;
use dispatch::cli::{Cli, Commands};
use dispatch::config::resolve_config;
use dispatch::logging::init_tracing;
use dispatch::protocol::BrokerRequest;

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
                Commands::Heartbeat { worker_id } => BrokerRequest::Heartbeat { worker_id },
            };
            let response = backend.send_request(&request).await?;
            let json = serde_json::to_string(&response)?;
            println!("{json}");
        }
    }

    Ok(())
}
