import type { CodeViewDiffItem } from "@pierre/diffs";
import type { QuestionThread } from "./question-state";
import type { CommentDraft, CommentMetadata } from "./review-state";

/** What a diff line annotation renders: a saved comment, a question thread, or the open comment draft. */
export type AnnotationMetadata =
  | { kind: "comment"; comment: CommentMetadata }
  | { kind: "question"; thread: QuestionThread }
  | { kind: "composer"; draft: CommentDraft };

export type ReviewDiffItem = CodeViewDiffItem<AnnotationMetadata>;

/** Identifies an item's annotations, so an unchanged item is redrawn only when they change. */
export function annotationKey(item: ReviewDiffItem) {
  return (item.annotations ?? []).map(({ side, lineNumber, metadata }) => {
    const id = metadata.kind === "comment" ? metadata.comment.id
      : metadata.kind === "question" ? metadata.thread.id
      : "draft";
    return metadata.kind + ":" + id + ":" + side + ":" + lineNumber;
  }).join("|");
}
