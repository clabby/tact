use super::{PRUNE_SQL, SCAN_SQL, SCAN_UPDATE_SQL, literal_match_query, row::CandidateRow};
use rusqlite::{Connection, params};
use tact_memory::MemoryCandidate;

const MIGRATION: &str = include_str!("../../migrations/0002_search.sql");

struct Database(Connection);

impl Database {
    fn legacy() -> Self {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch("PRAGMA foreign_keys = ON")
            .unwrap();
        connection
            .execute_batch(include_str!("../../migrations/0001_initial.sql"))
            .unwrap();
        Self(connection)
    }

    fn new() -> Self {
        let database = Self::legacy();
        database.0.execute_batch(MIGRATION).unwrap();
        database
    }

    fn insert(&self, namespace: &str, id: i64, content: &str) {
        self.0
            .execute(
                "INSERT INTO memory_namespaces(namespace) VALUES (?) ON CONFLICT DO NOTHING",
                [namespace],
            )
            .unwrap();
        self.0.execute("INSERT INTO memories(namespace,id,version,content,identity,created_at_ms,updated_at_ms) VALUES(?,?,1,?,?,1,1)", params![namespace,id,content,format!("{namespace}-{id}")]).unwrap();
    }

    fn scan(&self, query: &str, caller: &str, limit: i64) -> Vec<MemoryCandidate> {
        let expression = literal_match_query(query);
        self.0
            .prepare(SCAN_SQL)
            .unwrap()
            .query_map(
                params![expression, "100", caller, limit.to_string()],
                |row| {
                    let candidate: CandidateRow = serde_json::from_value(serde_json::json!({
                        "namespace": row.get::<_,String>("namespace")?,
                        "id": row.get::<_,String>("id")?,
                        "version": row.get::<_,String>("version")?,
                        "preview": row.get::<_,String>("preview")?,
                        "score": row.get::<_,f64>("score")?,
                    }))
                    .unwrap();
                    Ok(candidate.try_into().unwrap())
                },
            )
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    fn assert_consistent(&self) {
        let counts: (i64,i64,i64,i64) = self.0.query_row("SELECT (SELECT count(*) FROM memories), (SELECT count(*) FROM memory_search_documents), (SELECT count(*) FROM memory_search), (SELECT count(*) FROM memories m JOIN memory_search_documents d USING(namespace,id) JOIN memory_search f ON f.rowid=d.search_id WHERE m.content = f.content)", [], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).unwrap();
        assert_eq!(counts, (counts.0, counts.0, counts.0, counts.0));
        self.0
            .execute(
                "INSERT INTO memory_search(memory_search) VALUES('integrity-check')",
                [],
            )
            .unwrap();
    }
}

#[test]
fn populated_migration_and_vacuum_preserve_explicit_index_identity() {
    let database = Database::legacy();
    database.insert("a", 1, "remove");
    database.insert("a", 2, "survivor");
    database.0.execute_batch(MIGRATION).unwrap();
    database
        .0
        .execute("DELETE FROM memories WHERE id=1", [])
        .unwrap();
    let identity: i64 = database
        .0
        .query_row("SELECT search_id FROM memory_search_documents", [], |row| {
            row.get(0)
        })
        .unwrap();
    database.0.execute_batch("VACUUM").unwrap();
    assert_eq!(
        database
            .0
            .query_row("SELECT search_id FROM memory_search_documents", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap(),
        identity
    );
    assert_eq!(database.scan("survivor", "a", 10)[0].key.id, 2);
    database.assert_consistent();
}

#[test]
fn canonical_writers_maintain_search_through_replace_prune_sync_cascade_and_rollback() {
    let database = Database::new();
    database.insert("a", 1, "original");
    database.insert("b", 1, "foreign");
    database
        .0
        .execute(
            "UPDATE memories SET content='replacement',version=2 WHERE namespace='a'",
            [],
        )
        .unwrap();
    assert!(database.scan("original", "a", 10).is_empty());
    assert_eq!(database.scan("replacement", "a", 10)[0].key.version, 2);
    database.0.execute_batch("BEGIN; DELETE FROM memories WHERE namespace='a';
        INSERT INTO memories(namespace,id,version,content,identity,created_at_ms,updated_at_ms) VALUES('a',8,3,'snapshot','snapshot',1,1);
        COMMIT;").unwrap();
    assert!(database.scan("replacement", "a", 10).is_empty());
    assert_eq!(database.scan("snapshot", "a", 10)[0].key.id, 8);
    database
        .0
        .execute_batch("BEGIN; DELETE FROM memories WHERE namespace='a';")
        .unwrap();
    assert!(database.0.execute("INSERT INTO memories(namespace,id,version,content,identity,created_at_ms,updated_at_ms) VALUES('missing',1,1,'invalid','invalid',1,1)", []).is_err());
    database.0.execute_batch("ROLLBACK").unwrap();
    assert_eq!(database.scan("snapshot", "a", 10).len(), 1);
    database
        .0
        .execute(
            "UPDATE memories SET probation_until_ms=50 WHERE namespace='a'",
            [],
        )
        .unwrap();
    assert!(database.scan("snapshot", "a", 10).is_empty());
    database.assert_consistent();
    database.0.execute(PRUNE_SQL, ["100"]).unwrap();
    database
        .0
        .execute("DELETE FROM memory_namespaces WHERE namespace='b'", [])
        .unwrap();
    database.assert_consistent();
    assert!(database.scan("snapshot foreign", "a", 10).is_empty());
}

#[test]
fn failed_migration_rolls_back_schema_and_backfill() {
    let database = Database::legacy();
    database.insert("a", 1, "existing");
    database.0.execute_batch("BEGIN").unwrap();
    database.0.execute_batch(MIGRATION).unwrap();
    assert!(
        database
            .0
            .execute(
                "INSERT INTO memory_search_documents(namespace,id) VALUES('a',1)",
                []
            )
            .is_err()
    );
    database.0.execute_batch("ROLLBACK").unwrap();
    assert_eq!(
        database
            .0
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE name='memory_search'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    database.0.execute_batch(MIGRATION).unwrap();
    database.assert_consistent();
    assert_eq!(database.scan("existing", "a", 10).len(), 1);
}

#[test]
fn literal_match_any_handles_operators_punctuation_unicode_and_short_terms() {
    let database = Database::new();
    for (id, content) in [
        (1, "OR NOT NEAR"),
        (2, "café 東京"),
        (3, "x a"),
        (4, "snake_case camelCase"),
        (5, "alpha"),
        (6, "beta"),
    ] {
        database.insert("a", id, content);
    }
    assert!(database.scan("\"() + - _ ** :", "a", 10).is_empty());
    for query in [
        "OR",
        "\"NOT\"",
        "NEAR(alpha)",
        "cafe",
        "東京",
        "x",
        "a",
        "snake",
        "case",
        "CAMELCASE",
    ] {
        assert!(!database.scan(query, "a", 10).is_empty(), "{query}");
    }
    assert!(database.scan("camel", "a", 10).is_empty());
    let found = database.scan("alpha-beta", "a", 10);
    assert_eq!(found.len(), 2);
    assert_eq!(
        literal_match_query("\"alpha\" OR beta:*"),
        "\"alpha\" OR \"beta\" OR \"or\""
    );
}

#[test]
fn full_match_ranking_weights_every_own_result_and_uses_declared_ties() {
    let database = Database::new();
    for namespace in ["a", "B", "z"] {
        for id in [12, 2, 1, 3, 4, 5, 6, 7, 8, 9, 10, 11] {
            database.insert(namespace, id, "matching equal");
        }
    }
    for caller in ["missing", "z", "a", "B"] {
        let mut expected = database.0.prepare("SELECT namespace,id,-bm25(memory_search) FROM memory_search JOIN memory_search_documents ON search_id=memory_search.rowid WHERE memory_search MATCH ?").unwrap().query_map(["\"matching\""], |row| Ok((row.get::<_,String>(0)?,row.get::<_,i64>(1)?,row.get::<_,f64>(2)?))).unwrap().collect::<Result<Vec<_>,_>>().unwrap();
        expected.sort_by(|a, b| {
            let adjusted =
                |row: &(String, i64, f64)| row.2 * if row.0 == caller { 1.25 } else { 1.0 };
            adjusted(b)
                .total_cmp(&adjusted(a))
                .then(b.2.total_cmp(&a.2))
                .then(a.0.cmp(&b.0))
                .then(a.1.cmp(&b.1))
        });
        let actual = database.scan("matching", caller, 100);
        assert_eq!(actual.len(), 10);
        for (actual, expected) in actual.iter().zip(expected) {
            assert_eq!(actual.key.namespace.as_deref(), Some(expected.0.as_str()));
            assert_eq!(actual.key.id, expected.1);
            assert_eq!(
                actual.score,
                expected.2 * if expected.0 == caller { 1.25 } else { 1.0 }
            );
            assert!(actual.score > 0.0);
        }
    }
    assert!(database.scan("matching", "a", 0).is_empty());
    assert!(database.scan("matching", "a", -1).is_empty());
    assert_eq!(database.scan("matching", "z", 3).len(), 3);
}

#[test]
fn finalists_keep_exact_integers_utf8_previews_and_version_specific_telemetry() {
    let database = Database::new();
    let id = 9_007_199_254_740_993_i64;
    let version = i64::MAX;
    database.insert("a", id, &format!("match {}", "界".repeat(40)));
    database.insert("a", 2, "match other");
    database
        .0
        .execute(
            "UPDATE memories SET version=?,probation_until_ms=200 WHERE id=?",
            params![version, id],
        )
        .unwrap();
    let selected = database.scan("界", "a", 1);
    // unicode61 treats the uninterrupted CJK run as one token.
    assert!(selected.is_empty());
    let selected = database.scan(&"界".repeat(40), "a", 1);
    let candidate = &selected[0];
    assert_eq!(candidate.key.id, id);
    assert_eq!(candidate.key.version, version as u64);
    assert_eq!(candidate.preview.len(), 63);
    assert!(candidate.preview.is_char_boundary(candidate.preview.len()));
    assert_eq!(
        database
            .0
            .execute(
                SCAN_UPDATE_SQL,
                params!["100", "a", id.to_string(), (version - 1).to_string()]
            )
            .unwrap(),
        0
    );
    assert_eq!(
        database
            .0
            .execute(
                SCAN_UPDATE_SQL,
                params!["100", "a", id.to_string(), version.to_string()]
            )
            .unwrap(),
        1
    );
    assert_eq!(
        database
            .0
            .query_row(
                "SELECT scan_count,probation_until_ms,use_count FROM memories WHERE id=?",
                [id],
                |row| Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?
                ))
            )
            .unwrap(),
        (1, 200, 0)
    );
    assert_eq!(
        database
            .0
            .query_row("SELECT scan_count FROM memories WHERE id=2", [], |row| row
                .get::<_, i64>(
                0
            ))
            .unwrap(),
        0
    );
    database.assert_consistent();
}

#[test]
fn scans_above_old_corpus_budgets_return_only_finalists() {
    let database = Database::new();
    database.0.execute_batch("BEGIN").unwrap();
    let content = format!("matching {}", "payload ".repeat(65));
    for id in 1..=10_241 {
        database.insert("a", id, &content);
    }
    database.0.execute_batch("COMMIT").unwrap();
    let candidates = database.scan("matching", "a", 10);
    assert_eq!(candidates.len(), 10);
    assert_eq!(
        candidates
            .iter()
            .map(|candidate| candidate.key.id)
            .collect::<Vec<_>>(),
        (1..=10).collect::<Vec<_>>()
    );
    assert!(
        candidates
            .iter()
            .all(|candidate| candidate.preview.len() == 64)
    );
    database.assert_consistent();
}

#[test]
fn near_tied_own_results_beyond_global_top_fifty_receive_full_weight() {
    let database = Database::new();
    for id in 1..=50 {
        database.insert("foreign", id, "matching matching payload");
    }
    for id in 1..=10 {
        database.insert("own", id, "matching matching payload extra");
    }
    let unweighted = database.scan("matching", "absent", 10);
    assert!(
        unweighted
            .iter()
            .all(|candidate| candidate.key.namespace.as_deref() == Some("foreign"))
    );
    let weighted = database.scan("matching", "own", 10);
    assert!(
        weighted
            .iter()
            .all(|candidate| candidate.key.namespace.as_deref() == Some("own"))
    );
    let raw_own: f64 = database.0.query_row("SELECT -bm25(memory_search) FROM memory_search JOIN memory_search_documents ON search_id=memory_search.rowid WHERE memory_search MATCH 'matching' AND namespace='own' LIMIT 1", [], |row| row.get(0)).unwrap();
    assert!(raw_own < unweighted[0].score);
    assert!(
        weighted
            .iter()
            .all(|candidate| candidate.score == raw_own * 1.25)
    );
}

#[test]
fn visibility_filters_do_not_change_index_statistics_until_pruning() {
    let database = Database::new();
    database.insert("a", 1, "matching");
    database.insert("a", 2, "matching many extra words");
    database.insert("a", 3, "matching used");
    let before = database.scan("matching", "a", 10);
    database
        .0
        .execute(
            "UPDATE memories SET probation_until_ms=50 WHERE id IN (2,3)",
            [],
        )
        .unwrap();
    database
        .0
        .execute("UPDATE memories SET use_count=1 WHERE id=3", [])
        .unwrap();
    let visible = database.scan("matching", "a", 10);
    assert_eq!(visible.len(), 2);
    for candidate in &visible {
        assert_eq!(
            candidate.score,
            before
                .iter()
                .find(|old| old.key == candidate.key)
                .unwrap()
                .score
        );
    }
    database.0.execute(PRUNE_SQL, ["100"]).unwrap();
    let after = database.scan("matching", "a", 10);
    assert_eq!(after.len(), 2);
    assert_ne!(after[0].score, visible[0].score);
    database.assert_consistent();
}

#[test]
fn combining_marks_follow_unicode61_token_boundaries() {
    let database = Database::new();
    database.insert("a", 1, "école résumé xylophone");
    database.insert("a", 2, "alpha");
    database.insert("a", 3, "beta");
    for query in [
        "e\u{0301}cole",
        "re\u{0301}sume\u{0301}",
        "x\u{0301}ylophone",
    ] {
        assert_eq!(database.scan(query, "a", 10)[0].key.id, 1, "{query}");
    }
    let candidates = database.scan("alpha\u{0345}beta", "a", 10);
    assert_eq!(
        candidates
            .iter()
            .map(|candidate| candidate.key.id)
            .collect::<Vec<_>>(),
        [2, 3]
    );
    assert!(database.scan("\u{0301}\u{0345}", "a", 10).is_empty());
}

#[test]
fn query_categories_include_private_use_and_exclude_alphabetic_symbols() {
    let database = Database::new();
    database.insert("a", 1, "alpha");
    database.insert("a", 2, "beta");
    database.insert("a", 3, "\u{e000}");
    assert_eq!(database.scan("\u{e000}", "a", 10)[0].key.id, 3);
    let candidates = database.scan("alpha\u{24b6}beta", "a", 10);
    assert_eq!(
        candidates
            .iter()
            .map(|candidate| candidate.key.id)
            .collect::<Vec<_>>(),
        [1, 2]
    );
}

#[test]
fn repeated_case_variants_do_not_change_literal_term_scores_or_order() {
    let database = Database::new();
    database.insert("a", 1, "beta");
    database.insert("a", 2, "alpha");
    let expected = database.scan("alpha beta", "a", 10);
    assert_eq!(expected[0].key.id, 1);
    assert_eq!(expected[0].score, expected[1].score);
    for query in [
        "alpha beta alpha",
        "alpha beta ALPHA",
        "BETA alpha ALPHA beta",
    ] {
        assert_eq!(database.scan(query, "a", 10), expected);
    }
}
