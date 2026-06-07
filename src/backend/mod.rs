pub mod local;
pub mod monitor;
pub mod orchestrator;

pub use local::socket_path;

use async_trait::async_trait;

use crate::config::ResolvedConfig;
use crate::errors::DispatchError;
use crate::protocol::{BrokerRequest, BrokerResponse};

/// Backend trait for broker communication.
///
/// Implementations provide the transport and state management for a
/// dispatch cell. The `local` backend uses a Unix domain socket and
/// in-memory state; future backends may use remote services.
#[async_trait]
pub trait Backend: Send + Sync {
    /// Start the broker, blocking until shutdown.
    async fn serve(&self) -> Result<(), DispatchError>;

    /// Send a request to the broker and return the response.
    async fn send_request(&self, request: &BrokerRequest) -> Result<BrokerResponse, DispatchError>;
}

/// Create a backend instance based on the configured backend name.
///
/// - `None` or `"local"` → `LocalBackend`
/// - Anything else → `DispatchError::UnknownBackend`
pub fn create_backend(
    config: &ResolvedConfig,
    monitor_port: Option<u16>,
) -> Result<Box<dyn Backend>, DispatchError> {
    match config.backend.as_deref().unwrap_or("local") {
        "local" => Ok(Box::new(local::LocalBackend::new(config, monitor_port))),
        other => Err(DispatchError::UnknownBackend {
            name: other.to_string(),
        }),
    }
}
