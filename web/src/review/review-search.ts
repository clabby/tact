import type { FileDiffMetadata } from "@pierre/diffs";

export type ReviewSearchMatch =
  | { kind: "path"; itemId: string }
  | {
      kind: "content";
      itemId: string;
      side: "additions" | "deletions";
      lineNumber: number;
      start: number;
      length: number;
      occurrenceIndex: number;
      occurrenceCount: number;
    };

export function searchReview(
  files: readonly FileDiffMetadata[],
  query: string,
  target = 0,
  targetOccurrence = 0,
): { count: number; match?: ReviewSearchMatch } {
  if (!query.trim()) return { count: 0 };

  const needle = query.toLowerCase();
  let count = 0;
  let match: ReviewSearchMatch | undefined;
  const isTarget = () => count++ === target;

  for (const file of files) {
    if ((file.name.toLowerCase().includes(needle)
      || file.prevName?.toLowerCase().includes(needle)) && isTarget()) {
      match = { kind: "path", itemId: file.name };
    }

    const recordLines = (
      lines: readonly string[],
      startIndex: number,
      length: number,
      side: "additions" | "deletions",
      lineNumber: number,
    ) => {
      for (let index = 0; index < length; index++) {
        const line = lines[startIndex + index];
        const folded = line.toLowerCase();
        if (!folded.includes(needle)) continue;
        if (!isTarget()) continue;
        const occurrence = findOccurrence(folded, needle, targetOccurrence);
        match = {
          kind: "content",
          itemId: file.name,
          side,
          lineNumber: lineNumber + index,
          ...sourceRange(line, occurrence.start, needle.length),
          occurrenceIndex: occurrence.index,
          occurrenceCount: occurrence.count,
        };
      }
    };

    for (const hunk of file.hunks) {
      let deletionLine = hunk.deletionStart;
      let additionLine = hunk.additionStart;

      for (const content of hunk.hunkContent) {
        if (content.type === "context") {
          recordLines(file.additionLines, content.additionLineIndex, content.lines, "additions", additionLine);
          deletionLine += content.lines;
          additionLine += content.lines;
          continue;
        }

        recordLines(file.deletionLines, content.deletionLineIndex, content.deletions, "deletions", deletionLine);
        recordLines(file.additionLines, content.additionLineIndex, content.additions, "additions", additionLine);

        deletionLine += content.deletions;
        additionLine += content.additions;
      }
    }
  }

  return { count, match };
}

export function moveSearchTarget(
  match: ReviewSearchMatch | undefined,
  index: number,
  count: number,
  direction: -1 | 1,
): [number, number] {
  if (match?.kind === "content") {
    const occurrence = match.occurrenceIndex + direction;
    if (occurrence >= 0 && occurrence < match.occurrenceCount) return [index, occurrence];
  }
  return [
    (index + direction + count) % count,
    direction < 0 ? Number.MAX_SAFE_INTEGER : 0,
  ];
}

function findOccurrence(text: string, needle: string, target: number) {
  let count = 0;
  let start = 0;
  let selected = 0;
  const occurrence = Math.max(0, target);
  while ((start = text.indexOf(needle, start)) >= 0) {
    if (count <= occurrence) selected = start;
    count++;
    start += needle.length;
  }
  return { start: selected, index: Math.min(occurrence, count - 1), count };
}

function sourceRange(text: string, start: number, length: number) {
  let sourceOffset = 0;
  let foldedOffset = 0;
  let sourceStart = 0;
  const foldedEnd = start + length;
  for (const character of text) {
    const sourceEnd = sourceOffset + character.length;
    const nextFoldedOffset = foldedOffset + character.toLowerCase().length;
    if (foldedOffset <= start && start < nextFoldedOffset) sourceStart = sourceOffset;
    if (foldedOffset < foldedEnd && foldedEnd <= nextFoldedOffset) {
      return { start: sourceStart, length: sourceEnd - sourceStart };
    }
    sourceOffset = sourceEnd;
    foldedOffset = nextFoldedOffset;
  }
  return { start, length };
}
