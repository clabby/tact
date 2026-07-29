import { expect, test } from "bun:test";
import { pendingCommentCount } from "./comment-state";

test("counts pending comments for one file", () => {
  const comments = [
    { path: "src/main.rs" },
    { path: "README.md" },
    { path: "src/main.rs" },
  ];

  expect(pendingCommentCount(comments, "src/main.rs")).toBe(2);
  expect(pendingCommentCount(comments, "src/lib.rs")).toBe(0);
});
