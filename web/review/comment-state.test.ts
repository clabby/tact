import { expect, test } from "bun:test";
import { annotationPath, pendingCommentCount } from "./comment-state";

test("uses the old path for annotations on the deleted side of a rename", () => {
  const file = { name: "src/new.rs", prevName: "src/old.rs" };

  expect(annotationPath(file, "deletions")).toBe("src/old.rs");
  expect(annotationPath(file, "additions")).toBe("src/new.rs");
});

test("counts pending comments for one file", () => {
  const comments = [
    { path: "src/main.rs" },
    { path: "README.md" },
    { path: "src/main.rs" },
  ];

  expect(pendingCommentCount(comments, "src/main.rs")).toBe(2);
  expect(pendingCommentCount(comments, "src/lib.rs")).toBe(0);
});
