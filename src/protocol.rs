use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A message queued in a worker's mailbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub message_id: String,
    pub from: Option<String>,
    pub to: String,
    pub body: String,
}

/// A request sent from a client to the broker over the Unix socket.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BrokerRequest {
    /// Register a new worker.
    Register {
        name: String,
        role: String,
        description: String,
        capabilities: Vec<String>,
    },
    /// List active workers.
    Team,
    /// Send a message to a worker.
    Send {
        to: String,
        body: String,
        from: Option<String>,
    },
    /// Long-poll for next message in a worker's mailbox.
    Listen {
        worker_id: String,
        timeout_secs: u64,
    },
    /// Renew a worker's liveness TTL.
    Heartbeat { worker_id: String },
}

/// A response sent from the broker back to the client.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum BrokerResponse {
    /// Successful response with a payload.
    Ok {
        #[serde(flatten)]
        payload: ResponsePayload,
    },
    /// Error response.
    Error { message: String },
}

/// A registered worker in the broker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Worker {
    pub id: String,
    pub name: String,
    pub role: String,
    pub description: String,
    pub capabilities: Vec<String>,
    /// Unix timestamp (seconds) when this worker's TTL expires.
    pub expires_at: u64,
}

/// The payload inside a successful response, varies by request type.
///
/// **Variant order matters**: serde tries untagged variants in declaration order.
/// More-specific variants (with fields) must come before `Ack {}` which matches
/// any object. `Ack` must be last or it will swallow every response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponsePayload {
    /// A message received from listen.
    Message {
        message_id: String,
        from: Option<String>,
        to: String,
        body: String,
    },
    /// Heartbeat renewed.
    HeartbeatAck { worker_id: String, expires_at: u64 },
    /// List of active workers.
    WorkerList { workers: Vec<Worker> },
    /// A worker was registered; returns the assigned worker ID.
    WorkerRegistered { worker_id: String },
    /// A message delivered or queued.
    MessageAck { message_id: String },
    /// Listen timed out with no messages.
    Timeout { worker_id: String },
    /// Map of arbitrary key-value data (used as a flexible response shape).
    Data {
        data: HashMap<String, serde_json::Value>,
    },
    /// Generic acknowledgement with no extra data. Must be last — matches any object.
    Ack {},
}
