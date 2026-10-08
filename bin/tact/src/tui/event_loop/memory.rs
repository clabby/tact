//! The memory browser's store and its operations.
//!
//! Operations run off the loop. Each pane presents only the completion of its newest request, and
//! a configuration reload, which may replace the store, makes every outstanding completion stale.

use super::EventLoop;
use crate::{core::pane::PaneId, tui::components::AppEvent};
use std::collections::HashMap;
use tact_memory::{
    MemoryAccess, MemoryError, MemoryKey, MemoryRecord, MemorySource, MemoryStore,
    SelectedMemoryStore,
};
use tokio::task::{JoinError, JoinSet};

enum MemoryOperation {
    List,
    Delete(MemoryKey),
}

enum MemoryCompletion {
    Listed {
        pane: PaneId,
        generation: u64,
        source: MemorySource,
        result:
            std::result::Result<(MemoryAccess, Vec<MemoryRecord>), (Option<MemoryAccess>, String)>,
    },
    Deleted {
        pane: PaneId,
        generation: u64,
        key: MemoryKey,
        conflict: bool,
        result: std::result::Result<(), String>,
    },
}

impl MemoryCompletion {
    const fn identity(&self) -> (PaneId, u64) {
        match self {
            Self::Listed {
                pane, generation, ..
            }
            | Self::Deleted {
                pane, generation, ..
            } => (*pane, *generation),
        }
    }

    fn into_event(self) -> AppEvent {
        match self {
            Self::Listed {
                pane,
                result: Ok((access, records)),
                ..
            } => AppEvent::MemoriesLoaded {
                pane,
                access,
                records,
            },
            Self::Listed {
                pane,
                source,
                result: Err((access, error)),
                ..
            } => AppEvent::MemoryLoadFailed {
                pane,
                source,
                access,
                error,
            },
            Self::Deleted {
                pane,
                key,
                result: Ok(()),
                ..
            } => AppEvent::MemoryDeleted { pane, key },
            Self::Deleted {
                pane,
                conflict,
                result: Err(error),
                ..
            } => AppEvent::MemoryDeleteFailed {
                pane,
                error,
                conflict,
            },
        }
    }
}

impl MemoryOperation {
    async fn run(
        self,
        pane: PaneId,
        generation: u64,
        store: &SelectedMemoryStore,
    ) -> MemoryCompletion {
        match self {
            Self::List => MemoryCompletion::Listed {
                pane,
                generation,
                source: store.source(),
                result: match store.access().await {
                    Ok(access) => store
                        .list()
                        .await
                        .map(|records| (access.clone(), records))
                        .map_err(|error| (Some(access), error.to_string())),
                    Err(error) => Err((None, error.to_string())),
                },
            },
            Self::Delete(key) => {
                let result = store.delete(key.clone()).await;
                MemoryCompletion::Deleted {
                    pane,
                    generation,
                    key,
                    conflict: matches!(result, Err(MemoryError::Conflict)),
                    result: result.map_err(|error| error.to_string()),
                }
            }
        }
    }
}

/// Per-pane request counters for memory operations. Only the completion of a pane's newest
/// request is presented.
#[derive(Default)]
struct MemoryGenerations(HashMap<PaneId, u64>);

impl MemoryGenerations {
    /// Starts a new request for the pane and returns its generation.
    fn next(&mut self, pane: PaneId) -> u64 {
        let generation = self.0.entry(pane).or_default();
        *generation = generation.wrapping_add(1).max(1);
        *generation
    }

    fn invalidate_all(&mut self) {
        for generation in self.0.values_mut() {
            *generation = generation.wrapping_add(1).max(1);
        }
    }

    fn is_current(&self, pane: PaneId, generation: u64) -> bool {
        self.0.get(&pane) == Some(&generation)
    }
}

/// The configured memory store, absent when memory is disabled, and its operations in flight.
pub(super) struct Memory {
    store: Option<SelectedMemoryStore>,
    tasks: JoinSet<MemoryCompletion>,
    generations: MemoryGenerations,
}

impl Memory {
    pub(super) fn new(store: Option<SelectedMemoryStore>) -> Self {
        Self {
            store,
            tasks: JoinSet::new(),
            generations: MemoryGenerations::default(),
        }
    }

    pub(super) fn is_enabled(&self) -> bool {
        self.store.is_some()
    }

    pub(super) fn store(&self) -> Option<&SelectedMemoryStore> {
        self.store.as_ref()
    }

    /// Replaces the store after a configuration reload. Completions already in flight are stale.
    pub(super) fn replace_store(&mut self, store: Option<SelectedMemoryStore>) {
        self.generations.invalidate_all();
        self.store = store;
    }

    /// Runs `operation` for `pane`, superseding the pane's earlier operations. Returns `false` when
    /// memory is disabled.
    fn start(&mut self, pane: PaneId, operation: MemoryOperation) -> bool {
        let Some(store) = self.store.clone() else {
            return false;
        };
        let generation = self.generations.next(pane);
        self.tasks
            .spawn(async move { operation.run(pane, generation, &store).await });
        true
    }

    /// Waits for the next operation to finish, yielding the event to show for it when it is its
    /// pane's newest request. Cancel safe.
    pub(super) async fn join_next(
        &mut self,
    ) -> Option<std::result::Result<Option<AppEvent>, JoinError>> {
        let result = self.tasks.join_next().await?;
        Some(result.map(|completion| {
            let (pane, generation) = completion.identity();
            self.generations
                .is_current(pane, generation)
                .then(|| completion.into_event())
        }))
    }

    pub(super) fn abort_all(&mut self) {
        self.tasks.abort_all();
    }
}

impl EventLoop {
    pub(super) fn load_memories(&mut self, pane: PaneId) {
        if !self.memory.start(pane, MemoryOperation::List) {
            self.show(AppEvent::MemoryLoadFailed {
                pane,
                source: MemorySource::Local,
                access: None,
                error: "Memory is disabled. Enable it with memory.enabled = true.".to_owned(),
            });
        }
    }

    pub(super) fn delete_memory(&mut self, pane: PaneId, key: MemoryKey) {
        if !self.memory.start(pane, MemoryOperation::Delete(key)) {
            self.show(AppEvent::MemoryDeleteFailed {
                pane,
                error: "Memory was disabled before the deletion completed.".to_owned(),
                conflict: false,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Memory, MemoryCompletion, MemoryGenerations, MemoryOperation};
    use crate::{
        app::config::{Config, ConfigOverrides},
        core::{configured_memory_store, pane::PaneId},
        tui::components::AppEvent,
    };
    use std::fs;
    use tact_memory::{MemoryAccess, MemoryLimits, MemoryStore, SelectedMemoryStore};
    use tempfile::tempdir;

    fn config(memory: &str) -> (tempfile::TempDir, Config) {
        let directory = tempdir().unwrap();
        let config_path = directory.path().join("config.toml");
        fs::write(&config_path, memory).unwrap();
        let config = Config::load(ConfigOverrides {
            path: Some(config_path),
            workspace: Some(directory.path().to_path_buf()),
            ..ConfigOverrides::default()
        })
        .unwrap();
        (directory, config)
    }

    fn local_store(directory: &std::path::Path) -> SelectedMemoryStore {
        SelectedMemoryStore::local(directory.join("memory.sqlite3"), MemoryLimits::PRODUCTION)
    }

    #[test]
    fn disabled_memory_does_not_construct_or_open_the_database() {
        let (_directory, config) = config("");

        assert!(
            configured_memory_store(&config, config.agent().workspace())
                .unwrap()
                .is_none()
        );
        assert!(!config.memory_path().exists());
    }

    #[test]
    fn enabled_memory_constructs_the_global_store_without_eagerly_opening_it() {
        let (_directory, config) = config("[memory]\nenabled = true\n");

        assert!(
            configured_memory_store(&config, config.agent().workspace())
                .unwrap()
                .is_some()
        );
        assert!(!config.memory_path().exists());
    }

    #[tokio::test]
    async fn memory_list_inspection_does_not_change_use_telemetry() {
        let directory = tempdir().unwrap();
        let store = local_store(directory.path());
        store.put("inspect without using", None).await.unwrap();

        let MemoryCompletion::Listed {
            pane: PaneId::Fork(4),
            result: Ok((access, records)),
            ..
        } = MemoryOperation::List.run(PaneId::Fork(4), 1, &store).await
        else {
            panic!("list should complete for the originating pane");
        };

        assert_eq!(access, MemoryAccess::Local);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].scan_count, 0);
        assert_eq!(records[0].last_scanned_at_ms, None);
        assert_eq!(records[0].use_count, 0);
        assert_eq!(records[0].last_used_at_ms, None);
    }

    #[test]
    fn newer_memory_operations_supersede_older_pane_completions() {
        let mut generations = MemoryGenerations::default();

        let superseded = generations.next(PaneId::Main);
        let fork = generations.next(PaneId::Fork(1));
        let latest = generations.next(PaneId::Main);
        assert!(!generations.is_current(PaneId::Main, superseded));
        assert!(generations.is_current(PaneId::Main, latest));
        assert!(generations.is_current(PaneId::Fork(1), fork));

        generations.invalidate_all();
        assert!(!generations.is_current(PaneId::Main, latest));
        assert!(!generations.is_current(PaneId::Fork(1), fork));
        let renewed = generations.next(PaneId::Main);
        assert!(generations.is_current(PaneId::Main, renewed));
    }

    #[tokio::test]
    async fn stale_human_delete_is_reported_to_the_originating_pane_as_a_conflict() {
        let directory = tempdir().unwrap();
        let store = local_store(directory.path());
        let original = store.put("old value", None).await.unwrap();
        store
            .put("new value", Some(original.key.clone()))
            .await
            .unwrap();

        let completion = MemoryOperation::Delete(original.key.clone())
            .run(PaneId::Fork(9), 1, &store)
            .await;

        assert!(matches!(
            completion,
            MemoryCompletion::Deleted {
                pane: PaneId::Fork(9),
                key,
                conflict: true,
                result: Err(_),
                ..
            } if key == original.key
        ));
    }

    #[tokio::test]
    async fn a_reload_suppresses_completions_requested_before_it() {
        let directory = tempdir().unwrap();
        let mut memory = Memory::new(Some(local_store(directory.path())));

        assert!(memory.start(PaneId::Main, MemoryOperation::List));
        memory.replace_store(None);
        assert!(!memory.is_enabled());
        assert!(matches!(memory.join_next().await, Some(Ok(None))));

        memory.replace_store(Some(local_store(directory.path())));
        assert!(memory.start(PaneId::Main, MemoryOperation::List));
        assert!(matches!(
            memory.join_next().await,
            Some(Ok(Some(AppEvent::MemoriesLoaded {
                pane: PaneId::Main,
                ..
            })))
        ));
        assert!(!Memory::new(None).start(PaneId::Main, MemoryOperation::List));
    }
}
