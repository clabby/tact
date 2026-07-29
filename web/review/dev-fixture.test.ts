import { expect, test } from "bun:test";
import { parsePatchFiles } from "@pierre/diffs";
import { reviewFixture } from "./dev-fixture";

test("standalone review fixture contains a valid patch", () => {
  expect(() => parsePatchFiles(reviewFixture.patch, "dev-fixture", true)).not.toThrow();
});
