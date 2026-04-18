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

    /// Query event history
    Events {
        /// Filter by event type (register, send, deliver, ack, heartbeat, expire)
        #[arg(long, name = "type")]
        event_type: Option<String>,

        /// Filter by worker ID
        #[arg(long)]
        worker: Option<String>,

        /// Show events since timestamp (RFC3339 or relative: 5m, 1h, 30s)
        #[arg(long)]
        since: Option<String>,

        /// Show events until timestamp (RFC3339 or relative)
        #[arg(long)]
        until: Option<String>,

        /// Maximum number of events to return (default: 100)
        #[arg(long)]
        limit: Option<usize>,
    },

    /// Query message history (non-destructive)
    Messages {
        /// Worker ID to inspect messages for
        #[arg(long)]
        worker_id: String,

        /// Show only delivered but unacked messages
        #[arg(long)]
        unacked: bool,

        /// Show messages sent by this worker (instead of received)
        #[arg(long)]
        sent: bool,

        /// Show messages since timestamp (RFC3339 or relative: 5m, 1h, 30s)
        #[arg(long)]
        since: Option<String>,

        /// Maximum number of messages to return
        #[arg(long)]
        limit: Option<usize>,

        /// Look up a single message by ID
        #[arg(long)]
        id: Option<String>,
    },

    /// Query worker status
    Status {
        /// Show status for a specific worker
        #[arg(long)]
        worker_id: Option<String>,

        /// Clear the worker's status (requires --worker-id)
        #[arg(long)]
        clear: bool,
    },

    /// Acknowledge receipt of a message
    Ack {
        /// Worker ID that received the message
        #[arg(long)]
        worker_id: String,

        /// Message ID to acknowledge
        #[arg(long)]
        message_id: String,

        /// Optional note (e.g. "starting implementation")
        #[arg(long)]
        note: Option<String>,
    },

    /// Renew worker liveness TTL
    Heartbeat {
        /// Worker ID to heartbeat
        #[arg(long)]
        worker_id: String,

        /// Set a status tagline (e.g. "Running e2e tests 3/10")
        #[arg(long)]
        status: Option<String>,
    },

    /// Manage the lifecycle of configured agents (start, stop, restart)
    Agent {
        #[command(subcommand)]
        action: AgentAction,
    },
}

#[derive(Subcommand)]
pub enum AgentAction {
    /// Start a configured agent by name or worker ID
    Start {
        /// Agent name from config, or worker ID of a registered agent
        name: String,
    },
    /// Stop a running agent by name or worker ID
    Stop {
        /// Agent name from config, or worker ID of a registered agent
        name: String,
    },
    /// Restart a running agent by name or worker ID
    Restart {
        /// Agent name from config, or worker ID of a registered agent
        name: String,
    },
}
