pub mod local;
pub mod monitor;

use async_trait::async_trait;

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
    backend_name: Option<&str>,
    project_root: &std::path::Path,
    cell_id: &str,
    monitor_port: Option<u16>,
) -> Result<Box<dyn Backend>, DispatchError> {
    match backend_name.unwrap_or("local") {
        "local" => Ok(Box::new(local::LocalBackend::new(
            project_root,
            cell_id,
            monitor_port,
        ))),
        other => Err(DispatchError::UnknownBackend {
            name: other.to_string(),
        }),
    }
}
