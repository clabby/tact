CREATE TABLE memory_namespaces (
    namespace TEXT PRIMARY KEY NOT NULL,
    next_id INTEGER NOT NULL DEFAULT 1 CHECK (next_id > 0)
) STRICT;

CREATE TABLE memories (
    namespace TEXT NOT NULL,
    id INTEGER NOT NULL CHECK (id > 0),
    version INTEGER NOT NULL CHECK (version > 0),
    content TEXT NOT NULL,
    identity TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    last_scanned_at_ms INTEGER,
    scan_count INTEGER NOT NULL DEFAULT 0 CHECK (scan_count >= 0),
    last_used_at_ms INTEGER,
    use_count INTEGER NOT NULL DEFAULT 0 CHECK (use_count >= 0),
    probation_until_ms INTEGER,
    PRIMARY KEY (namespace, id),
    UNIQUE (namespace, identity),
    FOREIGN KEY (namespace) REFERENCES memory_namespaces(namespace) ON DELETE CASCADE
) STRICT;

CREATE INDEX memories_probation ON memories(probation_until_ms)
WHERE probation_until_ms IS NOT NULL AND use_count = 0;
