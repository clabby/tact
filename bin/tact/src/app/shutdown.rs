//! Platform-specific process shutdown signal handling.

use std::io;
#[cfg(unix)]
use tokio::signal::unix::{SignalKind, signal as unix_signal};

#[cfg(unix)]
pub(crate) async fn signal() -> io::Result<()> {
    let mut terminate = unix_signal(SignalKind::terminate())?;

    tokio::select! {
        result = tokio::signal::ctrl_c() => result,
        _ = terminate.recv() => Ok(()),
    }
}

#[cfg(not(unix))]
pub(crate) async fn signal() -> io::Result<()> {
    tokio::signal::ctrl_c().await
}
