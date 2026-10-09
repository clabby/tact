import { expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { ApiError } from "./api-client";
import { isMachineName, machineBase, machineFailure, machineFromSearch, machineStorage, machineUrl } from "./machine";
import { PROTOCOL_VERSION } from "./wire";
import { readSeen, writeSeen } from "../chat/seen";
import { loadCheckoutChoice, saveCheckoutChoice } from "../review/checkout-target";

function memoryStorage() {
  const values = new Map<string, string>();
  return { getItem: (key: string) => values.get(key) ?? null, setItem: (key: string, value: string) => void values.set(key, value), values };
}

test("only registry names select a machine", () => {
  for (const name of ["devbox", "a", "gpu-2", "0x", "a".repeat(32)]) expect(isMachineName(name)).toBe(true);
  for (const name of ["", "Dev", "-a", "a_b", "a/b", "../link", "a:m:b", "a b", "a".repeat(33), "dev%2Fbox"]) expect(isMachineName(name)).toBe(false);
  expect(machineFromSearch("?m=devbox&x=1")).toBe("devbox");
  expect(machineFromSearch("")).toBeNull();
  expect(machineFromSearch("?m=")).toBe("");
  expect(machineBase("devbox")).toBe("./api/m/devbox");
});

test("switching machines loads a fresh page without a link target", () => {
  expect(machineUrl("https://hub.ts.net/?x=1#s=abc&entry=2", "devbox")).toBe("https://hub.ts.net/?x=1&m=devbox");
  expect(machineUrl("https://hub.ts.net/?m=devbox#s=abc", "gpu")).toBe("https://hub.ts.net/?m=gpu");
  expect(machineUrl("https://hub.ts.net/?m=devbox#s=abc", null)).toBe("https://hub.ts.net/");
});

test("pins, checkout choices, and the seen marker never cross machines", () => {
  const storage = memoryStorage();
  const hub = machineStorage(storage, null);
  const devbox = machineStorage(storage, "devbox");
  const gpu = machineStorage(storage, "gpu");

  // A peer can announce any session id, including a hub session's.
  writeSeen(hub, "s1", { id: 7, at: 1 });
  expect(readSeen(devbox, "s1")).toBeNull();
  writeSeen(devbox, "s1", { id: 9, at: 2 });
  expect(readSeen(hub, "s1")).toEqual({ id: 7, at: 1 });
  expect(readSeen(devbox, "s1")).toEqual({ id: 9, at: 2 });
  expect(readSeen(gpu, "s1")).toBeNull();

  saveCheckoutChoice(devbox, "s1", "/ws2");
  expect(loadCheckoutChoice(hub, "s1")).toBeNull();
  expect(loadCheckoutChoice(gpu, "s1")).toBeNull();
  expect(loadCheckoutChoice(devbox, "s1")).toBe("/ws2");

  hub.setItem("tact.web.pinned-sessions", '["s1"]');
  expect(devbox.getItem("tact.web.pinned-sessions")).toBeNull();
  expect([...storage.values.keys()].filter((key) => key.endsWith(":m:devbox")).length).toBe(2);
});

test("a machine that rejects the hub's token or has left the registry gets its own card", () => {
  const failure = (code: string, status: number) => machineFailure(new ApiError(code, "x", status));
  expect(failure("machine_unauthorized", 409)).toBe("relink");
  expect(failure("unknown_machine", 404)).toBe("unregistered");
  expect(failure("machine_unreachable", 502)).toBeNull();
  expect(failure("unauthorized", 401)).toBeNull();
  expect(machineFailure(new Error("boom"))).toBeNull();
});

test("the bundle's protocol version is inside the range its manifest declares", () => {
  const build = readFileSync(new URL("../../tools/build.ts", import.meta.url), "utf8");
  const min = Number(/web_api: \{ min: (\d+)/.exec(build)![1]);
  const max = Number(/web_api: \{ min: \d+, max: (\d+)/.exec(build)![1]);
  expect(PROTOCOL_VERSION).toBeGreaterThanOrEqual(min);
  expect(PROTOCOL_VERSION).toBeLessThanOrEqual(max);
});
