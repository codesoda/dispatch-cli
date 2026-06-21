use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use thiserror::Error;
use tokio::sync::{broadcast, Notify};

use crate::protocol::{ControlState, Message, StatusEntry, Worker, STATUS_HISTORY_MAX};

/// Errors returned by `BrokerState` mutations. Distinct from `DispatchError`
/// because the broker is process-internal — these are translated at the IPC
/// boundary into `BrokerResponse::Error { message }` (whose `message` is
/// this enum's `Display`) and at the orchestrator boundary into
/// `DispatchError::AgentLaunchFailed`. Keeping them typed lets call sites
/// match on the variant (e.g. distinguish a collision from a future "not
/// found" or "quota exceeded") instead of string-matching on `format!` output.
#[derive(Debug, Error)]
pub enum BrokerError {
    #[error(
        "worker_id {supplied} already registered as {existing_name}/{existing_role} -- supplied {requested_name}/{requested_role} does not match"
    )]
    WorkerIdCollision {
        supplied: String,
        existing_name: String,
        existing_role: String,
        requested_name: String,
        requested_role: String,
    },
}

/// Default worker TTL in seconds (1 hour).
pub(super) const DEFAULT_WORKER_TTL_SECS: u64 = 3600;

/// Default maximum number of events retained in history.
const DEFAULT_EVENT_HISTORY_MAX: usize = 10_000;

/// How long (seconds) a `stopping` worker lingers as a tombstone before the
/// broker finalizes it. The drain window lets a dying agent's late stop-hook
/// call still see `stopping` (and be allowed to exit) instead of racing the
/// record's removal. Overridable via config `stopping_drain_secs`.
pub const DEFAULT_STOPPING_DRAIN_SECS: u64 = 10;

/// Why a worker was removed during an eviction sweep — drives which event the
/// caller emits (`expire` vs a `lifecycle` "stopped").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvictReason {
    /// The worker's TTL (`expires_at`) elapsed.
    TtlExpired,
    /// The worker was `stopping` and its drain window elapsed.
    StopDrained,
}

/// Event emitted by the broker for the monitor dashboard.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BrokerEvent {
    pub kind: String,
    pub worker_id: String,
    /// Human-readable worker name (for display in UI).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_name: Option<String>,
    pub detail: String,
    /// Full structured payload for the event (shown in web UI and console).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
    pub timestamp: u64,
}

/// Record of a message acknowledgement. When a `dispatch result` rides the ack
/// substrate, `status`/`summary`/`artifacts` capture the completion;
/// they are `None`/empty for a plain `ack`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AckRecord {
    pub message_id: String,
    pub worker_id: String,
    pub note: Option<String>,
    pub acked_at: u64,
    /// Completion status (`done` | `failed` | `blocked`) when recorded via
    /// `dispatch result`; `None` for a plain ack.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Optional free-text completion summary.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Optional artifact paths/URLs produced by the task.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<String>,
}

/// In-memory broker state.
#[derive(Debug)]
pub struct BrokerState {
    /// Registered workers keyed by worker ID.
    pub workers: HashMap<String, Worker>,
    /// Per-worker message mailboxes keyed by worker ID.
    pub mailboxes: HashMap<String, VecDeque<Message>>,
    /// Per-worker notification channels for long-poll wakeup.
    pub notifiers: HashMap<String, Arc<Notify>>,
    /// Message acknowledgement log keyed by message ID.
    pub ack_log: HashMap<String, AckRecord>,
    /// Bounded event history for queries.
    pub event_history: VecDeque<BrokerEvent>,
    /// Maximum number of events to retain.
    pub event_history_max: usize,
    /// Bounded message history for queries (non-destructive inspection).
    pub message_history: VecDeque<Message>,
    /// Maximum number of messages to retain in history.
    pub message_history_max: usize,
    /// Default TTL for workers that don't specify one.
    pub default_ttl: u64,
    /// Drain window (seconds) a `stopping` worker lingers before the broker
    /// finalizes it. Set from config `stopping_drain_secs` in `serve`.
    pub stopping_drain_secs: u64,
    /// When true, prompt/packet-body events may carry the full body; otherwise
    /// bodies are logged by hash + byte size only. Set from config
    /// `log_prompt_bodies` in `serve`; defaults to `false`.
    pub log_prompt_bodies: bool,
    /// Total number of messages sent through the broker.
    pub messages_sent: u64,
    /// Total number of messages delivered to listeners.
    pub messages_delivered: u64,
    /// Total number of requests handled.
    pub requests_handled: u64,
    /// Per-worker role-prompt body keyed by worker ID. Populated
    /// at orchestrator pre-register time; returned in the `WorkerRegistered`
    /// response when the spawned agent calls `dispatch register` to claim
    /// its session. Absent for workers registered the legacy way.
    pub role_prompts: HashMap<String, String>,
}

impl Default for BrokerState {
    fn default() -> Self {
        Self::with_default_ttl(DEFAULT_WORKER_TTL_SECS)
    }
}

impl BrokerState {
    pub fn new() -> Self {
        Self::with_default_ttl(DEFAULT_WORKER_TTL_SECS)
    }

    pub fn with_default_ttl(default_ttl: u64) -> Self {
        Self {
            workers: HashMap::new(),
            mailboxes: HashMap::new(),
            notifiers: HashMap::new(),
            ack_log: HashMap::new(),
            event_history: VecDeque::new(),
            event_history_max: DEFAULT_EVENT_HISTORY_MAX,
            message_history: VecDeque::new(),
            message_history_max: DEFAULT_EVENT_HISTORY_MAX,
            default_ttl,
            stopping_drain_secs: DEFAULT_STOPPING_DRAIN_SECS,
            log_prompt_bodies: false,
            messages_sent: 0,
            messages_delivered: 0,
            requests_handled: 0,
            role_prompts: HashMap::new(),
        }
    }

    /// Register a new worker and return its unique ID.
    ///
    /// If `evict` is true and a worker with the same name already exists, the
    /// old registration is removed (including its mailbox and notifier) before
    /// the new one is created.
    ///
    /// If `worker_id` is `Some(id)` (the pre-register flow):
    /// - When the id already exists with the same name+role, this is treated
    ///   as an idempotent claim — the existing id is returned and TTL is
    ///   renewed. This lets dispatch pre-register a worker server-side and
    ///   the spawned agent then call `dispatch register` to fetch its prompt
    ///   without creating a duplicate worker record.
    /// - When the id already exists with a different name or role, the call
    ///   is rejected with an error (config drift / collision).
    /// - Otherwise, the supplied id is used verbatim.
    ///
    /// If `worker_id` is `None`, a fresh UUID is generated (legacy behavior).
    ///
    /// `role_prompt` is the agent's role prompt body. Only the
    /// orchestrator passes it — at pre-register time it loads the agent's
    /// prompt file and ships the content here. The broker stores it under
    /// the worker id so the spawned agent can fetch it back via the
    /// `WorkerRegistered` response of its own `dispatch register` claim.
    /// Agents themselves never pass `role_prompt`, so the claim path leaves
    /// the stored value untouched when `role_prompt` is `None`.
    // Splitting these into a struct buys nothing — every caller passes them
    // positionally and clippy's 7-arg threshold is an arbitrary heuristic.
    #[allow(clippy::too_many_arguments)]
    pub fn register_worker(
        &mut self,
        name: String,
        role: String,
        description: String,
        capabilities: Vec<String>,
        ttl_secs: Option<u64>,
        evict: bool,
        worker_id: Option<String>,
        role_prompt: Option<String>,
    ) -> Result<String, BrokerError> {
        // Prune expired workers up front — every other broker entry point
        // (`list_workers`, `heartbeat_worker`, message / team / mailbox
        // handlers) does this, and skipping it here opens a race where the
        // pre-register path stores a role_prompt, the pre-registered
        // worker's TTL elapses before the agent claims, another broker
        // request (e.g. `listen`) drops it via its own `evict_expired` AND
        // its `role_prompts` entry, the agent's subsequent claim misses the
        // idempotent short-circuit and falls through to the insert branch,
        // and the response carries `role_prompt: None`. With the preamble
        // the failure is deterministic regardless of interleaving, and the
        // supervisor's respawn-time re-register (evict=true, role_prompt
        // populated) restores the prompt on the next attempt.
        self.evict_expired();

        // Idempotent-claim short-circuit: if the supplied id already exists
        // and matches name+role, return it (and renew TTL) without touching
        // the rest of the state. This must run BEFORE the evict pass so that
        // a pre-registered worker can be claimed by its agent without being
        // wiped by a same-name evict.
        if let Some(ref supplied) = worker_id {
            if let Some(existing) = self.workers.get_mut(supplied) {
                if existing.name == name && existing.role == role {
                    let ttl = ttl_secs.unwrap_or(self.default_ttl);
                    existing.ttl_secs = ttl;
                    existing.expires_at = now_secs() + ttl;
                    // Refresh mutable metadata so a re-register with updated
                    // config values isn't silently dropped. `capabilities` is
                    // only overwritten when non-empty — agent-side claims pass
                    // an empty vec and must not erase what the orchestrator
                    // registered.
                    existing.description = description;
                    if !capabilities.is_empty() {
                        existing.capabilities = capabilities;
                    }
                    // Flip `claimed` on the idempotent-claim path so the
                    // monitor can distinguish "pre-registered, waiting" from
                    // "agent attached." The orchestrator passes `role_prompt =
                    // Some(_)` when re-registering on respawn; agent claims
                    // pass `None`. Either way, reaching this branch means an
                    // actual process called register with our id, so mark it.
                    existing.claimed = true;
                    let id = supplied.clone();
                    // Only overwrite the stored prompt if the caller supplied
                    // one — agent claims pass `None` and must not erase what
                    // the orchestrator stored at pre-register time.
                    if let Some(prompt) = role_prompt {
                        self.role_prompts.insert(id.clone(), prompt);
                    }
                    return Ok(id);
                }
                return Err(BrokerError::WorkerIdCollision {
                    supplied: supplied.clone(),
                    existing_name: existing.name.clone(),
                    existing_role: existing.role.clone(),
                    requested_name: name,
                    requested_role: role,
                });
            }
        }

        if evict {
            let old_ids: Vec<String> = self
                .workers
                .iter()
                .filter(|(_, w)| w.name == name)
                .map(|(id, _)| id.clone())
                .collect();
            for old_id in &old_ids {
                self.remove_worker(old_id);
            }
        }

        // `claimed = false` only when a caller pre-registers with a supplied
        // id (the bootstrap flow — orchestrator creates the record
        // server-side before the agent process starts). A fresh register
        // without a supplied id is always an agent registering itself, so
        // mark it claimed immediately. Distinguishing these keeps the
        // "reserved, waiting" state precise rather than bleeding into the
        // legacy self-register path.
        let claimed = worker_id.is_none();
        let id = worker_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let now = now_secs();
        let ttl = ttl_secs.unwrap_or(self.default_ttl);
        let worker = Worker {
            id: id.clone(),
            name,
            role,
            description,
            capabilities,
            ttl_secs: ttl,
            expires_at: now + ttl,
            last_status: None,
            last_status_at: None,
            status_history: VecDeque::new(),
            claimed,
            // Freshly registered workers are alive. The coordinator moves them
            // to `stopping` via `set_control_state` (`agent stop`).
            control_state: ControlState::Active,
            stopping_since: None,
        };
        self.workers.insert(id.clone(), worker);
        if let Some(prompt) = role_prompt {
            self.role_prompts.insert(id.clone(), prompt);
        }
        Ok(id)
    }

    /// Remove a worker and all per-worker state (mailbox, notifier, role
    /// prompt). Callers should route every worker removal through this so
    /// the broker never retains partial state for a worker that no longer
    /// exists — an invariant the pre-register cleanup path relies
    /// on.
    pub fn remove_worker(&mut self, id: &str) {
        self.workers.remove(id);
        self.mailboxes.remove(id);
        self.notifiers.remove(id);
        self.role_prompts.remove(id);
    }

    /// Remove workers that should no longer exist, including their mailboxes,
    /// notifiers, and role prompts. Two reasons, checked in this request-driven
    /// sweep (there is no background tick): a worker's TTL elapsed
    /// (`TtlExpired`), or a `stopping` worker's drain window elapsed
    /// (`StopDrained`). Returns `(id, name, reason)` so callers can emit the
    /// right event (`expire` vs `lifecycle`) with the worker's name, which is
    /// otherwise lost on removal. With zero traffic a drained worker lingers
    /// until the next request triggers a sweep — acceptable, same as TTL.
    pub fn evict_expired(&mut self) -> Vec<(String, String, EvictReason)> {
        let now = now_secs();
        let drain = self.stopping_drain_secs;
        let removable: Vec<(String, String, EvictReason)> = self
            .workers
            .iter()
            .filter_map(|(id, w)| {
                if w.expires_at <= now {
                    Some((id.clone(), w.name.clone(), EvictReason::TtlExpired))
                } else if w.control_state == ControlState::Stopping
                    && w.stopping_since.is_some_and(|since| since + drain <= now)
                {
                    Some((id.clone(), w.name.clone(), EvictReason::StopDrained))
                } else {
                    None
                }
            })
            .collect();
        for (id, _, _) in &removable {
            self.remove_worker(id);
        }
        removable
    }

    /// Set a worker's coordinator-controlled lifecycle state. Stamps
    /// `stopping_since` on the first transition into `Stopping` so the drain
    /// window can be measured; clears it when leaving `Stopping`. Returns
    /// `false` if no such worker exists. Does **not** touch TTL — control
    /// state and liveness are orthogonal.
    pub fn set_control_state(&mut self, worker_id: &str, state: ControlState) -> bool {
        if let Some(w) = self.workers.get_mut(worker_id) {
            match state {
                ControlState::Stopping if w.control_state != ControlState::Stopping => {
                    w.stopping_since = Some(now_secs());
                }
                ControlState::Stopping => {}
                _ => w.stopping_since = None,
            }
            w.control_state = state;
            true
        } else {
            false
        }
    }

    /// Set the control state of every worker registered under `name` (normally
    /// one; eviction keeps same-name duplicates from accumulating). Returns the
    /// affected worker ids so the caller can emit lifecycle events. Used by the
    /// coordinator's `agent stop`/`restart` to mark a worker `stopping` before
    /// its process is killed.
    pub fn set_control_state_by_name(&mut self, name: &str, state: ControlState) -> Vec<String> {
        let ids: Vec<String> = self
            .workers
            .iter()
            .filter(|(_, w)| w.name == name)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &ids {
            self.set_control_state(id, state);
        }
        ids
    }

    /// Return a list of all active (non-expired) workers.
    pub fn list_workers(&mut self) -> Vec<Worker> {
        self.evict_expired();
        self.workers.values().cloned().collect()
    }

    /// Look up a worker's name by ID.
    pub fn worker_name(&self, worker_id: &str) -> Option<&str> {
        self.workers.get(worker_id).map(|w| w.name.as_str())
    }

    /// Renew a worker's TTL and optionally update status.
    /// Returns the new expiry timestamp, or None if not found/expired.
    ///
    /// When `status` differs from the worker's existing `last_status`, the
    /// previous tagline (with its set time) is pushed onto `status_history`
    /// so the card UI can show the last few values. Identical re-sets are
    /// deduped so a steady heartbeat doesn't fill the buffer with copies.
    pub fn heartbeat_worker(&mut self, worker_id: &str, status: Option<String>) -> Option<u64> {
        self.evict_expired();
        if let Some(worker) = self.workers.get_mut(worker_id) {
            let now = now_secs();
            worker.expires_at = now + worker.ttl_secs;
            // Any heartbeat from the agent is proof of life — flip claimed
            // so the monitor shows it as attached rather than "reserved,
            // waiting" (relevant when the orchestrator pre-registers and
            // the agent starts up by heartbeat-without-register, though
            // the bootstrap always registers first).
            worker.claimed = true;
            if let Some(s) = status {
                let unchanged = worker.last_status.as_deref() == Some(s.as_str());
                if !unchanged {
                    if let (Some(prev_status), Some(prev_at)) =
                        (worker.last_status.take(), worker.last_status_at)
                    {
                        push_status_history(
                            &mut worker.status_history,
                            StatusEntry {
                                status: prev_status,
                                set_at: prev_at,
                            },
                        );
                    }
                    worker.last_status = Some(s);
                    worker.last_status_at = Some(now);
                }
            }
            Some(worker.expires_at)
        } else {
            None
        }
    }

    /// Get status summaries for all active workers or a specific worker.
    pub fn get_status(&mut self, worker_id: Option<&str>) -> Vec<crate::protocol::WorkerStatus> {
        self.evict_expired();
        match worker_id {
            Some(id) => self
                .workers
                .get(id)
                .map(|w| {
                    vec![crate::protocol::WorkerStatus {
                        id: w.id.clone(),
                        name: w.name.clone(),
                        role: w.role.clone(),
                        last_status: w.last_status.clone(),
                        last_status_at: w.last_status_at,
                        control_state: w.control_state,
                    }]
                })
                .unwrap_or_default(),
            None => self
                .workers
                .values()
                .map(|w| crate::protocol::WorkerStatus {
                    id: w.id.clone(),
                    name: w.name.clone(),
                    role: w.role.clone(),
                    last_status: w.last_status.clone(),
                    last_status_at: w.last_status_at,
                    control_state: w.control_state,
                })
                .collect(),
        }
    }

    /// Emit an event: broadcast it, record in history, and print to stderr.
    ///
    /// `worker_name_override` wins when the worker has just been evicted (its
    /// name can't be looked up anymore) or when the caller already has the
    /// name cheaply. If `None`, falls back to `self.worker_name(worker_id)`.
    pub fn emit_and_record(
        &mut self,
        tx: &broadcast::Sender<BrokerEvent>,
        kind: &str,
        worker_id: &str,
        worker_name_override: Option<&str>,
        detail: &str,
        payload: Option<serde_json::Value>,
    ) {
        let worker_name = worker_name_override
            .map(|s| s.to_string())
            .or_else(|| self.worker_name(worker_id).map(|s| s.to_string()));
        let event = BrokerEvent {
            kind: kind.to_string(),
            worker_id: worker_id.to_string(),
            worker_name,
            detail: detail.to_string(),
            payload,
            timestamp: now_secs(),
        };
        // Foreground console echo for `dispatch serve`. Routed through
        // `tracing::info!` so it lands in both the daily log file AND on
        // stderr (the same place the previous `eprintln!` wrote), and so
        // `DISPATCH_LOG=warn` etc. can quiet the broker without losing
        // file-side logs.
        let display_name = event.worker_name.as_deref().unwrap_or(worker_id);
        match event.payload.as_ref() {
            Some(p) => tracing::info!(
                kind = %event.kind,
                worker = %display_name,
                detail = %event.detail,
                payload = %p,
                "broker event",
            ),
            None => tracing::info!(
                kind = %event.kind,
                worker = %display_name,
                detail = %event.detail,
                "broker event",
            ),
        }
        let _ = tx.send(event.clone());
        self.event_history.push_back(event);
        while self.event_history.len() > self.event_history_max {
            self.event_history.pop_front();
        }
    }

    /// Clear a worker's current status tagline. Does **not** touch
    /// `status_history` — clear is a display-level operation so the recent
    /// taglines stay visible on the agent card after the user resets the
    /// current state.
    pub fn clear_status(&mut self, worker_id: &str) -> Result<(), String> {
        self.evict_expired();
        if let Some(worker) = self.workers.get_mut(worker_id) {
            worker.last_status = None;
            worker.last_status_at = None;
            Ok(())
        } else {
            Err(format!("worker not found or expired: {worker_id}"))
        }
    }

    /// Record an acknowledgement for a message.
    ///
    /// Validates (in order) that the worker exists, the message exists in
    /// history, and the message was addressed to this worker. Without these
    /// checks a caller could record acks for arbitrary or nonexistent message
    /// IDs, corrupting the monitor's message state.
    pub fn ack_message(
        &mut self,
        worker_id: &str,
        message_id: &str,
        note: Option<String>,
        status: Option<String>,
        summary: Option<String>,
        artifacts: Vec<String>,
    ) -> Result<(), String> {
        self.evict_expired();
        if !self.workers.contains_key(worker_id) {
            return Err(format!("worker not found or expired: {worker_id}"));
        }
        let recipient = self
            .message_history
            .iter()
            .find(|m| m.message_id == message_id)
            .map(|m| m.to.clone())
            .ok_or_else(|| format!("message not found: {message_id}"))?;
        if recipient != worker_id {
            return Err(format!(
                "message {message_id} was not addressed to worker {worker_id}"
            ));
        }
        let now = now_secs();
        self.ack_log.insert(
            message_id.to_string(),
            AckRecord {
                message_id: message_id.to_string(),
                worker_id: worker_id.to_string(),
                note,
                acked_at: now,
                status,
                summary,
                artifacts,
            },
        );
        if let Some(hist) = self
            .message_history
            .iter_mut()
            .find(|m| m.message_id == message_id)
        {
            hist.acked_at = Some(now);
        }
        Ok(())
    }

    /// Queue a message in a worker's mailbox. Returns the message ID, or None if the
    /// recipient worker is not found or expired.
    pub fn send_message(
        &mut self,
        to: String,
        body: String,
        from: Option<String>,
    ) -> Option<String> {
        self.evict_expired();
        if !self.workers.contains_key(&to) {
            return None;
        }
        let now = now_secs();
        let message_id = uuid::Uuid::new_v4().to_string();
        let message = Message {
            message_id: message_id.clone(),
            from,
            to: to.clone(),
            body,
            sent_at: Some(now),
            delivered_at: None,
            acked_at: None,
        };
        // Record in history before moving into mailbox.
        self.message_history.push_back(message.clone());
        while self.message_history.len() > self.message_history_max {
            self.message_history.pop_front();
        }
        self.mailboxes
            .entry(to.clone())
            .or_default()
            .push_back(message);
        // Wake any long-polling listener for this worker.
        if let Some(notify) = self.notifiers.get(&to) {
            notify.notify_one();
        }
        Some(message_id)
    }

    /// Pop the next message from a worker's mailbox, if any.
    /// Also marks the message as delivered in the history.
    pub fn pop_message(&mut self, worker_id: &str) -> Option<Message> {
        let msg = self.mailboxes.get_mut(worker_id)?.pop_front()?;
        let now = now_secs();
        if let Some(hist) = self
            .message_history
            .iter_mut()
            .find(|m| m.message_id == msg.message_id)
        {
            hist.delivered_at = Some(now);
        }
        Some(msg)
    }

    /// Query event history with optional filters.
    pub fn query_events(
        &self,
        since: Option<u64>,
        until: Option<u64>,
        event_type: Option<&str>,
        worker: Option<&str>,
        limit: Option<usize>,
    ) -> Vec<&BrokerEvent> {
        let limit = limit.unwrap_or(100);
        self.event_history
            .iter()
            .rev() // most recent first
            .filter(|e| since.is_none_or(|ts| e.timestamp >= ts))
            .filter(|e| until.is_none_or(|ts| e.timestamp <= ts))
            .filter(|e| event_type.is_none_or(|t| e.kind == t))
            .filter(|e| worker.is_none_or(|w| e.worker_id == w))
            .take(limit)
            .collect()
    }

    /// Query message history with optional filters.
    pub fn query_messages(
        &self,
        worker_id: &str,
        unacked: bool,
        sent: bool,
        since: Option<u64>,
        limit: Option<usize>,
        id: Option<&str>,
    ) -> Vec<&Message> {
        let limit = limit.unwrap_or(100);

        // Single message by ID
        if let Some(msg_id) = id {
            return self
                .message_history
                .iter()
                .filter(|m| m.message_id == msg_id)
                .collect();
        }

        self.message_history
            .iter()
            .rev() // most recent first
            .filter(|m| {
                if sent {
                    m.from.as_deref() == Some(worker_id)
                } else {
                    m.to == worker_id
                }
            })
            .filter(|m| {
                if unacked {
                    m.delivered_at.is_some() && m.acked_at.is_none()
                } else {
                    true
                }
            })
            .filter(|m| since.is_none_or(|ts| m.sent_at.unwrap_or(0) >= ts))
            .take(limit)
            .collect()
    }

    /// Get or create the Notify handle for a worker's mailbox.
    pub fn get_notifier(&mut self, worker_id: &str) -> Arc<Notify> {
        self.notifiers
            .entry(worker_id.to_string())
            .or_insert_with(|| Arc::new(Notify::new()))
            .clone()
    }
}

/// Get current Unix timestamp in seconds.
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_secs()
}

/// Fingerprint a prompt/packet body for traceability: a hex hash plus
/// the byte length, logged in events instead of the full body so prompts don't
/// leak into the event history. Not cryptographic — it exists to answer "did
/// the prompt change / how big was it", not to resist forgery.
pub fn body_fingerprint(body: &str) -> (String, usize) {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    body.hash(&mut hasher);
    (format!("{:016x}", hasher.finish()), body.len())
}

/// Push a status entry into a worker's history ring, deduping against the
/// most-recent entry and capping the ring at `STATUS_HISTORY_MAX`.
///
/// Dedupe protects the buffer from filling with duplicates when an upstream
/// re-emits the same tagline — only transitions show up in the history.
fn push_status_history(history: &mut VecDeque<StatusEntry>, entry: StatusEntry) {
    if history.back().map(|e| e.status.as_str()) == Some(entry.status.as_str()) {
        return;
    }
    history.push_back(entry);
    while history.len() > STATUS_HISTORY_MAX {
        history.pop_front();
    }
}
