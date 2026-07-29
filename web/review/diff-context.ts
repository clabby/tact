import type { FileDiffMetadata } from "@pierre/diffs";

export type FileContext = {
  old_path: string;
  new_path: string;
  old_contents: string;
  new_contents: string;
};

export function addExpandableContext(
  files: FileDiffMetadata[],
  _contexts: FileContext[],
): FileDiffMetadata[] {
  return files;
}
