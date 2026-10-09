import { afterAll, beforeAll, expect, test } from "bun:test";
import { GlobalRegistrator } from "@happy-dom/global-registrator";
import { settle } from "./core/test-support";

/**
 * Drives the whole page in a DOM against a fake server. The machine behind `?m=` is semi-trusted:
 * whatever strings it sends must end up as text, never as elements or attributes.
 */

const PAYLOADS = ["<img src=x onerror=1>", '"><form id="injected">', "<script>window.pwned=1</script>"];

class FakeEventSource {
  static instances: FakeEventSource[] = [];
  onopen: ((event: Event) => void) | null = null;
  onerror: ((event: Event) => void) | null = null;
  private listeners = new Map<string, ((event: MessageEvent<string>) => void)[]>();
  constructor(readonly url: string) {
    FakeEventSource.instances.push(this);
  }
  addEventListener(type: string, listener: (event: MessageEvent<string>) => void) {
    this.listeners.set(type, [...(this.listeners.get(type) ?? []), listener]);
  }
  close() {}
  emit(type: string, data: unknown) {
    for (const listener of this.listeners.get(type) ?? []) listener({ data: JSON.stringify(data) } as MessageEvent<string>);
  }
}

type Reply = { status?: number; body: unknown };

/** Installs a DOM, a fake server, and the page at `search`; resolves once the page has run its start-up. */
async function openPage(search: string, replies: Record<string, Reply>) {
  FakeEventSource.instances = [];
  document.body.innerHTML = '<div id="app"></div>';
  (window as unknown as { happyDOM: { setURL(url: string): void } }).happyDOM.setURL("https://hub.test/" + search);
  const requests: string[] = [];
  globalThis.fetch = (async (url: string) => {
    requests.push(url);
    const reply = replies[url.replace(/^\.\//, "")] ?? { status: 404, body: {} };
    return new Response(JSON.stringify(reply.body), { status: reply.status ?? 200 });
  }) as unknown as typeof fetch;
  (globalThis as { EventSource: unknown }).EventSource = FakeEventSource;
  await import("./app?" + search + Math.random());
  await settle();
  return { requests, root: document.getElementById("app")! };
}

const instance = (protocol_version = 9) => ({ body: { protocol_version, workspace: "/w", repository: "r", live: 0, running: 0 } });

beforeAll(() => {
  GlobalRegistrator.register({ url: "https://hub.test/" });
  // The page measures and observes layout, which this DOM does not do.
  for (const name of ["ResizeObserver", "IntersectionObserver"]) {
    (globalThis as Record<string, unknown>)[name] ??= class { observe() {} unobserve() {} disconnect() {} };
  }
  // The review panel starts diff workers, which a test has no bundle for.
  (globalThis as Record<string, unknown>).Worker = class { postMessage() {} terminate() {} addEventListener() {} removeEventListener() {} };
  HTMLCanvasElement.prototype.getContext = (() => new Proxy({}, { get: () => () => {}, set: () => true })) as never;
  window.matchMedia ??= (() => ({ matches: false, addEventListener() {}, removeEventListener() {} })) as never;
});
afterAll(() => GlobalRegistrator.unregister());

test("a machine that rejects the hub's token shows the re-link card, not the sign-in screen", async () => {
  const { root } = await openPage("?m=devbox", { "api/m/devbox/instance": { status: 409, body: { code: "machine_unauthorized", error: "no" } } });

  expect(root.querySelector("h1")!.textContent).toBe("Link this machine again");
  expect(root.textContent).toContain("tact machine add devbox <url> --replace");
  expect(root.textContent).not.toContain("Signed out");
});

test("an unknown machine shows the not-registered card, and a malformed name never reaches the network", async () => {
  const unknown = await openPage("?m=ghost", { "api/m/ghost/instance": { status: 404, body: { code: "unknown_machine", error: "no" } } });
  expect(unknown.root.querySelector("h1")!.textContent).toBe("No such linked machine");

  const malformed = await openPage("?m=..%2Flink", {});
  expect(malformed.root.querySelector("h1")!.textContent).toBe("No such linked machine");
  expect(malformed.requests).toEqual([]);
});

test("a machine speaking another protocol blocks the page before its stream starts", async () => {
  const { root, requests } = await openPage("?m=devbox", { "api/m/devbox/instance": instance(8) });

  expect(root.querySelector("h1")!.textContent).toBe("Update Tact on this machine");
  expect(requests).toEqual(["./api/m/devbox/instance"]);
  expect(FakeEventSource.instances).toEqual([]);
});

test("an unreachable machine keeps retrying behind a reconnect card instead of the sign-in screen", async () => {
  const { root } = await openPage("?m=devbox", { "api/m/devbox/instance": { status: 502, body: { code: "machine_unreachable", error: "down" } } });

  expect(root.querySelector("h1")!.textContent).toBe("Reconnecting to devbox…");
});

test("machine mode sends every session call through the machine and hub-only calls to the hub", async () => {
  const { requests, root } = await openPage("?m=devbox", {
    "api/m/devbox/instance": instance(),
    "api/machines": { body: { machines: [{ name: "devbox" }, { name: "gpu" }] } },
  });

  expect(FakeEventSource.instances.map((source) => source.url)).toEqual(["./api/m/devbox/stream"]);
  expect(requests).toContain("./api/machines");
  expect(requests).not.toContain("./api/instances");
  expect(requests.filter((url) => url.startsWith("./api/") && !url.startsWith("./api/m/devbox/")).sort()).toEqual(["./api/machines"]);
  expect(root.querySelector(".machine-chip")!.textContent).toBe("devbox");
  expect(root.querySelector<HTMLElement>(".machine-button")!.hidden).toBe(false);
  expect(document.title).toStartWith("devbox · ");
});

test("without linked machines the menu stays hidden and nothing is relayed", async () => {
  const { requests, root } = await openPage("", { "api/instance": instance(), "api/machines": { body: { machines: [] } } });

  expect(FakeEventSource.instances.map((source) => source.url)).toEqual(["./api/stream"]);
  expect(requests.some((url) => url.includes("/m/"))).toBe(false);
  expect(root.querySelector<HTMLElement>(".machine-button")!.hidden).toBe(true);
  expect(root.querySelector<HTMLElement>(".machine-chip")!.hidden).toBe(true);
});

test("strings a machine sends render as text, never as markup", async () => {
  const [img, form, script] = PAYLOADS as [string, string, string];
  const { root } = await openPage("?m=devbox", { "api/m/devbox/instance": instance(), "api/machines": { body: { machines: [] } } });
  const stream = FakeEventSource.instances[0]!;
  const entry = (id: number, body: Record<string, unknown>) => ({ id, revision: 1, parent: null, ...body });

  stream.emit("hello", { protocol_version: 9, client_hint: img });
  stream.emit("live", {
    active: form,
    sessions: [{ id: form, title: img, model: form, state: "idle", unread: false, has_draft: false, last_activity_unix_ms: 1, workspace: img }],
  });
  stream.emit("snapshot", {
    session: form, title: img, model: img, effort: "high", reasoning_mode: "standard", speed: "standard",
    entries: [
      entry(1, { kind: "user", text: img }),
      entry(2, { kind: "assistant", text: script, complete: true, commentary: false }),
      entry(3, { kind: "tool", name: form, summary: img, state: form, duration_ns: 1, substeps: [form], child_count: 0, has_detail: false, outcome: { exit_code: 1, tail: [img], summary: form }, stats: { files: 1, additions: img, deletions: form }, significance: "landmark" }),
      entry(4, { kind: "error", message: form }),
      entry(5, { kind: "compaction_failed", message: img }),
      entry(6, { kind: "directed_message", from: img, to: form, body: img, delivery: form, thread: 1, messages: [] }),
      entry(7, { kind: "tool", name: "apply_patch", summary: "a.rs", state: "succeeded", duration_ns: 1, substeps: [], child_count: 0, has_detail: false, outcome: null, stats: { files: 1, additions: "<img id=peer-injected src=x onerror=1>", deletions: form }, significance: "landmark" }),
      entry(8, { kind: "turn_completed", duration_ns: 1_000_000_000 }),
    ],
    status: { kind: img }, queue: [{ id: 1, text: form, steering: false }], draft: { rev: 1, text: img, images: [] }, running: false, context: null,
    subagents: { max_subagents: 1, agents: [] },
  });
  await settle();

  expect(root.textContent).toContain("<img src=x onerror=1>");
  expect(root.querySelector('img[src="x"]')).toBeNull();
  expect(root.querySelector("#peer-injected")).toBeNull();
  expect(root.querySelector(".outcome-item .add")?.textContent).toBe("+0");
  expect(root.querySelector("[onerror]")).toBeNull();
  expect(root.querySelector("#injected")).toBeNull();
  expect(root.querySelector('script:not([src])')).toBeNull();
  expect((window as unknown as { pwned?: number }).pwned).toBeUndefined();
  for (const element of root.querySelectorAll("*")) {
    for (const attribute of element.getAttributeNames()) expect(attribute.startsWith("on")).toBe(false);
  }
});
