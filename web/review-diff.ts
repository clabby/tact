import {
  GIT_DIFF_FILE_BREAK_REGEX,
  parsePatchFiles,
  processFile,
  trimPatchContext,
  type FileContents,
  type FileDiffMetadata,
} from "@pierre/diffs";

const DEFAULT_CONTEXT_LINES = 3;

export function parseReviewPatch(
  patch: string,
  cacheKey: string,
  fullContext: boolean,
): FileDiffMetadata[] {
  const fullFiles = parsePatchFiles(patch, cacheKey, true).flatMap(
    (parsed) => parsed.files,
  );
  const displayPatches = patch
    .split(GIT_DIFF_FILE_BREAK_REGEX)
    .filter((filePatch) => filePatch.startsWith("diff --git"))
    .map((filePatch) => trimPatchContext(filePatch, DEFAULT_CONTEXT_LINES));

  if (displayPatches.length !== fullFiles.length) {
    throw new Error("The review patch contains mismatched file data.");
  }

  return displayPatches.map((filePatch, index) => {
    const fullFile = fullFiles[index];
    const fullContents = fullContext && containsCompleteFiles(fullFile)
      ? {
          oldFile: fileContents(fullFile.prevName ?? fullFile.name, fullFile.deletionLines),
          newFile: fileContents(fullFile.name, fullFile.additionLines),
        }
      : {};
    const file = processFile(filePatch, {
      cacheKey: `${cacheKey}-${index}`,
      ...fullContents,
      throwOnError: true,
    });
    if (!file) throw new Error(`The review patch for ${fullFile.name} is invalid.`);
    return file;
  });
}

function containsCompleteFiles(file: FileDiffMetadata) {
  if (file.hunks.length !== 1) return false;
  const hunk = file.hunks[0];
  return coversWholeFile(hunk.deletionStart, hunk.deletionCount, file.deletionLines.length)
    && coversWholeFile(hunk.additionStart, hunk.additionCount, file.additionLines.length);
}

function coversWholeFile(start: number, count: number, lineCount: number) {
  if (lineCount === 0) return start === 0 && count === 0;
  return start === 1 && count === lineCount;
}

function fileContents(name: string, lines: readonly string[]): FileContents {
  return { name, contents: lines.join("") };
}
