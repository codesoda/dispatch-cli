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
        process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<(), dispatch::errors::DispatchError> {
    let cwd = std::env::current_dir().map_err(dispatch::errors::DispatchError::Io)?;
    let config = resolve_config(cli.cell_id.as_deref(), &cwd)?;

    tracing::debug!(cell_id = %config.cell_id, project_root = %config.project_root.display(), "resolved config");

    let backend = create_backend(
        config.backend.as_deref(),
        &config.project_root,
        &config.cell_id,
    )?;

    match cli.command {
        Commands::Serve => {
            backend.serve().await?;
        }
        cmd => {
            let request = match cmd {
                Commands::Serve => unreachable!(),
                Commands::Register {
                    name,
                    role,
                    description,
                    capabilities,
                } => BrokerRequest::Register {
                    name,
                    role,
                    description,
                    capabilities,
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
