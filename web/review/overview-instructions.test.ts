import { expect, test } from "bun:test";
import { MAX_OVERVIEW_INSTRUCTIONS_BYTES, overviewInstructionError } from "./overview-instructions";

test("overview instructions enforce the server's UTF-8 byte limit", () => {
  expect(overviewInstructionError("x".repeat(MAX_OVERVIEW_INSTRUCTIONS_BYTES))).toBeUndefined();
  expect(overviewInstructionError(` ${"x".repeat(MAX_OVERVIEW_INSTRUCTIONS_BYTES)} `)).toBeUndefined();
  expect(overviewInstructionError("é".repeat(MAX_OVERVIEW_INSTRUCTIONS_BYTES / 2 + 1)))
    .toContain("at most 8 KiB");
});
