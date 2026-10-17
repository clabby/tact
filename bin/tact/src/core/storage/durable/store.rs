//! Nanocode's state store over the session database.
//!
//! Writes run on one thread that owns the process's writer connection, so blocking SQLite work
//! stays off the async runtime and a write accepted by that thread completes even when its caller
//! stops waiting.
//!
//! A commit carries what the contract has no place for, the session's streamed drafts and its
//! replacement resume state, in a host batch that the session's writer stages on its own store
//! handle immediately before the commit. The batch belongs to that writer: only a commit through
//! the same handle takes it, it returns to the handle unless the commit succeeds, and staging
//! while one is pending fails. A writer that stops discards its pending batch.

use super::{SessionStorage, StorageError, not_committed};
use nanocode::durability::{
    Committed, OwnedState, OwnerId, OwnerToken, StateCommit, StateStore, StoreError, StoreFuture,
    StoreRecord, StoreUpdate, StoredState, SummaryUpdate,
};
use std::{
    collections::{HashMap, hash_map::Entry},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    thread,
};
use tokio::sync::{mpsc, oneshot};

/// Writes queued for the store thread before callers wait for their turn.
const JOB_CAPACITY: usize = 64;

type Job = Box<dyn FnOnce(&mut Database) + Send>;

/// The store thread's writer connection, opened by the first write so that a process that never
/// writes a session leaves no database behind.
struct Database {
    config_path: PathBuf,
    storage: Option<SessionStorage>,
}

impl Database {
    fn storage(&mut self) -> Result<&mut SessionStorage, StorageError> {
        let storage = match self.storage.take() {
            Some(storage) => storage,
            None => loop {
                match SessionStorage::open(&self.config_path) {
                    Ok(storage) => break storage,
                    Err(error) if error.is_lock_contention() => {
                        thread::sleep(super::LOCK_RETRY_DELAY);
                    }
                    Err(error) => return Err(error),
                }
            },
        };
        Ok(self.storage.insert(storage))
    }
}

/// What a commit's host writes alongside its contract events: the drafts that sit between those
/// events and the session's replacement resume state.
#[derive(Clone, Debug, Default)]
pub(crate) struct HostBatch {
    pub(crate) rows: Vec<HostRow>,
    pub(crate) resume_state: Option<Vec<u8>>,
}

/// One transcript row of a host batch, in recorded order.
#[derive(Clone, Debug)]
pub(crate) enum HostRow {
    /// The next of the commit's contract events.
    Event,
    /// The JSON encoding of a streamed assistant draft, which the stream's final message replaces
    /// and which therefore never joins the session's event sequence.
    Draft(Vec<u8>),
}

/// Host batches staged by each writer for its next commit.
///
/// Every session writer commits through its own store handle, so a batch is keyed by its state and
/// the handle's writer. Staging over a pending batch is an error. A commit takes only its own
/// writer's batch and puts it back unless the commit succeeds, so a fenced owner can never consume
/// another owner's batch and a rejected request keeps its batch for the retry. A writer that stops
/// discards its own pending batch.
#[derive(Clone, Debug, Default)]
struct HostWrites(Arc<Mutex<HashMap<(String, u64), HostBatch>>>);

impl HostWrites {
    fn batches(&self) -> MutexGuard<'_, HashMap<(String, u64), HostBatch>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A handle to the process's session store. Clones share the store thread and writer identity;
/// [`SqliteStateStore::writer_handle`] creates a handle with a writer identity of its own.
#[derive(Clone, Debug)]
pub(crate) struct SqliteStateStore {
    jobs: mpsc::Sender<Job>,
    host: HostWrites,
    writer: u64,
}

impl SqliteStateStore {
    pub(crate) fn new(config_path: &Path) -> Result<Self, StorageError> {
        let (jobs, mut receiver) = mpsc::channel::<Job>(JOB_CAPACITY);
        let mut database = Database {
            config_path: config_path.to_path_buf(),
            storage: None,
        };
        thread::Builder::new()
            .name("tact-session-store".to_owned())
            .spawn(move || {
                while let Some(job) = receiver.blocking_recv() {
                    job(&mut database);
                }
            })
            .map_err(StorageError::Thread)?;
        Ok(Self {
            jobs,
            host: HostWrites::default(),
            writer: next_writer(),
        })
    }

    pub(crate) fn writer_handle(&self) -> Self {
        Self {
            writer: next_writer(),
            ..self.clone()
        }
    }

    /// Stages the host batch of this writer's next commit to `state_id`.
    pub(crate) fn stage(&self, state_id: &str, batch: HostBatch) -> Result<(), StorageError> {
        match self
            .host
            .batches()
            .entry((state_id.to_owned(), self.writer))
        {
            Entry::Occupied(_) => Err(StorageError::HostBatchPending {
                state_id: state_id.to_owned(),
            }),
            Entry::Vacant(entry) => {
                entry.insert(batch);
                Ok(())
            }
        }
    }

    pub(crate) fn discard(&self, state_id: &str) {
        self.host
            .batches()
            .remove(&(state_id.to_owned(), self.writer));
    }

    #[cfg(test)]
    pub(crate) fn staged_batches(&self) -> usize {
        self.host.batches().len()
    }

    fn run<T: Send + 'static>(
        &self,
        job: impl FnOnce(&mut Database) -> Result<T, StoreError> + Send + 'static,
    ) -> StoreFuture<'static, Result<T, StoreError>> {
        let jobs = self.jobs.clone();
        Box::pin(async move {
            let (reply, result) = oneshot::channel();
            jobs.send(Box::new(move |database| drop(reply.send(job(database)))))
                .await
                .map_err(|_| stopped())?;
            result.await.map_err(|_| stopped())?
        })
    }

    fn read_records_owned(
        &self,
        state_id: String,
        keys: Vec<String>,
    ) -> StoreFuture<'static, Result<Vec<Option<String>>, StoreError>> {
        self.run(move |database| {
            database
                .storage()
                .and_then(|storage| storage.read_stored_records(&state_id, &keys))
                .map_err(backend)
        })
    }

    fn acquire_owned(
        &self,
        state_id: &str,
        owner_id: OwnerId,
        update: Option<StoreUpdate>,
    ) -> StoreFuture<'static, Result<OwnedState, StoreError>> {
        let state_id = state_id.to_owned();
        self.run(move |database| {
            database
                .storage()
                .map_err(not_committed)?
                .acquire_state(&state_id, owner_id, update)
        })
    }
}

impl StateStore for SqliteStateStore {
    fn read_record<'a>(
        &'a mut self,
        state_id: &'a str,
        key: &'a str,
    ) -> StoreFuture<'a, Result<Option<String>, StoreError>> {
        let records = self.read_records_owned(state_id.to_owned(), vec![key.to_owned()]);
        Box::pin(async move { Ok(records.await?.pop().flatten()) })
    }

    fn read_records<'a>(
        &'a mut self,
        state_id: &'a str,
        keys: &'a [String],
    ) -> StoreFuture<'a, Result<Vec<Option<String>>, StoreError>> {
        self.read_records_owned(state_id.to_owned(), keys.to_vec())
    }

    fn peek<'a>(
        &'a mut self,
        state_id: &'a str,
    ) -> StoreFuture<'a, Result<StoredState, StoreError>> {
        let state_id = state_id.to_owned();
        self.run(move |database| {
            database
                .storage()
                .and_then(|storage| storage.read_stored_state(&state_id))
                .map_err(backend)
        })
    }

    fn list_states<'a>(
        &'a mut self,
        limit: usize,
    ) -> StoreFuture<'a, Result<Vec<String>, StoreError>> {
        self.run(move |database| {
            database
                .storage()
                .and_then(|storage| storage.state_ids(limit))
                .map_err(backend)
        })
    }

    fn acquire<'a>(
        &'a mut self,
        state_id: &'a str,
        owner_id: OwnerId,
    ) -> StoreFuture<'a, Result<OwnedState, StoreError>> {
        self.acquire_owned(state_id, owner_id, None)
    }

    fn acquire_with<'a>(
        &'a mut self,
        state_id: &'a str,
        owner_id: OwnerId,
        update: StoreUpdate,
    ) -> StoreFuture<'a, Result<OwnedState, StoreError>> {
        self.acquire_owned(state_id, owner_id, Some(update))
    }

    fn supports_events(&self) -> bool {
        true
    }

    fn supports_summaries(&self) -> bool {
        true
    }

    fn replace<'a>(
        &'a mut self,
        state_id: &'a str,
        owner: &'a OwnerToken,
        expected_revision: u64,
        payload: &'a str,
        records: &'a [StoreRecord],
    ) -> StoreFuture<'a, Result<u64, StoreError>> {
        let committed = self.commit(
            state_id,
            owner,
            expected_revision,
            StateCommit {
                payload,
                records,
                events: &[],
                summary: &SummaryUpdate::Unchanged,
            },
        );
        Box::pin(async move { Ok(committed.await?.revision) })
    }

    fn commit<'a>(
        &'a mut self,
        state_id: &'a str,
        owner: &'a OwnerToken,
        expected_revision: u64,
        commit: StateCommit<'a>,
    ) -> StoreFuture<'a, Result<Committed, StoreError>> {
        let host = self.host.clone();
        let key = (state_id.to_owned(), self.writer);
        let owner = owner.clone();
        let payload = commit.payload.to_owned();
        let records = commit.records.to_vec();
        let events = commit.events.to_vec();
        let summary = commit.summary.clone();
        self.run(move |database| {
            let batch = host.batches().remove(&key);
            let committed = database
                .storage()
                .map_err(not_committed)
                .and_then(|storage| {
                    storage.commit_state(
                        &key.0,
                        &owner,
                        expected_revision,
                        StateCommit {
                            payload: &payload,
                            records: &records,
                            events: &events,
                            summary: &summary,
                        },
                        batch.as_ref(),
                    )
                });
            if committed.is_err()
                && let Some(batch) = batch
            {
                host.batches().insert(key, batch);
            }
            committed
        })
    }
}

pub(super) fn backend(error: impl std::fmt::Display) -> StoreError {
    StoreError::Backend(error.to_string())
}

fn next_writer() -> u64 {
    static NEXT_WRITER: AtomicU64 = AtomicU64::new(1);
    NEXT_WRITER.fetch_add(1, Ordering::Relaxed)
}

fn stopped() -> StoreError {
    StoreError::Backend("the session store thread stopped".to_owned())
}

#[cfg(test)]
mod tests;
