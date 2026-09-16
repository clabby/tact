import assert from "node:assert/strict";
import { mkdir, copyFile, readdir, chmod } from "node:fs/promises";
import { resolve, join } from "node:path";
import { fileURLToPath } from "node:url";
import { activate } from "./activate";

const root = fileURLToPath(new URL("../", import.meta.url));
const scratch = Bun.argv[2];
if (!scratch) throw new Error("usage: bun run scripts/indexed.ts <empty-scratch-directory>");
await mkdir(scratch, { recursive: true });
assert.equal((await readdir(scratch)).length, 0, "use an empty directory for isolated local D1 data");
const work = resolve(scratch);
const migrations = join(work, "migrations");
await mkdir(migrations);
const config = join(work, "wrangler.json");
const persist = join(work, "state");
const wrangler = join(root, "node_modules/.bin/wrangler");
const tokens = Object.fromEntries(["alice", "bob", "absent"].map(name => [name, crypto.randomUUID()]));
await Bun.write(join(work, ".dev.vars"), `TACT_MEMORY_CREDENTIALS=${JSON.stringify(
  Object.entries(tokens).map(([name, token]) => `writer ${name} ${token}`).join("\n"),
)}\n`);
await chmod(join(work, ".dev.vars"), 0o600);
await Bun.write(config, JSON.stringify({
  name: "tact-memory-index-test", main: join(root, "build/worker/shim.mjs"),
  compatibility_date: "2026-08-14",
  vars: { TACT_MEMORY_MAX_RECORDS: "20000", TACT_MEMORY_MAX_RECORD_BYTES: "2048",
    TACT_MEMORY_MAX_TOTAL_BYTES: "20000000", TACT_MEMORY_MAX_REQUEST_BYTES: "2097152" },
  d1_databases: [{ binding: "DB", database_name: "test", database_id: "00000000-0000-0000-0000-000000000001",
    migrations_dir: migrations }],
}));

async function command(args: string[], failure = false): Promise<string> {
  const child = Bun.spawn([wrangler, ...args, "--config", config], {
    cwd: root, env: { ...process.env, WRANGLER_SEND_METRICS: "false" },
    stdout: "pipe", stderr: "pipe", stdin: "ignore",
  });
  const [out, err, code] = await Promise.all([
    new Response(child.stdout).text(), new Response(child.stderr).text(), child.exited,
  ]);
  if (failure) assert.notEqual(code, 0, "injected failure must fail");
  else assert.equal(code, 0, `${args.slice(0, 3).join(" ")} failed: ${out}\n${err}`);
  return out;
}

async function sql(statement: string, failure = false): Promise<any[]> {
  const path = join(work, "query.sql");
  await Bun.write(path, statement);
  const out = await command(["d1", "execute", "DB", "--local", "--persist-to", persist,
    "--file", path, "--json"], failure);
  if (failure) return [];
  const results = JSON.parse(out);
  assert(results.every((r: any) => r.success));
  return results;
}
const rows = async (statement: string) => (await sql(statement)).at(-1).results;
const migrate = (failure = false) => command(["d1", "migrations", "apply", "DB", "--local",
  "--persist-to", persist], failure);
const wire = (value: unknown) => JSON.stringify(value, (_, value) =>
  typeof value === "bigint" ? `@integer:${value}` : value).replace(/"@integer:(\d+)"/g, "$1");

await copyFile(join(root, "migrations/0001_initial.sql"), join(migrations, "0001_initial.sql"));
await migrate();
await sql(`
INSERT INTO memory_namespaces(namespace) VALUES ('alice'), ('bob'), ('absent');
WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<12000)
INSERT INTO memories(namespace,id,version,content,identity,created_at_ms,updated_at_ms)
SELECT 'alice', x, 1, 'needle ' || printf('%0500d',x), 'seed-' || x, 1, 1 FROM n;
INSERT INTO memories(namespace,id,version,content,identity,created_at_ms,updated_at_ms)
VALUES ('bob',1,1,'needle ' || printf('%0500d',12001),'bob-seed',1,1);
UPDATE memory_namespaces SET next_id=12002;
`);
const size = (await rows("SELECT count(*) records, sum(length(CAST(content AS BLOB))) bytes FROM memories"))[0];
assert(size.records > 10240 && size.bytes > 5242880);
const before = await rows("SELECT name FROM sqlite_master ORDER BY name");
const upgrades = (await readdir(join(root, "migrations"))).filter(n => n !== "0001_initial.sql").sort();
assert(upgrades.length > 0);
for (const name of upgrades) await copyFile(join(root, "migrations", name), join(migrations, name));
const lastMigration = join(migrations, upgrades.at(-1)!);
const validMigration = await Bun.file(lastMigration).text();
await Bun.write(lastMigration, validMigration + "\nSELECT injected_migration_failure();\n");
await migrate(true);
assert.deepEqual(await rows("SELECT name FROM sqlite_master ORDER BY name"), before,
  "failed migration must roll back schema and backfill");
await Bun.write(lastMigration, validMigration);
const activateLocal = () => activate("local", async args => ({
  exitCode: 0,
  stdout: await command([...args.slice(3), "--persist-to", persist]),
}));
await activateLocal();

// Reserve loopback ports before launching Wrangler so parallel runs use separate listeners.
function freePort(): number {
  const listener = Bun.listen({ hostname: "127.0.0.1", port: 0, socket: { data() {} } });
  const port = listener.port;
  listener.stop(true);
  return port;
}
const port = freePort();
const inspectorPort = freePort();
const worker = Bun.spawn([wrangler, "dev", "--config", config, "--local", "--persist-to", persist,
  "--port", String(port), "--inspector-port", String(inspectorPort)], {
  cwd: root, env: { ...process.env, WRANGLER_SEND_METRICS: "false", BROWSER: "none" },
  stdin: "ignore", stdout: "pipe", stderr: "pipe",
});
let logs = "";
const drain = async (stream: ReadableStream) => {
  for await (const chunk of stream) logs += new TextDecoder().decode(chunk);
};
const drains = [drain(worker.stdout), drain(worker.stderr)];
const bookmarks = new Map<string, string>();
let inspector: WebSocket | undefined;
const measurements: Record<string, unknown> = { corpus: size };
try {
  for (let attempt = 0; ; attempt++) {
    try { await fetch(`http://127.0.0.1:${port}/v1/session`); break; }
    catch {
      if (attempt === 200 || worker.exitCode !== null) throw new Error(`Worker did not start: ${logs}`);
      await Bun.sleep(100);
    }
  }
  async function request(namespace: string, operation: string, body: unknown, status = 200) {
    const headers: Record<string, string> = { authorization: `Bearer ${tokens[namespace]}`,
      "x-tact-memory-namespace": namespace, "content-type": "application/json" };
    if (bookmarks.has(namespace)) headers["x-tact-memory-bookmark"] = bookmarks.get(namespace)!;
    const response = await fetch(`http://127.0.0.1:${port}/v1/memories/${operation}`, {
      method: "POST", headers, body: wire(body),
    });
    const text = await response.text();
    assert.equal(response.status, status, `${operation}: ${text}`);
    const bookmark = response.headers.get("x-tact-memory-bookmark");
    if (bookmark) bookmarks.set(namespace, bookmark);
    return { value: text ? JSON.parse(text) : null, text };
  }
  const scan = async (namespace: string, query: string, limit = 10) =>
    (await request(namespace, "scan", { query, limit })).value.candidates;

  // Warm the production WASM module before collecting a local CPU profile.
  await scan("absent", "needle");
  const endpoints: any[] = await (await fetch(`http://127.0.0.1:${inspectorPort}/json/list`)).json();
  inspector = new WebSocket(endpoints[0].webSocketDebuggerUrl);
  await new Promise<void>((ok, fail) => { inspector!.onopen = () => ok(); inspector!.onerror = fail; });
  let messageId = 0;
  const pending = new Map<number, { resolve: (value: any) => void; reject: (error: Error) => void }>();
  inspector.onmessage = event => {
    const message = JSON.parse(String(event.data));
    const call = pending.get(message.id);
    if (!call) return;
    pending.delete(message.id);
    if (message.error) call.reject(new Error(JSON.stringify(message.error)));
    else call.resolve(message.result);
  };
  const cdp = (method: string) => new Promise<any>((resolve, reject) => {
    const id = ++messageId;
    const timeout = setTimeout(() => { pending.delete(id); reject(new Error(`inspector timed out: ${method}`)); }, 10_000);
    pending.set(id, {
      resolve: value => { clearTimeout(timeout); resolve(value); },
      reject: error => { clearTimeout(timeout); reject(error); },
    });
    inspector!.send(JSON.stringify({ id, method }));
  });
  await cdp("Profiler.enable");
  const heapBefore = await cdp("Runtime.getHeapUsage");
  await cdp("Profiler.start");
  const measuredLogStart = logs.length;
  const start = performance.now();
  const selected = await scan("bob", "needle");
  measurements.scan_wall_ms = performance.now() - start;
  const { profile } = await cdp("Profiler.stop");
  measurements.worker_heap_before = heapBefore;
  measurements.worker_heap_after = await cdp("Runtime.getHeapUsage");
  measurements.production_scan_metrics = logs.slice(measuredLogStart).split("\n")
    .filter(line => line.includes("indexed_scan"))
    .map(line => ({
      phase: line.includes("indexed_scan_maintenance") ? "maintenance" : "selection",
      ...Object.fromEntries([...line.matchAll(/(\w+)=(?:Some\()?([\d.]+)\)?/g)]
        .map(match => [match[1], Number(match[2])])),
    }));
  const idle = new Set(profile.nodes.filter((n: any) => ["(idle)", "(program)"].includes(n.callFrame.functionName)).map((n: any) => n.id));
  measurements.worker_sampled_cpu_ms = profile.samples.reduce((sum: number, id: number, i: number) =>
    sum + (idle.has(id) ? 0 : profile.timeDeltas[i]), 0) / 1000;
  measurements.returned_bytes = new TextEncoder().encode(JSON.stringify({ candidates: selected })).length;
  assert.equal(selected.length, 10);
  assert.equal(selected[0].key.namespace, "bob", "own match beyond global top K must be promoted");
  assert.equal(selected[0].key.id, 1);
  assert(selected.every((c: any) => c.score > 0 && new TextEncoder().encode(c.preview).length <= 64));
  const raw = await scan("absent", "needle");
  assert.equal(selected[0].score, raw[0].score * 1.25);
  assert.equal(selected[1].score, raw[0].score, "foreign score must remain unchanged");
  assert(bookmarks.has("bob"), "D1 response bookmark must be carried by the client");
  await sql("UPDATE memories SET scan_count=0, last_scanned_at_ms=NULL");
  const finalOnly = await scan("bob", "needle", 3);
  assert.deepEqual(await rows("SELECT namespace,id FROM memories WHERE scan_count>0 ORDER BY namespace,id"),
    finalOnly.map((c: any) => ({ namespace: c.key.namespace, id: c.key.id }))
      .sort((a: any,b: any) => a.namespace.localeCompare(b.namespace) || a.id-b.id));

  for (const query of ["!!!", "\"\"", "___", "() : * + -"]) assert.deepEqual(await scan("bob", query), []);
  await request("bob", "scan", { query: "x".repeat(513), limit: 10 }, 413);
  for (const limit of [0, 11]) await request("bob", "scan", { query: "needle", limit }, 400);
  await scan("bob", "x".repeat(512));
  const content = 'literal OR NOT AND NEAR café résumé 東京 x parse_request httpServer ' + 'é'.repeat(80);
  const created = (await request("bob", "put", { content })).value.memory;
  for (const query of ['"OR"', 'NOT', 'AND', 'NEAR', 'cafe', 're\u0301sume\u0301', '東京', 'x', 'parse', 'request', 'httpServer', 'literal\u0345unmatched']) {
    const matches = await scan("bob", query);
    assert(matches.some((c: any) => c.key.id === created.key.id && c.key.namespace === "bob"), query);
  }
  const updated = (await request("bob", "put", { content: "replacementonly", replacement: created.key })).value.memory;
  assert.deepEqual(await scan("bob", "parse"), []);
  assert.equal((await scan("bob", "replacementonly"))[0].key.version, updated.key.version);
  assert.equal((await request("bob", "read", { keys: [created.key] })).value.memories.length, 0);
  await request("bob", "put", { content: "stale replacement", replacement: created.key }, 409);
  await request("bob", "delete", { key: created.key }, 409);
  const beforeProbation = await rows(`SELECT probation_until_ms,use_count FROM memories WHERE namespace='bob' AND id=${updated.key.id}`);
  await scan("bob", "replacementonly");
  assert.deepEqual(await rows(`SELECT probation_until_ms,use_count FROM memories WHERE namespace='bob' AND id=${updated.key.id}`), beforeProbation);
  await request("bob", "delete", { key: updated.key });
  assert.deepEqual(await scan("bob", "replacementonly"), []);

  // Canonical SQL models writers from the version preceding the index migration.
  await sql(`INSERT INTO memories(namespace,id,version,content,identity,created_at_ms,updated_at_ms)
    VALUES('bob',9007199254740993,9007199254740995,'oldwriterexact','oldwriterexact',1,1);`);
  const exact = await request("bob", "scan", { query: "oldwriterexact", limit: 10 });
  assert(exact.text.includes('"id":9007199254740993') && exact.text.includes('"version":9007199254740995'));
  const exactKey = { namespace: "bob", id: 9007199254740993n, version: 9007199254740995n };
  const read = await request("bob", "read", { keys: [exactKey] });
  assert(read.text.includes('"id":9007199254740993'));
  await sql("UPDATE memories SET content='oldwriterupdated',identity='oldwriterupdated' WHERE namespace='bob' AND id=9007199254740993");
  assert.deepEqual(await scan("bob", "oldwriterexact"), []);
  assert.equal((await scan("bob", "oldwriterupdated")).length, 1);
  await sql("UPDATE memories SET probation_until_ms=1,use_count=0 WHERE namespace='bob' AND id=9007199254740993");
  assert.deepEqual(await scan("bob", "oldwriterupdated"), []);
  await request("bob", "put", { content: "prunetrigger" });
  assert.equal((await rows("SELECT count(*) n FROM memories WHERE namespace='bob' AND id=9007199254740993"))[0].n, 0);

  const snapshot = (id: bigint, text: string) => ({ key: { id, version: 9007199254740995n }, content: text,
    created_at_ms: 1, updated_at_ms: 1, last_scanned_at_ms: null, scan_count: 0,
    last_used_at_ms: null, use_count: 0, probation_until_ms: null });
  await request("bob", "sync", { memories: [snapshot(9007199254740993n, "snapshotfirst")] });
  assert.equal((await scan("bob", "snapshotfirst")).length, 1);
  assert.deepEqual(await scan("bob", "prunetrigger"), []);
  await request("bob", "sync", { memories: [snapshot(9007199254740997n, "snapshotsecond")] });
  assert.deepEqual(await scan("bob", "snapshotfirst"), []);
  assert.equal((await scan("bob", "snapshotsecond")).length, 1);
  await sql("DELETE FROM memory_namespaces WHERE namespace='bob'");
  assert.deepEqual(await scan("bob", "snapshotsecond"), []);

  await sql("CREATE TRIGGER reject_rollback BEFORE INSERT ON memories WHEN NEW.content='rejectrollback' BEGIN SELECT RAISE(ABORT,'injected'); END;");
  await request("bob", "put", { content: "rollbackbefore" });
  await request("bob", "sync", { memories: [snapshot(1n, "rejectrollback")] }, 503);
  assert.equal((await scan("bob", "rollbackbefore")).length, 1, "failed batch must retain old canonical and FTS data");
  assert.deepEqual(await scan("bob", "rejectrollback"), []);
  await sql("DROP TRIGGER reject_rollback");
  await activateLocal();
  assert((measurements.production_scan_metrics as any[]).some(event => event.phase === "selection"),
    "production scan must report database work");
  measurements.assertions = "populated upgrade, failed migration, weighted finalists, bounds, literals, Unicode, exact integers, mutations, stale keys, probation, sync, cascade, rollback";
  await Bun.write(join(work, "measurements.json"), JSON.stringify(measurements, null, 2) + "\n");
  console.log(JSON.stringify(measurements, null, 2));
} finally {
  inspector?.close();
  worker.kill("SIGTERM");
  await worker.exited;
  await Promise.all(drains);
  await Bun.write(join(work, "worker.log"), logs);
}
