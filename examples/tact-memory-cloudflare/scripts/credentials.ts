import { chmod } from "node:fs/promises";
import { fileURLToPath } from "node:url";

const ROOT = new URL("../", import.meta.url);
const CONFIG = new URL("credentials.toml", ROOT);
const DEV_VARS = new URL(".dev.vars", ROOT);
const SECRET_NAME = "TACT_MEMORY_CREDENTIALS";
const TOKEN_PATTERN = /^[A-Za-z0-9._~+/=-]+$/;
const NAMESPACE_PATTERN = /^[A-Za-z0-9._-]+$/;

type Credential = {
  namespace: string;
  role: "reader" | "writer";
  token: string;
};

type CredentialFile = {
  credentials: Credential[];
};

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function byteLength(value: string): number {
  return new TextEncoder().encode(value).byteLength;
}

/** Validates parsed TOML and emits the document consumed by the Worker. */
export function credentialDocument(value: unknown): string {
  if (!isRecord(value) || !Array.isArray(value.credentials) || value.credentials.length === 0) {
    throw new Error("credentials.toml must contain at least one [[credentials]] entry");
  }

  const tokens = new Set<string>();
  return value.credentials
    .map((entry, index) => {
      if (!isRecord(entry)) {
        throw new Error(`credentials[${index}] must be a TOML table`);
      }

      const { namespace, role, token } = entry;
      if (
        typeof namespace !== "string" ||
        byteLength(namespace) > 128 ||
        !NAMESPACE_PATTERN.test(namespace)
      ) {
        throw new Error(`credentials[${index}].namespace is invalid`);
      }
      if (role !== "reader" && role !== "writer") {
        throw new Error(`credentials[${index}].role must be reader or writer`);
      }
      if (
        typeof token !== "string" ||
        byteLength(token) > 4096 ||
        !TOKEN_PATTERN.test(token)
      ) {
        throw new Error(`credentials[${index}].token is invalid`);
      }
      if (tokens.has(token)) {
        throw new Error(`credentials[${index}].token duplicates an earlier entry`);
      }
      tokens.add(token);
      return `${role} ${namespace} ${token}`;
    })
    .join("\n");
}

async function loadDocument(): Promise<string> {
  if (!(await Bun.file(CONFIG).exists())) {
    throw new Error("credentials.toml is missing; copy credentials.example.toml first");
  }

  let module: { default: CredentialFile };
  try {
    module = await import(CONFIG.href, { with: { type: "toml" } });
  } catch {
    throw new Error("credentials.toml is not valid TOML");
  }
  return credentialDocument(module.default);
}

async function writeLocalCredentials(document: string): Promise<void> {
  await Bun.write(DEV_VARS, `${SECRET_NAME}=${JSON.stringify(document)}\n`);
  await chmod(fileURLToPath(DEV_VARS), 0o600);
}

async function pushCredentials(document: string): Promise<void> {
  const process = Bun.spawn(["bun", "x", "wrangler", "secret", "bulk"], {
    cwd: fileURLToPath(ROOT),
    stdin: new Blob([JSON.stringify({ [SECRET_NAME]: document })]),
    stdout: "inherit",
    stderr: "inherit",
  });
  const exitCode = await process.exited;
  if (exitCode !== 0) {
    throw new Error(`wrangler exited with status ${exitCode}`);
  }
}

async function main(): Promise<void> {
  const command = Bun.argv[2];
  const document = await loadDocument();
  if (command === "local") {
    await writeLocalCredentials(document);
  } else if (command === "push") {
    await pushCredentials(document);
  } else {
    throw new Error("expected credentials command: local or push");
  }
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
