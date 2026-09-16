# tact-memory-cloudflare

`tact-memory-cloudflare` runs Tact's shared memory service on Cloudflare Workers with D1 storage.
It mounts the authenticated Axum server from `tact-memory` over a D1 implementation of
`MemoryStore`, keeping the wire protocol and storage behavior consistent with other Tact memory
backends.

The reusable protocol, server, and storage contracts live in `tact-memory`. This package contains
the Cloudflare Worker entry point, D1 adapter, migration, credential loading, and deployment
configuration.

## Local development

Install Bun, the Worker build tool, and JavaScript dependencies, then create the local database:

```sh
cargo install worker-build --version 0.8.5 --locked
cd examples/tact-memory-cloudflare
bun ci
cp credentials.example.toml credentials.toml
chmod 600 credentials.toml
bun run migrate:local
```

Edit the ignored, mode-`0600` `credentials.toml` and add one table per bearer token:

```toml
[[credentials]]
namespace = "alice"
role = "writer"
token = "replace-with-a-high-entropy-token"

[[credentials]]
namespace = "auditor"
role = "reader"
token = "replace-with-an-independent-token"
```

A reader can scan, read, list, and export the shared corpus. A writer can also mutate records in
its own namespace. Tokens must be unique across the file. `bun run dev` validates the TOML and
writes the Worker's ignored `.dev.vars` file with mode `0600` before starting Wrangler.

Start the Worker:

```sh
bun run dev
```

Configure Tact with the address printed by Wrangler and the namespace associated with the token:

```toml
[memory.remote]
endpoint = "http://127.0.0.1:8787"
namespace = "alice"
bearer_token = "replace-with-a-high-entropy-token"
workspace_roots = ["/absolute/path/to/team/workspace"]
```

## Namespace capacity

Set these positive integer limits in `wrangler.jsonc`. Each writer namespace has its own capacity,
with the same configured limits applied to every namespace:

| Variable | Limit | Default |
| --- | --- | --- |
| `TACT_MEMORY_MAX_RECORDS` | Number of records | 512 |
| `TACT_MEMORY_MAX_RECORD_BYTES` | UTF-8 content bytes in one record | 1,024 (1 KiB) |
| `TACT_MEMORY_MAX_TOTAL_BYTES` | Total UTF-8 content bytes across records | 262,144 (256 KiB) |

The limits are independent. Increasing the record count does not change either byte limit.
Changing a limit does not delete existing records. Inserts and snapshot syncs enforce count and
content capacity. Replacements enforce the per-record limit and resulting total content bytes.

`TACT_MEMORY_MAX_REQUEST_BYTES` separately limits the encoded JSON request body and defaults to
2,097,152 bytes (2 MiB). Set it to a positive integer in `wrangler.jsonc`. The body includes record
metadata and JSON escaping overhead as well as authored content. A full snapshot sync remains one
atomic request, so raise this bound when larger snapshots need it. The client accepts responses up
to 8 MiB.

## Retrieval limits

The memory browser lists at most 512 visible records. The Worker selects the authenticated
namespace first in numeric ID order, then fills remaining slots from other namespaces in namespace
and numeric ID order. This prevents a large shared corpus from hiding the user's namespace. The
list remains a bounded window; use export to retrieve every record.

D1 selects scan results through a maintained FTS5 index. Prepared SQL performs literal matching,
visibility filtering, BM25 scoring, the authenticated caller's 1.25 weight, and the final limit.
Only the requested candidates, at most ten, reach the Worker. Queries remain bounded to 512 UTF-8
bytes and previews to 64 bytes. Scans no longer load the shared corpus or impose a shared record or
content budget. Per-namespace storage and request limits still apply.

The [retrieval contract](../../docs/memory.md#record-and-retrieval-contract) defines score direction,
ties, tokenization, and the effect of expired records on index statistics. SQL orders all matching
visible records by adjusted score, raw score, namespace, and numeric ID. This has no arbitrary
candidate prelimit: a caller-owned result can enter the final window even when it lies outside the
global unweighted top ten. Broad queries can still visit many database rows. Monitor D1 work and
latency as the corpus grows; bounded Worker results do not imply constant database work.

## Read replication

The Worker opens a first-unconstrained D1 session for a request without a bookmark. After read
replication is enabled on the database, indexed scan queries, list, and export may be served by a
replica. A D1-backed request without a bookmark can observe a stale snapshot, including temporarily
missing a recent write or returning a recently deleted record. Full record reads and mutations
update durable state and remain primary-bound. The remote client carries each returned bookmark
into its next request, so one client and its clones form a monotonic logical session across requests
and observe their successful writes.

## Deployment

Create the D1 database:

```sh
bun x wrangler d1 create tact-memory
```

Replace `REPLACE_WITH_D1_DATABASE_ID` in `wrangler.jsonc` with the returned database ID. Validate the
bundle, deploy the Worker, and then upload the validated TOML as its
encrypted secret:

```sh
bun run deploy:check
bun run deploy
bun run credentials:push
```

The uploader sends the secret directly to Wrangler over standard input. It does not create an
intermediate plaintext deployment file.

`bun run deploy` applies outstanding D1 migrations and verifies the index before deploying the
Worker. A migration or verification failure stops deployment. Use this entry point rather than
invoking `wrangler deploy` directly. `deploy:check` only builds the bundle; it does not change a
database or activate the Worker.

Migration `0002_search.sql` backfills existing records and installs insert, update, and delete
triggers in the same migration transaction. The index uses a separate `INTEGER PRIMARY KEY` as its
internal document identity; public keys remain `(namespace, id, version)`. Canonical writes maintain
the index in their transaction, including namespace snapshot replacement and cascading deletion.
The triggers also cover Workers running the preceding version during rollout. A failed migration
leaves the old schema and Worker usable; investigate the failure before retrying activation.

Roll back the Worker version without reverting this additive migration. The preceding Worker can
still read and write the canonical tables, and its writes keep the index current. Restore its
`TACT_MEMORY_SCAN_MAX_RECORDS` and `TACT_MEMORY_SCAN_MAX_CONTENT_BYTES` variables with its previous
configuration if rolling back to the corpus-loading version. Its old corpus limits still apply, so
a database that has grown beyond those limits may support mutations but fail scans on that version.
Keep backups for recovery from data loss; Worker rollback does not undo database mutations.

## Indexed retrieval validation

Build the production Worker and run the local D1 acceptance harness with an empty scratch directory:

```sh
bun run build
bun run test:indexed /absolute/path/to/empty-scratch-directory
```

The harness uses synthetic credentials and isolated local Wrangler state. It migrates a populated
pre-index database, injects a failed migration, and exercises production HTTP scans and mutations,
old canonical writers, snapshot sync, pruning, cascading deletion, and transaction rollback. It also
checks caller weighting, final-only telemetry, literal queries, bounds, and exact large integer keys.
It writes `measurements.json` and `worker.log` in the supplied directory. Measurements include D1
rows read/written and query duration, response bytes, request latency, and local inspector CPU/heap
observations. Local measurements are acceptance evidence, not Cloudflare production CPU billing or
replica-lag measurements.

A local run on September 16, 2026 with Wrangler 4.123.0 used 12,001 records containing 6,084,507
UTF-8 bytes. Every record matched the broad query, including a caller-owned record outside the
unweighted global result window. The run returned ten candidates and passed the acceptance checks.

| Measurement | Observed value |
| --- | ---: |
| D1 selection rows read / written | 60,005 / 0 |
| D1 maintenance rows read / written | 11 / 10 |
| D1 selection / maintenance duration | 11 ms / 1 ms |
| Finalist projection returned to Worker | 1,366 bytes |
| HTTP candidate response | 1,460 bytes |
| Local request wall time | 19.2 ms |
| Inspector sampled active CPU | 3.4 ms |
| Inspector used heap before / after request | 1,402,960 / 1,555,380 bytes |

These are single-run observations after warm-up, not latency percentiles. The inspector's used heap
is a JavaScript heap observation, not a process RSS or total WASM-memory measurement. Selection work
still scales with the matching set; the Worker result bound is independent of corpus size.

The Workers Free plan has daily request and CPU limits, while the Paid plan removes the daily
request cap and raises execution limits. D1 Time Travel retains 7 days on Free and 30 days on
Paid. Review Cloudflare's current [Workers limits](https://developers.cloudflare.com/workers/platform/limits/)
and [D1 limits](https://developers.cloudflare.com/d1/platform/limits/) when sizing a deployment.

## Operations

Cloudflare Workers Logs receive structured operation metadata without memory content, scan
queries, authorization headers, bearer tokens, or token hashes. Export snapshots outside D1 when
recovery requirements exceed the Time Travel retention window.

To rotate a credential, add another `[[credentials]]` table with the same role and namespace,
upload it, update clients, and then remove the old table and upload again. The Worker accepts both
tokens during the transition and rejects duplicate tokens.
