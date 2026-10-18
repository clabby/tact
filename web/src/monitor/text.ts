/**
 * A Markdown message as plain prose for a glance: a short code block, which often holds the
 * result itself, keeps its lines while longer ones become a "[code]" marker; images, footnotes,
 * and alert markers are dropped; and emphasis, inline code, link, heading, and quote markup gives
 * way to its text.
 */
export function plainText(markdown: string) {
  return markdown
    .replace(/^[ \t]*(`{3,}|~{3,})[^\n]*\n([\s\S]*?)(^[ \t]*\1[^\n]*$|(?![\s\S]))/gm, (_block, _fence, code: string) => {
      const text = code.replace(/\n$/, "");
      return text.split("\n").length <= 3 && text.length <= 200 ? text : "[code]";
    })
    .replace(/!\[[^\]]*\]\([^)]*\)/g, "")
    .replace(/\[([^\]]+)\]\([^)]*\)/g, "$1")
    .replace(/(\*\*|__)(?=\S)([\s\S]*?\S)\1/g, "$2")
    .replace(/`([^`\n]+)`/g, "$1")
    .replace(/^[ \t]{0,3}(#{1,6}[ \t]+|>[ \t]?)/gm, "")
    .replace(/^\[!(NOTE|TIP|IMPORTANT|WARNING|CAUTION)\][ \t]*\n?/gim, "")
    .replace(/^\[\^[^\]]+\]:[ \t]*/gm, "")
    .replace(/\[\^[^\]]+\]/g, "")
    .replace(/[ \t]+$/gm, "")
    .replace(/\n{3,}/g, "\n\n")
    .trim();
}

/** Characters of a long message kept on the closed card; Details shows all of it. */
const MESSAGE_PREVIEW = 280;

/**
 * The end of a message, where a closing question or result sits, starting at a line or word
 * boundary. Null when the whole message fits.
 */
export function messageTail(text: string, limit = MESSAGE_PREVIEW): string | null {
  const trimmed = text.trim();
  if (trimmed.length <= limit) return null;
  const tail = trimmed.slice(-limit);
  const line = tail.indexOf("\n");
  if (line >= 0 && line < limit / 2) return `…${tail.slice(line).trimStart()}`;
  const word = tail.search(/\s/);
  return `…${(word >= 0 && word < limit / 3 ? tail.slice(word) : tail).trimStart()}`;
}
