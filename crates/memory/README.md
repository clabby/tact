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
- `RemoteMemoryClient` and `MemoryServer` share versioned protocol types and preserve author
  namespaces across authenticated operations.
- `MemoryTool` exposes explicit scan, read, inspection, put, and delete operations to Nanocodex
  sessions under an application-provided mutation authority.

Each `MemoryStore` scan request returns one bounded candidate vector selected by a
`MemoryNamespaceFilter`. The agent-facing remote scan makes separate requests for the authenticated
namespace and all other visible namespaces, then exposes them as `ours` and `theirs`. Candidate keys
preserve ownership because the requests' BM25 scores are relative to different corpora.

Feature flags separate the local store, remote client, server, and Nanocodex tool integrations.
Default features enable the complete native client, local, server, and tool surface; server-only
deployments can select `server` without native-only dependencies.

Implementations enforce record, query, content, and aggregate corpus bounds. Client-side storage
boundaries reject secret-like content before mutation and suppress pre-existing unsafe records
before use. Server backends remain storage-policy agnostic.

See the [Tact memory guide](https://github.com/clabby/tact/blob/main/docs/memory.md) for backend
selection, protocol, authentication, transfer, and deployment contracts.
