import { expect, test } from "bun:test";
import { promptParts } from "./user-prompt";

test("an image is a block of its own between the text before and after it", () => {
  expect(promptParts("look at [Image #1] closely", 1)).toEqual([
    { kind: "text", text: "look at " },
    { kind: "image", index: 0, marker: "[Image #1]" },
    { kind: "text", text: " closely" },
  ]);
});

test("a marker-only prompt has just the image, and line breaks around it are dropped", () => {
  expect(promptParts("\n[Image #1]\n", 1)).toEqual([{ kind: "image", index: 0, marker: "[Image #1]" }]);
});

test("markers without an attachment stay as text", () => {
  expect(promptParts("[Image #1] and [Image #2]", 1)).toEqual([
    { kind: "image", index: 0, marker: "[Image #1]" },
    { kind: "text", text: " and [Image #2]" },
  ]);
  expect(promptParts("see [Image #1]", 0)).toEqual([{ kind: "text", text: "see [Image #1]" }]);
});
