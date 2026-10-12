import type { FileDiffMetadata } from "@pierre/diffs";
import { fileSignature } from "./review-live";

const SEEN_KEY = "tact.web.review-seen";
/** Checkouts whose marks are kept; the least recently marked are dropped first. */
const CHECKOUT_LIMIT = 30;
/** Marks kept per checkout; the least recently marked are dropped first. */
const FILES_LIMIT = 2000;

type SeenStorage = Pick<Storage, "getItem" | "setItem">;
/** Checkout path to its marks (file path to diff fingerprint), least recently marked first. */
type Marks = [string, [string, string][]][];

/** A 53-bit string hash (cyrb53). Fingerprints only need to distinguish edits, not resist attack. */
function hash(text: string) {
  let first = 0xdeadbeef;
  let second = 0x41c6ce57;
  for (let index = 0; index < text.length; index++) {
    const code = text.charCodeAt(index);
    first = Math.imul(first ^ code, 2654435761);
    second = Math.imul(second ^ code, 1597334677);
  }
  first = Math.imul(first ^ (first >>> 16), 2246822507) ^ Math.imul(second ^ (second >>> 13), 3266489909);
  second = Math.imul(second ^ (second >>> 16), 2246822507) ^ Math.imul(first ^ (first >>> 13), 3266489909);
  return (4294967296 * (2097151 & second) + (first >>> 0)).toString(36);
}

/**
 * Fingerprints the diff the reviewer sees for `file`: equal fingerprints mean identical content.
 * A diff with no text hunks (a binary file, a mode change) cannot show what changed, so it also
 * covers the content identities Git reports.
 */
function fingerprint(file: FileDiffMetadata) {
  const identity = file.hunks.length === 0 ? [file.prevObjectId, file.newObjectId] : [];
  return hash(JSON.stringify([fileSignature(file), file.prevMode, file.mode, identity]));
}

function readMarks(raw: string | null): Marks {
  try {
    const parsed: unknown = JSON.parse(raw ?? "[]");
    if (!Array.isArray(parsed)) return [];
    return parsed.flatMap((entry): Marks => {
      if (!Array.isArray(entry) || typeof entry[0] !== "string" || !Array.isArray(entry[1])) return [];
      const files = entry[1].filter((file): file is [string, string] =>
        Array.isArray(file) && typeof file[0] === "string" && typeof file[1] === "string");
      return [[entry[0], files]];
    });
  } catch {
    return [];
  }
}

/**
 * The files a reviewer marked as seen, remembered across reloads, ranges, and sessions. A mark
 * belongs to a checkout and records the fingerprint of the file's diff when it was made, so it
 * holds exactly as long as the reviewer would see the same diff for that file: any edit to the
 * file, in any range, shows it as unseen again, and a later revert to the marked content shows it
 * as seen once more.
 *
 * Storage is shared with other tabs, so it is re-read whenever its contents change. Marks the
 * storage refuses to keep stay in memory for the life of the page.
 */
export class SeenFiles {
  private raw: string | null = null;
  private marks: Marks = [];
  private cache: { checkout: string; files: readonly FileDiffMetadata[]; seen: ReadonlySet<string> } | null = null;

  constructor(private readonly storage: SeenStorage) {}

  /** The paths among `files` that are marked and unchanged since. */
  among(checkout: string, files: readonly FileDiffMetadata[]): ReadonlySet<string> {
    this.load();
    if (this.cache?.checkout === checkout && this.cache.files === files) return this.cache.seen;
    const marked = new Map(this.marks.find(([path]) => path === checkout)?.[1]);
    const seen = new Set(files.filter((file) => marked.get(file.name) === fingerprint(file)).map((file) => file.name));
    this.cache = { checkout, files, seen };
    return seen;
  }

  /** Marks `file` as seen as it is now, or clears its mark. */
  mark(checkout: string, file: FileDiffMetadata, seen: boolean) {
    this.load();
    const files = new Map(this.marks.find(([path]) => path === checkout)?.[1]);
    files.delete(file.name);
    if (seen) files.set(file.name, fingerprint(file));
    const others = this.marks.filter(([path]) => path !== checkout);
    if (files.size > 0) others.push([checkout, [...files].slice(-FILES_LIMIT)]);
    this.marks = others.slice(-CHECKOUT_LIMIT);
    this.cache = null;
    try {
      const raw = JSON.stringify(this.marks);
      this.storage.setItem(SEEN_KEY, raw);
      this.raw = raw;
    } catch {
      // The marks stay in memory; `raw` still names what storage holds, so they are not reloaded away.
    }
  }

  private load() {
    const raw = this.storage.getItem(SEEN_KEY);
    if (raw === this.raw) return;
    this.raw = raw;
    this.marks = readMarks(raw);
    this.cache = null;
  }
}
