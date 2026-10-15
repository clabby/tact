//! Full-text search over the conversations of stored sessions.
//!
//! The index holds what the user and the assistant said: the text of `user.submitted`,
//! `user.steered`, and `assistant.message` records. Tool calls and results are never indexed.
//! Matching uses SQLite's FTS5 trigram tokenizer, so any fragment of three or more characters
//! matches case-insensitively, and sessions rank by the bm25 score of their best message.
//!
//! The schema is additive and leaves the format version unchanged, so binaries that predate it
//! keep reading and writing the same database. A trigger on `events` indexes each new conversation
//! event inside the writer's own transaction, including events written by those older binaries.
//! Events recorded before the trigger existed lie below a boundary stored with it, and
//! [`SessionStorage::backfill_conversation_index`] indexes them newest first. The trigger and the
//! backfill therefore cover disjoint events, and `INSERT OR IGNORE` on the event id keeps either
//! from indexing an event twice.

use super::{SessionStorage, StorageError, query};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::{collections::HashMap, path::Path};

/// The fewest characters a search term needs; the trigram index cannot match shorter text.
const MIN_TERM_CHARS: usize = 3;
/// How many bytes of a record's JSON are checked for its type before the record is parsed.
/// Records serialize their header fields, `type` included, ahead of the payload, so this keeps
/// large tool results from being parsed.
const RECORD_TYPE_PREFIX_BYTES: usize = 200;
/// Characters of context a snippet shows before its match.
const SNIPPET_LEAD_CHARS: usize = 24;
/// The most characters a snippet shows, not counting its ellipses.
const SNIPPET_CHARS: usize = 120;

/// The best match in one session's conversation.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ConversationMatch {
    /// The bm25 score of the best-matching message; lower is better.
    pub(crate) rank: f64,
    /// Text of that message around its first match.
    pub(crate) snippet: String,
}

/// The words of `search` that the conversation index can match. Shorter words are left out
/// because a trigram index cannot match them.
pub(crate) fn search_terms(search: &str) -> impl Iterator<Item = &str> {
    search
        .split_whitespace()
        .filter(|word| word.chars().count() >= MIN_TERM_CHARS)
}

/// Creates the conversation index once per database. One immediate transaction records the
/// backfill boundary and installs the trigger, so every event is either below the boundary or
/// indexed by the trigger.
pub(super) fn ensure_schema(connection: &Connection, path: &Path) -> Result<(), StorageError> {
    let installed = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master\n\
             WHERE type = 'trigger' AND name = 'events_conversation_indexed')",
            [],
            |row| row.get::<_, bool>(0),
        )
        .map_err(|source| query(path, source))?;
    if installed {
        return Ok(());
    }
    let text = conversation_text("new");
    connection
        .execute_batch(&format!(
            "BEGIN IMMEDIATE;\n\
             CREATE TABLE IF NOT EXISTS conversation_messages(\n\
                 event_id INTEGER PRIMARY KEY, session_id TEXT NOT NULL, text TEXT NOT NULL\n\
             ) STRICT;\n\
             CREATE VIRTUAL TABLE IF NOT EXISTS conversation_index USING fts5(\n\
                 text, content = 'conversation_messages', content_rowid = 'event_id',\n\
                 tokenize = 'trigram', detail = full\n\
             );\n\
             CREATE TRIGGER IF NOT EXISTS conversation_messages_indexed\n\
             AFTER INSERT ON conversation_messages BEGIN\n\
                 INSERT INTO conversation_index(rowid, text) VALUES (new.event_id, new.text);\n\
             END;\n\
             CREATE TABLE IF NOT EXISTS conversation_backfill(\n\
                 id INTEGER PRIMARY KEY CHECK(id = 1), next_below INTEGER NOT NULL\n\
             ) STRICT;\n\
             INSERT OR IGNORE INTO conversation_backfill(id, next_below)\n\
                 SELECT 1, COALESCE(MAX(event_id), 0) + 1 FROM events;\n\
             CREATE TRIGGER IF NOT EXISTS events_conversation_indexed\n\
             AFTER INSERT ON events BEGIN\n\
                 INSERT OR IGNORE INTO conversation_messages(event_id, session_id, text)\n\
                 SELECT new.event_id, new.session_id, text FROM (SELECT {text} AS text)\n\
                 WHERE typeof(text) = 'text' AND text != '';\n\
             END;\n\
             COMMIT;"
        ))
        .map_err(|source| query(path, source))
}

/// SQL for the conversation text of the `events` row named `row`, or NULL for any other event.
/// Prompts already have their text in a column; only records whose prefix names an assistant
/// message are parsed.
fn conversation_text(row: &str) -> String {
    format!(
        "CASE WHEN {row}.prompt_text IS NOT NULL THEN {row}.prompt_text\n\
         WHEN instr(CAST(substr({row}.record_json, 1, {RECORD_TYPE_PREFIX_BYTES}) AS TEXT),\n\
                    '\"type\":\"assistant.message\"') > 0\n\
             AND json_valid(CAST({row}.record_json AS TEXT))\n\
             AND json_extract(CAST({row}.record_json AS TEXT), '$.type') = 'assistant.message'\n\
         THEN json_extract(CAST({row}.record_json AS TEXT), '$.payload.text') END"
    )
}

impl SessionStorage {
    /// Indexes up to `limit` of the events recorded before the conversation index existed, newest
    /// first, in one short write transaction, and returns whether older events may remain.
    /// Callers in any process serialize on the write lock and continue from the stored boundary,
    /// so concurrent backfills neither repeat nor skip events.
    pub(crate) fn backfill_conversation_index(
        &mut self,
        limit: usize,
    ) -> Result<bool, StorageError> {
        let path = self.path.clone();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|source| query(&path, source))?;
        let next_below = transaction
            .query_row("SELECT next_below FROM conversation_backfill", [], |row| {
                row.get::<_, i64>(0)
            })
            .optional()
            .map_err(|source| query(&path, source))?;
        let Some(next_below) = next_below.filter(|next_below| *next_below > 1) else {
            return Ok(false);
        };
        let limit = i64::try_from(limit.max(1)).unwrap_or(i64::MAX);
        let lowest = transaction
            .query_row(
                "SELECT min(event_id) FROM (\n\
                     SELECT event_id FROM events WHERE event_id < ?1\n\
                     ORDER BY event_id DESC LIMIT ?2\n\
                 )",
                params![next_below, limit],
                |row| row.get::<_, Option<i64>>(0),
            )
            .map_err(|source| query(&path, source))?
            .unwrap_or(1);
        transaction
            .execute(
                &format!(
                    "INSERT OR IGNORE INTO conversation_messages(event_id, session_id, text)\n\
                     SELECT event_id, session_id, text FROM (\n\
                         SELECT e.event_id, e.session_id, {} AS text FROM events e\n\
                         WHERE e.event_id >= ?1 AND e.event_id < ?2\n\
                     ) WHERE typeof(text) = 'text' AND text != ''",
                    conversation_text("e")
                ),
                params![lowest, next_below],
            )
            .map_err(|source| query(&path, source))?;
        transaction
            .execute("UPDATE conversation_backfill SET next_below = ?1", [lowest])
            .map_err(|source| query(&path, source))?;
        transaction
            .commit()
            .map_err(|source| query(&path, source))?;
        Ok(lowest > 1)
    }

    /// The best conversation match of every stored session whose conversation contains each
    /// [`search_terms`] word within one message. Empty when `search` has no such word or the
    /// database has no conversation index yet.
    pub(crate) fn search_conversations(
        &self,
        search: &str,
    ) -> Result<HashMap<String, ConversationMatch>, StorageError> {
        let terms = search_terms(search).collect::<Vec<_>>();
        if terms.is_empty() || !self.has_conversation_index()? {
            return Ok(HashMap::new());
        }
        // Each word becomes one quoted FTS string, so the user's text never parses as FTS syntax.
        let expression = terms
            .iter()
            .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" ");
        let mut best = self
            .connection
            .prepare_cached(
                "SELECT m.session_id, min(hits.rank), m.event_id FROM (\n\
                     SELECT rowid AS event_id, rank FROM conversation_index\n\
                     WHERE conversation_index MATCH ?1\n\
                 ) hits JOIN conversation_messages m ON m.event_id = hits.event_id\n\
                 GROUP BY m.session_id",
            )
            .map_err(|source| query(&self.path, source))?;
        let rows = best
            .query_map([expression], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, f64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .map_err(|source| query(&self.path, source))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|source| query(&self.path, source))?;
        let mut text = self
            .connection
            .prepare_cached("SELECT text FROM conversation_messages WHERE event_id = ?1")
            .map_err(|source| query(&self.path, source))?;
        let terms = terms
            .iter()
            .map(|term| term.to_ascii_lowercase())
            .collect::<Vec<_>>();
        rows.into_iter()
            .map(|(session_id, rank, event_id)| {
                let message = text
                    .query_row([event_id], |row| row.get::<_, String>(0))
                    .map_err(|source| query(&self.path, source))?;
                let snippet = snippet(&message, &terms);
                Ok((session_id, ConversationMatch { rank, snippet }))
            })
            .collect()
    }

    fn has_conversation_index(&self) -> Result<bool, StorageError> {
        self.connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = 'conversation_index')",
                [],
                |row| row.get(0),
            )
            .map_err(|source| query(&self.path, source))
    }
}

/// Up to [`SNIPPET_CHARS`] characters of `message`, whitespace collapsed, starting a little before
/// the earliest occurrence of any of `terms`. `terms` must be ASCII-lowercase; a match that
/// differs only in non-ASCII case is not located, and the snippet then starts at the beginning.
fn snippet(message: &str, terms: &[String]) -> String {
    let message = message.split_whitespace().collect::<Vec<_>>().join(" ");
    let lowercase = message.to_ascii_lowercase();
    let found = terms
        .iter()
        .filter_map(|term| lowercase.find(term.as_str()))
        .min()
        .unwrap_or(0);
    let start = message[..found]
        .chars()
        .count()
        .saturating_sub(SNIPPET_LEAD_CHARS);
    let mut characters = message.chars().skip(start);
    let excerpt = characters.by_ref().take(SNIPPET_CHARS).collect::<String>();
    let lead = if start > 0 { "…" } else { "" };
    let tail = if characters.next().is_some() {
        "…"
    } else {
        ""
    };
    format!("{lead}{excerpt}{tail}")
}

#[cfg(test)]
mod tests {
    use super::{SessionStorage, snippet};
    use crate::{
        app::config::{ReasoningEffort, ReasoningMode, Speed},
        core::{
            storage::database_path,
            transcript::{
                LocalEvent, SessionStarted, TranscriptRecord, TurnId, UserSteered, UserSubmitted,
            },
        },
    };
    use nanocodex::agent::events::{AgentEvent, AgentEventKind};
    use rusqlite::Connection;
    use serde_json::{json, value::to_raw_value};
    use std::{path::Path, sync::Arc};
    use tempfile::tempdir;

    fn local(sequence: u64, event: LocalEvent) -> Arc<TranscriptRecord> {
        Arc::new(TranscriptRecord::from_local(sequence, sequence, event).unwrap())
    }

    fn agent(
        sequence: u64,
        kind: AgentEventKind,
        payload: serde_json::Value,
    ) -> Arc<TranscriptRecord> {
        Arc::new(TranscriptRecord::from_agent(
            sequence,
            sequence,
            AgentEvent {
                protocol_version: 1,
                request_id: Arc::from("request"),
                seq: sequence,
                kind,
                payload: to_raw_value(&payload).unwrap().into(),
            },
        ))
    }

    fn started(session_id: &str) -> Arc<TranscriptRecord> {
        local(
            1,
            LocalEvent::SessionStarted(SessionStarted {
                session_id: session_id.to_owned(),
                parent_session_id: None,
                parent_sequence: None,
                model: "model".to_owned(),
                effort: ReasoningEffort::Medium,
                reasoning_mode: ReasoningMode::Standard,
                speed: Speed::Standard,
                workspace: "/work".into(),
                application_version: "test".to_owned(),
            }),
        )
    }

    fn prompt(sequence: u64, text: &str) -> Arc<TranscriptRecord> {
        local(
            sequence,
            LocalEvent::UserSubmitted(UserSubmitted {
                id: TurnId::new(sequence),
                text: text.to_owned(),
            }),
        )
    }

    fn assistant(sequence: u64, text: &str) -> Arc<TranscriptRecord> {
        agent(
            sequence,
            AgentEventKind::AssistantMessage,
            json!({ "model_call_index": sequence, "phase": null, "text": text }),
        )
    }

    /// Stores a session whose conversation is `messages`, alternating prompts and replies.
    fn store_session(storage: &mut SessionStorage, session_id: &str, messages: &[&str]) {
        let mut records = vec![started(session_id)];
        for (index, text) in messages.iter().enumerate() {
            let sequence = index as u64 + 2;
            records.push(if index % 2 == 0 {
                prompt(sequence, text)
            } else {
                assistant(sequence, text)
            });
        }
        storage.append_records(session_id, &records).unwrap();
    }

    fn found(storage: &SessionStorage, search: &str) -> Vec<String> {
        let mut sessions = storage
            .search_conversations(search)
            .unwrap()
            .into_keys()
            .collect::<Vec<_>>();
        sessions.sort();
        sessions
    }

    fn indexed_messages(config: &Path) -> i64 {
        Connection::open(database_path(config))
            .unwrap()
            .query_row("SELECT count(*) FROM conversation_messages", [], |row| {
                row.get(0)
            })
            .unwrap()
    }

    #[test]
    fn prompts_steers_and_replies_are_searchable_but_tool_output_is_not() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let mut storage = SessionStorage::open(&config).unwrap();
        storage
            .append_records(
                "session",
                &[
                    started("session"),
                    prompt(2, "Fix the Parser please"),
                    local(
                        3,
                        LocalEvent::UserSteered(UserSteered {
                            text: "also the tokenizer".to_owned(),
                        }),
                    ),
                    agent(
                        4,
                        AgentEventKind::ToolCall,
                        json!({ "name": "exec", "arguments": "grep calltoken" }),
                    ),
                    agent(
                        5,
                        AgentEventKind::ToolResult,
                        json!({ "output": "resulttoken" }),
                    ),
                    assistant(6, "The lexer was rewritten"),
                ],
            )
            .unwrap();

        assert_eq!(found(&storage, "parser"), ["session"]);
        assert_eq!(found(&storage, "TOKENIZER"), ["session"]);
        assert_eq!(found(&storage, "lexer was"), ["session"]);
        assert!(found(&storage, "calltoken").is_empty());
        assert!(found(&storage, "resulttoken").is_empty());
    }

    #[test]
    fn events_from_writers_that_do_not_know_the_index_are_indexed() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let mut storage = SessionStorage::open(&config).unwrap();
        store_session(&mut storage, "session", &["first prompt"]);
        let reply =
            serde_json::to_vec(assistant(3, "written by an older binary").as_ref()).unwrap();
        storage.append_raw_record("session", &reply).unwrap();

        assert_eq!(found(&storage, "older binary"), ["session"]);
    }

    #[test]
    fn backfill_indexes_earlier_events_once_across_concurrent_writers() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let mut storage = SessionStorage::open(&config).unwrap();
        store_session(&mut storage, "alpha", &["alpha prompt", "alpha reply"]);
        store_session(&mut storage, "beta", &["beta prompt", "beta reply"]);
        drop(storage);
        // A database written before the index existed.
        Connection::open(database_path(&config))
            .unwrap()
            .execute_batch(
                "DROP TRIGGER events_conversation_indexed;\n\
                 DROP TABLE conversation_index;\n\
                 DROP TABLE conversation_messages;\n\
                 DROP TABLE conversation_backfill;",
            )
            .unwrap();

        let mut first = SessionStorage::open(&config).unwrap();
        let mut second = SessionStorage::open(&config).unwrap();
        store_session(&mut first, "gamma", &["gamma prompt"]);
        assert_eq!(found(&first, "prompt"), ["gamma"]);

        let writers = [&mut first, &mut second];
        let mut remaining = [true, true];
        let mut turn = 0;
        while remaining.contains(&true) {
            let writer = turn % 2;
            if remaining[writer] {
                remaining[writer] = writers[writer].backfill_conversation_index(2).unwrap();
            }
            turn += 1;
        }

        assert_eq!(found(&first, "prompt"), ["alpha", "beta", "gamma"]);
        assert_eq!(found(&second, "reply"), ["alpha", "beta"]);
        assert_eq!(indexed_messages(&config), 5);
        assert!(!first.backfill_conversation_index(10).unwrap());
        assert_eq!(indexed_messages(&config), 5);
        Connection::open(database_path(&config))
            .unwrap()
            .execute(
                "INSERT INTO conversation_index(conversation_index) VALUES ('integrity-check')",
                [],
            )
            .unwrap();
    }

    #[test]
    fn search_text_is_matched_literally() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let mut storage = SessionStorage::open(&config).unwrap();
        store_session(
            &mut storage,
            "session",
            &[r#"run "quoted" NEAR(one two) with col:value and a*b OR NOT"#],
        );

        for search in [
            r#""quoted""#,
            "NEAR(one",
            "col:value",
            "a*b",
            "OR NOT",
            r#"""""#,
            "(((",
        ] {
            let sessions = storage.search_conversations(search).unwrap();
            let expected = !matches!(search, r#"""""# | "(((");
            assert_eq!(sessions.contains_key("session"), expected, "{search}");
        }
    }

    #[test]
    fn every_long_word_must_occur_in_one_message_and_short_words_are_ignored() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let mut storage = SessionStorage::open(&config).unwrap();
        store_session(&mut storage, "both", &["the parser rejects unicode"]);
        store_session(&mut storage, "split", &["the parser", "unicode handling"]);

        assert_eq!(found(&storage, "parser unicode"), ["both"]);
        assert_eq!(found(&storage, "a parser"), ["both", "split"]);
        assert!(found(&storage, "pa").is_empty());
        assert!(found(&storage, "").is_empty());
    }

    #[test]
    fn sessions_rank_by_their_best_message_with_a_snippet_of_it() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let mut storage = SessionStorage::open(&config).unwrap();
        let filler = "unrelated words ".repeat(20);
        store_session(
            &mut storage,
            "weak",
            &[&format!(
                "{filler}mentions the needle once\n\n   then {filler}"
            )],
        );
        store_session(&mut storage, "strong", &["needle needle needle"]);

        let matches = storage.search_conversations("NEEDLE").unwrap();
        assert!(matches["strong"].rank < matches["weak"].rank);
        assert_eq!(matches["strong"].snippet, "needle needle needle");
        let weak = &matches["weak"].snippet;
        assert!(weak.starts_with('…') && weak.ends_with('…'), "{weak}");
        assert!(weak.contains("mentions the needle once then"), "{weak}");
    }

    #[test]
    fn snippets_collapse_whitespace_and_start_before_the_earliest_match() {
        let terms = ["beta".to_owned(), "alpha".to_owned()];
        assert_eq!(
            snippet("one\n two  Alpha three", &terms),
            "one two Alpha three"
        );
        let long = format!("{} alpha {}", "x".repeat(40), "y".repeat(200));
        let excerpt = snippet(&long, &terms);
        assert!(
            excerpt.starts_with(&format!("…{} alpha", "x".repeat(23))),
            "{excerpt}"
        );
        assert_eq!(excerpt.chars().count(), super::SNIPPET_CHARS + 2);
    }

    #[test]
    fn databases_without_the_index_return_no_conversation_matches() {
        let directory = tempdir().unwrap();
        let config = directory.path().join("config.toml");
        let mut storage = SessionStorage::open(&config).unwrap();
        store_session(&mut storage, "session", &["needle"]);
        drop(storage);
        Connection::open(database_path(&config))
            .unwrap()
            .execute_batch(
                "DROP TRIGGER events_conversation_indexed; DROP TABLE conversation_index;",
            )
            .unwrap();

        let storage = SessionStorage::open_read_only(&config).unwrap().unwrap();
        assert!(storage.search_conversations("needle").unwrap().is_empty());
    }
}
