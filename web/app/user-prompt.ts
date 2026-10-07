export type PromptPart = { kind: "text"; text: string } | { kind: "image"; index: number; marker: string };

const IMAGE_MARKER = /\[Image #\d+\]/g;

/**
 * Splits a user prompt into text and image parts. The i-th attachment replaces the i-th
 * "[Image #N]" marker; markers beyond the attachment count stay as text. Surrounding line breaks
 * are dropped because every image is a block of its own, so a break always precedes and follows it.
 */
export function promptParts(text: string, images: number): PromptPart[] {
  const parts: PromptPart[] = [];
  let cursor = 0;
  const addText = (value: string) => {
    const trimmed = value.replace(/^\n+|\n+$/g, "");
    if (trimmed.trim()) parts.push({ kind: "text", text: trimmed });
  };
  for (const match of text.matchAll(IMAGE_MARKER)) {
    const index = parts.filter((part) => part.kind === "image").length;
    if (index >= images) break;
    addText(text.slice(cursor, match.index));
    parts.push({ kind: "image", index, marker: match[0] });
    cursor = match.index + match[0].length;
  }
  addText(text.slice(cursor));
  return parts;
}
