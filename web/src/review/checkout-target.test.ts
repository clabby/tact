import { describe, expect, test } from "bun:test";
import type { Checkout } from "../core/wire";
import { TouchedCheckouts, changeAffectsTarget, loadCheckoutChoice, saveCheckoutChoice, touchedNotice } from "./checkout-target";

function checkout(path: string, touched = false): Checkout {
  return { path, label: path.slice(1), name: path.slice(1), kind: "git", head: null, changed_files: 0, current: false, missing: false, touched };
}

function memoryStorage() {
  const values = new Map<string, string>();
  return { getItem: (key: string) => values.get(key) ?? null, setItem: (key: string, value: string) => void values.set(key, value), values };
}

describe("the reviewed checkout is remembered per session", () => {
  test("each session keeps its own choice, and null returns to the session workspace", () => {
    const storage = memoryStorage();
    saveCheckoutChoice(storage, "a", "/ws2");
    saveCheckoutChoice(storage, "b", "/ui");
    expect(loadCheckoutChoice(storage, "a")).toBe("/ws2");
    expect(loadCheckoutChoice(storage, "b")).toBe("/ui");
    saveCheckoutChoice(storage, "a", null);
    expect(loadCheckoutChoice(storage, "a")).toBeNull();
    expect(loadCheckoutChoice(storage, "c")).toBeNull();
  });

  test("old choices are dropped and unreadable storage is ignored", () => {
    const storage = memoryStorage();
    for (let index = 0; index < 60; index++) saveCheckoutChoice(storage, `s${index}`, "/ws2");
    expect(loadCheckoutChoice(storage, "s0")).toBeNull();
    expect(loadCheckoutChoice(storage, "s59")).toBe("/ws2");
    storage.setItem("tact.web.review-checkout", "{not json");
    expect(loadCheckoutChoice(storage, "s59")).toBeNull();
  });
});

describe("workspace events", () => {
  test("only changes to the reviewed checkout matter", () => {
    expect(changeAffectsTarget(null, null, "/main")).toBe(true);
    expect(changeAffectsTarget("/main", null, "/main")).toBe(true);
    expect(changeAffectsTarget("/ws2", null, "/main")).toBe(false);
    expect(changeAffectsTarget(null, "/ws2", "/main")).toBe(false);
    expect(changeAffectsTarget("/ws2", "/ws2", "/main")).toBe(true);
    expect(changeAffectsTarget(null, "/main", "/main")).toBe(true);
  });

  test("before the session's path is known, unattributed changes concern the session workspace", () => {
    expect(changeAffectsTarget("/ws2", null, null)).toBe(true);
    expect(changeAffectsTarget(null, "/ws2", null)).toBe(false);
    expect(changeAffectsTarget("/ws2", "/ws2", null)).toBe(true);
  });
});

describe("touched checkouts", () => {
  test("announce checkouts besides the target until acknowledged", () => {
    const touched = new TouchedCheckouts();
    touched.update([checkout("/main", true), checkout("/ws2", true), checkout("/ui")], "/main");
    expect(touched.unseen.map((entry) => entry.path)).toEqual(["/ws2"]);
    expect(touchedNotice(touched.unseen)).toBe("Agent is also working in ws2");
    touched.acknowledge();
    expect(touched.unseen).toEqual([]);
    touched.update([checkout("/ws2", true), checkout("/ui", true)], "/main");
    expect(touchedNotice(touched.unseen)).toBe("Agent is also working in ui");
  });

  test("a checkout touched again after a quiet poll is announced again", () => {
    const touched = new TouchedCheckouts();
    touched.update([checkout("/ws2", true), checkout("/ui", true)], "/main");
    expect(touchedNotice(touched.unseen)).toBe("2 other checkouts changed by the agent");
    touched.acknowledge();
    touched.update([checkout("/ws2")], "/main");
    touched.update([checkout("/ws2", true)], "/main");
    expect(touched.unseen.map((entry) => entry.path)).toEqual(["/ws2"]);
  });
});
