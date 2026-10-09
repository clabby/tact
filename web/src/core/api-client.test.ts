import { afterEach, expect, test } from "bun:test";
import { ApiClient, ApiError, describeError } from "./api-client";

const originalFetch = globalThis.fetch;
afterEach(() => { globalThis.fetch = originalFetch; });

function respond(handler: (url: string, init: RequestInit) => Response) {
  const requests: { url: string; init: RequestInit }[] = [];
  globalThis.fetch = (async (url: string, init: RequestInit) => {
    requests.push({ url, init });
    return handler(url, init);
  }) as unknown as typeof fetch;
  return requests;
}

test("commands use the single command route with a tagged envelope", async () => {
  const requests = respond(() => new Response("{}", { status: 200 }));
  const api = new ApiClient("./api", 42);

  await api.command("steer", { session: "s1", queue_id: 3 });
  await api.command("reload_config");

  expect(requests.map((request) => request.url)).toEqual(["./api/cmd", "./api/cmd"]);
  expect(requests[0]!.init.method).toBe("POST");
  expect((requests[0]!.init.headers as Record<string, string>)["x-tact"]).toBe("1");
  expect(JSON.parse(String(requests[0]!.init.body))).toEqual({ client: 42, cmd: "steer", args: { session: "s1", queue_id: 3 } });
  expect(JSON.parse(String(requests[1]!.init.body))).toEqual({ client: 42, cmd: "reload_config" });
  expect(api.origin).toBe("web:42");
});

test("queries use the single query route and omit absent arguments", async () => {
  const requests = respond(() => Response.json({ paths: ["src/"] }));
  const api = new ApiClient();

  expect(await api.query("files", { query: "sr" })).toEqual({ paths: ["src/"] });
  await api.query("models");

  expect(requests[0]!.url).toBe("./api/query");
  expect(JSON.parse(String(requests[0]!.init.body))).toEqual({ query: "files", args: { query: "sr" } });
  expect(JSON.parse(String(requests[1]!.init.body))).toEqual({ query: "models" });
});

test("a machine client carries every session call, stream, and image to the machine's API root", async () => {
  const requests = respond(() => Response.json({}));
  const machine = new ApiClient("./api/m/devbox", 1);

  await machine.command("reload_config");
  await machine.query("models");
  await machine.instance();
  await machine.toolDetail("s1", 4);
  await machine.agentEntries("s1", 2);

  expect(requests.map((request) => request.url)).toEqual([
    "./api/m/devbox/cmd",
    "./api/m/devbox/query",
    "./api/m/devbox/instance",
    "./api/m/devbox/sessions/s1/entries/4",
    "./api/m/devbox/sessions/s1/agents/2/entries",
  ]);
  expect(machine.url("stream")).toBe("./api/m/devbox/stream");
  expect(machine.imageUrl("s1", 4, 0)).toBe("./api/m/devbox/sessions/s1/entries/4/images/0");
  expect(machine.fileUrl("a b/c.png", "s 1")).toBe("./api/m/devbox/file?path=a%20b%2Fc.png&session=s%201");
});

test("the hub client keeps sign-in, the machine list, the phone link, and instances on the hub", async () => {
  const requests = respond(() => Response.json({ machines: [], instances: [], public_origin: null, token: "t" }));
  const hub = new ApiClient();

  await hub.login("t");
  await hub.machines();
  await hub.link();
  await hub.instances();

  expect(requests.map((request) => request.url)).toEqual(["./api/login", "./api/machines", "./api/link", "./api/instances"]);
});

test("refusals keep their wire code and message", async () => {
  respond(() => Response.json({ code: "draft_changed", message: "the draft changed" }, { status: 409 }));

  const error = await new ApiClient().command("submit", { session: "s1", rev: 4 }).catch((caught) => caught);

  expect(error).toBeInstanceOf(ApiError);
  expect(error.code).toBe("draft_changed");
  expect(error.message).toBe("the draft changed");
  expect(describeError(error)).toContain("changed in another window");
});

test("a 401 without a JSON body is reported as unauthorized", async () => {
  respond(() => new Response("nope", { status: 401 }));

  const error = await new ApiClient().instance().catch((caught) => caught);

  expect(error.code).toBe("unauthorized");
  expect(error.status).toBe(401);
});

test("an empty success body (204) resolves", async () => {
  respond(() => new Response(null, { status: 204 }));

  await expect(new ApiClient().login("token")).resolves.toBeUndefined();
});

test("network failures become retryable network errors", async () => {
  globalThis.fetch = (async () => { throw new TypeError("Failed to fetch"); }) as unknown as typeof fetch;

  const error = await new ApiClient().query("models").catch((caught) => caught);

  expect(error.code).toBe("network_error");
  expect(error.retryable).toBe(true);
});

test("client ids are safe JSON integers", () => {
  for (let index = 0; index < 100; index += 1) {
    expect(Number.isSafeInteger(new ApiClient().client)).toBe(true);
  }
});
