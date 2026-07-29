type PathComment = {
  path: string;
};

export function pendingCommentCount(comments: readonly PathComment[], path: string) {
  return comments.reduce(
    (count, comment) => count + Number(comment.path === path),
    0,
  );
}
