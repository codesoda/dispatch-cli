use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "dispatch",
    version,
    about = "Multi-agent, multi-vendor orchestration system",
    arg_required_else_help = true,
    after_help = "\
Exit codes:
  0  Success
  1  Runtime error
  2  Configuration error

Docs: https://github.com/codesoda/dispatch"
)]
pub struct Cli {
    /// Override the cell identity (takes precedence over env var and config)
    #[arg(long, global = true)]
    pub cell_id: Option<String>,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Initialize a dispatch.config.toml in the current directory
    Init,

    /// Start the embedded broker server on a Unix domain socket
    Serve,

    /// Register a worker with the broker
    Register {
        /// Worker name
        #[arg(long)]
        name: String,

        /// Worker role
        #[arg(long)]
        role: String,

        /// Worker description
        #[arg(long)]
        description: String,

        /// Worker capabilities (repeatable)
        #[arg(long = "capability")]
        capabilities: Vec<String>,
    },

    /// List active workers in the current cell
    Team,

    /// Send a message to a worker
    Send {
        /// Target worker ID
        #[arg(long)]
        to: String,

        /// Message body
        #[arg(long)]
        body: String,

        /// Sender identity (optional)
        #[arg(long)]
        from: Option<String>,
    },

    /// Long-poll for incoming messages
    Listen {
        /// Worker ID to listen as
        #[arg(long)]
        worker_id: String,

        /// Timeout in seconds (default: 30)
        #[arg(long, default_value = "30")]
        timeout: u64,
    },

    /// Renew worker liveness TTL
    Heartbeat {
        /// Worker ID to heartbeat
        #[arg(long)]
        worker_id: String,
    },
}
