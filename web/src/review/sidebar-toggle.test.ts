import { afterEach, beforeAll, expect, test } from "bun:test";
import { Window } from "happy-dom";
import { panelMarkup } from "./panel-markup";
import { bindSidebarToggle } from "./sidebar-toggle";

let window: Window;

beforeAll(() => {
  window = new Window();
  const { document, HTMLElement } = window;
  Object.assign(globalThis, { document, HTMLElement, window });
});

afterEach(() => document.body.replaceChildren());

function memoryStorage() {
  const values = new Map<string, string>();
  return {
    getItem: (key: string) => values.get(key) ?? null,
    setItem: (key: string, value: string) => void values.set(key, value),
  };
}

/** Renders the panel skeleton and binds its sidebar toggle to `storage`. */
function mount(storage: ReturnType<typeof memoryStorage>) {
  const root = document.createElement("div");
  root.innerHTML = panelMarkup("repository", "", "");
  document.body.append(root);
  bindSidebarToggle(root, storage);
  const button = root.querySelector<HTMLButtonElement>("#sidebar-toggle")!;
  const hidden = () => root.querySelector("#changes-panel")!.hasAttribute("data-sidebar-hidden");
  return { button, hidden };
}

test("the sidebar starts shown and folds away from the toolbar", () => {
  const { button, hidden } = mount(memoryStorage());
  expect(hidden()).toBe(false);
  expect(button.getAttribute("aria-expanded")).toBe("true");

  button.click();

  expect(hidden()).toBe(true);
  expect(button.getAttribute("aria-expanded")).toBe("false");
  expect(button.getAttribute("aria-label")).toBe("Show files and comments");
});

test("a folded sidebar stays folded when the panel is drawn again", () => {
  const storage = memoryStorage();
  mount(storage).button.click();
  document.body.replaceChildren();

  const { button, hidden } = mount(storage);
  expect(hidden()).toBe(true);

  button.click();
  document.body.replaceChildren();
  expect(mount(storage).hidden()).toBe(false);
});
