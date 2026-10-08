# tact-memory

[![GitHub Actions Workflow Status](https://img.shields.io/github/actions/workflow/status/clabby/tact/ci.yaml?style=for-the-badge&label=CI)](https://github.com/clabby/tact/actions/workflows/ci.yaml)
[![Crates.io License](https://img.shields.io/crates/l/tact-memory?style=for-the-badge)](https://crates.io/crates/tact-memory)
[![Crates.io MSRV](https://img.shields.io/crates/msrv/tact-memory?style=for-the-badge)](https://crates.io/crates/tact-memory)
[![Crates.io Version](https://img.shields.io/crates/v/tact-memory?style=for-the-badge)](https://crates.io/crates/tact-memory)

`tact-memory` provides bounded memory storage and retrieval for local agents and shared teams. It
defines a common asynchronous store contract, a SQLite-backed local store, an authenticated remote
client and server protocol, and a Nanocodex memory tool.

The crate exposes four integration boundaries:

- `MemoryStore` defines ordinary local and remote operations over bounded memory records.
- `LocalMemoryStore` persists the schema-v1 local format, while `SelectedMemoryStore` lets an
  application choose one local or remote backend for a runtime.
- `RemoteMemoryClient` and `MemoryServer` speak the versioned HTTP protocol defined in
  `protocol` and preserve author namespaces across authenticated operations.
- `MemoryTool` exposes explicit scan, read, put, and delete operations to Nanocodex sessions under
  an application-provided mutation authority.

Feature flags separate the local store, remote client, server, and Nanocodex tool integrations.
Default features enable the complete native client, local, server, and tool surface; server-only
deployments can select `server` without native-only dependencies.

Implementations enforce record, query, content, and aggregate corpus bounds. `LocalMemoryStore`
and `RemoteMemoryClient` reject secret-like content before writing or sending it, and neither
returns unsafe records that are already stored. Server backends do not inspect content.

A local store needs only a database path and limits:

```rust,no_run
use tact_memory::{LocalMemoryStore, MemoryError, MemoryLimits, MemoryStore};

async fn remember() -> Result<(), MemoryError> {
    let store = LocalMemoryStore::new("memory.sqlite3", MemoryLimits::PRODUCTION);
    let stored = store.put("CI runs cargo nextest.", None).await?;

    let scan = store.scan("nextest", 5).await?;
    let keys: Vec<_> = scan.candidates.into_iter().map(|candidate| candidate.key).collect();
    let records = store.read(&[], &keys).await?;
    assert_eq!(records[0].key, stored.key);
    Ok(())
}
```

See the [Tact memory guide](https://github.com/clabby/tact/blob/main/docs/memory.md) for backend
selection, protocol, authentication, transfer, and deployment contracts.
