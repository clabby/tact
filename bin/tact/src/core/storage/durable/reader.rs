//! The read-only side of the durability contract, for clients that hold no state store.
//!
//! Each read opens its own read-only connection, so readers never create the database, take its
//! write lock, or queue behind the store thread's writes.

use super::{
    EventCursor, EventHit, EventHits, EventOrder, EventPage, EventQuery, EventRange, IndexedState,
    LocalKind, Path, Row, SessionStorage, StateCursor, StatePage, StateQuery, StateSummary,
    StorageError, StoreError, StoreEvent, StoredEvent, StoredState, Value, decode_record,
    from_sql_u64, io, params, params_from_iter, query, schema_exists, store::backend,
};
use crate::core::storage::to_sql_u64;
use nanocode::durability::{StateReader, StoreFuture};
use std::sync::Arc;

/// Reads the session database without owning any state.
#[derive(Clone, Debug)]
pub(crate) struct SqliteStateReader {
    config_path: Arc<Path>,
}

impl SqliteStateReader {
    pub(crate) fn new(config_path: &Path) -> Self {
        Self {
            config_path: config_path.into(),
        }
    }

    /// Runs `read` on a blocking thread, or returns `missing` when no database exists yet.
    fn read<T: Send + 'static>(
        &self,
        missing: impl FnOnce() -> T + Send + 'static,
        read: impl FnOnce(&SessionStorage) -> Result<T, StorageError> + Send + 'static,
    ) -> StoreFuture<'static, Result<T, StoreError>> {
        let config_path = Arc::clone(&self.config_path);
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                match SessionStorage::open_read_only(&config_path)? {
                    Some(storage) => read(&storage),
                    None => Ok(missing()),
                }
            })
            .await
            .map_err(backend)?
            .map_err(backend)
        })
    }
}

impl StateReader for SqliteStateReader {
    fn read_state<'a>(
        &'a mut self,
        state_id: &'a str,
    ) -> StoreFuture<'a, Result<StoredState, StoreError>> {
        let state_id = state_id.to_owned();
        self.read(StoredState::default, move |storage| {
            storage.read_stored_state(&state_id)
        })
    }

    fn read_records<'a>(
        &'a mut self,
        state_id: &'a str,
        keys: &'a [String],
    ) -> StoreFuture<'a, Result<Vec<Option<String>>, StoreError>> {
        let state_id = state_id.to_owned();
        let keys = keys.to_vec();
        let missing = vec![None; keys.len()];
        self.read(
            move || missing,
            move |storage| storage.read_stored_records(&state_id, &keys),
        )
    }

    fn read_events<'a>(
        &'a mut self,
        state_id: &'a str,
        range: &'a EventRange,
    ) -> StoreFuture<'a, Result<EventPage, StoreError>> {
        let state_id = state_id.to_owned();
        let range = range.clone();
        self.read(EventPage::default, move |storage| {
            storage.read_events(&state_id, &range)
        })
    }

    fn query_states<'a>(
        &'a mut self,
        query: &'a StateQuery,
    ) -> StoreFuture<'a, Result<StatePage, StoreError>> {
        let query = query.clone();
        self.read(StatePage::default, move |storage| {
            storage.query_states(&query)
        })
    }

    fn query_events<'a>(
        &'a mut self,
        query: &'a EventQuery,
    ) -> StoreFuture<'a, Result<EventHits, StoreError>> {
        let query = query.clone();
        self.read(EventHits::default, move |storage| {
            storage.query_events(&query)
        })
    }
}

impl SessionStorage {
    fn read_events(&self, state_id: &str, range: &EventRange) -> Result<EventPage, StorageError> {
        // Sequences are SQLite integers, so an exclusive lower bound beyond them excludes every
        // event and an exclusive upper bound beyond them excludes none.
        let after = range
            .after
            .map_or(Some(0), |after| i64::try_from(after).ok());
        let before = range.before.and_then(|before| i64::try_from(before).ok());
        let Some(after) = after else {
            return Ok(EventPage::default());
        };
        if range.limit == 0 || !schema_exists(&self.connection, &self.path)? {
            return Ok(EventPage::default());
        }
        let order = match range.order {
            EventOrder::Ascending => "ASC",
            EventOrder::Descending => "DESC",
        };
        let mut statement = self
            .connection
            .prepare(&format!(
                "SELECT {EVENT_COLUMNS} FROM events e\n\
                 LEFT JOIN conversation_messages m ON m.event_id = e.event_id\n\
                 WHERE e.session_id = ?1 AND e.seq > ?2 AND (?3 IS NULL OR e.seq < ?3)\n\
                 ORDER BY e.seq {order}"
            ))
            .map_err(|source| query(&self.path, source))?;
        let mut rows = statement
            .query(params![state_id, after, before])
            .map_err(|source| query(&self.path, source))?;
        let mut page = EventPage::default();
        while let Some(row) = rows.next().map_err(|source| query(&self.path, source))? {
            let event = stored_event(&self.path, row)?;
            if !matches_kind(&range.kinds, &event.event.kind) {
                continue;
            }
            if page.events.len() == range.limit {
                page.next = page.events.last().map(|event| event.seq);
                break;
            }
            page.events.push(event);
        }
        Ok(page)
    }

    fn query_states(&self, state_query: &StateQuery) -> Result<StatePage, StorageError> {
        if state_query.limit == 0 || !schema_exists(&self.connection, &self.path)? {
            return Ok(StatePage::default());
        }
        let mut parameters = Vec::new();
        let mut filters = String::new();
        let mut bind = |value: Value| {
            parameters.push(value);
            parameters.len()
        };
        if let Some(scope) = &state_query.scope {
            let scope = bind(Value::Text(scope.clone()));
            filters.push_str(&format!("AND s.scope = ?{scope}\n"));
        }
        if let Some(parent) = &state_query.parent {
            let parent = bind(Value::Text(parent.clone()));
            filters.push_str(&format!("AND s.parent = ?{parent}\n"));
        }
        for (key, value) in &state_query.attributes {
            let key = bind(Value::Text(key.clone()));
            let value = bind(Value::Text(value.clone()));
            filters.push_str(&format!(
                "AND EXISTS(SELECT 1 FROM json_each(s.attributes)\n\
                 WHERE key = ?{key} AND value = ?{value})\n"
            ));
        }
        if !state_query.text.is_empty() {
            let matches = state_query
                .text
                .iter()
                .map(|pattern| {
                    let pattern = bind(Value::Text(pattern.clone()));
                    format!(
                        "instr(lower(COALESCE(s.title, '')), lower(?{pattern})) > 0\n\
                         OR instr(lower(COALESCE(s.preview, '')), lower(?{pattern})) > 0\n\
                         OR EXISTS(SELECT 1 FROM events e\n\
                             LEFT JOIN conversation_messages m ON m.event_id = e.event_id\n\
                             WHERE e.session_id = s.state_id AND e.seq IS NOT NULL\n\
                             AND instr(lower(COALESCE(e.prompt_text, m.text)), lower(?{pattern})) > 0)"
                    )
                })
                .collect::<Vec<_>>()
                .join("\nOR ");
            filters.push_str(&format!("AND ({matches})\n"));
        }
        // A cursor time beyond SQLite's integers lies after every stored summary.
        if let Some(after) = &state_query.after
            && let Ok(updated_at) = i64::try_from(after.updated_at_ms)
        {
            let updated_at = bind(Value::Integer(updated_at));
            let state_id = bind(Value::Text(after.state_id.clone()));
            filters.push_str(&format!(
                "AND (s.updated_at_ms < ?{updated_at}\n\
                 OR (s.updated_at_ms = ?{updated_at} AND s.state_id < ?{state_id}))\n"
            ));
        }
        let limit = bind(Value::Integer(
            i64::try_from(state_query.limit.saturating_add(1)).unwrap_or(i64::MAX),
        ));
        let mut statement = self
            .connection
            .prepare(&format!(
                "SELECT s.state_id, s.revision, s.scope, s.parent, s.title, s.preview,\n\
                 s.attributes, s.updated_at_ms, s.value\n\
                 FROM durable_summaries s WHERE s.live = 1\n\
                 {filters}\
                 ORDER BY s.updated_at_ms DESC, s.state_id DESC LIMIT ?{limit}"
            ))
            .map_err(|source| query(&self.path, source))?;
        let rows = statement
            .query_map(params_from_iter(parameters), |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    StateSummary {
                        scope: row.get(2)?,
                        parent: row.get(3)?,
                        title: row.get(4)?,
                        preview: row.get(5)?,
                        attributes: Default::default(),
                        updated_at_ms: from_sql_u64(row.get(7)?),
                        value: row.get(8)?,
                    },
                    row.get::<_, String>(6)?,
                ))
            })
            .map_err(|source| query(&self.path, source))?;
        let mut states = Vec::new();
        for row in rows {
            let (state_id, revision, mut summary, attributes) =
                row.map_err(|source| query(&self.path, source))?;
            summary.attributes = serde_json::from_str(&attributes)?;
            states.push(IndexedState {
                state_id,
                revision: from_sql_u64(revision),
                summary,
            });
        }
        let next = (states.len() > state_query.limit)
            .then(|| {
                states.truncate(state_query.limit);
                states.last().map(|state| StateCursor {
                    updated_at_ms: state.summary.updated_at_ms,
                    state_id: state.state_id.clone(),
                })
            })
            .flatten();
        Ok(StatePage { states, next })
    }

    /// Event hits are ordered by occurrence time, which only a record's body holds, so every
    /// candidate is decoded and ordered before the page's bodies are read. Recent prompts are
    /// served by Tact's own indexed query rather than through this scan.
    fn query_events(&self, event_query: &EventQuery) -> Result<EventHits, StorageError> {
        if event_query.limit == 0 || !schema_exists(&self.connection, &self.path)? {
            return Ok(EventHits::default());
        }
        let mut parameters = Vec::new();
        let mut filters = String::new();
        let mut bind = |value: Value| {
            parameters.push(value);
            parameters.len()
        };
        if let Some(scope) = &event_query.scope {
            let scope = bind(Value::Text(scope.clone()));
            filters.push_str(&format!("AND s.scope = ?{scope}\n"));
        }
        if let Some(state_id) = &event_query.state_id {
            let state_id = bind(Value::Text(state_id.clone()));
            filters.push_str(&format!("AND e.session_id = ?{state_id}\n"));
        }
        if !event_query.kinds.is_empty()
            && event_query
                .kinds
                .iter()
                .all(|kind| PROMPT_KINDS.contains(&kind.as_str()))
        {
            filters.push_str("AND e.prompt_text IS NOT NULL\n");
        }
        if !event_query.text.is_empty() {
            let matches = event_query
                .text
                .iter()
                .map(|pattern| {
                    let pattern = bind(Value::Text(pattern.clone()));
                    format!("instr(lower(COALESCE(e.prompt_text, m.text)), lower(?{pattern})) > 0")
                })
                .collect::<Vec<_>>()
                .join(" OR ");
            filters.push_str(&format!("AND ({matches})\n"));
        }
        let mut statement = self
            .connection
            .prepare(&format!(
                "SELECT e.session_id, e.seq, e.record_json FROM events e\n\
                 JOIN durable_summaries s ON s.state_id = e.session_id AND s.live = 1\n\
                 LEFT JOIN conversation_messages m ON m.event_id = e.event_id\n\
                 WHERE e.seq IS NOT NULL\n\
                 {filters}"
            ))
            .map_err(|source| query(&self.path, source))?;
        let mut rows = statement
            .query(params_from_iter(parameters))
            .map_err(|source| query(&self.path, source))?;
        let mut cursors = Vec::new();
        while let Some(row) = rows.next().map_err(|source| query(&self.path, source))? {
            let encoded = row
                .get_ref(2)
                .and_then(|value| value.as_blob().map_err(Into::into))
                .map_err(|source| query(&self.path, source))?;
            let record = decode_record(encoded)?;
            if !matches_kind(&event_query.kinds, record.kind().as_str()) {
                continue;
            }
            let cursor = EventCursor {
                at_ms: record.recorded_at_unix_ms(),
                state_id: row.get(0).map_err(|source| query(&self.path, source))?,
                seq: from_sql_u64(row.get(1).map_err(|source| query(&self.path, source))?),
            };
            if event_query
                .after
                .as_ref()
                .is_none_or(|after| cursor < *after)
            {
                cursors.push(cursor);
            }
        }
        cursors.sort_unstable_by(|left, right| right.cmp(left));
        let more = cursors.len() > event_query.limit;
        cursors.truncate(event_query.limit);
        let mut statement = self
            .connection
            .prepare(&format!(
                "SELECT {EVENT_COLUMNS} FROM events e\n\
                 LEFT JOIN conversation_messages m ON m.event_id = e.event_id\n\
                 WHERE e.session_id = ?1 AND e.seq = ?2"
            ))
            .map_err(|source| query(&self.path, source))?;
        let mut hits = Vec::with_capacity(cursors.len());
        for cursor in &cursors {
            let mut rows = statement
                .query(params![cursor.state_id, to_sql_u64(cursor.seq)])
                .map_err(|source| query(&self.path, source))?;
            let row = rows
                .next()
                .map_err(|source| query(&self.path, source))?
                .ok_or_else(|| query(&self.path, rusqlite::Error::QueryReturnedNoRows))?;
            hits.push(EventHit {
                state_id: cursor.state_id.clone(),
                event: stored_event(&self.path, row)?,
            });
        }
        Ok(EventHits {
            hits,
            next: more.then(|| cursors.pop()).flatten(),
        })
    }
}

/// The columns [`stored_event`] decodes, for `events e` joined to `conversation_messages m`.
const EVENT_COLUMNS: &str = "e.seq, e.revision, e.record_json, COALESCE(e.prompt_text, m.text)";

const PROMPT_KINDS: [&str; 2] = [
    LocalKind::UserSubmitted.as_str(),
    LocalKind::UserSteered.as_str(),
];

fn stored_event(path: &Path, row: &Row<'_>) -> Result<StoredEvent, StorageError> {
    let column = |source| query(path, source);
    let body = String::from_utf8(row.get::<_, Vec<u8>>(2).map_err(column)?)
        .map_err(|error| StorageError::Decode(io::Error::new(io::ErrorKind::InvalidData, error)))?;
    let record = decode_record(body.as_bytes())?;
    Ok(StoredEvent {
        seq: from_sql_u64(row.get(0).map_err(column)?),
        revision: row
            .get::<_, Option<i64>>(1)
            .map_err(column)?
            .map_or(0, from_sql_u64),
        event: StoreEvent {
            kind: record.kind().as_str().to_owned(),
            at_ms: record.recorded_at_unix_ms(),
            body,
            text: row.get(3).map_err(column)?,
        },
    })
}

fn matches_kind(kinds: &[String], kind: &str) -> bool {
    kinds.is_empty() || kinds.iter().any(|candidate| candidate == kind)
}
