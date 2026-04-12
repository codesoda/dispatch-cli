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

    let config = resolve_config(cli.cell_id.as_deref(), &cwd)?;

    tracing::debug!(cell_id = %config.cell_id, project_root = %config.project_root.display(), "resolved config");

    // Extract monitor port before matching (Serve consumes it).
    let monitor_port = if let Commands::Serve { monitor } = &cli.command {
        *monitor
    } else {
        None
    };

    let backend = create_backend(
        config.backend.as_deref(),
        &config.project_root,
        &config.cell_id,
        monitor_port,
    )?;

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
                } => BrokerRequest::Register {
                    name,
                    role,
                    description,
                    capabilities,
                    ttl_secs: ttl,
                },
                Commands::Team => BrokerRequest::Team,
                Commands::Send { to, body, from } => BrokerRequest::Send { to, body, from },
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
