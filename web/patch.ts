import { parsePatchFiles, type FileDiffMetadata } from "@pierre/diffs";

/** One file operation of an `apply_patch` envelope. */
export type PatchFile = {
  path: string;
  kind: "add" | "update" | "delete";
  /** The destination of a rename. */
  moveTo?: string;
  /** Each hunk's lines, prefixed with " ", "+" or "-"; `heading` is the text after "@@". */
  hunks: { heading: string; lines: string[] }[];
};

const OPERATIONS = [
  ["*** Add File: ", "add"],
  ["*** Update File: ", "update"],
  ["*** Delete File: ", "delete"],
] as const;

/** Parses the `*** Begin Patch` envelope that `apply_patch` receives. */
export function parseApplyPatch(envelope: string): PatchFile[] {
  const files: PatchFile[] = [];
  let file: PatchFile | undefined;
  let hunk: PatchFile["hunks"][number] | undefined;
  for (const line of envelope.replace(/\r\n/g, "\n").split("\n")) {
    const operation = OPERATIONS.find(([prefix]) => line.startsWith(prefix));
    if (operation) {
      file = { path: line.slice(operation[0].length).trim(), kind: operation[1], hunks: [] };
      files.push(file);
      hunk = undefined;
    } else if (!file || line.startsWith("*** Begin Patch") || line.startsWith("*** End")) {
      continue;
    } else if (line.startsWith("*** Move to: ")) {
      file.moveTo = line.slice("*** Move to: ".length).trim();
    } else if (line.startsWith("@@")) {
      hunk = { heading: line.slice(2).trim(), lines: [] };
      file.hunks.push(hunk);
    } else if (/^[ +-]/.test(line) || (line === "" && hunk)) {
      if (!hunk) {
        hunk = { heading: "", lines: [] };
        file.hunks.push(hunk);
      }
      hunk.lines.push(line === "" ? " " : line);
    }
  }
  return files;
}

/** Lines added and removed across the patch. */
export function patchStats(files: readonly PatchFile[]) {
  let additions = 0;
  let deletions = 0;
  for (const file of files) {
    for (const { lines } of file.hunks) {
      for (const line of lines) {
        if (line.startsWith("+")) additions += 1;
        else if (line.startsWith("-")) deletions += 1;
      }
    }
  }
  return { additions, deletions };
}

/**
 * Renders updates and additions as a git-style unified diff for Pierre. Envelope hunks carry no
 * line numbers, so each hunk starts where the previous one ended; displays must hide line numbers.
 * Deletions have no content to show and are left to the caller.
 */
export function toUnifiedDiff(file: PatchFile): string {
  const path = file.moveTo ?? file.path;
  const lines = [
    `diff --git a/${file.path} b/${path}`,
    file.kind === "add" ? "new file mode 100644" : "",
    `--- ${file.kind === "add" ? "/dev/null" : `a/${file.path}`}`,
    `+++ b/${path}`,
  ].filter(Boolean);
  let oldStart = file.kind === "add" ? 0 : 1;
  let newStart = 1;
  for (const hunk of file.hunks) {
    const oldCount = hunk.lines.filter((line) => !line.startsWith("+")).length;
    const newCount = hunk.lines.filter((line) => !line.startsWith("-")).length;
    lines.push(`@@ -${oldCount === 0 ? 0 : oldStart},${oldCount} +${newCount === 0 ? 0 : newStart},${newCount} @@${hunk.heading ? ` ${hunk.heading}` : ""}`);
    lines.push(...hunk.lines);
    oldStart += oldCount;
    newStart += newCount;
  }
  return `${lines.join("\n")}\n`;
}

/** Pierre's view of one patch file, or null when the content cannot be parsed. */
export function patchFileDiff(file: PatchFile, cacheKey: string): FileDiffMetadata | null {
  if (file.kind === "delete" || file.hunks.length === 0) return null;
  try {
    return parsePatchFiles(toUnifiedDiff(file), cacheKey, true).flatMap((parsed) => parsed.files)[0] ?? null;
  } catch {
    return null;
  }
}
