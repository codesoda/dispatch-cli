use serde::{Deserialize, Serialize};

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

/// The payload inside a successful response, varies by request type.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponsePayload {
    /// Generic acknowledgement with no extra data.
    Ack {},
}
