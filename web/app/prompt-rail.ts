/** The prompt list shows at most this many lines before it pages. */
export const MAX_LINES = 12;

/**
 * The slice of prompts to draw. Unless the reader paged (\`manualStart\`), it centres on the active
 * prompt so the current position is always on screen.
 */
export function windowFor(total: number, active: number, max: number, manualStart: number | null = null) {
  if (total <= max) return { start: 0, end: total };
  const wanted = manualStart ?? active - Math.floor(max / 2);
  const start = Math.min(Math.max(0, wanted), total - max);
  return { start, end: start + max };
}

/** The last prompt that starts at or above \`line\` (offsets ascending); the first when none has. */
export function activeIndex(tops: readonly number[], line: number) {
  let active = 0;
  for (const [index, top] of tops.entries()) {
    if (top <= line) active = index;
    else break;
  }
  return active;
}

/** A tick's length in pixels: longer prompts draw longer lines, like a minimap of the text. */
export function lineLength(text: string) {
  return Math.round(Math.min(26, Math.max(10, 8 + Math.sqrt(text.length) * 1.7)));
}

/** A prompt's first line without image markers, for the list label. */
export function promptLabel(text: string) {
  const line = text.replace(/\[Image #\d+\]/g, " ").split("\n").map((part) => part.trim()).find(Boolean);
  return line ?? "Image";
}

