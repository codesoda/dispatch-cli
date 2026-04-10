use std::process;

use clap::Parser;

use dispatch::cli::{Cli, Commands};
use dispatch::logging::init_tracing;

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
    match cli.command {
        Commands::Serve => {
            eprintln!("dispatch serve: not yet implemented");
        }
        Commands::Register {
            name,
            role,
            description,
            capabilities,
        } => {
            eprintln!(
                "dispatch register: not yet implemented (name={name}, role={role}, desc={description}, caps={capabilities:?})"
            );
        }
        Commands::Team => {
            eprintln!("dispatch team: not yet implemented");
        }
        Commands::Send { to, body, from } => {
            eprintln!("dispatch send: not yet implemented (to={to}, body={body}, from={from:?})");
        }
        Commands::Listen { worker_id, timeout } => {
            eprintln!(
                "dispatch listen: not yet implemented (worker_id={worker_id}, timeout={timeout})"
            );
        }
        Commands::Heartbeat { worker_id } => {
            eprintln!("dispatch heartbeat: not yet implemented (worker_id={worker_id})");
        }
    }

    Ok(())
}
