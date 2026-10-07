import { describe, expect, test } from "bun:test";
import { commentSelectionCallbacks } from "./comment-selection";

describe("comment selection timing", () => {
  test("waits for pointer release before opening the composer", () => {
    const opened: unknown[] = [];
    const callbacks = commentSelectionCallbacks((selection) => opened.push(selection));
    const range = { start: 4, end: 8, side: "additions" as const };

    expect("onLineSelectionStart" in callbacks).toBe(false);
    expect("onLineSelectionChange" in callbacks).toBe(false);
    expect(opened).toEqual([]);

    callbacks.onLineSelectionEnd(range, { item: { id: "src/main.rs" } });

    expect(opened).toEqual([{ id: "src/main.rs", range }]);
  });
});
