import { describe, expect, test } from "bun:test";
import type { Checkout, Workspaces } from "./wire";
import { checkoutMenu, relativePath, workspaceChip, workspaceLabel } from "./workspaces";

function checkout(path: string, label: string, extra: Partial<Checkout> = {}): Checkout {
  return {
    path, label, name: path.split("/").pop()!, kind: "git", head: null, changed_files: 0,
    current: false, missing: false, touched: false, ...extra,
  };
}

const family: Workspaces = {
  default: "/src/tact",
  checkouts: [
    checkout("/src/tact", "main", { current: true, changed_files: 2 }),
    checkout("/src/tact-ws2", "ws2", { changed_files: 4, touched: true }),
    checkout("/src/review-ui", "review-ui", { kind: "jj" }),
    checkout("/src/gone", "gone", { missing: true }),
  ],
  recent: ["/src/tact-ws2", "/src/commonware", "/elsewhere/a", "/elsewhere/b", "/elsewhere/c"],
};

describe("relative paths", () => {
  test("siblings and nearby directories are written relative", () => {
    expect(relativePath("/src/tact", "/src/tact-ws2")).toBe("../tact-ws2");
    expect(relativePath("/src/tact", "/src/tact")).toBe(".");
    expect(relativePath("/src/tact", "/src/tact/sub")).toBe("sub");
  });

  test("distant directories stay absolute", () => {
    expect(relativePath("/a/b/c/d", "/a/x")).toBe("/a/x");
    expect(relativePath("/a/b", "/z/y")).toBe("/z/y");
  });
});

describe("checkout menu rows", () => {
  test("list the repository with counts, the chosen row, touched dots, and missing checkouts", () => {
    const rows = checkoutMenu(family, { selected: "/src/tact", recent: false, pick() {} });
    expect(rows.map(({ label, hint, detail, checked, disabled, swatch }) => ({ label, hint, detail, checked, disabled, swatch }))).toEqual([
      { label: "main", hint: "session", detail: "2 changed", checked: true, disabled: false, swatch: "transparent" },
      { label: "ws2", hint: "../tact-ws2", detail: "4 changed", checked: false, disabled: false, swatch: "var(--accent)" },
      { label: "review-ui", hint: "../review-ui", detail: undefined, checked: false, disabled: false, swatch: "transparent" },
      { label: "gone", hint: "../gone", detail: "missing", checked: false, disabled: true, swatch: "transparent" },
    ]);
    expect(new Set(rows.map((row) => row.section))).toEqual(new Set(["This repository"]));
  });

  test("offer at most three recent workspaces that are not already listed", () => {
    const rows = checkoutMenu(family, { selected: "/src/tact", recent: true, pick() {} }).filter((row) => row.section === "Recent");
    expect(rows.map((row) => [row.label, row.hint])).toEqual([
      ["commonware", "../commonware"], ["a", "/elsewhere/a"], ["b", "/elsewhere/b"],
    ]);
  });

  test("choosing a row picks its path", () => {
    const picked: string[] = [];
    checkoutMenu(family, { selected: "/src/tact", recent: true, pick: (path) => picked.push(path) })[1]!.run();
    expect(picked).toEqual(["/src/tact-ws2"]);
  });
});

describe("workspace chip", () => {
  test("appears when the repository has several checkouts", () => {
    expect(workspaceChip(family, "/src/tact")).toEqual({ path: "/src/tact", label: "main" });
  });

  test("appears for a lone checkout only when it is not the default workspace", () => {
    const lone = { default: "/src/tact", checkouts: [checkout("/src/tact", "main", { current: true })], recent: [] };
    expect(workspaceChip(lone, "/src/tact")).toBeNull();
    expect(workspaceChip(lone, "/src/notes")).toEqual({ path: "/src/notes", label: "notes" });
  });

  test("labels prefer the checkout's branch or workspace name", () => {
    expect(workspaceLabel("/src/tact-ws2", family.checkouts)).toBe("ws2");
    expect(workspaceLabel("/src/other/")).toBe("other");
  });
});
