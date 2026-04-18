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
        #[serde(skip_serializing_if = "Option::is_none")]
        ttl_secs: Option<u64>,
        /// If true, evict any existing worker with the same name.
        #[serde(default)]
        evict: bool,
    },
    /// List active workers.
    Team {
        #[serde(skip_serializing_if = "Option::is_none")]
        from: Option<String>,
    },
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
    /// TTL duration in seconds (used to renew `expires_at` on heartbeat).
    /// `#[serde(default)]` so newer clients can deserialise responses from
    /// older brokers that don't yet send this field (rolling-upgrade safe).
    #[serde(default)]
    pub ttl_secs: u64,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify each BrokerRequest variant survives a JSON round-trip.
    #[test]
    fn request_round_trip() {
        let cases = vec![
            BrokerRequest::Register {
                name: "w1".into(),
                role: "builder".into(),
                description: "test".into(),
                capabilities: vec!["rust".into()],
                ttl_secs: None,
                evict: false,
            },
            BrokerRequest::Team { from: None },
            BrokerRequest::Send {
                to: "w1".into(),
                body: "hello".into(),
                from: Some("w2".into()),
            },
            BrokerRequest::Listen {
                worker_id: "w1".into(),
                timeout_secs: 30,
            },
            BrokerRequest::Heartbeat {
                worker_id: "w1".into(),
            },
        ];
        for req in &cases {
            let json = serde_json::to_string(req).expect("serialize request");
            let back: BrokerRequest = serde_json::from_str(&json).expect("deserialize request");
            assert_eq!(
                serde_json::to_value(req).unwrap(),
                serde_json::to_value(&back).unwrap(),
            );
        }
    }

    /// Verify each ResponsePayload variant survives a round-trip when wrapped
    /// in BrokerResponse::Ok, ensuring the untagged enum deserialises to the
    /// correct variant.
    #[test]
    fn response_round_trip() {
        let payloads = vec![
            ResponsePayload::WorkerRegistered {
                worker_id: "abc".into(),
            },
            ResponsePayload::MessageAck {
                message_id: "msg-1".into(),
            },
            ResponsePayload::WorkerList {
                workers: vec![Worker {
                    id: "w1".into(),
                    name: "worker-1".into(),
                    role: "builder".into(),
                    description: "test worker".into(),
                    capabilities: vec![],
                    ttl_secs: 300,
                    expires_at: 1000,
                }],
            },
            ResponsePayload::HeartbeatAck {
                worker_id: "w1".into(),
                expires_at: 2000,
            },
            ResponsePayload::Message {
                message_id: "m1".into(),
                from: Some("w2".into()),
                to: "w1".into(),
                body: "payload".into(),
            },
            ResponsePayload::Timeout {
                worker_id: "w1".into(),
            },
            ResponsePayload::Ack {},
        ];
        for payload in payloads {
            let resp = BrokerResponse::Ok {
                payload: payload.clone(),
            };
            let json = serde_json::to_string(&resp).expect("serialize response");
            let back: BrokerResponse = serde_json::from_str(&json).expect("deserialize response");
            assert_eq!(
                serde_json::to_value(&resp).unwrap(),
                serde_json::to_value(&back).unwrap(),
            );
        }
    }

    /// BrokerResponse::Error round-trips correctly.
    #[test]
    fn error_response_round_trip() {
        let resp = BrokerResponse::Error {
            message: "not found".into(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let back: BrokerResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(
            serde_json::to_value(&resp).unwrap(),
            serde_json::to_value(&back).unwrap(),
        );
    }
}
