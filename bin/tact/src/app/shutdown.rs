//! Platform-specific process shutdown signal handling.

use crate::app::error::{Result, RuntimeError};
use std::{future::Future, io};
#[cfg(unix)]
use tokio::signal::unix::{SignalKind, signal as unix_signal};
use tokio_util::sync::CancellationToken;

/// Drives `run` to completion. The first shutdown signal cancels `shutdown`, and `run` is
/// still awaited so it can stop gracefully. A failure to listen for signals is reported only
/// after `run` has finished.
pub(crate) async fn run_until_complete<T>(
    shutdown: CancellationToken,
    run: impl Future<Output = Result<T>>,
) -> Result<T> {
    tokio::pin!(run);
    tokio::select! {
        result = &mut run => result,
        signal = signal() => {
            shutdown.cancel();
            let result = run.await;
            signal.map_err(RuntimeError::ShutdownSignal)?;
            result
        }
    }
}

#[cfg(unix)]
async fn signal() -> io::Result<()> {
    let mut terminate = unix_signal(SignalKind::terminate())?;

    tokio::select! {
        result = tokio::signal::ctrl_c() => result,
        _ = terminate.recv() => Ok(()),
    }
}

#[cfg(not(unix))]
async fn signal() -> io::Result<()> {
    tokio::signal::ctrl_c().await
}
