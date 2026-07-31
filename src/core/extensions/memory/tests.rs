use super::{MemoryError, MemoryKey, MemoryLimits, MemoryStore};
use rusqlite::{Connection, params};
use std::{sync::Barrier, thread};
use tempfile::TempDir;

fn store() -> (TempDir, MemoryStore) {
    let directory = tempfile::tempdir().unwrap();
    let store = MemoryStore::new(directory.path().join("memory/v1.sqlite3"));
    (directory, store)
}

fn tiny_limits() -> MemoryLimits {
    MemoryLimits {
        content_bytes: 32,
        records: 4,
        total_content_bytes: 64,
        database_bytes: 4 * 1_024 * 1_024,
        scan_results: 2,
        query_bytes: 16,
        probation_duration_ms: 10,
    }
}

#[test]
fn enforces_exact_ascii_and_unicode_byte_bounds() {
    let directory = tempfile::tempdir().unwrap();
    let limits = MemoryLimits {
        content_bytes: 8,
        total_content_bytes: 32,
        query_bytes: 8,
        ..tiny_limits()
    };
    let store = MemoryStore::with_limits(directory.path().join("memory.sqlite3"), limits);

    store.put("12345678", None, 0).unwrap();
    assert!(matches!(
        store.put("123456789", None, 0),
        Err(MemoryError::ContentTooLarge { maximum_bytes: 8 })
    ));
    store.put("éééé", None, 0).unwrap();
    assert!(matches!(
        store.put("ééééé", None, 0),
        Err(MemoryError::ContentTooLarge { maximum_bytes: 8 })
    ));

    store.scan("12345678", 1, 0).unwrap();
    store.scan("éééé", 1, 0).unwrap();
    assert!(matches!(
        store.scan("123456789", 1, 0),
        Err(MemoryError::QueryTooLarge { maximum_bytes: 8 })
    ));
    assert!(matches!(
        store.scan("ééééé", 1, 0),
        Err(MemoryError::QueryTooLarge { maximum_bytes: 8 })
    ));
}

#[test]
fn normalized_identity_deduplicates_case_and_whitespace() {
    let (_directory, store) = store();
    store.put("Remember  SQLite", None, 0).unwrap();

    assert!(matches!(
        store.put("  remember sqlite \n", None, 0),
        Err(MemoryError::Duplicate)
    ));
}

#[test]
fn replacement_preserves_id_checks_version_and_adjusts_accounting() {
    let directory = tempfile::tempdir().unwrap();
    let limits = MemoryLimits {
        content_bytes: 10,
        total_content_bytes: 10,
        ..tiny_limits()
    };
    let store = MemoryStore::with_limits(directory.path().join("memory.sqlite3"), limits);
    let original = store.put("123456", None, 1).unwrap();
    store.scan("123456", 1, 2).unwrap();
    store.read(&[original.key.id], 3).unwrap();

    let replacement = store.put("1234", Some(original.key), 4).unwrap();

    assert_eq!(replacement.key.id, original.key.id);
    assert_eq!(replacement.key.version, original.key.version + 1);
    assert_eq!(replacement.created_at_ms, original.created_at_ms);
    assert_eq!(replacement.updated_at_ms, 4);
    assert_eq!(replacement.scan_count, 0);
    assert_eq!(replacement.use_count, 0);
    assert_eq!(replacement.last_scanned_at_ms, None);
    assert_eq!(replacement.last_used_at_ms, None);
    assert_eq!(replacement.probation_until_ms, Some(14));
    store.put("abcdef", None, 4).unwrap();

    assert!(matches!(
        store.put("other", Some(original.key), 5),
        Err(MemoryError::Conflict)
    ));
    assert!(matches!(
        store.put("toolarge", None, 5),
        Err(MemoryError::ContentCapacity { maximum_bytes: 10 })
    ));
}

#[test]
fn record_capacity_is_derived_from_live_rows() {
    let directory = tempfile::tempdir().unwrap();
    let limits = MemoryLimits {
        records: 2,
        ..tiny_limits()
    };
    let store = MemoryStore::with_limits(directory.path().join("memory.sqlite3"), limits);
    let first = store.put("one", None, 0).unwrap();
    store.put("two", None, 0).unwrap();
    assert!(matches!(
        store.put("three", None, 0),
        Err(MemoryError::RecordCapacity { maximum: 2 })
    ));

    store.delete(first.key, 0).unwrap();
    store.put("three", None, 0).unwrap();
    assert_eq!(store.list(0).unwrap().len(), 2);
}

#[test]
fn delete_requires_the_current_version() {
    let (_directory, store) = store();
    let record = store.put("delete me", None, 0).unwrap();
    let stale = MemoryKey {
        id: record.key.id,
        version: record.key.version + 1,
    };

    assert!(matches!(store.delete(stale, 0), Err(MemoryError::Conflict)));
    store.delete(record.key, 0).unwrap();
    assert!(store.list(0).unwrap().is_empty());
    assert!(matches!(
        store.delete(record.key, 0),
        Err(MemoryError::NotFound)
    ));
}

#[test]
fn probation_prunes_at_the_exact_deadline() {
    let directory = tempfile::tempdir().unwrap();
    let store = MemoryStore::with_limits(directory.path().join("memory.sqlite3"), tiny_limits());
    store.put("expires", None, 100).unwrap();

    assert_eq!(store.list(109).unwrap().len(), 1);
    assert!(store.list(110).unwrap().is_empty());
}

#[test]
fn pre_expiry_read_clears_probation() {
    let directory = tempfile::tempdir().unwrap();
    let store = MemoryStore::with_limits(directory.path().join("memory.sqlite3"), tiny_limits());
    let record = store.put("keep this", None, 100).unwrap();

    let read = store.read(&[record.key.id], 109).unwrap();

    assert_eq!(read[0].probation_until_ms, None);
    assert_eq!(read[0].use_count, 1);
    assert_eq!(store.list(10_000).unwrap().len(), 1);
}

#[test]
fn scan_updates_only_returned_rows() {
    let (_directory, store) = store();
    let first = store.put("rust sqlite concise", None, 0).unwrap();
    let second = store
        .put("rust sqlite verbose padding words", None, 0)
        .unwrap();
    let unrelated = store.put("python network", None, 0).unwrap();

    let scan = store.scan("rust sqlite", 1, 1).unwrap();

    assert!(!scan.abstained);
    assert_eq!(scan.candidates.len(), 1);
    let records = store.list(1).unwrap();
    let scanned = records
        .iter()
        .find(|record| record.key.id == scan.candidates[0].key.id)
        .unwrap();
    assert_eq!(scanned.scan_count, 1);
    assert_eq!(scanned.last_scanned_at_ms, Some(1));
    for id in [first.key.id, second.key.id, unrelated.key.id] {
        let record = records.iter().find(|record| record.key.id == id).unwrap();
        if id != scanned.key.id {
            assert_eq!(record.scan_count, 0);
        }
    }

    let abstained = store.scan("no-overlap", 2, 2).unwrap();
    assert!(abstained.abstained);
    assert!(abstained.candidates.is_empty());
    assert_eq!(
        store
            .list(2)
            .unwrap()
            .iter()
            .map(|record| record.scan_count)
            .sum::<u64>(),
        1
    );
}

#[test]
fn scan_clamps_results_to_the_production_limit() {
    let (_directory, store) = store();
    for index in 0..6 {
        store.put(&format!("shared term {index}"), None, 0).unwrap();
    }

    let scan = store.scan("shared", usize::MAX, 1).unwrap();

    assert_eq!(scan.candidates.len(), 5);
}

#[test]
fn read_deduplicates_ids_and_updates_use_telemetry_once() {
    let (_directory, store) = store();
    let first = store.put("first", None, 0).unwrap();
    let second = store.put("second", None, 0).unwrap();

    let records = store
        .read(&[first.key.id, first.key.id, 999, second.key.id], 5)
        .unwrap();

    assert_eq!(records.len(), 2);
    assert_eq!(records[0].key.id, first.key.id);
    assert_eq!(records[1].key.id, second.key.id);
    assert!(records.iter().all(|record| record.use_count == 1));
    assert!(
        records
            .iter()
            .all(|record| record.last_used_at_ms == Some(5))
    );
    let listed = store.list(5).unwrap();
    assert!(listed.iter().all(|record| record.use_count == 1));
}

#[test]
fn concurrent_writers_preserve_every_record() {
    let (_directory, store) = store();
    let barrier = std::sync::Arc::new(Barrier::new(8));
    let threads = (0..8)
        .map(|index| {
            let store = store.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                store.put(&format!("concurrent record {index}"), None, 0)
            })
        })
        .collect::<Vec<_>>();

    for thread in threads {
        thread.join().unwrap().unwrap();
    }
    assert_eq!(store.list(0).unwrap().len(), 8);
}

#[test]
fn rejects_secrets_without_exposing_them_in_the_error() {
    let (_directory, store) = store();
    let secret = "password=hunter2";

    let error = store.put(secret, None, 0).unwrap_err();

    assert!(matches!(error, MemoryError::SecretRejected));
    assert!(!error.to_string().contains("hunter2"));
    assert!(store.list(0).unwrap().is_empty());
}

#[test]
fn suppresses_unsafe_legacy_rows_but_keeps_them_deletable() {
    let (_directory, store) = store();
    store.list(0).unwrap();
    let connection = store.open().unwrap();
    connection
        .execute(
            "INSERT INTO memories (
                content, normalized_identity, created_at_ms, updated_at_ms,
                scan_count, use_count, version
             ) VALUES (?1, 'legacy', 0, 0, 0, 0, 1)",
            ["password=hunter2"],
        )
        .unwrap();
    let id = connection.last_insert_rowid();
    drop(connection);

    assert!(store.scan("hunter2", 1, 0).unwrap().abstained);
    assert!(store.read(&[id], 0).unwrap().is_empty());
    assert!(store.list(0).unwrap().is_empty());
    store.delete(MemoryKey { id, version: 1 }, 0).unwrap();
}

#[test]
fn database_uses_delete_journaling_and_the_page_limit() {
    let (_directory, store) = store();
    store.put("integrity", None, 0).unwrap();
    let connection = store.open().unwrap();

    let journal_mode: String = connection
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    let page_size: i64 = connection
        .query_row("PRAGMA page_size", [], |row| row.get(0))
        .unwrap();
    let maximum_pages: i64 = connection
        .query_row("PRAGMA max_page_count", [], |row| row.get(0))
        .unwrap();
    let integrity: String = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .unwrap();

    assert_eq!(journal_mode, "delete");
    assert_eq!(page_size, 4 * 1_024);
    assert_eq!(maximum_pages * page_size, 4 * 1_024 * 1_024);
    assert_eq!(integrity, "ok");
}

#[test]
fn newer_database_schema_versions_are_rejected_without_relabeling_them() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("memory.sqlite3");
    let connection = Connection::open(&path).unwrap();
    connection.pragma_update(None, "user_version", 2).unwrap();
    drop(connection);
    let store = MemoryStore::new(path.clone());

    assert!(matches!(
        store.list(0),
        Err(MemoryError::UnsupportedSchemaVersion {
            found: 2,
            supported: 1
        })
    ));

    let connection = Connection::open(path).unwrap();
    let schema_version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(schema_version, 2);
}

#[test]
fn stores_opening_the_same_global_path_share_the_corpus() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("global/memory/v1.sqlite3");
    let first = MemoryStore::new(path.clone());
    let second = MemoryStore::new(path);

    let record = first.put("shared globally", None, 0).unwrap();

    assert_eq!(second.list(0).unwrap(), vec![record]);
}

#[test]
fn replacement_of_a_legacy_row_does_not_require_reading_its_content() {
    let (_directory, store) = store();
    store.list(0).unwrap();
    let connection = Connection::open(store.path.as_path()).unwrap();
    connection
        .execute(
            "INSERT INTO memories (
                content, normalized_identity, created_at_ms, updated_at_ms,
                scan_count, use_count, version
             ) VALUES (?1, 'legacy unsafe', 0, 0, 0, 0, 1)",
            params!["token=abcdefghijklmnop"],
        )
        .unwrap();
    let id = connection.last_insert_rowid();
    drop(connection);

    let replacement = store
        .put("safe replacement", Some(MemoryKey { id, version: 1 }), 1)
        .unwrap();

    assert_eq!(replacement.key, MemoryKey { id, version: 2 });
    assert_eq!(store.list(1).unwrap(), [replacement]);
}

#[cfg(unix)]
#[test]
fn creates_a_private_database_directory() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let parent = directory.path().join("memory");
    let store = MemoryStore::new(parent.join("v1.sqlite3"));
    store.list(0).unwrap();

    let mode = std::fs::metadata(parent).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700);
}
