// Renders a memory tool call by operation: scan, read, store or replace, and delete.
import { FileDiff, parseDiffFromFile } from "@pierre/diffs";
import type { Theme } from "../core/theme";

/** What the caller lends the renderer: the theme and a way to release the diff instances it creates. */
export type MemoryContext = { theme: Theme; track(instances: FileDiff[]): void };

type Json = Record<string, unknown>;

const OPERATIONS = ["scan", "read", "put", "delete"] as const;
type Operation = (typeof OPERATIONS)[number];

const METADATA: Array<[string, string]> = [
  ["created_at_ms", "created"],
  ["updated_at_ms", "updated"],
  ["last_scanned_at_ms", "last scanned"],
  ["scan_count", "scans"],
  ["last_used_at_ms", "last used"],
  ["use_count", "uses"],
  ["probation_until_ms", "probation until"],
];

const record = (value: unknown): Json | null => (typeof value === "object" && value !== null && !Array.isArray(value) ? value as Json : null);

/** A memory key as "namespace:id@vN"; the namespace and version are omitted when absent. */
function keyLabel(value: unknown): string | null {
  const outer = record(value);
  const key = record(outer?.key) ?? outer;
  const id = key?.id;
  if (typeof id !== "number" && !(typeof id === "string" && id !== "")) return null;
  const namespace = typeof key?.namespace === "string" ? key.namespace + ":" : "";
  const version = typeof key?.version === "number" ? "@v" + key.version : "";
  return namespace + id + version;
}

function element(tag: string, className: string, text = "") {
  const node = document.createElement(tag);
  node.className = className;
  node.textContent = text;
  return node;
}

function count(total: number, singular: string, plural: string) {
  return total + " " + (total === 1 ? singular : plural);
}

/** The operation and its subject (the query or the keys it names), from the call's arguments. */
function describe(args: Json): { operation: Operation; label: string; subject: string } | null {
  const operation = OPERATIONS.find((candidate) => candidate === args.operation);
  if (!operation) return null;
  if (operation === "scan") return typeof args.query === "string" ? { operation, label: "Scan", subject: args.query } : null;
  if (operation === "read") {
    const keys = Array.isArray(args.keys) ? args.keys : Array.isArray(args.ids) ? args.ids : [];
    return { operation, label: "Read", subject: keys.map(keyLabel).filter(Boolean).join(", ") };
  }
  if (operation === "delete") return { operation, label: "Delete", subject: keyLabel(args) ?? "" };
  const replace = args.replace === undefined ? null : keyLabel(args.replace);
  return { operation, label: replace ? "Replace" : "Store", subject: replace ?? "" };
}

function memoryItem(memory: unknown): HTMLElement | null {
  const fields = record(memory);
  const label = keyLabel(memory);
  if (!fields || !label || typeof fields.content !== "string") return null;
  const item = element("div", "memory-call-item");
  item.append(element("div", "memory-call-key", label), element("div", "memory-call-text", fields.content));
  const metadata = METADATA
    .flatMap(([field, name]) => {
      const value = fields[field];
      if (typeof value !== "number" && typeof value !== "string") return [];
      return [name + " " + (field.endsWith("_ms") && typeof value === "number" ? new Date(value).toLocaleString() : value)];
    })
    .join(" · ");
  if (metadata) item.append(element("div", "memory-call-meta", metadata));
  return item;
}

/** A Pierre diff of the replaced record's content. */
function replacement(label: string, before: string, after: string, context: MemoryContext) {
  const item = element("div", "memory-call-item");
  item.append(element("div", "memory-call-key", label));
  const host = element("div", "memory-call-diff");
  item.append(host);
  const name = "memory-" + label.replace(/[^A-Za-z0-9@:.-]/g, "-") + ".md";
  const instance = new FileDiff({
    theme: context.theme === "dark" ? "pierre-dark" : "pierre-light",
    themeType: context.theme,
    diffStyle: "unified",
    diffIndicators: "bars",
    hunkSeparators: "simple",
    lineDiffType: "word-alt",
    disableLineNumbers: true,
    disableFileHeader: true,
    overflow: "wrap",
  });
  instance.render({
    fileDiff: parseDiffFromFile({ name, contents: before + "\n" }, { name, contents: after + "\n" }),
    containerWrapper: host,
  });
  context.track([instance]);
  return item;
}

/** The body of a completed or pending memory call, or null when the call is not recognised. */
export function presentMemory(args: unknown, result: unknown, context: MemoryContext): HTMLElement | null {
  const call = record(args);
  const described = call && describe(call);
  if (!call || !described) return null;
  const root = element("div", "memory-call");
  root.dataset.operation = described.operation;
  const head = element("div", "memory-call-head");
  head.append(element("span", "memory-call-op", described.label));
  const outcome = record(result);
  const backend = record(outcome?.backend)?.source;
  if (typeof backend === "string") head.append(element("span", "memory-call-backend", backend));
  if (described.subject) head.append(element("span", "memory-call-subject", described.subject));
  root.append(head);

  const failure = typeof outcome?.error === "string" ? outcome.error : null;
  if (failure) {
    root.append(element("div", "memory-call-error", failure));
    return root;
  }
  if (!outcome) {
    if (described.operation === "put" && typeof call.content === "string") root.append(element("div", "memory-call-text", call.content));
    return root;
  }

  let footer = "";
  if (described.operation === "scan") {
    const candidates = Array.isArray(outcome.candidates) ? outcome.candidates : [];
    for (const candidate of candidates) {
      const label = keyLabel(candidate);
      const fields = record(candidate);
      if (!label || !fields) return null;
      const item = element("div", "memory-call-item");
      const key = element("div", "memory-call-key", label);
      if (typeof fields.score === "number") key.append(element("span", "memory-call-score", "score " + Number(fields.score.toFixed(3))));
      item.append(key, element("div", "memory-call-text", typeof fields.preview === "string" ? fields.preview : ""));
      root.append(item);
    }
    footer = outcome.abstained === true ? "No relevant memories" : count(candidates.length, "candidate", "candidates");
  } else if (described.operation === "read") {
    const memories = Array.isArray(outcome.memories) ? outcome.memories : [];
    for (const memory of memories) {
      const item = memoryItem(memory);
      if (!item) return null;
      root.append(item);
    }
    footer = count(memories.length, "memory", "memories");
  } else if (described.operation === "put") {
    const item = memoryItem(outcome.memory);
    const label = keyLabel(outcome.memory);
    if (!item || !label) return null;
    const content = record(outcome.memory)!.content as string;
    root.append(typeof outcome.previous_content === "string" && outcome.replaced
      ? replacement(label, outcome.previous_content, content, context)
      : item);
  } else {
    const label = keyLabel(outcome);
    if (label) root.append(element("div", "memory-call-key", label));
  }
  if (footer) root.append(element("div", "memory-call-foot", footer));
  return root;
}
