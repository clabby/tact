type PathComment = {
  path: string;
};

type DiffPath = {
  name: string;
  prevName?: string;
};

export function annotationPath(
  file: DiffPath,
  side: "additions" | "deletions",
) {
  if (side === "deletions") return file.prevName ?? file.name;
  return file.name;
}

export function pendingCommentCount(comments: readonly PathComment[], path: string) {
  return comments.reduce(
    (count, comment) => count + Number(comment.path === path),
    0,
  );
}
