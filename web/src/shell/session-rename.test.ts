import { afterEach, beforeAll, expect, test } from "bun:test";
import { Window } from "happy-dom";
import { ApiClient, ApiError } from "../core/api-client";
import { openSessionRename } from "./session-rename";

let window: Window;
beforeAll(() => {
  window = new Window();
  Object.assign(globalThis, { document: window.document });
});
afterEach(() => document.body.replaceChildren());

function rename(api: ApiClient) {
  openSessionRename(api, "chosen-session", "Existing title");
  const form = document.querySelector("form")!;
  const input = form.querySelector("input")!;
  return {
    input,
    save: form.querySelector<HTMLButtonElement>('[type="submit"]')!,
    notice: form.querySelector<HTMLElement>('[role="alert"]')!,
    change(value: string) {
      input.value = value;
      input.dispatchEvent(new window.Event("input") as unknown as Event);
    },
    async submit() {
      form.dispatchEvent(new window.Event("submit", { cancelable: true }) as unknown as Event);
      await Promise.resolve();
    },
  };
}

test("prefills the title and sends a trimmed rename command", async () => {
  const calls: unknown[] = [];
  const api = { command: async (...args: unknown[]) => { calls.push(args); } } as unknown as ApiClient;
  const dialog = rename(api);
  expect(dialog.input.value).toBe("Existing title");
  dialog.change("  Chosen title  ");
  await dialog.submit();
  expect(calls).toEqual([["rename_session", { session: "chosen-session", title: "Chosen title" }]]);
  expect(document.querySelector("dialog")).toBeNull();
});

test("blocks blank titles", async () => {
  const calls: unknown[] = [];
  const api = { command: async (...args: unknown[]) => { calls.push(args); } } as unknown as ApiClient;
  const dialog = rename(api);
  dialog.change("   ");
  expect(dialog.save.disabled).toBe(true);
  await dialog.submit();
  expect(calls).toEqual([]);
});

test("retains the title and enables retry after a refusal", async () => {
  const api = { command: async () => { throw new ApiError("invalid_request", "Title is too long."); } } as unknown as ApiClient;
  const dialog = rename(api);
  dialog.change("Chosen title");
  await dialog.submit();
  expect(dialog.notice.textContent).toBe("Title is too long.");
  expect(dialog.notice.hidden).toBe(false);
  expect(dialog.input.value).toBe("Chosen title");
  expect(dialog.input.disabled).toBe(false);
  expect(dialog.save.disabled).toBe(false);
  expect(document.querySelector("dialog")).not.toBeNull();
});

test("cancel dismisses without sending a command", () => {
  const dialog = rename({} as ApiClient);
  dialog.input.closest("form")!.querySelector<HTMLButtonElement>(".cancel")!.click();
  expect(document.querySelector("dialog")).toBeNull();
});
