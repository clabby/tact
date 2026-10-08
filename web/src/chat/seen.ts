import type { TranscriptData } from "../core/store";
import { formatAge } from "../core/format";

/** The last entry the reader had in front of them, and when. */
export type Seen = { id: number; at: number };

type SeenStorage = Pick<Storage, "getItem" | "setItem">;

const key = (transcript: string) => `tact.web.seen.${transcript}`;

export function readSeen(storage: SeenStorage, transcript: string): Seen | null {
  try {
    const value = JSON.parse(storage.getItem(key(transcript)) ?? "null") as Partial<Seen> | null;
    return typeof value?.id === "number" && typeof value.at === "number" ? { id: value.id, at: value.at } : null;
  } catch {
    return null;
  }
}

export function writeSeen(storage: SeenStorage, transcript: string, seen: Seen) {
  try {
    storage.setItem(key(transcript), JSON.stringify(seen));
  } catch {
    // Storage may be full or disabled; the marker is a convenience.
  }
}

/** The first top-level entry after the last one seen, or null when nothing is new. */
export function firstUnseen(data: TranscriptData, seen: Seen): number | null {
  for (const id of data.order) {
    if (id <= seen.id) continue;
    if (data.entries.get(id)?.parent === null) return id;
  }
  return null;
}

/** The newest top-level entry, which counts as seen once the reader has looked. */
export function latestEntry(data: TranscriptData): number | null {
  for (let index = data.order.length - 1; index >= 0; index -= 1) {
    const id = data.order[index]!;
    if (data.entries.get(id)?.parent === null) return id;
  }
  return null;
}

export function newSinceLabel(seenAt: number, now = Date.now()) {
  const age = formatAge(seenAt, now);
  return age === "now" ? "New since moments ago" : /^\d/.test(age) ? `New since ${age} ago` : `New since ${age}`;
}
