/** Composer mention triggers, matching the terminal: `@` files, `@@` sessions, `$` skills. */
export type MentionKind = "file" | "session" | "skill";

export type Mention = {
  kind: MentionKind;
  /** The text typed after the trigger. */
  query: string;
  /** Offset of the trigger character in the text. */
  start: number;
  /** Offset just past the query (the caret). */
  end: number;
};

/**
 * The mention being typed at \`caret\`, if any. A trigger only counts at the start of the text or
 * after whitespace, and the query runs up to the caret without whitespace, so e-mail addresses
 * and prices are not mistaken for mentions.
 */
export function findMention(text: string, caret: number): Mention | null {
  let start = caret;
  while (start > 0 && !/\s/.test(text[start - 1]!)) start -= 1;
  const token = text.slice(start, caret);
  const match = /^(@@|@|\$)(\S*)$/.exec(token);
  if (!match) return null;
  const kind: MentionKind = match[1] === "@@" ? "session" : match[1] === "@" ? "file" : "skill";
  return { kind, query: match[2]!, start, end: caret };
}

/** Replaces the mention with its completed form plus a separating space. */
export function completeMention(text: string, mention: Mention, value: string) {
  const trigger = { file: "@", session: "@@", skill: "$" }[mention.kind];
  const insertion = `${trigger}${value}`;
  const after = text.slice(mention.end);
  const separator = after.startsWith(" ") || value.endsWith("/") ? "" : " ";
  const next = `${text.slice(0, mention.start)}${insertion}${separator}${after}`;
  return { text: next, caret: mention.start + insertion.length + separator.length };
}
