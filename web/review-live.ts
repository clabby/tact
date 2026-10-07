import type { FileDiffMetadata } from "@pierre/diffs";
import { annotationPath } from "./comment-state";
import type { ReviewComment } from "./protocol";
import type { QuestionThread } from "./question-state";
import type { FeedbackState } from "./review-state";

type Side = ReviewComment["side"];
type Anchor = { path: string; side: Side; start: number; end: number };

/**
 * The diff lines Pierre can show for one side of a file, keyed by line number. Lines outside the
 * rendered hunks are unknown and therefore cannot anchor feedback.
 */
function sideLines(file: FileDiffMetadata, side: Side) {
  const lines = new Map<number, string>();
  for (const hunk of file.hunks) {
    let deletion = hunk.deletionStart;
    let addition = hunk.additionStart;
    for (const content of hunk.hunkContent) {
      if (content.type === "context") {
        for (let offset = 0; offset < content.lines; offset++) {
          if (side === "additions") {
            lines.set(addition + offset, file.additionLines[content.additionLineIndex + offset]);
          } else {
            lines.set(deletion + offset, file.deletionLines[content.deletionLineIndex + offset]);
          }
        }
        deletion += content.lines;
        addition += content.lines;
        continue;
      }
      if (side === "deletions") {
        for (let offset = 0; offset < content.deletions; offset++) {
          lines.set(deletion + offset, file.deletionLines[content.deletionLineIndex + offset]);
        }
      } else {
        for (let offset = 0; offset < content.additions; offset++) {
          lines.set(addition + offset, file.additionLines[content.additionLineIndex + offset]);
        }
      }
      deletion += content.deletions;
      addition += content.additions;
    }
  }
  return lines;
}

/** Identifies a file's rendered diff, so equal signatures mean the reviewer sees identical content. */
function fileSignature(file: FileDiffMetadata) {
  return JSON.stringify([
    file.name,
    file.prevName,
    file.type,
    file.hunks.map((hunk) => [
      hunk.additionStart, hunk.additionCount, hunk.deletionStart, hunk.deletionCount,
    ]),
    file.additionLines,
    file.deletionLines,
  ]);
}

/** The names of files in `next` whose diff is not identical to the same file in `previous`. */
export function changedFileNames(
  previous: readonly FileDiffMetadata[],
  next: readonly FileDiffMetadata[],
) {
  const before = new Map(previous.map((file) => [file.name, fileSignature(file)]));
  return new Set(
    next.filter((file) => before.get(file.name) !== fileSignature(file)).map((file) => file.name),
  );
}

/**
 * Finds where the text an anchor covered now lives. The nearest identical run of lines wins, so
 * insertions above the anchor move it and edits to the anchored text orphan it.
 */
function relocate(
  previous: readonly FileDiffMetadata[],
  next: readonly FileDiffMetadata[],
  anchor: Anchor,
) {
  const before = previous.find((file) => annotationPath(file, anchor.side) === anchor.path);
  const after = next.find((file) => annotationPath(file, anchor.side) === anchor.path);
  if (!before || !after) return undefined;
  const old = sideLines(before, anchor.side);
  const text: string[] = [];
  for (let line = anchor.start; line <= anchor.end; line++) {
    const content = old.get(line);
    if (content === undefined) return undefined;
    text.push(content);
  }
  const current = sideLines(after, anchor.side);
  let best: number | undefined;
  for (const start of current.keys()) {
    if (!text.every((content, offset) => current.get(start + offset) === content)) continue;
    if (best === undefined || Math.abs(start - anchor.start) < Math.abs(best - anchor.start)) best = start;
  }
  if (best === undefined) return undefined;
  return { file: after, start: best, end: best + text.length - 1 };
}

/**
 * Moves feedback from one snapshot of the diff to the next. Seen marks survive only on files whose
 * diff is unchanged. Comments and drafts follow their lines; those whose lines were edited or
 * removed stay as outdated comments so the reviewer's text is never lost.
 */
export function carryFeedback(
  previous: readonly FileDiffMetadata[],
  next: readonly FileDiffMetadata[],
  feedback: FeedbackState,
  nextCommentId: () => number,
): FeedbackState {
  const changed = changedFileNames(previous, next);
  const carried: FeedbackState = {
    summary: feedback.summary,
    comments: [],
    seenPaths: new Set([...feedback.seenPaths].filter(
      (path) => next.some((file) => file.name === path) && !changed.has(path),
    )),
  };
  const move = (
    anchored: { path: string; side: Side },
    start: number,
    end: number,
  ) => {
    const moved = relocate(previous, next, { path: anchored.path, side: anchored.side, start, end });
    return moved && { itemId: moved.file.name, start: moved.start, end: moved.end };
  };

  for (const comment of feedback.comments) {
    const moved = comment.outdated ? undefined : move(comment, comment.start_line, comment.end_line);
    carried.comments.push(moved
      ? { ...comment, itemId: moved.itemId, start_line: moved.start, end_line: moved.end }
      : { ...comment, itemId: "", outdated: true });
  }

  const draft = feedback.draft;
  if (!draft) return carried;
  const moved = move(draft, draft.startLine, draft.endLine);
  if (moved) {
    carried.draft = { ...draft, itemId: moved.itemId, startLine: moved.start, endLine: moved.end };
    return carried;
  }
  const edited = carried.comments.find((comment) => comment.id === draft.editingId);
  if (edited) edited.body = draft.body;
  else if (draft.body.trim()) {
    carried.comments.push({
      id: nextCommentId(),
      itemId: "",
      path: draft.path,
      side: draft.side,
      start_line: draft.startLine,
      end_line: draft.endLine,
      body: draft.body.trim(),
      outdated: true,
    });
  }
  return carried;
}

/** Question threads whose code is still in the diff, moved to where that code now is. */
export function carryQuestions(
  previous: readonly FileDiffMetadata[],
  next: readonly FileDiffMetadata[],
  threads: readonly QuestionThread[],
): QuestionThread[] {
  const carried: QuestionThread[] = [];
  for (const thread of threads) {
    const moved = relocate(previous, next, {
      path: thread.path,
      side: thread.side,
      start: thread.startLine,
      end: thread.endLine,
    });
    if (moved) {
      carried.push({ ...thread, itemId: moved.file.name, startLine: moved.start, endLine: moved.end });
    }
  }
  return carried;
}
