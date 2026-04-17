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
    /// Path to dispatch.config.toml (default: ./dispatch.config.toml)
    #[arg(long, global = true)]
    pub config: Option<std::path::PathBuf>,

    /// Override the cell identity (takes precedence over env var and config)
    #[arg(long, global = true)]
    pub cell_id: Option<String>,

    /// Identify the calling worker (renews TTL on all commands, used as sender on send)
    #[arg(long, global = true)]
    pub from: Option<String>,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Initialize a dispatch.config.toml in the current directory
    Init,

    /// Start the embedded broker server on a Unix domain socket
    Serve {
        /// Start an HTTP monitor dashboard on this port
        #[arg(long)]
        monitor: Option<u16>,

        /// Auto-launch configured agents (default: print commands only)
        #[arg(long)]
        launch: bool,
    },

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

        /// Worker TTL in seconds (default: 3600)
        #[arg(long)]
        ttl: Option<u64>,

        /// Evict any existing worker with the same name before registering
        #[arg(long)]
        evict: bool,
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
