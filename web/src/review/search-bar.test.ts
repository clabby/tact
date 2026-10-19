import { afterEach, beforeAll, expect, test } from "bun:test";
import { Window } from "happy-dom";
import { panelMarkup } from "./panel-markup";
import { SearchBar } from "./search-bar";

let window: Window;
const mounted: SearchBar[] = [];

beforeAll(() => {
  window = new Window();
  const { document, HTMLElement } = window;
  Object.assign(globalThis, { document, HTMLElement, CSS: {} });
});

afterEach(() => {
  for (const search of mounted.splice(0)) search.dispose();
  document.body.replaceChildren();
});

/** Binds a search bar with no changed files to `markup`, the rendered panel skeleton by default. */
function mount(markup = panelMarkup("repository", "", "")) {
  const root = document.createElement("div");
  root.innerHTML = markup;
  document.body.append(root);
  const search = new SearchBar(root, {
    visible: () => true,
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
