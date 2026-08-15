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

## Retrieval limits

Tact performs exact BM25 ranking over a bounded shared corpus inside the Worker.
`TACT_MEMORY_SCAN_MAX_RECORDS` and `TACT_MEMORY_SCAN_MAX_CONTENT_BYTES` bound the records and
authored content loaded from D1 for one scan. The defaults permit 10,240 records and 5 MiB of
content.

Choose limits from observed corpus size and Worker CPU usage. When the corpus exceeds either
bound, scans return a storage-capacity error. Export and mutation operations remain available so
operators can reduce or migrate the corpus without editing D1 directly.

## Deployment

Create the D1 database:

```sh
bun x wrangler d1 create tact-memory
```

Replace `REPLACE_WITH_D1_DATABASE_ID` in `wrangler.jsonc` with the returned database ID. Apply the
migration, validate the bundle, deploy the Worker, and then upload the validated TOML as its
encrypted secret:

```sh
bun run migrate:remote
bun run deploy:check
bun run deploy
bun run credentials:push
```

The uploader sends the secret directly to Wrangler over standard input. It does not create an
intermediate plaintext deployment file.

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
