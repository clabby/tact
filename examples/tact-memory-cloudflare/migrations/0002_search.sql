-- The explicit INTEGER PRIMARY KEY remains stable across VACUUM. Canonical writers need no
-- knowledge of this SQL-only identity, including Workers running before this migration.
CREATE TABLE memory_search_documents (
    search_id INTEGER PRIMARY KEY,
    namespace TEXT NOT NULL,
    id INTEGER NOT NULL,
    UNIQUE (namespace, id)
) STRICT;

CREATE VIRTUAL TABLE memory_search USING fts5(content, tokenize = 'unicode61');

CREATE TRIGGER memories_search_insert AFTER INSERT ON memories BEGIN
    INSERT INTO memory_search_documents(namespace, id) VALUES (new.namespace, new.id);
    INSERT INTO memory_search(rowid, content)
        SELECT search_id, new.content FROM memory_search_documents
        WHERE namespace = new.namespace AND id = new.id;
END;

CREATE TRIGGER memories_search_update AFTER UPDATE OF namespace, id, content ON memories BEGIN
    UPDATE memory_search SET content = new.content WHERE rowid = (
        SELECT search_id FROM memory_search_documents WHERE namespace = old.namespace AND id = old.id
    );
    UPDATE memory_search_documents SET namespace = new.namespace, id = new.id
        WHERE namespace = old.namespace AND id = old.id;
END;

CREATE TRIGGER memories_search_delete AFTER DELETE ON memories BEGIN
    DELETE FROM memory_search WHERE rowid = (
        SELECT search_id FROM memory_search_documents WHERE namespace = old.namespace AND id = old.id
    );
    DELETE FROM memory_search_documents WHERE namespace = old.namespace AND id = old.id;
END;

INSERT INTO memory_search_documents(namespace, id) SELECT namespace, id FROM memories;
INSERT INTO memory_search(rowid, content)
    SELECT search_id, content FROM memory_search_documents JOIN memories USING (namespace, id);
