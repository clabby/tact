import { parseDiffFromFile, type FileDiffMetadata } from "@pierre/diffs";

export type FileContext = {
  old_path: string;
  new_path: string;
  old_contents: string;
  new_contents: string;
};

export function addExpandableContext(
  files: FileDiffMetadata[],
  contexts: FileContext[],
): FileDiffMetadata[] {
  const contextsByPath = new Map(
    contexts.map((context) => [context.new_path, context]),
  );

  return files.map((file) => {
    const context = contextsByPath.get(file.name);
    if (!context || context.old_path !== (file.prevName ?? file.name)) return file;

    try {
      const fullDiff = parseDiffFromFile(
        {
          name: context.old_path,
          contents: context.old_contents,
        },
        {
          name: context.new_path,
          contents: context.new_contents,
        },
        undefined,
        true,
      );
      return {
        ...file,
        hunks: fullDiff.hunks,
        splitLineCount: fullDiff.splitLineCount,
        unifiedLineCount: fullDiff.unifiedLineCount,
        isPartial: false,
        deletionLines: fullDiff.deletionLines,
        additionLines: fullDiff.additionLines,
        cacheKey: undefined,
      };
    } catch {
      return file;
    }
  });
}
