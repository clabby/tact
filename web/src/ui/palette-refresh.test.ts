import { afterEach, beforeEach, expect, test } from "bun:test";
import { Window } from "happy-dom";
import { Palette, type PaletteCommand } from "./palette";

let window: Window;
beforeEach(() => {
  window = new Window();
  Object.assign(globalThis, { document: window.document });
});
afterEach(() => document.body.replaceChildren());

const command = (id: string, title = id): PaletteCommand => ({ id, title, group: "Sessions", run() {} });
const highlighted = () => document.querySelector('[aria-selected="true"] .palette-title')?.textContent;
function moveDown() {
  document.querySelector("input")!.dispatchEvent(new window.KeyboardEvent("keydown", { key: "ArrowDown" }) as unknown as Event);
}

test("refreshes an open picker with its current query and keeps the highlighted session", async () => {
  const palette = new Palette();
  let results = [command("a"), command("b"), command("c")];
  const queries: string[] = [];
  palette.pick({
    placeholder: "Search sessions", empty: "No sessions",
    async load(query) {
      queries.push(query);
      return results;
    },
  });
  await Promise.resolve();
  const input = document.querySelector("input")!;
  input.value = "saved";
  input.dispatchEvent(new window.Event("input") as unknown as Event);
  await Promise.resolve();
  moveDown();
  expect(highlighted()).toBe("b");
  results = [command("b", "Generated title"), command("a"), command("c")];
  await palette.refresh();
  expect(queries).toEqual(["", "saved", "saved"]);
  expect(input.value).toBe("saved");
  expect(highlighted()).toBe("Generated title");
  expect(input.getAttribute("aria-activedescendant")).toBe("palette-item-0");
});

test("refresh clamps selection after removal and handles an empty registry", async () => {
  const palette = new Palette();
  let commands = [command("a"), command("b"), command("c")];
  palette.register(() => commands);
  palette.open();
  moveDown();
  moveDown();
  expect(highlighted()).toBe("c");
  commands = [command("a", "Updated title")];
  await palette.refresh();
  expect(highlighted()).toBe("Updated title");
  commands = [];
  await palette.refresh();
  expect(highlighted()).toBeUndefined();
  expect(document.querySelector("input")!.getAttribute("aria-activedescendant")).toBe("");
});

test("refresh does not query a closed picker", async () => {
  const palette = new Palette();
  let calls = 0;
  palette.pick({ placeholder: "Search", empty: "Empty", async load() { calls += 1; return []; } });
  await Promise.resolve();
  palette.close();
  await palette.refresh();
  expect(calls).toBe(1);
});
