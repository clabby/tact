import { afterEach, beforeAll, expect, test } from "bun:test";
import { Window } from "happy-dom";
import { openMenu } from "../ui/menu";
import { panelMarkup } from "./panel-markup";
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
    seenFiles: () => new Set(),
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
