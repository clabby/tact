import { fileURLToPath } from "node:url";

const ROOT = fileURLToPath(new URL("../", import.meta.url));

const VERIFICATION_SQL = `
INSERT INTO memory_search(memory_search) VALUES ('integrity-check');
WITH required_triggers(name) AS (
    VALUES
        ('memories_search_insert'),
        ('memories_search_update'),
        ('memories_search_delete')
), inconsistencies AS (
    SELECT COUNT(*) AS count
    FROM memories AS memory
    LEFT JOIN memory_search_documents AS document
        ON document.namespace = memory.namespace AND document.id = memory.id
    WHERE document.search_id IS NULL
    UNION ALL
    SELECT COUNT(*)
    FROM memory_search_documents AS document
    LEFT JOIN memories AS memory
        ON memory.namespace = document.namespace AND memory.id = document.id
    WHERE memory.id IS NULL
    UNION ALL
    SELECT COUNT(*)
    FROM memory_search_documents AS document
    LEFT JOIN memory_search AS search ON search.rowid = document.search_id
    WHERE search.rowid IS NULL
    UNION ALL
    SELECT COUNT(*)
    FROM memory_search AS search
    LEFT JOIN memory_search_documents AS document ON document.search_id = search.rowid
    WHERE document.search_id IS NULL
    UNION ALL
    SELECT COUNT(*)
    FROM memories AS memory
    JOIN memory_search_documents AS document
        ON document.namespace = memory.namespace AND document.id = memory.id
    JOIN memory_search AS search ON search.rowid = document.search_id
    WHERE search.content IS NOT memory.content
)
SELECT CASE WHEN
    (SELECT COUNT(*) FROM d1_migrations WHERE name = '0002_search.sql') = 1
    AND (SELECT COUNT(*) FROM required_triggers AS required
        JOIN sqlite_master AS schema
            ON schema.type = 'trigger' AND schema.name = required.name) = 3
    AND (SELECT COALESCE(SUM(count), 0) FROM inconsistencies) = 0
THEN 1 ELSE 0 END AS verified;
`;

export type CommandResult = {
  exitCode: number;
  stdout: string;
};

export type CommandRunner = (command: string[]) => Promise<CommandResult>;

export type ActivationOptions = {
  config?: string;
  persistTo?: string;
};

async function runChecked(runner: CommandRunner, command: string[]): Promise<CommandResult> {
  const result = await runner(command);
  if (result.exitCode !== 0) {
    const output = result.stdout.trim();
    const details = output === "" ? "" : `: ${output}`;
    throw new Error(
      `${command.slice(0, 5).join(" ")} exited with status ${result.exitCode}${details}`,
    );
  }
  return result;
}

function verifyWranglerResult(stdout: string): void {
  let results: unknown;
  try {
    results = JSON.parse(stdout);
  } catch {
    throw new Error("wrangler returned invalid verification JSON");
  }

  if (!Array.isArray(results) || results.length === 0) {
    throw new Error("wrangler returned no verification results");
  }

  const rows: unknown[] = [];
  for (const result of results) {
    if (typeof result !== "object" || result === null || !("success" in result)) {
      throw new Error("wrangler returned malformed verification results");
    }
    if (result.success !== true) {
      throw new Error("wrangler reported that verification failed");
    }
    if (!("results" in result) || !Array.isArray(result.results)) {
      throw new Error("wrangler returned malformed verification rows");
    }
    rows.push(...result.results);
  }

  if (rows.length !== 1 || typeof rows[0] !== "object" || rows[0] === null) {
    throw new Error("wrangler returned malformed verification rows");
  }
  if (!("verified" in rows[0]) || rows[0].verified !== 1) {
    throw new Error("database schema or search index verification failed");
  }
}

export async function activate(
  mode: "local" | "remote" | "deploy",
  runner: CommandRunner,
  options: ActivationOptions = {},
): Promise<void> {
  if (options.persistTo !== undefined && mode !== "local") {
    throw new Error("--persist-to is supported only for local activation");
  }

  const target = mode === "local" ? "--local" : "--remote";
  const configArgs = options.config === undefined ? [] : ["--config", options.config];
  const persistenceArgs =
    options.persistTo === undefined ? [] : ["--persist-to", options.persistTo];
  await runChecked(runner, [
    "bun",
    "x",
    "wrangler",
    "d1",
    "migrations",
    "apply",
    "DB",
    target,
    ...persistenceArgs,
    ...configArgs,
  ]);

  const verification = await runChecked(runner, [
    "bun",
    "x",
    "wrangler",
    "d1",
    "execute",
    "DB",
    target,
    "--command",
    VERIFICATION_SQL,
    "--json",
    ...persistenceArgs,
    ...configArgs,
  ]);
  verifyWranglerResult(verification.stdout);

  if (mode === "deploy") {
    await runChecked(runner, ["bun", "x", "wrangler", "deploy", ...configArgs]);
  }
}

async function runCommand(command: string[]): Promise<CommandResult> {
  const child = Bun.spawn(command, {
    cwd: ROOT,
    stdin: "inherit",
    stdout: "pipe",
    stderr: "inherit",
  });
  const [exitCode, stdout] = await Promise.all([
    child.exited,
    new Response(child.stdout).text(),
  ]);
  return {
    exitCode,
    stdout,
  };
}

async function main(): Promise<void> {
  const mode = Bun.argv[2];
  if (mode !== "local" && mode !== "remote" && mode !== "deploy") {
    throw new Error("expected activation command: local, remote, or deploy");
  }

  const options: ActivationOptions = {};
  for (let index = 3; index < Bun.argv.length; index += 2) {
    const flag = Bun.argv[index];
    const value = Bun.argv[index + 1];
    if (value === undefined) {
      throw new Error(`${flag} requires a value`);
    }
    if (flag === "--config" && options.config === undefined) {
      options.config = value;
    } else if (flag === "--persist-to" && options.persistTo === undefined) {
      options.persistTo = value;
    } else {
      throw new Error(`unknown or duplicate activation option: ${flag}`);
    }
  }

  await activate(mode, runCommand, options);
}

if (import.meta.main) {
  try {
    await main();
  } catch (error) {
    const message = error instanceof Error ? error.message : "unknown failure";
    console.error(`error: ${message}`);
    process.exit(1);
  }
}
