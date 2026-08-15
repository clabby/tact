# Tact memory on Cloudflare

This crate deploys Tact's shared memory protocol as a Rust Cloudflare Worker backed directly by
D1. The Worker reuses `tact-memory`'s authenticated Axum server and supplies only the D1
`MemoryStore` implementation.

Use a Workers Paid plan for production so traffic is not interrupted by daily Free-plan limits and
the database retains 30 days of point-in-time history.

## Local development

Install the Worker build tool and JavaScript dependencies:

```sh
cargo install worker-build --version 0.8.5 --locked
cd crates/memory-cloudflare
npm ci
cp .dev.vars.example .dev.vars
npm run migrate:local
npm run dev
```

Edit `.dev.vars` with independent high-entropy tokens. Each non-empty credential line has the
form `ROLE NAMESPACE TOKEN`, where `ROLE` is `reader` or `writer`. A reader can inspect and export
the complete team corpus. A writer can additionally mutate only its own namespace.

Configure Tact against the address printed by Wrangler, using the matching namespace and token.

## Scan budget

Tact's exact BM25 search ranks the shared corpus inside the Worker. The
`TACT_MEMORY_SCAN_MAX_RECORDS` and `TACT_MEMORY_SCAN_MAX_CONTENT_BYTES` variables bound that work
before D1 returns records to WebAssembly. The included values allow 10,240 records and 5 MiB of
authored content; adjust them for the deployment's corpus and measured Worker CPU/memory use. A
scan returns a storage-capacity error when either bound is exceeded, while export and mutation
operations remain available so an operator can recover without direct database edits.

## Production deployment

Create the database:

```sh
npx wrangler d1 create tact-memory
```

Replace `REPLACE_WITH_D1_DATABASE_ID` in `wrangler.jsonc` with the returned ID. Store the production
credential document without writing it to disk:

```sh
npx wrangler secret put TACT_MEMORY_CREDENTIALS
```

Then migrate and deploy:

```sh
npm run migrate:remote
npm run deploy:check
npm run deploy
```

Cloudflare Workers Logs receive structured, content-free operation records. Memory content,
queries, authorization headers, bearer tokens, and token hashes are never logged. D1 Time Travel
provides point-in-time recovery; production operators should also export snapshots outside its
retention window when longer retention is required.

## Credential rotation

Generate a replacement token, include old and new credential lines temporarily for the same
namespace, update clients, and then remove the old credential. Duplicate tokens are rejected when
the Worker builds the authenticated server.
