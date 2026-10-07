import { expect, test } from "bun:test";
import { activeIndex, lineLength, promptLabel, windowFor } from "./prompt-rail";

test("a short list is shown whole, and a long one centres on the active prompt", () => {
  expect(windowFor(5, 2, 12)).toEqual({ start: 0, end: 5 });
  expect(windowFor(40, 20, 12)).toEqual({ start: 14, end: 26 });
  expect(windowFor(40, 0, 12)).toEqual({ start: 0, end: 12 });
  expect(windowFor(40, 39, 12)).toEqual({ start: 28, end: 40 });
});

test("paging overrides centring but stays inside the list", () => {
  expect(windowFor(40, 20, 12, 3)).toEqual({ start: 3, end: 15 });
  expect(windowFor(40, 20, 12, -5)).toEqual({ start: 0, end: 12 });
  expect(windowFor(40, 20, 12, 99)).toEqual({ start: 28, end: 40 });
});

test("the active prompt is the last one that has started above the reading line", () => {
  expect(activeIndex([0, 400, 900], 450)).toBe(1);
  expect(activeIndex([0, 400, 900], 5000)).toBe(2);
  expect(activeIndex([100, 400], 10)).toBe(0);
  expect(activeIndex([], 10)).toBe(0);
});

test("lines scale with the prompt's length within bounds", () => {
  expect(lineLength("hi")).toBeLessThan(lineLength("x".repeat(400)));
  expect(lineLength("")).toBeGreaterThanOrEqual(10);
  expect(lineLength("x".repeat(100000))).toBe(26);
});

test("labels use the first line and skip image markers", () => {
  expect(promptLabel("[Image #1]\n  Fix the bug  \nmore")).toBe("Fix the bug");
  expect(promptLabel("[Image #1]")).toBe("Image");
});
