import { beforeAll, describe, expect, test } from "bun:test";
import { Window } from "happy-dom";
import { ApiError } from "../core/api-client";
import { viewerTarget, type ViewerTarget } from "../core/local-files";
import type { LocalDocument } from "../core/wire";
import { showFileViewer } from "./file-viewer";

beforeAll(() => {
  const window = new Window();
  Object.assign(globalThis, {
    document: window.document,
    Node: window.Node,
    CSS: window.CSS,
    localStorage: window.localStorage,
    matchMedia: window.matchMedia.bind(window),
  });
});

function target(path: string, overrides: Partial<ViewerTarget> = {}): ViewerTarget {
  return { path, line: null, fragment: "", session: "s1", ...overrides };
}

/** Opens the viewer over a fake instance that answers with `answer` and records each request. */
async function open(viewed: ViewerTarget, answer: LocalDocument | ApiError) {
  const requests: { path: string; session?: string }[] = [];
  const api = {
    document: async (path: string, session?: string) => {
      requests.push({ path, session });
      if (answer instanceof ApiError) throw answer;
      return answer;
    },
  };
  const root = document.createElement("div");
  document.body.replaceChildren(root);
  await showFileViewer(root, api, viewed);
  return { root, requests };
}

describe("file viewer", () => {
  test("initial and in-page fragments decode percent escapes once", async () => {
    const prototype = document.defaultView!.HTMLElement.prototype;
    const original = prototype.scrollIntoView;
    const scrolled: string[] = [];
    prototype.scrollIntoView = function () {
      scrolled.push(this.getAttribute("data-heading") ?? "");
    };
    try {
      const { root } = await open(
        viewerTarget("?file=report.md", "#%2541")!,
        { path: "/tmp/report.md", text: "[jump](#%2541)\n\n## %41\n\n## A" },
      );
      expect(scrolled).toEqual(["41"]);
      root.querySelector<HTMLAnchorElement>("article a")!.click();
      expect(scrolled).toEqual(["41", "41"]);
    } finally {
      prototype.scrollIntoView = original;
    }
  });

  test("text shows numbered lines and marks the linked line", async () => {
    const { root, requests } = await open(
      target("src/main.rs", { line: 2 }),
      { path: "/work/src/main.rs", text: "fn main() {\n    run();\n}\n" },
    );

    expect(requests).toEqual([{ path: "src/main.rs", session: "s1" }]);
    expect(root.querySelector(".file-viewer-title h1")!.textContent).toBe("main.rs");
    expect(root.querySelector(".file-viewer-title p")!.textContent).toBe("/work/src/main.rs");
    const source = root.querySelector<HTMLElement>(".file-viewer-source")!;
    expect(source.dataset.numbers).toBe("1\n2\n3");
    expect(source.textContent).toBe("fn main() {\n    run();\n}");
    expect(source.querySelector(".file-viewer-target")!.textContent).toBe("    run();");
    expect(root.querySelector<HTMLElement>(".segmented")!.hidden).toBe(true);
  });

  test("many short lines retain their text and target without one element per line", async () => {
    const text = "a\n".repeat(249999) + "target\n" + "a\n".repeat(12144);
    const { root } = await open(target("large.txt", { line: 250000 }), { path: "/tmp/large.txt", text });
    const source = root.querySelector<HTMLElement>(".file-viewer-source")!;

    expect(source.textContent).toBe("a\n".repeat(9999) + "target");
    expect(source.querySelector<HTMLElement>(".file-viewer-target")!.dataset.line).toBe("250000");
    expect(source.querySelectorAll("*").length).toBeLessThan(10);
    expect(root.querySelector("nav.file-viewer-pages span")!.textContent).toBe("Lines 240001–250000 of 262144");
    const next = root.querySelectorAll<HTMLButtonElement>(".file-viewer-pages button")[1]!;
    next.focus();
    next.click();
    expect(document.activeElement === next).toBe(true);
    expect(root.querySelector("nav.file-viewer-pages span")!.textContent).toBe("Lines 250001–260000 of 262144");
    root.querySelectorAll<HTMLButtonElement>(".file-viewer-pages button")[0]!.click();
    expect(root.querySelector<HTMLElement>(".file-viewer-target")!.dataset.line).toBe("250000");
  });

  test("Markdown renders with links and images relative to the file, never as active HTML", async () => {
    const { root } = await open(
      target("/tmp/scratch/report.md"),
      {
        path: "/tmp/scratch/report.md",
        text: "# Report\n\nSee [notes](../notes.md#plan) and [site](https://example.com).\n\n![chart](img/chart.png)\n\n<script>alert(1)</script>",
      },
    );
    const preview = root.querySelector(".file-viewer-document")!;

    expect(preview.querySelector("h1")!.textContent).toBe("Report");
    expect(preview.querySelector("script")).toBeNull();
    const [notes, site] = preview.querySelectorAll("a");
    expect(notes!.getAttribute("href")).toBe("./?file=%2Ftmp%2Fscratch%2F..%2Fnotes.md#plan");
    expect(site!.getAttribute("href")).toBe("https://example.com");
    expect(preview.querySelector("img")!.getAttribute("src")).toBe("./api/file?path=%2Ftmp%2Fscratch%2Fimg%2Fchart.png");

    root.querySelector<HTMLButtonElement>('[data-mode="source"]')!.click();
    expect(root.querySelector(".file-viewer-document")).toBeNull();
    expect(root.querySelector<HTMLElement>(".file-viewer-source")!.dataset.numbers).toBe("1\n2\n3\n4\n5\n6\n7");
  });

  test("a line in a Markdown file opens its source", async () => {
    const { root } = await open(
      target("/tmp/report.md", { line: 3 }),
      { path: "/tmp/report.md", text: "# Report\n\nBody\n" },
    );

    expect(root.querySelector('[data-mode="source"]')!.getAttribute("aria-pressed")).toBe("true");
    expect(root.querySelector(".file-viewer-target")!.textContent).toBe("Body");
  });

  test("a UTF-8 byte-order mark does not hide the first Markdown heading", async () => {
    const text = "\uFEFF# Report\n\nBody\n";
    const { root } = await open(target("report.md"), { path: "/tmp/report.md", text });

    expect(root.querySelector("article h1")?.textContent).toBe("Report");
    root.querySelector<HTMLButtonElement>('[data-mode="source"]')!.click();
    expect(root.querySelector(".file-viewer-source")!.textContent).toBe(text.slice(0, -1));
  });

  test("the linked filename chooses the format when a symlink resolves to a different suffix", async () => {
    for (const [requested, resolved, markdown] of [
      ["/work/report.md", "/shared/report", true],
      ["/work/report.txt", "/shared/report.md", false],
    ] as const) {
      const { root } = await open(target(requested), { path: resolved, text: "# Report\n" });

      expect(root.querySelector(".file-viewer-document") !== null).toBe(markdown);
      expect(root.querySelector(".file-viewer-title p")!.textContent).toBe(resolved);
      if (!markdown) expect(root.querySelector(".file-viewer-source")!.textContent).toBe("# Report");
    }
  });

  test("a file that cannot be shown explains why", async () => {
    const { root } = await open(
      target("/tmp/blob.bin"),
      new ApiError("binary", "This file is not UTF-8 text.", 415),
    );

    expect(root.querySelector('[role="alert"] h1')!.textContent).toBe("Cannot open this file");
    expect(root.querySelector('[role="alert"] p')!.textContent).toBe("This file is not UTF-8 text.");
    expect(root.querySelector(".file-viewer-source")).toBeNull();
  });
});
