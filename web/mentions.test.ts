import { expect, test } from "bun:test";
import { completeMention, findMention } from "./mentions";

test("triggers are recognised at word starts only", () => {
  expect(findMention("see @src/ma", 11)).toEqual({ kind: "file", query: "src/ma", start: 4, end: 11 });
  expect(findMention("@@019a", 6)).toEqual({ kind: "session", query: "019a", start: 0, end: 6 });
  expect(findMention("use $rev", 8)).toEqual({ kind: "skill", query: "rev", start: 4, end: 8 });
  expect(findMention("mail me@host", 12)).toBeNull();
  expect(findMention("costs 5$", 8)).toBeNull();
  expect(findMention("@file done", 10)).toBeNull();
});

test("an empty query still opens the picker", () => {
  expect(findMention("look at @", 9)).toEqual({ kind: "file", query: "", start: 8, end: 9 });
});

test("completion replaces the typed query and keeps the rest of the text", () => {
  const text = "fix @sr please";
  const mention = findMention(text, 7)!;

  expect(completeMention(text, mention, "src/main.rs")).toEqual({ text: "fix @src/main.rs please", caret: 16 });
  expect(completeMention("@", findMention("@", 1)!, "src/")).toEqual({ text: "@src/", caret: 5 });
  expect(completeMention("$r", findMention("$r", 2)!, "review")).toEqual({ text: "$review ", caret: 8 });
});
