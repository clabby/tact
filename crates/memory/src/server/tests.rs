use super::{
    Credential, MAX_JSON_BODY_BYTES, MemoryServer, ServerBuildError,
    protocol::{
        self, DeleteRequest, ErrorResponse, ExportCursor, ExportRequest, ExportResponse,
        ListResponse, PutRequest, PutResponse, ReadRequest, ReadResponse, RemoteErrorCode,
        RemoteRole, ScanRequest, ScanResponse, SessionResponse, SyncReport, SyncRequest,
    },
};
use crate::{
    MemoryCandidate, MemoryError, MemoryKey, MemoryLimits, MemoryRecord, MemoryScan, MemoryStore,
    RemoteClientError, RemoteMemoryClient, RemoteToken, model::normalize_identity,
};
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    http::{Request, Response, StatusCode, header},
    response::IntoResponse,
    routing::post,
};
use serde::{Serialize, de::DeserializeOwned};
use std::{
    collections::{BTreeMap, HashSet},
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Notify, Semaphore};
use tower::ServiceExt;

const ALICE_TOKEN: &str = "alice-test-token-000000000001";
const BOB_TOKEN: &str = "bob-test-token-00000000000002";
const READER_TOKEN: &str = "reader-test-token-00000000004";

fn credential(namespace: &str, role: RemoteRole, token: &str) -> Credential {
    Credential::new(namespace.to_owned(), role, token.to_owned()).unwrap()
}

#[derive(Clone, Default)]
struct TestMemoryDatabase {
    state: Arc<Mutex<TestMemoryState>>,
}

#[derive(Default)]
struct TestMemoryState {
    records: BTreeMap<(String, i64), MemoryRecord>,
    next_ids: BTreeMap<String, i64>,
}

#[derive(Clone)]
struct TestMemoryStore {
    database: TestMemoryDatabase,
    namespace: String,
}

impl TestMemoryDatabase {
    fn bind(&self, namespace: String) -> TestMemoryStore {
        TestMemoryStore {
            database: self.clone(),
            namespace,
        }
    }
}

fn memory_app(credentials: Vec<Credential>) -> Router {
    let database = TestMemoryDatabase::default();
    MemoryServer::new(move |namespace| database.bind(namespace), credentials)
        .unwrap()
        .router()
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

fn prune_expired(state: &mut TestMemoryState, now: i64) {
    state.records.retain(|_, record| {
        !record
            .probation_until_ms
            .is_some_and(|expiry| expiry <= now && record.use_count == 0)
    });
}

fn visible_records(state: &TestMemoryState) -> Vec<MemoryRecord> {
    state.records.values().cloned().collect()
}

impl MemoryStore for TestMemoryStore {
    async fn scan(&self, query: &str, limit: usize) -> Result<MemoryScan, MemoryError> {
        let now = now_ms();
        let query = normalize_identity(query);
        let terms = query.split_whitespace().collect::<Vec<_>>();
        let mut state = self.database.state.lock().unwrap();
        prune_expired(&mut state, now);
        let mut seen = HashSet::new();
        let mut candidates = visible_records(&state)
            .into_iter()
            .filter(|record| {
                let content = normalize_identity(&record.content);
                terms.iter().all(|term| content.contains(term))
                    && seen.insert(normalize_identity(&record.content))
            })
            .map(|record| MemoryCandidate {
                key: record.key,
                preview: record.content,
                score: 1.0,
            })
            .take(limit.min(MemoryLimits::PRODUCTION.scan_results))
            .collect::<Vec<_>>();
        candidates.sort_by(|left, right| {
            left.key
                .namespace
                .cmp(&right.key.namespace)
                .then_with(|| left.key.id.cmp(&right.key.id))
        });
        for candidate in &candidates {
            if let Some(record) = state
                .records
                .get_mut(&(candidate.key.namespace.clone().unwrap(), candidate.key.id))
            {
                record.last_scanned_at_ms = Some(now);
                record.scan_count = record.scan_count.saturating_add(1);
            }
        }
        Ok(MemoryScan {
            abstained: candidates.is_empty(),
            candidates,
        })
    }

    async fn read(
        &self,
        ids: &[i64],
        keys: &[MemoryKey],
    ) -> Result<Vec<MemoryRecord>, MemoryError> {
        let now = now_ms();
        let mut state = self.database.state.lock().unwrap();
        prune_expired(&mut state, now);
        let references = ids
            .iter()
            .map(|id| (self.namespace.clone(), *id, None))
            .chain(keys.iter().filter_map(|key| {
                key.namespace
                    .clone()
                    .map(|namespace| (namespace, key.id, Some(key.version)))
            }));
        let mut seen = HashSet::new();
        let mut records = Vec::new();
        for (namespace, id, version) in references {
            let Some(record) = state.records.get_mut(&(namespace, id)) else {
                continue;
            };
            if version.is_some_and(|version| version != record.key.version)
                || !seen.insert(normalize_identity(&record.content))
            {
                continue;
            }
            record.last_used_at_ms = Some(now);
            record.use_count = record.use_count.saturating_add(1);
            record.probation_until_ms = None;
            records.push(record.clone());
        }
        Ok(records)
    }

    async fn list(&self) -> Result<Vec<MemoryRecord>, MemoryError> {
        let mut state = self.database.state.lock().unwrap();
        prune_expired(&mut state, now_ms());
        Ok(visible_records(&state)
            .into_iter()
            .take(MemoryLimits::PRODUCTION.records)
            .collect())
    }

    async fn put(
        &self,
        content: &str,
        replacement: Option<MemoryKey>,
    ) -> Result<MemoryRecord, MemoryError> {
        let identity = normalize_identity(content);
        if identity.is_empty() {
            return Err(MemoryError::EmptyContent);
        }
        let now = now_ms();
        let mut state = self.database.state.lock().unwrap();
        prune_expired(&mut state, now);
        if state.records.values().any(|record| {
            record.key.namespace.as_deref() == Some(&self.namespace)
                && normalize_identity(&record.content) == identity
                && replacement
                    .as_ref()
                    .is_none_or(|key| key.id != record.key.id)
        }) {
            return Err(MemoryError::Duplicate);
        }

        let namespace_records = state
            .records
            .values()
            .filter(|record| record.key.namespace.as_deref() == Some(&self.namespace))
            .collect::<Vec<_>>();
        let replacing_bytes = replacement
            .as_ref()
            .and_then(|key| state.records.get(&(self.namespace.clone(), key.id)))
            .map_or(0, |record| record.content.len());
        let total_bytes = namespace_records
            .iter()
            .map(|record| record.content.len())
            .sum::<usize>()
            .saturating_sub(replacing_bytes)
            .saturating_add(content.len());
        if total_bytes > MemoryLimits::PRODUCTION.total_content_bytes {
            return Err(MemoryError::ContentCapacity {
                maximum_bytes: MemoryLimits::PRODUCTION.total_content_bytes,
            });
        }

        let (id, version, created_at_ms) = if let Some(key) = replacement {
            if key.namespace.as_deref() != Some(&self.namespace) {
                return Err(MemoryError::RemoteReadOnly);
            }
            let existing = state
                .records
                .get(&(self.namespace.clone(), key.id))
                .ok_or(MemoryError::NotFound)?;
            if existing.key.version != key.version {
                return Err(MemoryError::Conflict);
            }
            (key.id, key.version + 1, existing.created_at_ms)
        } else {
            if namespace_records.len() >= MemoryLimits::PRODUCTION.records {
                return Err(MemoryError::RecordCapacity {
                    maximum: MemoryLimits::PRODUCTION.records,
                });
            }
            let next = state.next_ids.entry(self.namespace.clone()).or_insert(1);
            let id = *next;
            *next = next.saturating_add(1);
            (id, 1, now)
        };
        let memory = MemoryRecord {
            key: MemoryKey::remote(self.namespace.clone(), id, version),
            content: content.to_owned(),
            created_at_ms,
            updated_at_ms: now,
            last_scanned_at_ms: None,
            scan_count: 0,
            last_used_at_ms: None,
            use_count: 0,
            probation_until_ms: Some(
                now.saturating_add(MemoryLimits::PRODUCTION.probation_duration_ms),
            ),
        };
        state
            .records
            .insert((self.namespace.clone(), id), memory.clone());
        Ok(memory)
    }

    async fn delete(&self, key: MemoryKey) -> Result<(), MemoryError> {
        if key.namespace.as_deref() != Some(&self.namespace) {
            return Err(MemoryError::RemoteReadOnly);
        }
        let mut state = self.database.state.lock().unwrap();
        let index = (self.namespace.clone(), key.id);
        if let Some(record) = state.records.get(&index)
            && record.key.version != key.version
        {
            return Err(MemoryError::Conflict);
        }
        state.records.remove(&index);
        Ok(())
    }

    async fn sync(&self, memories: &[MemoryRecord]) -> Result<SyncReport, MemoryError> {
        let mut identities = HashSet::new();
        if memories.len() > MemoryLimits::PRODUCTION.records {
            return Err(MemoryError::RecordCapacity {
                maximum: MemoryLimits::PRODUCTION.records,
            });
        }
        let total_bytes = memories.iter().try_fold(0usize, |total, memory| {
            if memory.key.id <= 0
                || memory.key.version == 0
                || !identities.insert(normalize_identity(&memory.content))
            {
                return Err(MemoryError::Conflict);
            }
            total
                .checked_add(memory.content.len())
                .ok_or(MemoryError::ContentCapacity {
                    maximum_bytes: MemoryLimits::PRODUCTION.total_content_bytes,
                })
        })?;
        if total_bytes > MemoryLimits::PRODUCTION.total_content_bytes {
            return Err(MemoryError::ContentCapacity {
                maximum_bytes: MemoryLimits::PRODUCTION.total_content_bytes,
            });
        }

        let mut state = self.database.state.lock().unwrap();
        let existing = state
            .records
            .iter()
            .filter(|((namespace, _), _)| namespace == &self.namespace)
            .map(|((_, id), record)| (*id, record.clone()))
            .collect::<BTreeMap<_, _>>();
        let incoming_ids = memories
            .iter()
            .map(|memory| memory.key.id)
            .collect::<HashSet<_>>();
        let mut report = SyncReport {
            deleted: existing
                .keys()
                .filter(|id| !incoming_ids.contains(id))
                .count(),
            ..SyncReport::default()
        };
        state
            .records
            .retain(|(namespace, _), _| namespace != &self.namespace);
        for memory in memories {
            let mut memory = memory.clone();
            memory.key.namespace = Some(self.namespace.clone());
            match existing.get(&memory.key.id) {
                None => report.inserted += 1,
                Some(previous) if previous == &memory => report.unchanged += 1,
                Some(_) => report.replaced += 1,
            }
            state
                .records
                .insert((self.namespace.clone(), memory.key.id), memory);
        }
        let next = memories
            .iter()
            .map(|memory| memory.key.id)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        state
            .next_ids
            .entry(self.namespace.clone())
            .and_modify(|current| *current = (*current).max(next))
            .or_insert(next.max(1));
        Ok(report)
    }

    async fn export_page(
        &self,
        namespaces: Option<&[String]>,
        cursor: Option<&ExportCursor>,
        limit: usize,
    ) -> Result<(Vec<MemoryRecord>, Option<ExportCursor>), MemoryError> {
        let mut state = self.database.state.lock().unwrap();
        prune_expired(&mut state, now_ms());
        let selected = namespaces.map(|values| values.iter().collect::<HashSet<_>>());
        let mut records = state
            .records
            .iter()
            .filter(|((namespace, id), _)| {
                selected
                    .as_ref()
                    .is_none_or(|selected| selected.contains(namespace))
                    && cursor.is_none_or(|cursor| {
                        (namespace.as_str(), *id) > (cursor.namespace.as_str(), cursor.id)
                    })
            })
            .map(|(_, record)| record.clone())
            .collect::<Vec<_>>();
        let limit = limit.min(protocol::MAX_EXPORT_PAGE_RECORDS);
        let has_more = records.len() > limit;
        records.truncate(limit);
        let next_cursor = has_more.then(|| {
            let key = &records.last().unwrap().key;
            ExportCursor {
                namespace: key.namespace.clone().unwrap(),
                id: key.id,
            }
        });
        Ok((records, next_cursor))
    }
}

fn record(id: i64, version: u64, content: &str) -> MemoryRecord {
    MemoryRecord {
        key: MemoryKey::local(id, version),
        content: content.to_owned(),
        created_at_ms: 10,
        updated_at_ms: 10 + i64::try_from(version).unwrap(),
        last_scanned_at_ms: None,
        scan_count: 0,
        last_used_at_ms: None,
        use_count: 0,
        probation_until_ms: None,
    }
}

fn request<T: Serialize>(
    method: &str,
    path: &str,
    token: Option<&str>,
    namespace: Option<&str>,
    body: Option<&T>,
) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(format!("/{path}"));
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    if let Some(namespace) = namespace {
        builder = builder.header(protocol::NAMESPACE_HEADER, namespace);
    }
    let body = body
        .map(|body| Body::from(serde_json::to_vec(body).unwrap()))
        .unwrap_or_else(Body::empty);
    builder
        .header(header::CONTENT_TYPE, "application/json")
        .body(body)
        .unwrap()
}

async fn send<T: Serialize>(
    app: &Router,
    path: &str,
    token: &str,
    namespace: &str,
    body: &T,
) -> Response<Body> {
    app.clone()
        .oneshot(request(
            "POST",
            path,
            Some(token),
            Some(namespace),
            Some(body),
        ))
        .await
        .unwrap()
}

async fn json<T: DeserializeOwned>(response: Response<Body>) -> T {
    serde_json::from_slice(&response_bytes(response).await).unwrap()
}

async fn response_bytes(response: Response<Body>) -> Vec<u8> {
    to_bytes(response.into_body(), MAX_JSON_BODY_BYTES)
        .await
        .unwrap()
        .to_vec()
}

async fn put(app: &Router, namespace: &str, token: &str, content: &str) -> MemoryRecord {
    let response = send(
        app,
        protocol::PUT_PATH,
        token,
        namespace,
        &PutRequest {
            content: content.to_owned(),
            replacement: None,
        },
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    json::<PutResponse>(response).await.memory
}

#[tokio::test]
async fn authentication_namespace_and_role_are_enforced() {
    let app = memory_app(vec![
        credential("alice", RemoteRole::Writer, ALICE_TOKEN),
        credential("reader", RemoteRole::Reader, READER_TOKEN),
    ]);

    let missing = app
        .clone()
        .oneshot(request::<()>(
            "GET",
            protocol::SESSION_PATH,
            None,
            Some("alice"),
            None,
        ))
        .await
        .unwrap();
    assert_error(
        missing,
        StatusCode::UNAUTHORIZED,
        RemoteErrorCode::Unauthorized,
    )
    .await;

    let mismatch = app
        .clone()
        .oneshot(request::<()>(
            "GET",
            protocol::SESSION_PATH,
            Some(ALICE_TOKEN),
            Some("reader"),
            None,
        ))
        .await
        .unwrap();
    assert_error(
        mismatch,
        StatusCode::FORBIDDEN,
        RemoteErrorCode::NamespaceMismatch,
    )
    .await;

    let session = app
        .clone()
        .oneshot(request::<()>(
            "GET",
            protocol::SESSION_PATH,
            Some(ALICE_TOKEN),
            Some("alice"),
            None,
        ))
        .await
        .unwrap();
    let session = json::<SessionResponse>(session).await;
    assert_eq!(session.namespace, "alice");
    assert_eq!(session.role, RemoteRole::Writer);

    let denied = send(
        &app,
        protocol::PUT_PATH,
        READER_TOKEN,
        "reader",
        &PutRequest {
            content: "reader cannot write".to_owned(),
            replacement: None,
        },
    )
    .await;
    assert_error(denied, StatusCode::FORBIDDEN, RemoteErrorCode::Forbidden).await;
    let denied = send(
        &app,
        protocol::DELETE_PATH,
        READER_TOKEN,
        "reader",
        &DeleteRequest {
            key: MemoryKey::remote("reader".to_owned(), 1, 1),
        },
    )
    .await;
    assert_error(denied, StatusCode::FORBIDDEN, RemoteErrorCode::Forbidden).await;
    let denied = send(
        &app,
        protocol::SYNC_PATH,
        READER_TOKEN,
        "reader",
        &SyncRequest {
            memories: Vec::new(),
        },
    )
    .await;
    assert_error(denied, StatusCode::FORBIDDEN, RemoteErrorCode::Forbidden).await;
}

#[test]
fn duplicate_bearer_tokens_are_rejected_without_exposing_tokens() {
    let result = MemoryServer::new(
        |namespace| TestMemoryDatabase::default().bind(namespace),
        [
            credential("alice", RemoteRole::Writer, ALICE_TOKEN),
            credential("bob", RemoteRole::Reader, ALICE_TOKEN),
        ],
    );
    assert!(matches!(
        result,
        Err(ServerBuildError::DuplicateBearerToken)
    ));
    let diagnostic = format!("{:?}", result.err().unwrap());
    assert!(!diagnostic.contains(ALICE_TOKEN));
}

#[tokio::test]
async fn scan_read_and_list_return_only_caller_visible_records() {
    let app = memory_app(vec![
        credential("alice", RemoteRole::Writer, ALICE_TOKEN),
        credential("bob", RemoteRole::Writer, BOB_TOKEN),
    ]);
    let alice = put(&app, "alice", ALICE_TOKEN, "alice private indexing note").await;
    let bob = put(&app, "bob", BOB_TOKEN, "bob visible concurrent sqlite note").await;

    let listed = send(&app, protocol::LIST_PATH, ALICE_TOKEN, "alice", &()).await;
    let listed = json::<ListResponse>(listed).await.memories;
    assert_eq!(
        listed.iter().map(|memory| &memory.key).collect::<Vec<_>>(),
        [&alice.key, &bob.key]
    );

    let scanned = send(
        &app,
        protocol::SCAN_PATH,
        ALICE_TOKEN,
        "alice",
        &ScanRequest {
            query: "concurrent sqlite".to_owned(),
            limit: 5,
        },
    )
    .await;
    let scanned = json::<ScanResponse>(scanned).await.candidates;
    assert_eq!(scanned.len(), 1);
    assert_eq!(scanned[0].key, bob.key);

    let read = send(
        &app,
        protocol::READ_PATH,
        ALICE_TOKEN,
        "alice",
        &ReadRequest {
            ids: vec![alice.key.id],
            keys: vec![bob.key.clone()],
        },
    )
    .await;
    let read = json::<ReadResponse>(read).await.memories;
    assert_eq!(read.len(), 2);
    assert!(read.iter().any(|memory| memory.key == alice.key));
    assert!(read.iter().any(|memory| memory.key == bob.key));
}

#[tokio::test]
async fn put_replace_and_delete_are_server_authored() {
    let app = memory_app(vec![credential("alice", RemoteRole::Writer, ALICE_TOKEN)]);
    let inserted = put(&app, "alice", ALICE_TOKEN, "initial server-authored note").await;
    assert_eq!(inserted.key.namespace.as_deref(), Some("alice"));
    assert_eq!(inserted.key.version, 1);

    let replaced = send(
        &app,
        protocol::PUT_PATH,
        ALICE_TOKEN,
        "alice",
        &PutRequest {
            content: "replacement server-authored note".to_owned(),
            replacement: Some(inserted.key.clone()),
        },
    )
    .await;
    let replaced = json::<PutResponse>(replaced).await.memory;
    assert_eq!(replaced.key.id, inserted.key.id);
    assert_eq!(replaced.key.version, inserted.key.version + 1);

    let deleted = send(
        &app,
        protocol::DELETE_PATH,
        ALICE_TOKEN,
        "alice",
        &DeleteRequest {
            key: replaced.key.clone(),
        },
    )
    .await;
    assert_eq!(deleted.status(), StatusCode::OK);
    let read = send(
        &app,
        protocol::READ_PATH,
        ALICE_TOKEN,
        "alice",
        &ReadRequest {
            ids: vec![replaced.key.id],
            keys: Vec::new(),
        },
    )
    .await;
    assert!(json::<ReadResponse>(read).await.memories.is_empty());
}

#[tokio::test]
async fn foreign_keys_cannot_be_mutated() {
    let app = memory_app(vec![
        credential("alice", RemoteRole::Writer, ALICE_TOKEN),
        credential("bob", RemoteRole::Writer, BOB_TOKEN),
    ]);
    let alice = put(&app, "alice", ALICE_TOKEN, "alice owns this note").await;
    let local_shaped_delete = send(
        &app,
        protocol::DELETE_PATH,
        ALICE_TOKEN,
        "alice",
        &DeleteRequest {
            key: MemoryKey::local(alice.key.id, alice.key.version),
        },
    )
    .await;
    assert_error(
        local_shaped_delete,
        StatusCode::FORBIDDEN,
        RemoteErrorCode::Forbidden,
    )
    .await;

    let bob = put(&app, "bob", BOB_TOKEN, "bob owns this note").await;

    let replace = send(
        &app,
        protocol::PUT_PATH,
        ALICE_TOKEN,
        "alice",
        &PutRequest {
            content: "alice cannot replace bob".to_owned(),
            replacement: Some(bob.key.clone()),
        },
    )
    .await;
    assert_error(replace, StatusCode::FORBIDDEN, RemoteErrorCode::Forbidden).await;

    let delete = send(
        &app,
        protocol::DELETE_PATH,
        ALICE_TOKEN,
        "alice",
        &DeleteRequest { key: bob.key },
    )
    .await;
    assert_error(delete, StatusCode::FORBIDDEN, RemoteErrorCode::Forbidden).await;
}

#[tokio::test]
async fn concurrent_puts_allocate_distinct_monotonic_ids() {
    let app = memory_app(vec![credential("alice", RemoteRole::Writer, ALICE_TOKEN)]);
    let seed = put(&app, "alice", ALICE_TOKEN, "initialize concurrent database").await;
    assert_eq!(seed.key.id, 1);
    let mut tasks = Vec::new();
    for index in 0..16 {
        let app = app.clone();
        tasks.push(tokio::spawn(async move {
            put(
                &app,
                "alice",
                ALICE_TOKEN,
                &format!("concurrent note number {index}"),
            )
            .await
            .key
            .id
        }));
    }
    let mut ids = Vec::new();
    for task in tasks {
        ids.push(task.await.unwrap());
    }
    ids.sort_unstable();
    assert_eq!(ids, (2..=17).collect::<Vec<_>>());
}

#[tokio::test]
async fn sync_replaces_the_callers_complete_snapshot() {
    let app = memory_app(vec![credential("alice", RemoteRole::Writer, ALICE_TOKEN)]);
    let first = SyncRequest {
        memories: vec![
            record(10, 1, "first snapshot note"),
            record(20, 1, "second snapshot note"),
        ],
    };
    let report =
        json::<SyncReport>(send(&app, protocol::SYNC_PATH, ALICE_TOKEN, "alice", &first).await)
            .await;
    assert_eq!(
        report,
        SyncReport {
            inserted: 2,
            ..SyncReport::default()
        }
    );

    let second = SyncRequest {
        memories: vec![
            record(20, 2, "second snapshot revised"),
            record(30, 1, "third snapshot note"),
        ],
    };
    let report =
        json::<SyncReport>(send(&app, protocol::SYNC_PATH, ALICE_TOKEN, "alice", &second).await)
            .await;
    assert_eq!(report.inserted, 1);
    assert_eq!(report.replaced, 1);
    assert_eq!(report.deleted, 1);

    let exported = export_page(
        &app,
        ALICE_TOKEN,
        "alice",
        Some(vec!["alice".to_owned()]),
        None,
        10,
    )
    .await;
    assert_eq!(
        exported
            .memories
            .iter()
            .map(|memory| memory.key.id)
            .collect::<Vec<_>>(),
        [20, 30]
    );
}

#[tokio::test]
async fn sync_rejects_duplicate_snapshot_ids_without_mutation() {
    let app = memory_app(vec![credential("alice", RemoteRole::Writer, ALICE_TOKEN)]);
    let response = send(
        &app,
        protocol::SYNC_PATH,
        ALICE_TOKEN,
        "alice",
        &SyncRequest {
            memories: vec![record(10, 1, "first value"), record(10, 2, "second value")],
        },
    )
    .await;
    assert_error(
        response,
        StatusCode::BAD_REQUEST,
        RemoteErrorCode::BadRequest,
    )
    .await;

    let exported = export_page(
        &app,
        ALICE_TOKEN,
        "alice",
        Some(vec!["alice".to_owned()]),
        None,
        10,
    )
    .await;
    assert!(exported.memories.is_empty());
}

#[tokio::test]
async fn export_paginates_all_or_selected_without_visibility_filtering_or_deduplication() {
    let app = memory_app(vec![
        credential("alice", RemoteRole::Writer, ALICE_TOKEN),
        credential("bob", RemoteRole::Writer, BOB_TOKEN),
    ]);
    put(&app, "alice", ALICE_TOKEN, "same normalized content").await;
    put(&app, "alice", ALICE_TOKEN, "alice second export note").await;
    put(&app, "bob", BOB_TOKEN, "same normalized content").await;

    let mut cursor = None;
    let mut all = Vec::new();
    loop {
        let page = export_page(&app, ALICE_TOKEN, "alice", None, cursor, 1).await;
        all.extend(page.memories);
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(all.len(), 3);
    assert_eq!(
        all.iter()
            .filter(|memory| memory.content == "same normalized content")
            .count(),
        2
    );
    assert_eq!(all[0].key.namespace.as_deref(), Some("alice"));

    let selected = export_page(
        &app,
        ALICE_TOKEN,
        "alice",
        Some(vec!["bob".to_owned()]),
        None,
        10,
    )
    .await;
    assert_eq!(selected.memories.len(), 1);
    assert_eq!(selected.memories[0].key.namespace.as_deref(), Some("bob"));
}

async fn export_page(
    app: &Router,
    token: &str,
    namespace: &str,
    namespaces: Option<Vec<String>>,
    cursor: Option<ExportCursor>,
    limit: usize,
) -> ExportResponse {
    json(
        send(
            app,
            protocol::EXPORT_PATH,
            token,
            namespace,
            &ExportRequest {
                namespaces,
                cursor,
                limit,
            },
        )
        .await,
    )
    .await
}

#[tokio::test]
async fn body_and_request_bounds_are_content_free_client_errors() {
    let app = memory_app(vec![credential("alice", RemoteRole::Writer, ALICE_TOKEN)]);

    let cases = [
        send(
            &app,
            protocol::SCAN_PATH,
            ALICE_TOKEN,
            "alice",
            &ScanRequest {
                query: "q".repeat(MemoryLimits::PRODUCTION.query_bytes + 1),
                limit: 1,
            },
        )
        .await,
        send(
            &app,
            protocol::PUT_PATH,
            ALICE_TOKEN,
            "alice",
            &PutRequest {
                content: "c".repeat(MemoryLimits::PRODUCTION.content_bytes + 1),
                replacement: None,
            },
        )
        .await,
    ];
    let [query_error, content_error] = cases;
    assert_error(
        query_error,
        StatusCode::PAYLOAD_TOO_LARGE,
        RemoteErrorCode::QueryTooLarge,
    )
    .await;
    assert_error(
        content_error,
        StatusCode::PAYLOAD_TOO_LARGE,
        RemoteErrorCode::ContentTooLarge,
    )
    .await;

    let invalid_limit = send(
        &app,
        protocol::SCAN_PATH,
        ALICE_TOKEN,
        "alice",
        &ScanRequest {
            query: "bounded".to_owned(),
            limit: MemoryLimits::PRODUCTION.scan_results + 1,
        },
    )
    .await;
    assert_error(
        invalid_limit,
        StatusCode::BAD_REQUEST,
        RemoteErrorCode::BadRequest,
    )
    .await;

    let invalid_export = send(
        &app,
        protocol::EXPORT_PATH,
        ALICE_TOKEN,
        "alice",
        &ExportRequest {
            namespaces: None,
            cursor: None,
            limit: 0,
        },
    )
    .await;
    assert_error(
        invalid_export,
        StatusCode::BAD_REQUEST,
        RemoteErrorCode::BadRequest,
    )
    .await;

    let too_many = send(
        &app,
        protocol::READ_PATH,
        ALICE_TOKEN,
        "alice",
        &ReadRequest {
            ids: (1..=i64::try_from(MemoryLimits::PRODUCTION.records + 1).unwrap()).collect(),
            keys: Vec::new(),
        },
    )
    .await;
    assert_error(
        too_many,
        StatusCode::BAD_REQUEST,
        RemoteErrorCode::BadRequest,
    )
    .await;

    put(&app, "alice", ALICE_TOKEN, "duplicate response marker").await;
    let duplicate = send(
        &app,
        protocol::PUT_PATH,
        ALICE_TOKEN,
        "alice",
        &PutRequest {
            content: "duplicate response marker".to_owned(),
            replacement: None,
        },
    )
    .await;
    assert_error(duplicate, StatusCode::CONFLICT, RemoteErrorCode::Duplicate).await;

    let oversized = Request::builder()
        .method("POST")
        .uri(format!("/{}", protocol::PUT_PATH))
        .header(header::AUTHORIZATION, format!("Bearer {ALICE_TOKEN}"))
        .header(protocol::NAMESPACE_HEADER, "alice")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(vec![b'x'; MAX_JSON_BODY_BYTES + 1]))
        .unwrap();
    let oversized = app.oneshot(oversized).await.unwrap();
    assert_error(
        oversized,
        StatusCode::PAYLOAD_TOO_LARGE,
        RemoteErrorCode::BadRequest,
    )
    .await;
}

async fn assert_error(response: Response<Body>, status: StatusCode, code: RemoteErrorCode) {
    assert_eq!(response.status(), status);
    let bytes = response_bytes(response).await;
    let decoded: ErrorResponse = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(decoded.code, code);
    assert_eq!(bytes, serde_json::to_vec(&ErrorResponse { code }).unwrap());
}

#[derive(Clone)]
struct RetryState {
    list_calls: Arc<AtomicUsize>,
    put_calls: Arc<AtomicUsize>,
}

async fn retrying_list(
    axum::extract::State(state): axum::extract::State<RetryState>,
) -> Response<Body> {
    if state.list_calls.fetch_add(1, Ordering::SeqCst) == 0 {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                code: RemoteErrorCode::Unavailable,
            }),
        )
            .into_response();
    }
    Json(ListResponse {
        memories: Vec::new(),
    })
    .into_response()
}

#[derive(Clone, Default)]
struct ConsistencyTokenState {
    requests: Arc<Mutex<Vec<Option<String>>>>,
}

async fn consistent_list(
    axum::extract::State(state): axum::extract::State<ConsistencyTokenState>,
    headers: axum::http::HeaderMap,
) -> Response<Body> {
    let token = headers
        .get(protocol::CONSISTENCY_TOKEN_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    state.requests.lock().unwrap().push(token.clone());

    tokio::time::sleep(Duration::from_millis(50)).await;
    let response_token = match token.as_deref() {
        None => "token-1",
        Some("token-1") => "token-2",
        Some("token-2") => "token-3",
        _ => return StatusCode::CONFLICT.into_response(),
    };
    let mut response = Json(ListResponse {
        memories: Vec::new(),
    })
    .into_response();
    response.headers_mut().insert(
        protocol::CONSISTENCY_TOKEN_HEADER,
        response_token.parse().unwrap(),
    );
    response
}

async fn unavailable_put(
    axum::extract::State(state): axum::extract::State<RetryState>,
) -> Response<Body> {
    state.put_calls.fetch_add(1, Ordering::SeqCst);
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(ErrorResponse {
            code: RemoteErrorCode::Unavailable,
        }),
    )
        .into_response()
}

async fn rate_limited() -> StatusCode {
    StatusCode::TOO_MANY_REQUESTS
}

async fn oversized_list() -> Json<ListResponse> {
    Json(ListResponse {
        memories: (1..=MemoryLimits::PRODUCTION.records + 1)
            .map(|id| {
                let mut memory = record(id as i64, 1, "visible");
                memory.key = MemoryKey::remote("alice".to_owned(), id as i64, 1);
                memory
            })
            .collect(),
    })
}

async fn unsafe_scan() -> Json<ScanResponse> {
    Json(ScanResponse {
        candidates: vec![MemoryCandidate {
            key: MemoryKey::remote("alice".to_owned(), 1, 1),
            preview: "password=hunter2".to_owned(),
            score: 1.0,
        }],
    })
}

async fn oversized_scan() -> Json<ScanResponse> {
    Json(ScanResponse {
        candidates: (1..=2)
            .map(|id| MemoryCandidate {
                key: MemoryKey::remote("alice".to_owned(), id, 1),
                preview: format!("candidate {id}"),
                score: 1.0,
            })
            .collect(),
    })
}

async fn ambiguous_version_scan() -> Json<ScanResponse> {
    Json(ScanResponse {
        candidates: (1..=2)
            .map(|version| MemoryCandidate {
                key: MemoryKey::remote("alice".to_owned(), 1, version),
                preview: format!("version {version}"),
                score: 1.0,
            })
            .collect(),
    })
}

async fn ascending_score_scan() -> Json<ScanResponse> {
    Json(ScanResponse {
        candidates: (1..=2)
            .map(|id| MemoryCandidate {
                key: MemoryKey::remote("alice".to_owned(), id, 1),
                preview: format!("candidate {id}"),
                score: id as f64,
            })
            .collect(),
    })
}

async fn oversized_export() -> Json<ExportResponse> {
    Json(ExportResponse {
        memories: (1..=2)
            .map(|id| {
                let mut memory = record(id, 1, &format!("memory {id}"));
                memory.key = MemoryKey::remote("alice".to_owned(), id, 1);
                memory
            })
            .collect(),
        next_cursor: None,
    })
}

async fn impossible_sync_report() -> Json<SyncReport> {
    Json(SyncReport {
        inserted: 2,
        replaced: 0,
        unchanged: 0,
        deleted: 0,
    })
}

async fn unrelated_put() -> Json<PutResponse> {
    let mut memory = record(2, 1, "different content");
    memory.key = MemoryKey::remote("alice".to_owned(), 2, 1);
    Json(PutResponse { memory })
}

async fn equivalent_content_read() -> Json<ReadResponse> {
    let mut alice = record(1, 1, "shared operating note");
    alice.key = MemoryKey::remote("alice".to_owned(), 1, 1);
    let mut bob = record(1, 1, "shared operating note");
    bob.key = MemoryKey::remote("bob".to_owned(), 1, 1);
    Json(ReadResponse {
        memories: vec![alice, bob],
    })
}

async fn ambiguous_version_read() -> Json<ReadResponse> {
    Json(ReadResponse {
        memories: (1..=3)
            .map(|version| {
                let mut memory = record(1, version, &format!("version {version}"));
                memory.key = MemoryKey::remote("alice".to_owned(), 1, version);
                memory
            })
            .collect(),
    })
}

async fn ambiguous_version_list() -> Json<ListResponse> {
    Json(ListResponse {
        memories: (1..=2)
            .map(|version| {
                let mut memory = record(1, version, &format!("version {version}"));
                memory.key = MemoryKey::remote("alice".to_owned(), 1, version);
                memory
            })
            .collect(),
    })
}

#[tokio::test]
async fn client_retries_safe_operations_but_does_not_replay_put_responses() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let state = RetryState {
        list_calls: Arc::new(AtomicUsize::new(0)),
        put_calls: Arc::new(AtomicUsize::new(0)),
    };
    let app = Router::new()
        .route(&format!("/{}", protocol::LIST_PATH), post(retrying_list))
        .route(&format!("/{}", protocol::PUT_PATH), post(unavailable_put))
        .with_state(state.clone());
    let (endpoint, task) = live_server(app).await;
    let client = RemoteMemoryClient::new(
        &endpoint,
        "alice".to_owned(),
        RemoteToken::new(ALICE_TOKEN.to_owned()).unwrap(),
    )
    .unwrap();

    assert!(client.list().await.unwrap().is_empty());
    assert_eq!(state.list_calls.load(Ordering::SeqCst), 2);
    let error = client.put("one-shot put", None).await.unwrap_err();
    let MemoryError::Unavailable { source } = error else {
        panic!("expected unavailable error, got {error:?}");
    };
    assert!(matches!(
        source.downcast_ref::<RemoteClientError>(),
        Some(RemoteClientError::Rejected {
            code: RemoteErrorCode::Unavailable
        })
    ));
    assert_eq!(state.put_calls.load(Ordering::SeqCst), 1);
    task.abort();
}

#[tokio::test]
async fn client_carries_consistency_tokens_across_concurrent_clones() {
    let state = ConsistencyTokenState::default();
    let app = Router::new()
        .route(&format!("/{}", protocol::LIST_PATH), post(consistent_list))
        .with_state(state.clone());
    let (endpoint, task) = live_server(app).await;
    let client = RemoteMemoryClient::new(
        &endpoint,
        "alice".to_owned(),
        RemoteToken::new(ALICE_TOKEN.to_owned()).unwrap(),
    )
    .unwrap();
    let clone = client.clone();

    let (first, second) = tokio::join!(client.list(), clone.list());
    assert!(first.unwrap().is_empty());
    assert!(second.unwrap().is_empty());
    assert!(client.list().await.unwrap().is_empty());
    assert_eq!(
        *state.requests.lock().unwrap(),
        vec![None, Some("token-1".to_owned()), Some("token-2".to_owned()),]
    );
    task.abort();
}

#[tokio::test]
async fn client_preserves_empty_content_and_exhausted_rate_limit_errors() {
    let client = RemoteMemoryClient::new(
        "http://127.0.0.1:1/",
        "alice".to_owned(),
        RemoteToken::new(ALICE_TOKEN.to_owned()).unwrap(),
    )
    .unwrap();
    assert!(matches!(
        client.put("   ", None).await,
        Err(MemoryError::EmptyContent)
    ));

    let app = Router::new().route(&format!("/{}", protocol::LIST_PATH), post(rate_limited));
    let (endpoint, task) = live_server(app).await;
    let client = RemoteMemoryClient::new(
        &endpoint,
        "alice".to_owned(),
        RemoteToken::new(ALICE_TOKEN.to_owned()).unwrap(),
    )
    .unwrap();
    assert!(matches!(
        client.list().await,
        Err(MemoryError::Unavailable { .. })
    ));
    task.abort();
}

#[tokio::test]
async fn client_reports_a_missing_versioned_session_route_as_incompatible() {
    let (endpoint, task) = live_server(Router::new()).await;
    let client = RemoteMemoryClient::new(
        &endpoint,
        "alice".to_owned(),
        RemoteToken::new(ALICE_TOKEN.to_owned()).unwrap(),
    )
    .unwrap();

    assert!(matches!(
        client.session().await,
        Err(RemoteClientError::IncompatibleProtocol)
    ));
    task.abort();
}

#[tokio::test]
async fn client_rejects_an_unbounded_list_window() {
    let app = Router::new().route(&format!("/{}", protocol::LIST_PATH), post(oversized_list));
    let (endpoint, task) = live_server(app).await;
    let client = RemoteMemoryClient::new(
        &endpoint,
        "alice".to_owned(),
        RemoteToken::new(ALICE_TOKEN.to_owned()).unwrap(),
    )
    .unwrap();

    let error = client.list().await.unwrap_err();
    let MemoryError::Backend { source } = error else {
        panic!("expected backend error, got {error:?}");
    };
    assert!(matches!(
        source.downcast_ref::<RemoteClientError>(),
        Some(RemoteClientError::InvalidResponse)
    ));
    task.abort();
}

#[tokio::test]
async fn client_suppresses_unsafe_scan_previews() {
    let app = Router::new().route(&format!("/{}", protocol::SCAN_PATH), post(unsafe_scan));
    let (endpoint, task) = live_server(app).await;
    let client = RemoteMemoryClient::new(
        &endpoint,
        "alice".to_owned(),
        RemoteToken::new(ALICE_TOKEN.to_owned()).unwrap(),
    )
    .unwrap();

    assert!(
        client
            .scan("password", 5)
            .await
            .unwrap()
            .candidates
            .is_empty()
    );
    task.abort();
}

#[tokio::test]
async fn client_rejects_oversized_scan_responses() {
    let app = Router::new().route(&format!("/{}", protocol::SCAN_PATH), post(oversized_scan));
    let (endpoint, task) = live_server(app).await;
    let client = RemoteMemoryClient::new(
        &endpoint,
        "alice".to_owned(),
        RemoteToken::new(ALICE_TOKEN.to_owned()).unwrap(),
    )
    .unwrap();

    let error = client.scan("candidate", 1).await.unwrap_err();
    let MemoryError::Backend { source } = error else {
        panic!("expected backend error, got {error:?}");
    };
    assert!(matches!(
        source.downcast_ref::<RemoteClientError>(),
        Some(RemoteClientError::InvalidResponse)
    ));
    task.abort();
}

#[tokio::test]
async fn client_rejects_ambiguous_versions_in_scan_responses() {
    let app = Router::new().route(
        &format!("/{}", protocol::SCAN_PATH),
        post(ambiguous_version_scan),
    );
    let (endpoint, task) = live_server(app).await;
    let client = RemoteMemoryClient::new(
        &endpoint,
        "alice".to_owned(),
        RemoteToken::new(ALICE_TOKEN.to_owned()).unwrap(),
    )
    .unwrap();

    let error = client.scan("version", 2).await.unwrap_err();
    let MemoryError::Backend { source } = error else {
        panic!("expected backend error, got {error:?}");
    };
    assert!(matches!(
        source.downcast_ref::<RemoteClientError>(),
        Some(RemoteClientError::InvalidResponse)
    ));
    task.abort();
}

#[tokio::test]
async fn client_rejects_scan_responses_out_of_rank_order() {
    let app = Router::new().route(
        &format!("/{}", protocol::SCAN_PATH),
        post(ascending_score_scan),
    );
    let (endpoint, task) = live_server(app).await;
    let client = RemoteMemoryClient::new(
        &endpoint,
        "alice".to_owned(),
        RemoteToken::new(ALICE_TOKEN.to_owned()).unwrap(),
    )
    .unwrap();

    let error = client.scan("candidate", 2).await.unwrap_err();
    let MemoryError::Backend { source } = error else {
        panic!("expected backend error, got {error:?}");
    };
    assert!(matches!(
        source.downcast_ref::<RemoteClientError>(),
        Some(RemoteClientError::InvalidResponse)
    ));
    task.abort();
}

#[tokio::test]
async fn client_enforces_the_requested_export_page_size() {
    let app = Router::new().route(
        &format!("/{}", protocol::EXPORT_PATH),
        post(oversized_export),
    );
    let (endpoint, task) = live_server(app).await;
    let client = RemoteMemoryClient::new(
        &endpoint,
        "alice".to_owned(),
        RemoteToken::new(ALICE_TOKEN.to_owned()).unwrap(),
    )
    .unwrap();

    let error = client.export_page(None, None, 1).await.unwrap_err();
    let MemoryError::Backend { source } = error else {
        panic!("expected backend error, got {error:?}");
    };
    assert!(matches!(
        source.downcast_ref::<RemoteClientError>(),
        Some(RemoteClientError::InvalidResponse)
    ));
    task.abort();
}

#[tokio::test]
async fn client_rejects_sync_reports_that_do_not_match_the_snapshot() {
    let app = Router::new().route(
        &format!("/{}", protocol::SYNC_PATH),
        post(impossible_sync_report),
    );
    let (endpoint, task) = live_server(app).await;
    let client = RemoteMemoryClient::new(
        &endpoint,
        "alice".to_owned(),
        RemoteToken::new(ALICE_TOKEN.to_owned()).unwrap(),
    )
    .unwrap();

    let error = client.sync(&[record(1, 1, "snapshot")]).await.unwrap_err();
    let MemoryError::Backend { source } = error else {
        panic!("expected backend error, got {error:?}");
    };
    assert!(matches!(
        source.downcast_ref::<RemoteClientError>(),
        Some(RemoteClientError::InvalidResponse)
    ));
    task.abort();
}

#[tokio::test]
async fn client_rejects_put_responses_unrelated_to_the_request() {
    let app = Router::new().route(&format!("/{}", protocol::PUT_PATH), post(unrelated_put));
    let (endpoint, task) = live_server(app).await;
    let client = RemoteMemoryClient::new(
        &endpoint,
        "alice".to_owned(),
        RemoteToken::new(ALICE_TOKEN.to_owned()).unwrap(),
    )
    .unwrap();

    for replacement in [None, Some(MemoryKey::remote("alice".to_owned(), 1, 1))] {
        let error = client
            .put("submitted content", replacement)
            .await
            .unwrap_err();
        let MemoryError::Backend { source } = error else {
            panic!("expected backend error, got {error:?}");
        };
        assert!(matches!(
            source.downcast_ref::<RemoteClientError>(),
            Some(RemoteClientError::InvalidResponse)
        ));
    }
    task.abort();
}

#[tokio::test]
async fn client_preserves_equivalent_content_from_distinct_namespaces() {
    let app = Router::new().route(
        &format!("/{}", protocol::READ_PATH),
        post(equivalent_content_read),
    );
    let (endpoint, task) = live_server(app).await;
    let client = RemoteMemoryClient::new(
        &endpoint,
        "alice".to_owned(),
        RemoteToken::new(ALICE_TOKEN.to_owned()).unwrap(),
    )
    .unwrap();

    let memories = client
        .read(
            &[],
            &[
                MemoryKey::remote("alice".to_owned(), 1, 1),
                MemoryKey::remote("bob".to_owned(), 1, 1),
            ],
        )
        .await
        .unwrap();
    assert_eq!(memories.len(), 2);
    task.abort();
}

#[tokio::test]
async fn client_rejects_ambiguous_versions_for_an_unversioned_id() {
    let app = Router::new().route(
        &format!("/{}", protocol::READ_PATH),
        post(ambiguous_version_read),
    );
    let (endpoint, task) = live_server(app).await;
    let client = RemoteMemoryClient::new(
        &endpoint,
        "alice".to_owned(),
        RemoteToken::new(ALICE_TOKEN.to_owned()).unwrap(),
    )
    .unwrap();

    let error = client.read(&[1], &[]).await.unwrap_err();
    let MemoryError::Backend { source } = error else {
        panic!("expected backend error, got {error:?}");
    };
    assert!(matches!(
        source.downcast_ref::<RemoteClientError>(),
        Some(RemoteClientError::InvalidResponse)
    ));
    task.abort();
}

#[tokio::test]
async fn client_ignores_namespace_less_remote_keys() {
    let client = RemoteMemoryClient::new(
        "http://127.0.0.1:1/",
        "alice".to_owned(),
        RemoteToken::new(ALICE_TOKEN.to_owned()).unwrap(),
    )
    .unwrap();

    assert!(
        client
            .read(&[], &[MemoryKey::local(1, 1)])
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn client_rejects_ambiguous_versions_in_list_responses() {
    let app = Router::new().route(
        &format!("/{}", protocol::LIST_PATH),
        post(ambiguous_version_list),
    );
    let (endpoint, task) = live_server(app).await;
    let client = RemoteMemoryClient::new(
        &endpoint,
        "alice".to_owned(),
        RemoteToken::new(ALICE_TOKEN.to_owned()).unwrap(),
    )
    .unwrap();

    let error = client.list().await.unwrap_err();
    let MemoryError::Backend { source } = error else {
        panic!("expected backend error, got {error:?}");
    };
    assert!(matches!(
        source.downcast_ref::<RemoteClientError>(),
        Some(RemoteClientError::InvalidResponse)
    ));
    task.abort();
}

async fn live_server(router: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (format!("http://{address}/"), task)
}

#[derive(Clone)]
struct AsyncStore {
    namespace: String,
    puts: Arc<Mutex<Vec<(String, String)>>>,
    gate: Option<Gate>,
}

#[derive(Clone)]
struct Gate {
    active: Arc<AtomicUsize>,
    maximum_active: Arc<AtomicUsize>,
    started: Arc<Notify>,
    release: Arc<Semaphore>,
}

impl MemoryStore for AsyncStore {
    fn scan(
        &self,
        _query: &str,
        _limit: usize,
    ) -> impl Future<Output = Result<MemoryScan, MemoryError>> + Send {
        async {
            Ok(MemoryScan {
                abstained: true,
                candidates: Vec::new(),
            })
        }
    }

    fn read(
        &self,
        _ids: &[i64],
        _keys: &[MemoryKey],
    ) -> impl Future<Output = Result<Vec<MemoryRecord>, MemoryError>> + Send {
        async { Ok(Vec::new()) }
    }

    async fn list(&self) -> Result<Vec<MemoryRecord>, MemoryError> {
        if let Some(gate) = &self.gate {
            let active = gate.active.fetch_add(1, Ordering::SeqCst) + 1;
            gate.maximum_active.fetch_max(active, Ordering::SeqCst);
            gate.started.notify_one();
            gate.release.acquire().await.unwrap().forget();
            gate.active.fetch_sub(1, Ordering::SeqCst);
        }
        Ok(Vec::new())
    }

    fn put(
        &self,
        content: &str,
        _replacement: Option<MemoryKey>,
    ) -> impl Future<Output = Result<MemoryRecord, MemoryError>> + Send {
        let namespace = self.namespace.clone();
        let content = content.to_owned();
        let puts = self.puts.clone();
        async move {
            puts.lock()
                .unwrap()
                .push((namespace.clone(), content.clone()));
            Ok(MemoryRecord {
                key: MemoryKey::remote(namespace, 41, 1),
                content,
                created_at_ms: 1,
                updated_at_ms: 1,
                last_scanned_at_ms: None,
                scan_count: 0,
                last_used_at_ms: None,
                use_count: 0,
                probation_until_ms: None,
            })
        }
    }

    async fn delete(&self, _key: MemoryKey) -> Result<(), MemoryError> {
        Ok(())
    }

    fn sync(
        &self,
        _memories: &[MemoryRecord],
    ) -> impl Future<Output = Result<SyncReport, MemoryError>> + Send {
        async { Ok(SyncReport::default()) }
    }

    fn export_page(
        &self,
        _namespaces: Option<&[String]>,
        _cursor: Option<&ExportCursor>,
        _limit: usize,
    ) -> impl Future<Output = Result<(Vec<MemoryRecord>, Option<ExportCursor>), MemoryError>> + Send
    {
        async { Ok((Vec::new(), None)) }
    }
}

#[tokio::test]
async fn generic_async_store_implementors_plug_into_the_public_server() {
    let puts = Arc::new(Mutex::new(Vec::new()));
    let bindings = Arc::new(Mutex::new(Vec::new()));
    let factory_puts = puts.clone();
    let factory_bindings = bindings.clone();
    let app = MemoryServer::new(
        move |namespace| {
            factory_bindings.lock().unwrap().push(namespace.clone());
            AsyncStore {
                namespace,
                puts: factory_puts.clone(),
                gate: None,
            }
        },
        [credential("alice", RemoteRole::Writer, ALICE_TOKEN)],
    )
    .unwrap()
    .router();

    let mismatch = send(
        &app,
        protocol::PUT_PATH,
        ALICE_TOKEN,
        "bob",
        &PutRequest {
            content: "must not bind".to_owned(),
            replacement: None,
        },
    )
    .await;
    assert_error(
        mismatch,
        StatusCode::FORBIDDEN,
        RemoteErrorCode::NamespaceMismatch,
    )
    .await;
    assert!(bindings.lock().unwrap().is_empty());

    let memory = json::<PutResponse>(
        send(
            &app,
            protocol::PUT_PATH,
            ALICE_TOKEN,
            "alice",
            &PutRequest {
                content: "native async custom store".to_owned(),
                replacement: None,
            },
        )
        .await,
    )
    .await
    .memory;
    assert_eq!(memory.key, MemoryKey::remote("alice".to_owned(), 41, 1));
    assert_eq!(*bindings.lock().unwrap(), ["alice"]);
    assert_eq!(
        *puts.lock().unwrap(),
        [("alice".to_owned(), "native async custom store".to_owned())]
    );
}

#[tokio::test]
async fn router_limits_store_operations_to_64_in_flight() {
    let gate = Gate {
        active: Arc::new(AtomicUsize::new(0)),
        maximum_active: Arc::new(AtomicUsize::new(0)),
        started: Arc::new(Notify::new()),
        release: Arc::new(Semaphore::new(0)),
    };
    let factory_gate = gate.clone();
    let app = MemoryServer::new(
        move |namespace| AsyncStore {
            namespace,
            puts: Arc::new(Mutex::new(Vec::new())),
            gate: Some(factory_gate.clone()),
        },
        [credential("alice", RemoteRole::Reader, ALICE_TOKEN)],
    )
    .unwrap()
    .router();

    let mut requests = tokio::task::JoinSet::new();
    for _ in 0..65 {
        let app = app.clone();
        requests.spawn(async move {
            app.oneshot(request::<()>(
                "POST",
                protocol::LIST_PATH,
                Some(ALICE_TOKEN),
                Some("alice"),
                Some(&()),
            ))
            .await
            .unwrap()
        });
    }

    tokio::time::timeout(Duration::from_secs(1), async {
        while gate.active.load(Ordering::SeqCst) < 64 {
            gate.started.notified().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(gate.maximum_active.load(Ordering::SeqCst), 64);

    gate.release.add_permits(65);
    while let Some(result) = requests.join_next().await {
        assert_eq!(result.unwrap().status(), StatusCode::OK);
    }
    assert_eq!(gate.maximum_active.load(Ordering::SeqCst), 64);
}
