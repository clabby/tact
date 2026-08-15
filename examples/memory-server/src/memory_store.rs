use std::{
    collections::{BTreeMap, HashSet},
    future::Future,
    sync::{Arc, Mutex, MutexGuard},
    time::{SystemTime, UNIX_EPOCH},
};
use tact_memory::{
    MemoryError, MemoryKey, MemoryLimits, MemoryRecord, MemoryScan, MemoryStore,
    server::protocol::{self, ExportCursor, SyncReport},
};

#[derive(Clone, Debug, Default)]
pub(crate) struct InMemoryBackend {
    state: Arc<Mutex<BackendState>>,
}

impl InMemoryBackend {
    pub(crate) fn bind(&self, namespace: String) -> InMemoryStore {
        InMemoryStore {
            namespace,
            state: Arc::clone(&self.state),
        }
    }
}

#[derive(Debug, Default)]
struct BackendState {
    namespaces: BTreeMap<String, NamespaceState>,
}

#[derive(Debug)]
struct NamespaceState {
    next_id: i64,
    records: BTreeMap<i64, MemoryRecord>,
}

impl Default for NamespaceState {
    fn default() -> Self {
        Self {
            next_id: 1,
            records: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct InMemoryStore {
    namespace: String,
    state: Arc<Mutex<BackendState>>,
}

impl InMemoryStore {
    fn state(&self) -> MutexGuard<'_, BackendState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl MemoryStore for InMemoryStore {
    fn scan(
        &self,
        query: &str,
        limit: usize,
    ) -> impl Future<Output = Result<MemoryScan, MemoryError>> + Send {
        let query = query.to_owned();
        let store = self.clone();
        async move {
            let limits = MemoryLimits::PRODUCTION;
            if query.len() > limits.query_bytes {
                return Err(MemoryError::QueryTooLarge {
                    maximum_bytes: limits.query_bytes,
                });
            }
            let now_ms = current_time_ms();
            let mut state = store.state();
            prune_expired(&mut state, now_ms);
            let records = state
                .namespaces
                .values()
                .flat_map(|namespace| namespace.records.values())
                .cloned()
                .collect::<Vec<_>>();
            let scan = MemoryScan::rank(&query, &records, limit.min(limits.scan_results));
            for candidate in &scan.candidates {
                if let Some(record) = record_mut(&mut state, &candidate.key) {
                    record.last_scanned_at_ms = Some(now_ms);
                    record.scan_count = record.scan_count.saturating_add(1);
                }
            }
            Ok(scan)
        }
    }

    fn read(
        &self,
        ids: &[i64],
        keys: &[MemoryKey],
    ) -> impl Future<Output = Result<Vec<MemoryRecord>, MemoryError>> + Send {
        let store = self.clone();
        let ids = ids.to_vec();
        let keys = keys.to_vec();
        async move {
            let now_ms = current_time_ms();
            let mut state = store.state();
            prune_expired(&mut state, now_ms);
            let mut requested = keys
                .into_iter()
                .filter_map(|key| {
                    key.namespace
                        .clone()
                        .map(|namespace| (namespace, key.id, Some(key.version)))
                })
                .collect::<Vec<_>>();
            requested.extend(
                ids.into_iter()
                    .map(|id| (store.namespace.clone(), id, None)),
            );

            let mut seen = HashSet::new();
            let mut records = Vec::new();
            for (namespace, id, version) in requested {
                if !seen.insert((namespace.clone(), id)) {
                    continue;
                }
                let Some(record) = state
                    .namespaces
                    .get_mut(&namespace)
                    .and_then(|state| state.records.get_mut(&id))
                else {
                    continue;
                };
                if version.is_some_and(|version| version != record.key.version) {
                    continue;
                }
                record.last_used_at_ms = Some(now_ms);
                record.use_count = record.use_count.saturating_add(1);
                record.probation_until_ms = None;
                records.push(record.clone());
            }
            Ok(records)
        }
    }

    fn list(&self) -> impl Future<Output = Result<Vec<MemoryRecord>, MemoryError>> + Send {
        let store = self.clone();
        async move {
            let mut state = store.state();
            prune_expired(&mut state, current_time_ms());
            Ok(all_records(&state)
                .into_iter()
                .take(MemoryLimits::PRODUCTION.records)
                .collect())
        }
    }

    fn put(
        &self,
        content: &str,
        replacement: Option<MemoryKey>,
    ) -> impl Future<Output = Result<MemoryRecord, MemoryError>> + Send {
        let store = self.clone();
        let content = content.to_owned();
        async move {
            validate_content(&content)?;
            if replacement
                .as_ref()
                .is_some_and(|key| key.namespace.as_deref() != Some(store.namespace.as_str()))
            {
                return Err(MemoryError::RemoteReadOnly);
            }
            let now_ms = current_time_ms();
            let mut state = store.state();
            prune_expired(&mut state, now_ms);
            let namespace = state.namespaces.entry(store.namespace.clone()).or_default();
            if namespace.records.values().any(|record| {
                replacement
                    .as_ref()
                    .is_none_or(|key| key.id != record.key.id)
                    && normalize(&record.content) == normalize(&content)
            }) {
                return Err(MemoryError::Duplicate);
            }
            let current_bytes = content_bytes(namespace);
            let record = match replacement {
                Some(key) => {
                    let current = namespace
                        .records
                        .get(&key.id)
                        .ok_or(MemoryError::NotFound)?;
                    if current.key.version != key.version {
                        return Err(MemoryError::Conflict);
                    }
                    check_content_capacity(current_bytes, current.content.len(), content.len())?;
                    let version = key.version.checked_add(1).ok_or(MemoryError::Conflict)?;
                    MemoryRecord {
                        key: MemoryKey::remote(store.namespace.clone(), key.id, version),
                        content,
                        created_at_ms: current.created_at_ms,
                        updated_at_ms: now_ms,
                        last_scanned_at_ms: None,
                        scan_count: 0,
                        last_used_at_ms: None,
                        use_count: 0,
                        probation_until_ms: Some(
                            now_ms.saturating_add(MemoryLimits::PRODUCTION.probation_duration_ms),
                        ),
                    }
                }
                None => {
                    if namespace.records.len() >= MemoryLimits::PRODUCTION.records {
                        return Err(MemoryError::RecordCapacity {
                            maximum: MemoryLimits::PRODUCTION.records,
                        });
                    }
                    check_content_capacity(current_bytes, 0, content.len())?;
                    let id = namespace.next_id;
                    namespace.next_id = namespace
                        .next_id
                        .checked_add(1)
                        .ok_or(MemoryError::Conflict)?;
                    MemoryRecord {
                        key: MemoryKey::remote(store.namespace.clone(), id, 1),
                        content,
                        created_at_ms: now_ms,
                        updated_at_ms: now_ms,
                        last_scanned_at_ms: None,
                        scan_count: 0,
                        last_used_at_ms: None,
                        use_count: 0,
                        probation_until_ms: Some(
                            now_ms.saturating_add(MemoryLimits::PRODUCTION.probation_duration_ms),
                        ),
                    }
                }
            };
            namespace.records.insert(record.key.id, record.clone());
            Ok(record)
        }
    }

    fn delete(&self, key: MemoryKey) -> impl Future<Output = Result<(), MemoryError>> + Send {
        let store = self.clone();
        async move {
            if key.namespace.as_deref() != Some(store.namespace.as_str()) {
                return Err(MemoryError::RemoteReadOnly);
            }
            let mut state = store.state();
            let Some(namespace) = state.namespaces.get_mut(&store.namespace) else {
                return Ok(());
            };
            let Some(record) = namespace.records.get(&key.id) else {
                return Ok(());
            };
            if record.key.version != key.version {
                return Err(MemoryError::Conflict);
            }
            namespace.records.remove(&key.id);
            Ok(())
        }
    }

    fn sync(
        &self,
        memories: &[MemoryRecord],
    ) -> impl Future<Output = Result<SyncReport, MemoryError>> + Send {
        let store = self.clone();
        let memories = memories.to_vec();
        async move {
            validate_snapshot(&memories)?;
            let mut state = store.state();
            prune_expired(&mut state, current_time_ms());
            let previous = state.namespaces.get(&store.namespace);
            let mut report = SyncReport::default();
            let incoming_ids = memories
                .iter()
                .map(|memory| memory.key.id)
                .collect::<HashSet<_>>();
            if let Some(previous) = previous {
                report.deleted = previous
                    .records
                    .keys()
                    .filter(|id| !incoming_ids.contains(id))
                    .count();
            }
            let mut records = BTreeMap::new();
            let mut maximum_id = 0;
            for mut memory in memories {
                memory.key.namespace = Some(store.namespace.clone());
                let old = previous.and_then(|namespace| namespace.records.get(&memory.key.id));
                match old {
                    Some(old) if old == &memory => report.unchanged += 1,
                    Some(_) => report.replaced += 1,
                    None => report.inserted += 1,
                }
                maximum_id = maximum_id.max(memory.key.id);
                records.insert(memory.key.id, memory);
            }
            let old_next_id = previous.map_or(1, |namespace| namespace.next_id);
            let next_id = old_next_id.max(maximum_id.checked_add(1).ok_or(MemoryError::Conflict)?);
            state
                .namespaces
                .insert(store.namespace.clone(), NamespaceState { next_id, records });
            Ok(report)
        }
    }

    fn export_page(
        &self,
        namespaces: Option<&[String]>,
        cursor: Option<&ExportCursor>,
        limit: usize,
    ) -> impl Future<Output = Result<(Vec<MemoryRecord>, Option<ExportCursor>), MemoryError>> + Send
    {
        let store = self.clone();
        let namespaces = namespaces.map(<[String]>::to_vec);
        let cursor = cursor.cloned();
        async move {
            let mut state = store.state();
            prune_expired(&mut state, current_time_ms());
            let selected = namespaces.map(|values| values.into_iter().collect::<HashSet<_>>());
            let mut records = all_records(&state);
            records.retain(|record| {
                selected.as_ref().is_none_or(|namespaces| {
                    record
                        .key
                        .namespace
                        .as_ref()
                        .is_some_and(|namespace| namespaces.contains(namespace))
                }) && cursor.as_ref().is_none_or(|cursor| {
                    record.key.namespace.as_deref().is_some_and(|namespace| {
                        (namespace, record.key.id) > (cursor.namespace.as_str(), cursor.id)
                    })
                })
            });
            let limit = limit.clamp(1, protocol::MAX_EXPORT_PAGE_RECORDS);
            let has_more = records.len() > limit;
            records.truncate(limit);
            let next_cursor = has_more.then(|| {
                let key = &records
                    .last()
                    .expect("a limited page with more records is non-empty")
                    .key;
                ExportCursor {
                    namespace: key
                        .namespace
                        .clone()
                        .expect("remote records have namespaces"),
                    id: key.id,
                }
            });
            Ok((records, next_cursor))
        }
    }
}

fn all_records(state: &BackendState) -> Vec<MemoryRecord> {
    state
        .namespaces
        .values()
        .flat_map(|namespace| namespace.records.values().cloned())
        .collect()
}

fn record_mut<'a>(state: &'a mut BackendState, key: &MemoryKey) -> Option<&'a mut MemoryRecord> {
    state
        .namespaces
        .get_mut(key.namespace.as_deref()?)?
        .records
        .get_mut(&key.id)
}

fn prune_expired(state: &mut BackendState, now_ms: i64) {
    for namespace in state.namespaces.values_mut() {
        namespace.records.retain(|_, record| {
            !record
                .probation_until_ms
                .is_some_and(|until| until <= now_ms && record.use_count == 0)
        });
    }
}

fn validate_content(content: &str) -> Result<(), MemoryError> {
    if content.trim().is_empty() {
        return Err(MemoryError::EmptyContent);
    }
    if content.len() > MemoryLimits::PRODUCTION.content_bytes {
        return Err(MemoryError::ContentTooLarge {
            maximum_bytes: MemoryLimits::PRODUCTION.content_bytes,
        });
    }
    Ok(())
}

fn validate_snapshot(memories: &[MemoryRecord]) -> Result<(), MemoryError> {
    if memories.len() > MemoryLimits::PRODUCTION.records {
        return Err(MemoryError::RecordCapacity {
            maximum: MemoryLimits::PRODUCTION.records,
        });
    }
    let mut ids = HashSet::new();
    let mut identities = HashSet::new();
    let mut bytes = 0usize;
    for memory in memories {
        if !memory.key.is_local()
            || memory.key.id <= 0
            || memory.key.version == 0
            || !ids.insert(memory.key.id)
        {
            return Err(MemoryError::Conflict);
        }
        validate_content(&memory.content)?;
        if !identities.insert(normalize(&memory.content)) {
            return Err(MemoryError::Duplicate);
        }
        bytes = bytes
            .checked_add(memory.content.len())
            .ok_or(MemoryError::ContentCapacity {
                maximum_bytes: MemoryLimits::PRODUCTION.total_content_bytes,
            })?;
    }
    if bytes > MemoryLimits::PRODUCTION.total_content_bytes {
        return Err(MemoryError::ContentCapacity {
            maximum_bytes: MemoryLimits::PRODUCTION.total_content_bytes,
        });
    }
    Ok(())
}

fn content_bytes(namespace: &NamespaceState) -> usize {
    namespace
        .records
        .values()
        .map(|record| record.content.len())
        .sum()
}

fn check_content_capacity(current: usize, replaced: usize, new: usize) -> Result<(), MemoryError> {
    if current.saturating_sub(replaced).saturating_add(new)
        > MemoryLimits::PRODUCTION.total_content_bytes
    {
        return Err(MemoryError::ContentCapacity {
            maximum_bytes: MemoryLimits::PRODUCTION.total_content_bytes,
        });
    }
    Ok(())
}

fn normalize(content: &str) -> String {
    content
        .split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

fn current_time_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::{InMemoryBackend, MemoryStore};
    use tact_memory::{MemoryKey, MemoryRecord};

    fn snapshot(id: i64, content: &str) -> MemoryRecord {
        MemoryRecord {
            key: MemoryKey::local(id, 1),
            content: content.to_owned(),
            created_at_ms: 1,
            updated_at_ms: 1,
            last_scanned_at_ms: None,
            scan_count: 0,
            last_used_at_ms: None,
            use_count: 0,
            probation_until_ms: None,
        }
    }

    #[tokio::test]
    async fn namespaces_share_reads_but_only_owners_mutate() {
        let backend = InMemoryBackend::default();
        let alice = backend.bind("alice".to_owned());
        let bob = backend.bind("bob".to_owned());
        let record = alice.put("shared indexing note", None).await.unwrap();

        assert_eq!(bob.list().await.unwrap(), std::slice::from_ref(&record));
        assert_eq!(
            bob.read(&[], std::slice::from_ref(&record.key))
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(bob.delete(record.key).await.is_err());
    }

    #[tokio::test]
    async fn sync_is_authoritative_and_preserves_monotonic_ids() {
        let store = InMemoryBackend::default().bind("alice".to_owned());
        store.put("old note", None).await.unwrap();
        let memory = snapshot(10, "snapshot note");
        let report = store.sync(std::slice::from_ref(&memory)).await.unwrap();
        assert_eq!((report.inserted, report.deleted), (1, 1));

        let mut used = memory;
        used.last_used_at_ms = Some(2);
        used.use_count = 1;
        let report = store.sync(&[used]).await.unwrap();
        assert_eq!((report.replaced, report.unchanged), (1, 0));
        assert_eq!(store.put("later note", None).await.unwrap().key.id, 11);
    }

    #[tokio::test]
    async fn scan_uses_the_shared_code_identifier_tokenization() {
        let store = InMemoryBackend::default().bind("alice".to_owned());
        store
            .put("the httpServer owns routing", None)
            .await
            .unwrap();

        let scan = store.scan("server", 5).await.unwrap();

        assert_eq!(scan.candidates.len(), 1);
    }

    #[tokio::test]
    async fn export_uses_stable_namespace_and_id_order() {
        let backend = InMemoryBackend::default();
        backend
            .bind("bob".to_owned())
            .put("bob note", None)
            .await
            .unwrap();
        backend
            .bind("alice".to_owned())
            .put("alice note", None)
            .await
            .unwrap();
        let store = backend.bind("reader".to_owned());

        let (first, cursor) = store.export_page(None, None, 1).await.unwrap();
        let (second, cursor) = store.export_page(None, cursor.as_ref(), 1).await.unwrap();
        assert_eq!(first[0].key.namespace.as_deref(), Some("alice"));
        assert_eq!(second[0].key.namespace.as_deref(), Some("bob"));
        assert!(cursor.is_none());
    }
}
