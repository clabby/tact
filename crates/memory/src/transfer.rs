//! Whole-corpus replication between the local store and one remote namespace.
//!
//! A push makes the caller's remote namespace an exact copy of the complete local corpus, so
//! remote-only records are deleted. Local writes may land while a snapshot uploads, so the push
//! re-reads the corpus after each upload and uploads again until the uploaded snapshot is still
//! current, giving up after a bounded number of attempts. A pull imports remote records as new
//! probationary local records and never deletes local data.

use crate::{
    LocalMemoryStore, MemoryError, MemoryImportReport, MemoryLimits, MemoryRecord, MemoryStore,
    RemoteClientError, RemoteMemoryClient, RemoteRole, protocol::SyncReport,
};
use thiserror::Error;

/// Uploads attempted before a push gives up on a local corpus that keeps changing.
const PUSH_ATTEMPTS: usize = 3;

/// Why a replication between the local store and the remote namespace failed.
#[derive(Debug, Error)]
pub enum TransferError {
    #[error("failed to read the local memory snapshot for push")]
    Local(#[source] MemoryError),
    #[error(
        "local memories kept changing while the remote snapshot was synchronized; retry once writes settle"
    )]
    LocalChanged,
    #[error("memory push failed")]
    Push(#[source] RemoteClientError),
    #[error("remote memory rejected the push")]
    PushStore(#[source] MemoryError),
    #[error("memory pull failed")]
    Pull(#[source] RemoteClientError),
    #[error("remote memory rejected the pull")]
    PullStore(#[source] MemoryError),
    #[error("failed to merge pulled memories into the local store")]
    Merge(#[source] MemoryError),
}

/// The outcome of a push.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PushReport {
    /// Records in the snapshot the remote namespace now holds.
    pub memories: usize,
    /// How the remote namespace changed.
    pub sync: SyncReport,
}

/// The outcome of a pull.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PullReport {
    /// Records read from the remote namespaces.
    pub fetched: usize,
    /// How the local store changed.
    pub import: MemoryImportReport,
}

/// The complete local corpus in key order, which is the snapshot a push uploads.
pub async fn local_snapshot(
    local: &LocalMemoryStore,
    limits: MemoryLimits,
) -> Result<Vec<MemoryRecord>, TransferError> {
    let mut memories = local
        .export_all(None, limits)
        .await
        .map_err(TransferError::Local)?;
    memories.sort_unstable_by_key(|memory| memory.key.id);
    Ok(memories)
}

/// Replaces the remote namespace with the complete local corpus. The remote token must grant
/// write access.
pub async fn push(
    local: &LocalMemoryStore,
    remote: RemoteMemoryClient,
    limits: MemoryLimits,
) -> Result<PushReport, TransferError> {
    if remote.session().await.map_err(TransferError::Push)? != RemoteRole::Writer {
        return Err(TransferError::Push(RemoteClientError::ReadOnly));
    }
    let mut memories = local_snapshot(local, limits).await?;
    for _ in 0..PUSH_ATTEMPTS {
        let sync = remote
            .sync(&memories)
            .await
            .map_err(TransferError::PushStore)?;
        // Telemetry changes count as changes, so the remote also receives current usage data.
        let current = local_snapshot(local, limits).await?;
        if current == memories {
            return Ok(PushReport {
                memories: memories.len(),
                sync,
            });
        }
        memories = current;
    }
    Err(TransferError::LocalChanged)
}

/// Imports the selected remote namespaces, or every namespace visible to the token when
/// `namespaces` is `None`, into the local store.
pub async fn pull(
    remote: RemoteMemoryClient,
    local: &LocalMemoryStore,
    namespaces: Option<&[String]>,
    limits: MemoryLimits,
) -> Result<PullReport, TransferError> {
    remote.session().await.map_err(TransferError::Pull)?;
    let memories = remote
        .export_all(namespaces, limits)
        .await
        .map_err(TransferError::PullStore)?;
    let fetched = memories.len();
    let import = local
        .merge_remote_export(memories)
        .await
        .map_err(TransferError::Merge)?;
    Ok(PullReport { fetched, import })
}

#[cfg(test)]
mod tests {
    use super::local_snapshot;
    use crate::{LocalMemoryStore, MemoryLimits, MemoryStore};
    use tempfile::tempdir;

    #[tokio::test]
    async fn local_snapshots_differ_when_only_telemetry_changes() {
        let directory = tempdir().unwrap();
        let local = LocalMemoryStore::new(
            directory.path().join("memory.sqlite3"),
            MemoryLimits::PRODUCTION,
        );
        local.put("telemetry", None).await.unwrap();
        let before = local_snapshot(&local, MemoryLimits::PRODUCTION)
            .await
            .unwrap();

        local.scan("telemetry", 1).await.unwrap();
        let after = local_snapshot(&local, MemoryLimits::PRODUCTION)
            .await
            .unwrap();

        assert_eq!(before.len(), 1);
        assert_eq!(before[0].content, after[0].content);
        assert_ne!(before, after);
    }
}
