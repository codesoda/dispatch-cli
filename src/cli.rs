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

        /// Pre-assigned worker id (issue #43). When set, the broker uses
        /// this id rather than generating one. If a worker with this id
        /// already exists with the same name+role, the call is treated as
        /// an idempotent claim — used by spawned agents to attach to a
        /// worker that dispatch pre-registered for them at spawn time.
        #[arg(long = "worker-id")]
        worker_id: Option<String>,

        /// Role prompt body to associate with this worker (issue #43).
        /// Only the orchestrator passes this — at pre-register time it
        /// loads the agent's `prompt_file` and ships the content here so
        /// the spawned agent can fetch it back via `--for-agent`.
        #[arg(long = "role-prompt")]
        role_prompt: Option<String>,

        /// Output the role prompt body to stdout instead of the JSON
        /// envelope (issue #43). Intentional CLI wart whose only purpose
        /// is to be friendly to a downstream LLM tool result: the spawned
        /// agent's first tool call is `dispatch register --for-agent`,
        /// and the prompt body landing on stdout becomes its next
        /// instruction. Without the flag, behavior is unchanged.
        #[arg(long = "for-agent")]
        for_agent: bool,
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
        #[arg(long = "type", value_name = "type")]
        event_type: Option<String>,

        /// Filter by worker ID
        #[arg(long)]
        worker: Option<String>,

        /// Show events since timestamp (Unix seconds, or relative: 30s, 5m, 1h, 2d)
        #[arg(long)]
        since: Option<String>,

        /// Show events until timestamp (Unix seconds, or relative: 30s, 5m, 1h, 2d)
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

        /// Show messages since timestamp (Unix seconds, or relative: 30s, 5m, 1h, 2d)
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

    /// Codex CLI hook integration (stop handler, install, uninstall)
    CodexHook {
        #[command(subcommand)]
        action: HookAction,
    },

    /// Claude Code CLI hook integration (stop handler, install, uninstall)
    ClaudeHook {
        #[command(subcommand)]
        action: HookAction,
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

#[derive(Subcommand)]
pub enum HookAction {
    /// Handler invoked by the vendor CLI's Stop hook. Prints a JSON block
    /// decision on stdout so the agent stays alive to wait for more messages.
    Stop,
    /// Register the dispatch stop hook in this project's vendor config files
    Install,
    /// Remove the dispatch stop hook from this project's vendor config files
    Uninstall,
}
