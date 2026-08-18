//! Storage contract, backend selection, and shared failures.

#[cfg(feature = "local")]
mod local;
#[cfg(feature = "client")]
mod remote;

#[cfg(all(feature = "client", feature = "local"))]
use crate::{MemoryAccess, MemorySource, secrets::contains_likely_secret};
use crate::{
    MemoryKey, MemoryLimits, MemoryRecord, MemoryScan,
    server::protocol::{self, ExportCursor, SyncReport},
};
#[cfg(feature = "local")]
pub use local::LocalMemoryStore;
#[cfg(feature = "client")]
pub use remote::{RemoteClientError, RemoteMemoryClient, RemoteToken};
#[cfg(all(feature = "client", feature = "local"))]
use std::path::PathBuf;
#[cfg(feature = "local")]
use std::time::{SystemTime, UNIX_EPOCH};
use std::{error::Error, future::Future};
use thiserror::Error;

/// Ordinary operations shared by local and authenticated remote memory backends.
///
/// Implementations own their namespace and storage boundary. Returned records are ordered
/// deterministically by the implementation, direct mutations use key versions as compare-and-swap
/// preconditions, and each implementation captures its authoritative operation time internally.
/// Dropping a returned future requests cancellation. Implementations may finish an already-started
/// atomic storage transaction after cancellation.
pub trait MemoryStore: Clone + Send + Sync + 'static {
    /// Searches visible records and records scan telemetry.
    fn scan(
        &self,
        query: &str,
        limit: usize,
    ) -> impl Future<Output = Result<MemoryScan, MemoryError>> + Send;

    /// Reads unversioned IDs and versioned keys, recording use telemetry.
    fn read(
        &self,
        ids: &[i64],
        keys: &[MemoryKey],
    ) -> impl Future<Output = Result<Vec<MemoryRecord>, MemoryError>> + Send;

    /// Lists a deterministic window that excludes expired probationary records.
    ///
    /// Implementations bound this result so interactive inspection does not grow with the complete
    /// shared corpus. Full transfer uses paginated export instead.
    fn list(&self) -> impl Future<Output = Result<Vec<MemoryRecord>, MemoryError>> + Send;

    /// Inserts content or compare-and-swap replaces `replacement`.
    fn put(
        &self,
        content: &str,
        replacement: Option<MemoryKey>,
    ) -> impl Future<Output = Result<MemoryRecord, MemoryError>> + Send;

    /// Compare-and-swap deletes `key`.
    ///
    /// Deleting an already-absent key succeeds, making safe request replay idempotent. An existing
    /// record with a different version returns [`MemoryError::Conflict`].
    fn delete(&self, key: MemoryKey) -> impl Future<Output = Result<(), MemoryError>> + Send;

    /// Atomically applies an authoritative full snapshot to the owned namespace.
    ///
    /// Records absent from `memories` are deleted. Input is validated before commit, IDs remain
    /// stable, and future insertions must not reuse IDs observed in the snapshot.
    fn sync(
        &self,
        memories: &[MemoryRecord],
    ) -> impl Future<Output = Result<SyncReport, MemoryError>> + Send;

    /// Exports one page in stable `(namespace, id)` order after `cursor`.
    ///
    /// The page is a point-in-time transaction snapshot. `limit` is clamped to the protocol bound;
    /// callers continue only with the exact returned cursor. Cancellation before storage work starts
    /// prevents the operation; an already-started transaction may finish.
    fn export_page(
        &self,
        namespaces: Option<&[String]>,
        cursor: Option<&ExportCursor>,
        limit: usize,
    ) -> impl Future<Output = Result<(Vec<MemoryRecord>, Option<ExportCursor>), MemoryError>> + Send;

    /// Collects a complete export through the paginated storage contract within caller-provided
    /// record and content bounds.
    ///
    /// The collector rejects non-progressing cursors and stops before retaining more records or
    /// content than `limits` permits.
    fn export_all(
        &self,
        namespaces: Option<&[String]>,
        limits: MemoryLimits,
    ) -> impl Future<Output = Result<Vec<MemoryRecord>, MemoryError>> + Send {
        async move {
            let mut cursor = None;
            let mut records = Vec::new();
            let mut content_bytes = 0usize;
            loop {
                let (page, next_cursor) = self
                    .export_page(
                        namespaces,
                        cursor.as_ref(),
                        protocol::MAX_EXPORT_PAGE_RECORDS,
                    )
                    .await?;
                let next_record_count = records
                    .len()
                    .checked_add(page.len())
                    .ok_or(MemoryError::InvalidPagination)?;
                let page_bytes = page.iter().try_fold(0usize, |total, record| {
                    total.checked_add(record.content.len())
                });
                content_bytes = content_bytes
                    .checked_add(page_bytes.ok_or(MemoryError::InvalidPagination)?)
                    .ok_or(MemoryError::InvalidPagination)?;
                if next_record_count > limits.records {
                    return Err(MemoryError::RecordCapacity {
                        maximum: limits.records,
                    });
                }
                if content_bytes > limits.total_content_bytes {
                    return Err(MemoryError::ContentCapacity {
                        maximum_bytes: limits.total_content_bytes,
                    });
                }
                if next_cursor
                    .as_ref()
                    .is_some_and(|next| cursor.as_ref() == Some(next))
                    || (page.is_empty() && next_cursor.is_some())
                {
                    return Err(MemoryError::InvalidPagination);
                }
                records.extend(page);
                match next_cursor {
                    Some(next) => cursor = Some(next),
                    None => return Ok(records),
                }
            }
        }
    }
}

/// Runtime-selected local-or-remote memory backend.
#[cfg(all(feature = "client", feature = "local"))]
#[derive(Clone, Debug)]
pub enum SelectedMemoryStore {
    /// Private local SQLite storage.
    Local(LocalMemoryStore),
    /// Authenticated namespaced HTTP storage.
    Remote(RemoteMemoryClient),
}

#[cfg(all(feature = "client", feature = "local"))]
impl SelectedMemoryStore {
    /// Selects a private local SQLite backend with explicit resource limits.
    pub fn local(path: impl Into<PathBuf>, limits: MemoryLimits) -> Self {
        Self::Local(LocalMemoryStore::new(path, limits))
    }

    /// Selects an authenticated remote HTTP backend.
    pub const fn remote(client: RemoteMemoryClient) -> Self {
        Self::Remote(client)
    }

    /// Returns the selected backend kind without performing I/O.
    pub const fn source(&self) -> MemorySource {
        match self {
            Self::Local(_) => MemorySource::Local,
            Self::Remote(_) => MemorySource::Remote,
        }
    }

    /// Returns backend provenance and negotiated remote authorization.
    pub async fn access(&self) -> Result<MemoryAccess, MemoryError> {
        match self {
            Self::Local(_) => Ok(MemoryAccess {
                source: MemorySource::Local,
                namespace: None,
                role: None,
            }),
            Self::Remote(client) => Ok(MemoryAccess {
                source: MemorySource::Remote,
                namespace: Some(client.namespace().to_owned()),
                role: Some(client.session().await?),
            }),
        }
    }
}

#[cfg(all(feature = "client", feature = "local"))]
impl MemoryStore for SelectedMemoryStore {
    fn scan(
        &self,
        query: &str,
        limit: usize,
    ) -> impl Future<Output = Result<MemoryScan, MemoryError>> + Send {
        async move {
            match self {
                Self::Local(store) => MemoryStore::scan(store, query, limit).await,
                Self::Remote(client) => MemoryStore::scan(client, query, limit).await,
            }
        }
    }
    fn read(
        &self,
        ids: &[i64],
        keys: &[MemoryKey],
    ) -> impl Future<Output = Result<Vec<MemoryRecord>, MemoryError>> + Send {
        async move {
            match self {
                Self::Local(store) => MemoryStore::read(store, ids, keys).await,
                Self::Remote(client) => MemoryStore::read(client, ids, keys).await,
            }
        }
    }
    fn list(&self) -> impl Future<Output = Result<Vec<MemoryRecord>, MemoryError>> + Send {
        async move {
            match self {
                Self::Local(store) => MemoryStore::list(store).await,
                Self::Remote(client) => MemoryStore::list(client).await,
            }
        }
    }
    fn put(
        &self,
        content: &str,
        replacement: Option<MemoryKey>,
    ) -> impl Future<Output = Result<MemoryRecord, MemoryError>> + Send {
        async move {
            reject_unsafe(content)?;
            match self {
                Self::Local(store) => MemoryStore::put(store, content, replacement).await,
                Self::Remote(client) => MemoryStore::put(client, content, replacement).await,
            }
        }
    }
    fn delete(&self, key: MemoryKey) -> impl Future<Output = Result<(), MemoryError>> + Send {
        async move {
            match self {
                Self::Local(store) => MemoryStore::delete(store, key).await,
                Self::Remote(client) => MemoryStore::delete(client, key).await,
            }
        }
    }
    fn sync(
        &self,
        memories: &[MemoryRecord],
    ) -> impl Future<Output = Result<SyncReport, MemoryError>> + Send {
        async move {
            for memory in memories {
                reject_unsafe(&memory.content)?;
            }
            match self {
                Self::Local(store) => MemoryStore::sync(store, memories).await,
                Self::Remote(client) => MemoryStore::sync(client, memories).await,
            }
        }
    }
    fn export_page(
        &self,
        namespaces: Option<&[String]>,
        cursor: Option<&ExportCursor>,
        limit: usize,
    ) -> impl Future<Output = Result<(Vec<MemoryRecord>, Option<ExportCursor>), MemoryError>> + Send
    {
        async move {
            match self {
                Self::Local(store) => {
                    MemoryStore::export_page(store, namespaces, cursor, limit).await
                }
                Self::Remote(client) => {
                    MemoryStore::export_page(client, namespaces, cursor, limit).await
                }
            }
        }
    }
}

#[cfg(all(feature = "client", feature = "local"))]
fn reject_unsafe(content: &str) -> Result<(), MemoryError> {
    if contains_likely_secret(content) {
        return Err(MemoryError::SecretRejected);
    }
    Ok(())
}

#[cfg(feature = "local")]
fn current_time_ms() -> i64 {
    let milliseconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    i64::try_from(milliseconds).unwrap_or(i64::MAX)
}

/// Failure from local storage, remote transport, validation, or optimistic concurrency.
#[derive(Debug, Error)]
pub enum MemoryError {
    /// Content is empty after trimming.
    #[error("memory content is empty")]
    EmptyContent,
    /// One record exceeds its byte bound.
    #[error("memory content exceeds the {maximum_bytes}-byte limit")]
    ContentTooLarge {
        /// Configured maximum bytes per record.
        maximum_bytes: usize,
    },
    /// A scan query exceeds its byte bound.
    #[error("memory query exceeds the {maximum_bytes}-byte limit")]
    QueryTooLarge {
        /// Configured maximum query bytes.
        maximum_bytes: usize,
    },
    /// The record-count bound is exhausted.
    #[error("memory record capacity of {maximum} was reached")]
    RecordCapacity {
        /// Configured maximum records.
        maximum: usize,
    },
    /// The aggregate content-byte bound is exhausted.
    #[error("memory content capacity of {maximum_bytes} bytes was reached")]
    ContentCapacity {
        /// Configured maximum aggregate bytes.
        maximum_bytes: usize,
    },
    /// The backing store reached its configured storage bound.
    #[error("memory storage capacity was reached")]
    StorageCapacity,
    /// Content was rejected as a likely credential or secret.
    #[error("memory content was rejected as a likely secret")]
    SecretRejected,
    /// Equivalent normalized content already exists.
    #[error("an equivalent memory already exists")]
    Duplicate,
    /// The requested record does not exist.
    #[error("memory was not found")]
    NotFound,
    /// A key version or snapshot state is stale.
    #[error("memory changed since it was read")]
    Conflict,
    /// A mutation targeted a namespace not owned by this backend.
    #[error("memories from other namespaces are read-only")]
    RemoteReadOnly,
    /// A store returned an invalid or unbounded pagination sequence.
    #[error("memory store returned invalid pagination")]
    InvalidPagination,
    /// Database schema is newer than this implementation supports.
    #[error(
        "memory schema version {found} is unsupported; this build supports version {supported}"
    )]
    UnsupportedSchemaVersion {
        /// Schema version found on disk.
        found: i64,
        /// Newest schema supported by this implementation.
        supported: i64,
    },
    /// A backend-specific operation failed.
    #[error("memory backend operation failed")]
    Backend {
        /// Backend-specific failure.
        #[source]
        source: Box<dyn Error + Send + Sync>,
    },
    /// A backend-specific operation failed transiently and may be retried.
    #[error("memory backend is temporarily unavailable")]
    Unavailable {
        /// Backend-specific transient failure.
        #[source]
        source: Box<dyn Error + Send + Sync>,
    },
}

impl MemoryError {
    /// Wraps a permanent or unclassified backend-specific failure.
    pub fn backend(source: impl Error + Send + Sync + 'static) -> Self {
        Self::Backend {
            source: Box::new(source),
        }
    }

    /// Wraps a transient backend-specific failure that may succeed when retried.
    pub fn unavailable(source: impl Error + Send + Sync + 'static) -> Self {
        Self::Unavailable {
            source: Box::new(source),
        }
    }

    /// Returns whether the backend classified this failure as transient.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Unavailable { .. })
    }
}

#[cfg(feature = "client")]
impl From<RemoteClientError> for MemoryError {
    fn from(source: RemoteClientError) -> Self {
        match source {
            error @ (RemoteClientError::Transport | RemoteClientError::Unavailable) => {
                Self::unavailable(error)
            }
            RemoteClientError::ReadOnly | RemoteClientError::NamespaceMismatch => {
                Self::RemoteReadOnly
            }
            RemoteClientError::Rejected { code, maximum } => match code {
                protocol::RemoteErrorCode::QueryTooLarge => Self::QueryTooLarge {
                    maximum_bytes: maximum.unwrap_or(MemoryLimits::PRODUCTION.query_bytes),
                },
                protocol::RemoteErrorCode::ContentTooLarge => Self::ContentTooLarge {
                    maximum_bytes: maximum.unwrap_or(MemoryLimits::PRODUCTION.content_bytes),
                },
                protocol::RemoteErrorCode::RecordCapacity => Self::RecordCapacity {
                    maximum: maximum.unwrap_or(MemoryLimits::PRODUCTION.records),
                },
                protocol::RemoteErrorCode::ContentCapacity => maximum
                    .map_or(Self::StorageCapacity, |maximum_bytes| {
                        Self::ContentCapacity { maximum_bytes }
                    }),
                protocol::RemoteErrorCode::Duplicate => Self::Duplicate,
                protocol::RemoteErrorCode::NotFound => Self::NotFound,
                protocol::RemoteErrorCode::Conflict => Self::Conflict,
                protocol::RemoteErrorCode::Forbidden
                | protocol::RemoteErrorCode::NamespaceMismatch => Self::RemoteReadOnly,
                protocol::RemoteErrorCode::Unavailable => {
                    Self::unavailable(RemoteClientError::Rejected { code, maximum })
                }
                _ => Self::backend(RemoteClientError::Rejected { code, maximum }),
            },
            error => Self::backend(error),
        }
    }
}
