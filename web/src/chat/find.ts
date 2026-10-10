import type { TranscriptData } from "../core/store";
import type { WireEntry } from "../core/wire";

/**
 * The text find-in-transcript searches for an entry: prompts and messages, a tool call's summary
 * and the end of its output, agent messages, and notices. Thoughts and tool arguments are not
 * searched; they are not on screen without opening a detail.
 */
export function searchableText(entry: WireEntry): string {
  switch (entry.kind) {
    case "user":
    case "assistant":
    case "session_message":
      return entry.text;
    case "tool":
      return [entry.summary, entry.outcome?.summary ?? "", ...(entry.outcome?.tail ?? [])].join("\n");
    case "directed_message":
      return entry.messages.map((message) => message.body).join("\n");
    case "error":
    case "compaction_failed":
      return entry.message;
    default:
      return "";
  }
}

/** The ids of entries whose text contains `query`, ignoring case, in transcript order. */
export function findMatches(data: TranscriptData, query: string): number[] {
  const needle = query.trim().toLowerCase();
  if (!needle) return [];
  return data.order.filter((id) => {
    const entry = data.entries.get(id);
    return entry !== undefined && searchableText(entry).toLowerCase().includes(needle);
  });
}

/** The start offsets of `query` in `text`, ignoring case. */
export function occurrences(text: string, query: string): number[] {
  const needle = query.trim().toLowerCase();
  if (!needle) return [];
  const haystack = text.toLowerCase();
  const found: number[] = [];
  for (let index = haystack.indexOf(needle); index !== -1; index = haystack.indexOf(needle, index + needle.length)) {
    found.push(index);
  }
  return found;
}
