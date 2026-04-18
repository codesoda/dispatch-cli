use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};

/// Maximum number of historical status taglines retained per worker.
/// Current tagline is `Worker.last_status`; this cap bounds the ring that
/// backs the "last N" strip on the agent card.
pub const STATUS_HISTORY_MAX: usize = 3;

/// A prior status tagline plus when it was set. Oldest first in the ring.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StatusEntry {
    pub status: String,
    pub set_at: u64,
}

/// A message queued in a worker's mailbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub message_id: String,
    pub from: Option<String>,
    pub to: String,
    pub body: String,
    /// When the message was sent (queued by the broker).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sent_at: Option<u64>,
    /// When the message was delivered to the recipient via listen.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivered_at: Option<u64>,
    /// When the message was acknowledged by the recipient.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acked_at: Option<u64>,
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
    /// Renew a worker's liveness TTL, optionally updating status.
    Heartbeat {
        worker_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<String>,
    },
    /// Acknowledge receipt of a message.
    Ack {
        worker_id: String,
        message_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    /// Query event history.
    Events {
        #[serde(skip_serializing_if = "Option::is_none")]
        since: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        until: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        event_type: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        worker: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        limit: Option<usize>,
    },
    /// Query message history.
    Messages {
        worker_id: String,
        #[serde(default)]
        unacked: bool,
        #[serde(default)]
        sent: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        since: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        limit: Option<usize>,
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
    },
    /// Query worker status or clear a worker's status.
    Status {
        #[serde(skip_serializing_if = "Option::is_none")]
        worker_id: Option<String>,
        #[serde(default)]
        clear: bool,
    },
    /// Start a configured agent by name.
    AgentStart { name: String },
    /// Stop a managed agent by name.
    AgentStop { name: String },
    /// Restart a managed agent by name (stop then start).
    AgentRestart { name: String },
}

/// Summary of a worker's status for the status query response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerStatus {
    pub id: String,
    pub name: String,
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_status_at: Option<u64>,
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
    /// Human-readable status tagline (e.g. "Running e2e tests 3/10").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_status: Option<String>,
    /// Unix timestamp when `last_status` was last set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_status_at: Option<u64>,
    /// Recent status taglines, oldest first. Capped at `STATUS_HISTORY_MAX`.
    /// The current `last_status` is **not** included here — it lives on its
    /// own field. Skipped when empty so older clients see the same shape.
    #[serde(default, skip_serializing_if = "VecDeque::is_empty")]
    pub status_history: VecDeque<StatusEntry>,
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
    /// Message acknowledgement confirmed. MUST appear before `MessageAck`:
    /// serde's untagged deserializer tries variants in declaration order and
    /// ignores unknown fields, so an `AckConfirm` payload would otherwise
    /// silently deserialize as `MessageAck` and drop `ack_confirmed`.
    AckConfirm {
        message_id: String,
        ack_confirmed: bool,
    },
    /// A message delivered or queued.
    MessageAck { message_id: String },
    /// Worker status query result.
    StatusResult { workers: Vec<WorkerStatus> },
    /// Event history query result.
    EventList { events: Vec<serde_json::Value> },
    /// Message history query result.
    MessageList { messages: Vec<Message> },
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
                status: Some("running tests".into()),
            },
            BrokerRequest::Ack {
                worker_id: "w1".into(),
                message_id: "m1".into(),
                note: Some("starting impl".into()),
            },
            BrokerRequest::Events {
                since: Some(100),
                until: Some(200),
                event_type: Some("send".into()),
                worker: Some("w1".into()),
                limit: Some(10),
            },
            BrokerRequest::Messages {
                worker_id: "w1".into(),
                unacked: true,
                sent: false,
                since: None,
                limit: Some(50),
                id: None,
            },
            BrokerRequest::Status {
                worker_id: Some("w1".into()),
                clear: false,
            },
            BrokerRequest::AgentStart {
                name: "reviewer".into(),
            },
            BrokerRequest::AgentStop {
                name: "reviewer".into(),
            },
            BrokerRequest::AgentRestart {
                name: "reviewer".into(),
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
                    last_status: None,
                    last_status_at: None,
                    status_history: VecDeque::new(),
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
            ResponsePayload::AckConfirm {
                message_id: "msg-2".into(),
                ack_confirmed: true,
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

    /// Worker round-trips with a populated status_history; the empty field
    /// is omitted from JSON so older clients keep parsing the response.
    #[test]
    fn worker_status_history_round_trip() {
        let mut history = VecDeque::new();
        history.push_back(StatusEntry {
            status: "starting".into(),
            set_at: 100,
        });
        history.push_back(StatusEntry {
            status: "running tests".into(),
            set_at: 105,
        });
        let worker = Worker {
            id: "w1".into(),
            name: "worker-1".into(),
            role: "builder".into(),
            description: "test".into(),
            capabilities: vec![],
            ttl_secs: 300,
            expires_at: 2000,
            last_status: Some("waiting on review".into()),
            last_status_at: Some(110),
            status_history: history.clone(),
        };
        let json = serde_json::to_string(&worker).unwrap();
        assert!(json.contains("status_history"));
        let back: Worker = serde_json::from_str(&json).unwrap();
        assert_eq!(back.status_history, history);

        // Empty history is omitted from JSON and absent fields default to empty.
        let empty = Worker {
            status_history: VecDeque::new(),
            ..worker
        };
        let json = serde_json::to_string(&empty).unwrap();
        assert!(!json.contains("status_history"));
        let back: Worker = serde_json::from_str(&json).unwrap();
        assert!(back.status_history.is_empty());
    }

    /// Regression: `AckConfirm` is structurally a superset of `MessageAck`.
    /// Because serde's untagged enum ignores unknown fields, `AckConfirm` MUST
    /// be declared before `MessageAck` or its payload silently degrades and
    /// `ack_confirmed` is dropped on deserialize.
    #[test]
    fn ack_confirm_does_not_degrade_to_message_ack() {
        let payload = ResponsePayload::AckConfirm {
            message_id: "m1".into(),
            ack_confirmed: true,
        };
        let resp = BrokerResponse::Ok { payload };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(
            json.contains("\"ack_confirmed\":true"),
            "serialized form must include ack_confirmed: {json}"
        );
        let back: BrokerResponse = serde_json::from_str(&json).unwrap();
        match back {
            BrokerResponse::Ok {
                payload: ResponsePayload::AckConfirm { ack_confirmed, .. },
            } => assert!(ack_confirmed),
            other => panic!("expected AckConfirm, got {other:?}"),
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
