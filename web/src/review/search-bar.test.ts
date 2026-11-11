import { afterEach, beforeAll, expect, test } from "bun:test";
import type { CodeView } from "@pierre/diffs";
import { Window } from "happy-dom";
import { openMenu } from "../ui/menu";
import type { AnnotationMetadata, ReviewDiffItem } from "./annotations";
import { panelMarkup } from "./panel-markup";
import { parseReviewPatch } from "./review-diff";
import { SearchBar } from "./search-bar";

let window: Window;
const mounted: SearchBar[] = [];
const cleanups: Array<() => void> = [];

beforeAll(() => {
  window = new Window();
  const { document, HTMLElement } = window;
  Object.assign(globalThis, { document, HTMLElement, window, innerWidth: 1024, innerHeight: 768, CSS: {} });
});

afterEach(() => {
  for (const search of mounted.splice(0)) search.dispose();
  for (const cleanUp of cleanups.splice(0)) cleanUp();
  document.body.replaceChildren();
});

/** Binds a search bar with no changed files to `markup`, the rendered panel skeleton by default. */
function mount(markup = panelMarkup("repository", "", ""), visible = () => true) {
  const root = document.createElement("div");
  root.innerHTML = markup;
  document.body.append(root);
  const search = new SearchBar(root, {
    visible,
    files: () => [],
    items: () => [],
    viewer: () => undefined,
    setCollapsed() {},
    selectMobilePanel() {},
  });
  search.bind();
  mounted.push(search);
  return { root, search };
}

test("opens and focuses the query wherever the focus was", () => {
  const outside = document.createElement("button");
  document.body.append(outside);
  const { root, search } = mount();
  outside.focus();

  expect(search.open()).toBe(true);
  expect(search.isOpen()).toBe(true);
  expect(document.activeElement).toBe(root.querySelector("#review-search-input"));
});

test("does not open before the changes have rendered", () => {
  const { search } = mount('<div class="panel-notice" role="status">Loading changes…</div>');

  expect(search.open()).toBe(false);
  expect(search.isOpen()).toBe(false);
});

/** Records whether each keydown reached the page already handled, as the app's shortcuts see it. */
function pageShortcuts() {
  const handled: boolean[] = [];
  const listener = (event: KeyboardEvent) => handled.push(event.defaultPrevented);
  document.addEventListener("keydown", listener);
  cleanups.push(() => document.removeEventListener("keydown", listener));
  return handled;
}

function pressEscape(target: Element) {
  target.dispatchEvent(new window.KeyboardEvent("keydown", { key: "Escape", bubbles: true, cancelable: true }) as unknown as Event);
}

test("Esc closes the search before the page's shortcuts see it", () => {
  const handled = pageShortcuts();
  const { root, search } = mount();
  search.open();

  pressEscape(root.querySelector("#review-search-input")!);

  expect(search.isOpen()).toBe(false);
  expect(handled).toEqual([true]);
});

test("Esc belongs to the page while the review is hidden", () => {
  const handled = pageShortcuts();
  let visible = true;
  const { search } = mount(undefined, () => visible);
  search.open();
  visible = false;

  pressEscape(document.body);

  expect(search.isOpen()).toBe(true);
  expect(handled).toEqual([false]);
});

test.each(["before", "after"])("Esc preserves search when the menu opens %s changes load", (when) => {
  const handled = pageShortcuts();
  const anchor = document.createElement("button");
  document.body.append(anchor);
  const open = () => openMenu(anchor, [{ label: "Theme", run() {} }], "Theme");
  if (when === "before") open();
  const { search } = mount();
  search.open();
  if (when === "after") open();

  pressEscape(document.activeElement!);

  expect(document.querySelector('[role="menu"]')).toBeNull();
  expect(search.isOpen()).toBe(true);
  expect(handled).toEqual([]);
});

/** Binds a search bar to collapsed files named `alpha.ts` and `beta.ts`, each with one line naming it. */
function mountCollapsedFiles() {
  const patch = ["alpha", "beta"].map((name) => [
    `diff --git a/${name}.ts b/${name}.ts\n`,
    "new file mode 100644\n",
    "--- /dev/null\n",
    `+++ b/${name}.ts\n`,
    "@@ -0,0 +1 @@\n",
    `+export const ${name} = true;\n`,
  ].join("")).join("");
  const files = parseReviewPatch(patch, "collapsed", false);
  const items: ReviewDiffItem[] = files.map((file) => ({ id: file.name, type: "diff", fileDiff: file, collapsed: true }));
  const viewer = {
    getSelectedLines: () => null,
    setSelectedLines() {},
    scrollTo() {},
    getRenderedItems: () => [],
  } as unknown as CodeView<AnnotationMetadata>;
  const root = document.createElement("div");
  root.innerHTML = panelMarkup("repository", "", "");
  document.body.append(root);
  const search = new SearchBar(root, {
    visible: () => true,
    files: () => files,
    items: () => items,
    viewer: () => viewer,
    setCollapsed(item, collapsed) { item.collapsed = collapsed; },
    selectMobilePanel() {},
  });
  search.bind();
  mounted.push(search);
  const input = root.querySelector<HTMLInputElement>("#review-search-input")!;
  const find = (query: string) => {
    input.value = query;
    input.dispatchEvent(new window.Event("input") as unknown as Event);
  };
  const collapsed = () => Object.fromEntries(items.map((item) => [item.id, item.collapsed]));
  return { search, find, collapsed };
}

test("a collapsed file opens for a match in it and closes again when the match moves on", () => {
  const { search, find, collapsed } = mountCollapsedFiles();
  search.open();

  find("alpha =");
  expect(collapsed()).toEqual({ "alpha.ts": false, "beta.ts": true });

  find("beta =");
  expect(collapsed()).toEqual({ "alpha.ts": true, "beta.ts": false });

  search.close();
  expect(collapsed()).toEqual({ "alpha.ts": true, "beta.ts": true });
});

test("a file the reviewer opened during a search stays open after it", () => {
  const { search, find, collapsed } = mountCollapsedFiles();
  search.open();

  find("alpha =");
  search.fileExpanded("alpha.ts");
  search.close();

  expect(collapsed()).toEqual({ "alpha.ts": false, "beta.ts": true });
});
