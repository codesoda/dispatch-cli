use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum DispatchError {
    #[error("dispatch.config.toml already exists at {path} -- use a text editor to modify it")]
    ConfigAlreadyExists { path: PathBuf },

    #[error("config not found at {path} -- run `dispatch init` or create dispatch.config.toml")]
    ConfigNotFound { path: PathBuf },

    #[error("invalid config at {path}: {reason}")]
    ConfigInvalid { path: PathBuf, reason: String },

    #[error("broker not running for cell {cell_id} -- run `dispatch serve` first")]
    BrokerNotRunning { cell_id: String },

    #[error("broker already running for cell {cell_id} at {socket_path}")]
    BrokerAlreadyRunning {
        cell_id: String,
        socket_path: PathBuf,
    },

    #[error("worker not found: {worker_id}")]
    WorkerNotFound { worker_id: String },

    #[error("worker expired: {worker_id}")]
    WorkerExpired { worker_id: String },

    #[error("unknown backend \"{name}\" -- only \"local\" is supported")]
    UnknownBackend { name: String },

    #[error("connection failed: {reason}")]
    ConnectionFailed { reason: String },

    #[error("agent config error for \"{name}\": {reason}")]
    AgentConfigError { name: String, reason: String },

    #[error("failed to launch agent \"{name}\": {reason}")]
    AgentLaunchFailed { name: String, reason: String },

    #[error("prompt file not found for agent \"{name}\": {path}")]
    PromptFileNotFound { name: String, path: PathBuf },

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}
