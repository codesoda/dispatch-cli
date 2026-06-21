use std::path::{Path, PathBuf};

use tokio::net::UnixStream;

use crate::errors::DispatchError;

/// Derive the Unix domain socket path for a given cell identity.
///
/// Socket is placed in `/tmp/dispatch-cli/sockets/<cell_id>.sock`.
/// The cell_id already encodes the project identity (hashed canonical path),
/// so no additional path components are needed. Using `/tmp` avoids the
/// Unix domain socket `SUN_LEN` limit (104 bytes on macOS) that triggers
/// when project paths are deeply nested.
pub fn socket_path(_project_root: &Path, cell_id: &str) -> PathBuf {
    PathBuf::from("/tmp/dispatch-cli/sockets").join(format!("{cell_id}.sock"))
}

/// Check whether a broker is already running for this cell by testing
/// if the socket file exists and a connection can be made.
pub(super) async fn check_no_existing_broker(
    socket: &Path,
    cell_id: &str,
) -> Result<(), DispatchError> {
    if !socket.exists() {
        return Ok(());
    }

    // Socket file exists — try to connect to see if a broker is actually listening.
    match UnixStream::connect(socket).await {
        Ok(_) => Err(DispatchError::BrokerAlreadyRunning {
            cell_id: cell_id.to_string(),
            socket_path: socket.to_path_buf(),
        }),
        Err(_) => {
            // Stale socket file from a previous crashed run — remove it.
            tracing::warn!(path = %socket.display(), "removing stale socket file");
            std::fs::remove_file(socket).map_err(DispatchError::Io)?;
            Ok(())
        }
    }
}
