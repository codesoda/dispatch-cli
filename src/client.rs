use std::path::Path;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tracing::instrument;

use crate::broker;
use crate::errors::DispatchError;
use crate::protocol::{BrokerRequest, BrokerResponse};

/// Client for communicating with the Dispatch broker over a Unix domain socket.
pub struct Client {
    cell_id: String,
    socket_path: std::path::PathBuf,
}

impl Client {
    /// Create a new client that will connect to the broker for the given cell.
    pub fn new(project_root: &Path, cell_id: &str) -> Self {
        Self {
            cell_id: cell_id.to_string(),
            socket_path: broker::socket_path(project_root, cell_id),
        }
    }

    /// Send a request to the broker and return the response.
    ///
    /// Opens a new connection for each request (the broker protocol is
    /// one request, one response, then close).
    #[instrument(skip(self, request), fields(cell_id = %self.cell_id))]
    pub async fn send_request(
        &self,
        request: &BrokerRequest,
    ) -> Result<BrokerResponse, DispatchError> {
        let stream = UnixStream::connect(&self.socket_path).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound
                || e.kind() == std::io::ErrorKind::ConnectionRefused
            {
                DispatchError::BrokerNotRunning {
                    cell_id: self.cell_id.clone(),
                }
            } else {
                DispatchError::ConnectionFailed {
                    reason: e.to_string(),
                }
            }
        })?;

        let (reader, mut writer) = stream.into_split();

        // Serialize and send the request as a single JSON line.
        let mut request_bytes = serde_json::to_vec(request)?;
        request_bytes.push(b'\n');
        writer
            .write_all(&request_bytes)
            .await
            .map_err(DispatchError::Io)?;

        // Read the response line.
        let mut reader = BufReader::new(reader);
        let mut response_line = String::new();
        let n = reader
            .read_line(&mut response_line)
            .await
            .map_err(DispatchError::Io)?;

        if n == 0 {
            return Err(DispatchError::ConnectionFailed {
                reason: "broker closed connection without responding".to_string(),
            });
        }

        let response: BrokerResponse = serde_json::from_str(response_line.trim())?;
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_client_broker_not_running() {
        let tmp = TempDir::new().unwrap();
        let client = Client::new(tmp.path(), "nonexistent-cell");

        let result = client.send_request(&BrokerRequest::Team).await;
        assert!(result.is_err());

        match result.unwrap_err() {
            DispatchError::BrokerNotRunning { cell_id } => {
                assert_eq!(cell_id, "nonexistent-cell");
            }
            other => panic!("expected BrokerNotRunning, got: {other}"),
        }
    }

    #[tokio::test]
    async fn test_client_send_and_receive() {
        let tmp = TempDir::new().unwrap();
        let project_root = tmp.path().to_path_buf();
        let cell_id = "client-test";

        // Start broker in background.
        let root = project_root.clone();
        let serve_handle =
            tokio::spawn(async move { crate::broker::serve(&root, "client-test").await });

        // Wait for broker to start.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let client = Client::new(&project_root, cell_id);
        let response = client.send_request(&BrokerRequest::Team).await;
        assert!(response.is_ok(), "expected Ok response, got: {response:?}");

        let resp = response.unwrap();
        match resp {
            BrokerResponse::Ok { .. } => {} // Expected
            BrokerResponse::Error { message } => {
                panic!("expected Ok response, got error: {message}");
            }
        }

        serve_handle.abort();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}
