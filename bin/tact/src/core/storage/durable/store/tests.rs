use super::{HostBatch, HostRow, SqliteStateStore};
use crate::core::{
    storage::{
        BUSY_TIMEOUT, SessionStorage, StorageError, database_path,
        durable::{
            reader::SqliteStateReader,
            store_event,
            tests::{assistant, prompt, started},
        },
    },
    transcript::TranscriptRecord,
};
use nanocode::{
    agent::events::AgentEventKind,
    durability::{
        Committed, EventCursor, EventOrder, EventQuery, EventRange, OwnerId, OwnerToken,
        StateCommit, StateCursor, StateQuery, StateReader, StateStore, StateSummary, StoreError,
        StoreEvent, StoreRecord, SummaryUpdate,
    },
};
use rusqlite::Connection;
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};
use tempfile::TempDir;

struct Fixture {
    _directory: TempDir,
    config: PathBuf,
    store: SqliteStateStore,
    reader: SqliteStateReader,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("config.toml");
        Self {
            store: SqliteStateStore::new(&config).unwrap(),
            reader: SqliteStateReader::new(&config),
            config,
            _directory: directory,
        }
    }

    async fn owner(&mut self, state_id: &str) -> OwnerToken {
        self.store
            .acquire(state_id, OwnerId::new())
            .await
            .unwrap()
            .owner
    }

    async fn commit(
        &mut self,
        state_id: &str,
        owner: &OwnerToken,
        expected_revision: u64,
        events: &[StoreEvent],
        summary: SummaryUpdate,
    ) -> Result<Committed, StoreError> {
        self.store
            .commit(
                state_id,
                owner,
                expected_revision,
                StateCommit {
                    payload: "payload",
                    records: &[],
                    events,
                    summary: &summary,
                },
            )
            .await
    }

    async fn events(&mut self, state_id: &str, range: EventRange) -> Vec<u64> {
        self.reader
            .read_events(state_id, &range)
            .await
            .unwrap()
            .events
            .iter()
            .map(|event| event.seq)
            .collect()
    }

    fn kinds(&self, state_id: &str) -> Vec<String> {
        SessionStorage::open_read_only(&self.config)
            .unwrap()
            .unwrap()
            .load_records(state_id)
            .unwrap()
            .iter()
            .map(|record| record.kind().as_str().to_owned())
            .collect()
    }
}

fn event(record: &TranscriptRecord) -> StoreEvent {
    store_event(record, serde_json::to_string(record).unwrap()).unwrap()
}

fn range(limit: usize) -> EventRange {
    EventRange {
        limit,
        ..EventRange::default()
    }
}

fn summary(scope: &str, updated_at_ms: u64) -> StateSummary {
    StateSummary {
        scope: Some(scope.to_owned()),
        updated_at_ms,
        value: "{}".to_owned(),
        ..StateSummary::default()
    }
}

#[tokio::test]
async fn owners_are_fenced_and_revisions_must_match() {
    let mut fixture = Fixture::new();
    let first = fixture.owner("state").await;
    let second = fixture.owner("state").await;

    assert_eq!(
        fixture
            .commit("state", &first, 0, &[], SummaryUpdate::Unchanged)
            .await,
        Err(StoreError::Fenced)
    );
    let committed = fixture
        .commit("state", &second, 0, &[], SummaryUpdate::Unchanged)
        .await
        .unwrap();
    assert_eq!(
        committed,
        Committed {
            revision: 1,
            first_event: None
        }
    );
    assert_eq!(
        fixture
            .commit("state", &second, 0, &[], SummaryUpdate::Unchanged)
            .await,
        Err(StoreError::Conflict {
            expected: 0,
            actual: 1
        })
    );
    let stored = fixture.store.peek("state").await.unwrap();
    assert_eq!(stored.revision, 1);
    assert_eq!(stored.payload.as_deref(), Some("payload"));
    assert_eq!(fixture.store.list_states(10).await.unwrap(), ["state"]);
}

#[tokio::test]
async fn rejected_commits_change_nothing() {
    let mut fixture = Fixture::new();
    let owner = fixture.owner("session").await;
    let record = StoreRecord {
        key: "key".to_owned(),
        value: "value".to_owned(),
    };
    fixture
        .store
        .replace(
            "session",
            &owner,
            0,
            "payload",
            std::slice::from_ref(&record),
        )
        .await
        .unwrap();
    fixture
        .commit(
            "session",
            &owner,
            1,
            &[event(&started("session"))],
            SummaryUpdate::Unchanged,
        )
        .await
        .unwrap();

    let conflicting = StoreRecord {
        value: "other".to_owned(),
        ..record
    };
    let mut mislabeled = event(&prompt(2, "text"));
    mislabeled.text = Some("different".to_owned());
    let foreign = StoreEvent {
        kind: "foreign".to_owned(),
        at_ms: 1,
        body: "{}".to_owned(),
        text: None,
    };
    let rejected = [
        fixture
            .store
            .replace("session", &owner, 2, "next", &[conflicting])
            .await
            .map(|_| ()),
        fixture
            .commit(
                "session",
                &owner,
                2,
                &[mislabeled],
                SummaryUpdate::Unchanged,
            )
            .await
            .map(drop),
        fixture
            .commit("session", &owner, 2, &[foreign], SummaryUpdate::Unchanged)
            .await
            .map(drop),
        {
            fixture
                .store
                .stage(
                    "session",
                    HostBatch {
                        rows: vec![HostRow::Event, HostRow::Event],
                        resume_state: Some(b"state".to_vec()),
                    },
                )
                .unwrap();
            fixture
                .commit(
                    "session",
                    &owner,
                    2,
                    &[event(&prompt(2, "text"))],
                    SummaryUpdate::Unchanged,
                )
                .await
                .map(drop)
        },
    ];
    for result in rejected {
        assert!(
            matches!(result, Err(StoreError::NotCommitted(_))),
            "{result:?}"
        );
    }

    assert_eq!(fixture.store.peek("session").await.unwrap().revision, 2);
    assert_eq!(fixture.events("session", range(usize::MAX)).await, [1]);
    assert_eq!(
        fixture.store.read_record("session", "key").await.unwrap(),
        Some("value".to_owned())
    );
    let storage = SessionStorage::open_read_only(&fixture.config)
        .unwrap()
        .unwrap();
    assert_eq!(storage.load_resume_state("session").unwrap(), None);
}

#[tokio::test]
async fn events_are_sequenced_around_drafts_and_page_in_both_directions() {
    let mut fixture = Fixture::new();
    let owner = fixture.owner("session").await;
    fixture
        .store
        .stage(
            "session",
            HostBatch {
                rows: vec![
                    HostRow::Event,
                    HostRow::Draft(
                        serde_json::to_vec(
                            assistant(3, AgentEventKind::AssistantDelta, 1, "rep").as_ref(),
                        )
                        .unwrap(),
                    ),
                    HostRow::Event,
                ],
                resume_state: Some(b"state".to_vec()),
            },
        )
        .unwrap();
    let first = fixture
        .commit(
            "session",
            &owner,
            0,
            &[event(&started("session")), event(&prompt(2, "question"))],
            SummaryUpdate::Unchanged,
        )
        .await
        .unwrap();
    assert_eq!(first.first_event, Some(1));
    assert_eq!(
        fixture.kinds("session"),
        ["session.started", "assistant.delta", "user.submitted"]
    );

    let reply = event(&assistant(4, AgentEventKind::AssistantMessage, 1, "reply"));
    let second = fixture
        .commit(
            "session",
            &owner,
            1,
            &[reply.clone(), event(&prompt(5, "follow up"))],
            SummaryUpdate::Unchanged,
        )
        .await
        .unwrap();
    assert_eq!(second.first_event, Some(3));
    assert_eq!(
        fixture.kinds("session"),
        [
            "session.started",
            "user.submitted",
            "assistant.message",
            "user.submitted"
        ]
    );
    let storage = SessionStorage::open_read_only(&fixture.config)
        .unwrap()
        .unwrap();
    assert_eq!(
        storage.load_resume_state("session").unwrap().as_deref(),
        Some(&b"state"[..])
    );

    assert_eq!(fixture.events("session", range(2)).await, [1, 2]);
    let page = fixture
        .reader
        .read_events("session", &range(2))
        .await
        .unwrap();
    assert_eq!(page.next, Some(2));
    let rest = fixture
        .reader
        .read_events(
            "session",
            &EventRange {
                after: Some(2),
                ..range(usize::MAX)
            },
        )
        .await
        .unwrap();
    assert_eq!(rest.next, None);
    assert_eq!(rest.events[0].event, reply);
    assert_eq!(rest.events[0].revision, 2);
    assert_eq!(rest.events[1].seq, 4);
    let descending = fixture
        .reader
        .read_events(
            "session",
            &EventRange {
                before: Some(4),
                order: EventOrder::Descending,
                ..range(1)
            },
        )
        .await
        .unwrap();
    assert_eq!((descending.events[0].seq, descending.next), (3, Some(3)));
    assert_eq!(
        fixture
            .events(
                "session",
                EventRange {
                    kinds: vec!["user.submitted".to_owned()],
                    ..range(usize::MAX)
                },
            )
            .await,
        [2, 4]
    );
    let empty = fixture
        .reader
        .read_events("session", &range(0))
        .await
        .unwrap();
    assert!(empty.events.is_empty() && empty.next.is_none());
}

#[tokio::test]
async fn acquiring_a_legacy_session_numbers_its_records() {
    let mut fixture = Fixture::new();
    SessionStorage::open(&fixture.config)
        .unwrap()
        .append_records("legacy", &[started("legacy"), prompt(2, "old")])
        .unwrap();

    let owner = fixture.owner("legacy").await;
    let adopted = fixture
        .reader
        .read_events("legacy", &range(usize::MAX))
        .await
        .unwrap();
    assert_eq!(
        adopted
            .events
            .iter()
            .map(|event| (event.seq, event.revision, event.event.text.clone()))
            .collect::<Vec<_>>(),
        [(1, 0, None), (2, 0, Some("old".to_owned()))]
    );
    let committed = fixture
        .commit(
            "legacy",
            &owner,
            0,
            &[event(&prompt(3, "new"))],
            SummaryUpdate::Unchanged,
        )
        .await
        .unwrap();
    assert_eq!(committed.first_event, Some(3));
}

#[tokio::test]
async fn summaries_follow_revisions_and_queries_page_by_recency() {
    let mut fixture = Fixture::new();
    for (state_id, scope, parent, updated_at_ms) in [
        ("alpha", "/work", None, 10),
        ("beta", "/work", Some("alpha"), 20),
        ("gamma", "/other", None, 30),
    ] {
        let owner = fixture.owner(state_id).await;
        let mut indexed = summary(scope, updated_at_ms);
        indexed.parent = parent.map(str::to_owned);
        indexed.title = (state_id == "alpha").then(|| "Alpha Title".to_owned());
        indexed.attributes = BTreeMap::from([("model".to_owned(), format!("{state_id}-model"))]);
        let events = if state_id == "gamma" {
            vec![event(&started(state_id)), event(&prompt(2, "Hello there"))]
        } else {
            Vec::new()
        };
        fixture
            .commit(state_id, &owner, 0, &events, SummaryUpdate::Set(indexed))
            .await
            .unwrap();
    }
    let reader = fixture.reader.clone();
    let query = |query: StateQuery| {
        let mut reader = reader.clone();
        async move {
            let page = reader.query_states(&query).await.unwrap();
            (
                page.states
                    .into_iter()
                    .map(|state| state.state_id)
                    .collect::<Vec<_>>(),
                page.next,
            )
        }
    };
    let limited = |limit| StateQuery {
        limit,
        ..StateQuery::default()
    };

    assert_eq!(
        query(limited(2)).await,
        (
            vec!["gamma".to_owned(), "beta".to_owned()],
            Some(StateCursor {
                updated_at_ms: 20,
                state_id: "beta".to_owned()
            })
        )
    );
    let after = StateQuery {
        after: Some(StateCursor {
            updated_at_ms: 20,
            state_id: "beta".to_owned(),
        }),
        ..limited(2)
    };
    assert_eq!(query(after).await, (vec!["alpha".to_owned()], None));
    for (filter, expected) in [
        (
            StateQuery {
                scope: Some("/work".to_owned()),
                ..limited(10)
            },
            vec!["beta", "alpha"],
        ),
        (
            StateQuery {
                parent: Some("alpha".to_owned()),
                ..limited(10)
            },
            vec!["beta"],
        ),
        (
            StateQuery {
                attributes: BTreeMap::from([("model".to_owned(), "gamma-model".to_owned())]),
                ..limited(10)
            },
            vec!["gamma"],
        ),
        (
            StateQuery {
                text: vec!["ALPHA".to_owned(), "hello".to_owned()],
                ..limited(10)
            },
            vec!["gamma", "alpha"],
        ),
        (limited(0), vec![]),
    ] {
        assert_eq!(query(filter).await.0, expected);
    }

    let owner = fixture.owner("beta").await;
    fixture
        .commit("beta", &owner, 1, &[], SummaryUpdate::Remove)
        .await
        .unwrap();
    assert_eq!(
        query(limited(10)).await.0,
        vec!["gamma".to_owned(), "alpha".to_owned()]
    );
}

#[tokio::test]
async fn event_queries_cover_listed_states_newest_first() {
    let mut fixture = Fixture::new();
    for (state_id, summary_update, prompts) in [
        (
            "listed",
            SummaryUpdate::Set(summary("/work", 1)),
            vec![prompt(2, "first prompt"), prompt(3, "second prompt")],
        ),
        (
            "unlisted",
            SummaryUpdate::Unchanged,
            vec![prompt(9, "hidden prompt")],
        ),
    ] {
        let owner = fixture.owner(state_id).await;
        let events = std::iter::once(started(state_id))
            .chain(prompts)
            .map(|record| event(&record))
            .collect::<Vec<_>>();
        fixture
            .commit(state_id, &owner, 0, &events, summary_update)
            .await
            .unwrap();
    }
    let prompts = EventQuery {
        kinds: vec!["user.submitted".to_owned()],
        limit: 1,
        ..EventQuery::default()
    };
    let newest = fixture.reader.query_events(&prompts).await.unwrap();
    assert_eq!(newest.hits.len(), 1);
    assert_eq!(newest.hits[0].state_id, "listed");
    assert_eq!(newest.hits[0].event.seq, 3);
    assert_eq!(
        newest.next,
        Some(EventCursor {
            at_ms: 103,
            state_id: "listed".to_owned(),
            seq: 3
        })
    );
    let older = fixture
        .reader
        .query_events(&EventQuery {
            after: newest.next,
            ..prompts.clone()
        })
        .await
        .unwrap();
    assert_eq!(
        older
            .hits
            .iter()
            .map(|hit| hit.event.event.text.clone())
            .collect::<Vec<_>>(),
        [Some("first prompt".to_owned())]
    );
    assert_eq!(older.next, None);
    let matched = fixture
        .reader
        .query_events(&EventQuery {
            text: vec!["SECOND".to_owned(), "hidden".to_owned()],
            scope: Some("/work".to_owned()),
            limit: usize::MAX,
            ..EventQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(
        matched
            .hits
            .iter()
            .map(|hit| (hit.state_id.as_str(), hit.event.seq))
            .collect::<Vec<_>>(),
        [("listed", 3)]
    );
}

#[tokio::test]
async fn readers_neither_create_the_database_nor_wait_for_the_writer() {
    let mut fixture = Fixture::new();
    assert_eq!(
        fixture.reader.read_state("session").await.unwrap(),
        Default::default()
    );
    assert_eq!(
        fixture
            .reader
            .read_records("session", &["key".to_owned()])
            .await
            .unwrap(),
        [None]
    );
    assert!(fixture.events("session", range(10)).await.is_empty());
    assert!(!database_path(&fixture.config).exists());

    let owner = fixture.owner("session").await;
    fixture
        .commit(
            "session",
            &owner,
            0,
            &[event(&started("session"))],
            SummaryUpdate::Unchanged,
        )
        .await
        .unwrap();
    let lock = Connection::open(database_path(&fixture.config)).unwrap();
    lock.execute_batch("BEGIN IMMEDIATE").unwrap();
    assert_eq!(fixture.events("session", range(10)).await, [1]);
    assert_eq!(
        fixture.reader.read_state("session").await.unwrap().revision,
        1
    );
    lock.execute_batch("COMMIT").unwrap();
}

#[tokio::test]
async fn write_lock_contention_delays_a_commit_without_failing_it() {
    let mut fixture = Fixture::new();
    let owner = fixture.owner("session").await;
    fixture
        .commit(
            "session",
            &owner,
            0,
            &[event(&started("session"))],
            SummaryUpdate::Unchanged,
        )
        .await
        .unwrap();
    let lock = Connection::open(database_path(&fixture.config)).unwrap();
    lock.execute_batch("BEGIN IMMEDIATE").unwrap();

    let mut store = fixture.store.clone();
    let events = Arc::new([event(&prompt(2, "waiting"))]);
    let commit = tokio::spawn(async move {
        store
            .commit(
                "session",
                &owner,
                1,
                StateCommit {
                    payload: "payload",
                    records: &[],
                    events: events.as_slice(),
                    summary: &SummaryUpdate::Unchanged,
                },
            )
            .await
    });
    // Outlast the writer's busy timeout so it observes contention and retries.
    tokio::time::sleep(BUSY_TIMEOUT * 2).await;
    assert!(!commit.is_finished());
    lock.execute_batch("COMMIT").unwrap();

    assert_eq!(commit.await.unwrap().unwrap().first_event, Some(2));
}

async fn acquire(store: &mut SqliteStateStore, state_id: &str) -> OwnerToken {
    store.acquire(state_id, OwnerId::new()).await.unwrap().owner
}

fn draft(sequence: u64, text: &str) -> HostRow {
    HostRow::Draft(
        serde_json::to_vec(assistant(sequence, AgentEventKind::AssistantDelta, 1, text).as_ref())
            .unwrap(),
    )
}

fn resume_state(config: &std::path::Path, state_id: &str) -> Option<Vec<u8>> {
    SessionStorage::open_read_only(config)
        .unwrap()
        .unwrap()
        .load_resume_state(state_id)
        .unwrap()
}

#[tokio::test]
async fn a_fenced_owner_cannot_take_another_owners_host_batch() {
    let fixture = Fixture::new();
    let mut stale = fixture.store.writer_handle();
    let mut current = fixture.store.writer_handle();
    let stale_owner = acquire(&mut stale, "session").await;
    let current_owner = acquire(&mut current, "session").await;
    current
        .stage(
            "session",
            HostBatch {
                rows: vec![HostRow::Event, draft(2, "draft"), HostRow::Event],
                resume_state: Some(b"current".to_vec()),
            },
        )
        .unwrap();

    let stale_commit = stale
        .commit(
            "session",
            &stale_owner,
            0,
            StateCommit {
                payload: "stale",
                records: &[],
                events: &[event(&started("session"))],
                summary: &SummaryUpdate::Unchanged,
            },
        )
        .await;
    assert_eq!(stale_commit, Err(StoreError::Fenced));
    assert_eq!(fixture.store.staged_batches(), 1);

    current
        .commit(
            "session",
            &current_owner,
            0,
            StateCommit {
                payload: "current",
                records: &[],
                events: &[event(&started("session")), event(&prompt(3, "question"))],
                summary: &SummaryUpdate::Unchanged,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        fixture.kinds("session"),
        ["session.started", "assistant.delta", "user.submitted"]
    );
    assert_eq!(
        resume_state(&fixture.config, "session").as_deref(),
        Some(&b"current"[..])
    );
    assert_eq!(fixture.store.staged_batches(), 0);
}

#[tokio::test]
async fn a_rejected_commit_keeps_its_host_batch_for_the_retry() {
    let mut fixture = Fixture::new();
    let owner = fixture.owner("session").await;
    fixture
        .store
        .replace(
            "session",
            &owner,
            0,
            "payload",
            &[StoreRecord {
                key: "key".to_owned(),
                value: "value".to_owned(),
            }],
        )
        .await
        .unwrap();
    fixture
        .store
        .stage(
            "session",
            HostBatch {
                rows: vec![HostRow::Event, draft(2, "draft")],
                resume_state: Some(b"state".to_vec()),
            },
        )
        .unwrap();
    let events = [event(&started("session"))];
    let conflicting = [StoreRecord {
        key: "key".to_owned(),
        value: "other".to_owned(),
    }];
    for (expected_revision, records) in [(0, &[][..]), (1, &conflicting[..])] {
        let rejected = fixture
            .store
            .commit(
                "session",
                &owner,
                expected_revision,
                StateCommit {
                    payload: "payload",
                    records,
                    events: &events,
                    summary: &SummaryUpdate::Unchanged,
                },
            )
            .await;
        assert!(rejected.is_err(), "{rejected:?}");
    }
    assert_eq!(fixture.store.staged_batches(), 1);
    assert!(fixture.kinds("session").is_empty());

    fixture
        .commit("session", &owner, 1, &events, SummaryUpdate::Unchanged)
        .await
        .unwrap();
    assert_eq!(
        fixture.kinds("session"),
        ["session.started", "assistant.delta"]
    );
    assert_eq!(
        resume_state(&fixture.config, "session").as_deref(),
        Some(&b"state"[..])
    );
    assert_eq!(fixture.store.staged_batches(), 0);
}

#[tokio::test]
async fn staging_over_a_pending_host_batch_is_rejected() {
    let fixture = Fixture::new();
    fixture
        .store
        .stage("session", HostBatch::default())
        .unwrap();
    assert!(matches!(
        fixture.store.stage("session", HostBatch::default()),
        Err(StorageError::HostBatchPending { .. })
    ));
    fixture
        .store
        .writer_handle()
        .stage("session", HostBatch::default())
        .unwrap();
    fixture.store.discard("session");
    fixture
        .store
        .stage("session", HostBatch::default())
        .unwrap();
}

#[tokio::test]
async fn streamed_drafts_cannot_become_contract_events() {
    let mut fixture = Fixture::new();
    let owner = fixture.owner("session").await;
    let delta = event(&assistant(2, AgentEventKind::AssistantDelta, 1, "draft"));
    assert!(matches!(
        fixture
            .commit(
                "session",
                &owner,
                0,
                &[event(&started("session")), delta],
                SummaryUpdate::Unchanged,
            )
            .await,
        Err(StoreError::NotCommitted(_))
    ));
    fixture
        .store
        .stage(
            "session",
            HostBatch {
                rows: vec![
                    HostRow::Event,
                    HostRow::Draft(serde_json::to_vec(prompt(2, "not a draft").as_ref()).unwrap()),
                ],
                resume_state: None,
            },
        )
        .unwrap();
    assert!(matches!(
        fixture
            .commit(
                "session",
                &owner,
                0,
                &[event(&started("session"))],
                SummaryUpdate::Unchanged,
            )
            .await,
        Err(StoreError::NotCommitted(_))
    ));
    assert!(fixture.kinds("session").is_empty());
    assert_eq!(fixture.store.peek("session").await.unwrap().revision, 0);
}

#[tokio::test]
async fn final_messages_delete_only_unsequenced_drafts() {
    let mut fixture = Fixture::new();
    let owner = fixture.owner("session").await;
    fixture
        .store
        .stage(
            "session",
            HostBatch {
                rows: vec![HostRow::Event, draft(2, "draft")],
                resume_state: None,
            },
        )
        .unwrap();
    fixture
        .commit(
            "session",
            &owner,
            0,
            &[event(&started("session"))],
            SummaryUpdate::Unchanged,
        )
        .await
        .unwrap();
    // A sequenced row carrying an assistant stream, as an acknowledged contract event would.
    let connection = Connection::open(database_path(&fixture.config)).unwrap();
    connection
        .execute_batch(
            "UPDATE events SET seq = 2, revision = 1 WHERE assistant_stream IS NOT NULL;
             UPDATE durable_event_sequences SET last_seq = 2;",
        )
        .unwrap();

    fixture
        .commit(
            "session",
            &owner,
            1,
            &[event(&assistant(
                3,
                AgentEventKind::AssistantMessage,
                1,
                "reply",
            ))],
            SummaryUpdate::Unchanged,
        )
        .await
        .unwrap();
    assert_eq!(
        fixture.events("session", range(usize::MAX)).await,
        [1, 2, 3]
    );
}

#[tokio::test]
async fn noncanonical_event_bodies_keep_their_conversation_text() {
    let mut fixture = Fixture::new();
    let owner = fixture.owner("session").await;
    let reply = assistant(
        2,
        AgentEventKind::AssistantMessage,
        1,
        "a pretty printed reply",
    );
    let pretty =
        serde_json::to_string_pretty(&serde_json::to_value(reply.as_ref()).unwrap()).unwrap();
    let reply = store_event(&reply, pretty).unwrap();
    fixture
        .commit(
            "session",
            &owner,
            0,
            &[event(&started("session")), reply.clone()],
            SummaryUpdate::Set(summary("/work", 1)),
        )
        .await
        .unwrap();

    let read = fixture
        .reader
        .read_events("session", &range(usize::MAX))
        .await
        .unwrap();
    assert_eq!(read.events[1].event, reply);
    let page = fixture
        .reader
        .query_states(&StateQuery {
            text: vec!["PRETTY PRINTED".to_owned()],
            limit: 10,
            ..StateQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(page.states.len(), 1);
    let matches = SessionStorage::open_read_only(&fixture.config)
        .unwrap()
        .unwrap()
        .search_conversations("pretty printed")
        .unwrap();
    assert!(matches.contains_key("session"));
}

#[tokio::test]
async fn sequences_and_bounds_stay_within_sqlite_integers() {
    let mut fixture = Fixture::new();
    let owner = fixture.owner("session").await;
    fixture
        .commit(
            "session",
            &owner,
            0,
            &[event(&started("session"))],
            SummaryUpdate::Unchanged,
        )
        .await
        .unwrap();
    let largest = u64::try_from(i64::MAX).unwrap();
    Connection::open(database_path(&fixture.config))
        .unwrap()
        .execute(
            "UPDATE durable_event_sequences SET last_seq = ?1",
            [i64::MAX - 1],
        )
        .unwrap();

    let last = fixture
        .commit(
            "session",
            &owner,
            1,
            &[event(&prompt(2, "last"))],
            SummaryUpdate::Unchanged,
        )
        .await
        .unwrap();
    assert_eq!(last.first_event, Some(largest));
    assert!(matches!(
        fixture
            .commit(
                "session",
                &owner,
                2,
                &[event(&prompt(3, "overflow"))],
                SummaryUpdate::Unchanged,
            )
            .await,
        Err(StoreError::NotCommitted(_))
    ));
    assert!(matches!(
        fixture
            .commit(
                "session",
                &owner,
                2,
                &[],
                SummaryUpdate::Set(summary("/work", u64::MAX)),
            )
            .await,
        Err(StoreError::NotCommitted(_))
    ));

    for (after, before, expected) in [
        (None, None, vec![1, largest]),
        (None, Some(u64::MAX), vec![1, largest]),
        (Some(largest - 1), None, vec![largest]),
        (Some(largest), None, vec![]),
        (Some(u64::MAX), None, vec![]),
    ] {
        let bounded = EventRange {
            after,
            before,
            ..range(usize::MAX)
        };
        assert_eq!(fixture.events("session", bounded).await, expected);
    }

    fixture
        .commit(
            "session",
            &owner,
            2,
            &[],
            SummaryUpdate::Set(summary("/work", 5)),
        )
        .await
        .unwrap();
    let page = fixture
        .reader
        .query_states(&StateQuery {
            after: Some(StateCursor {
                updated_at_ms: u64::MAX,
                state_id: String::new(),
            }),
            limit: 10,
            ..StateQuery::default()
        })
        .await
        .unwrap();
    assert_eq!(page.states.len(), 1);
}

#[tokio::test]
async fn older_binaries_can_append_after_contract_commits() {
    let mut fixture = Fixture::new();
    let owner = fixture.owner("session").await;
    fixture
        .commit(
            "session",
            &owner,
            0,
            &[event(&started("session")), event(&prompt(2, "contract"))],
            SummaryUpdate::Unchanged,
        )
        .await
        .unwrap();
    let connection = Connection::open(database_path(&fixture.config)).unwrap();
    connection
        .execute(
            "INSERT INTO events(\n\
                 session_id, record_json, prompt_text, prompt_recorded_at_ms, assistant_stream\n\
             ) VALUES ('session', ?1, 'older binary', 103, NULL)",
            [serde_json::to_vec(prompt(3, "older binary").as_ref()).unwrap()],
        )
        .unwrap();

    let mut reopened = SqliteStateStore::new(&fixture.config).unwrap();
    let owner = acquire(&mut reopened, "session").await;
    let adopted = fixture
        .reader
        .read_events("session", &range(usize::MAX))
        .await
        .unwrap();
    assert_eq!(
        adopted
            .events
            .iter()
            .map(|event| (event.seq, event.revision))
            .collect::<Vec<_>>(),
        [(1, 1), (2, 1), (3, 0)]
    );
    assert!(
        connection
            .execute("UPDATE events SET seq = 1 WHERE seq = 3", [])
            .is_err()
    );
    let committed = reopened
        .commit(
            "session",
            &owner,
            1,
            StateCommit {
                payload: "payload",
                records: &[],
                events: &[event(&prompt(4, "newer binary"))],
                summary: &SummaryUpdate::Unchanged,
            },
        )
        .await
        .unwrap();
    assert_eq!(committed.first_event, Some(4));
}
