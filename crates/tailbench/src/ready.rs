//! Socket setup and readiness handshake.
//!
//! `depends_on` in compose waits for container start, not readiness. Without a
//! handshake the first requests hit an absent peer and poison the warmup.

use anyhow::{bail, Result};
use std::path::Path;
use std::time::Duration;
use tokio::net::UnixListener;


/// Bind a listener and publish a `.ready` marker beside it.
pub fn bind(socket: &Path) -> Result<UnixListener> {
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if socket.exists() {
        std::fs::remove_file(socket)?;
    }
    let listener = UnixListener::bind(socket)?;
    std::fs::write(socket.with_extension("ready"), b"ok")?;
    Ok(listener)
}

/// Block until a peer has published its `.ready` marker.
pub async fn wait_for(socket: &Path) -> Result<()> {
    let marker = socket.with_extension("ready");
    for _ in 0..300 {
        if marker.exists() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bail!("timed out waiting for {}", marker.display())
}

/// A peer closing at end of run is normal, not an error worth logging.
pub fn is_disconnect(e: &anyhow::Error) -> bool {
    e.downcast_ref::<std::io::Error>().is_some_and(|io| {
        matches!(
            io.kind(),
            std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::BrokenPipe
        )
    })
}
