import { expect, test } from "bun:test";
import { MockRefusal, MockTact } from "./dev-mock";

test("the mock follows the shared-draft contract: tagged echoes and revision-checked submit", async () => {
  const mock = new MockTact();
  mock.pace = 0;
  const session = mock.active!;
  const drafts: { rev: number; text: string; origin: string | null }[] = [];
  mock.subscribe((name, data) => {
    if (name === "draft") drafts.push(data as (typeof drafts)[number]);
  });

  mock.command("set_draft", { session, text: "hello" }, 7);
  const { rev } = drafts.at(-1)!;
  expect(drafts.at(-1)).toMatchObject({ text: "hello", origin: "web:7" });

  expect(() => mock.command("submit", { session, rev: rev - 1 }, 7)).toThrow(MockRefusal);
  mock.command("submit", { session, rev }, 7);
  expect(drafts.at(-1)).toMatchObject({ rev: rev + 1, text: "" });

  mock.command("interrupt", { session }, 7);
});
