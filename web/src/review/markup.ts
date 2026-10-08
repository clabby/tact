/** Escapes text for interpolation into HTML content and attribute values. */
export function escapeHtml(value: string) {
  return value.replace(/[&<>'"]/g, (character) => ({
    "&": "&amp;", "<": "&lt;", ">": "&gt;", "'": "&#39;", '"': "&quot;",
  })[character] ?? character);
}

export function formatRange(start: number, end: number) {
  return start === end ? String(start) : `${Math.min(start, end)}–${Math.max(start, end)}`;
}

/** A badge for a comment body that starts with a [P0]–[P3] severity tag. */
export function severityBadge(body: string) {
  const severity = /^\[(P[0-3])\]/.exec(body)?.[1];
  return severity ? `<b class="severity-badge severity-${severity.toLowerCase()}">${severity}</b>` : "";
}
