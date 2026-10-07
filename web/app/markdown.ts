import {
  getSharedHighlighter,
  type DiffsThemeNames,
  type SupportedLanguages,
} from "@pierre/diffs";
import { Marked, Renderer } from "marked";
import type { SyntaxTheme } from "./review-settings";

const renderer = new Renderer();
renderer.html = ({ text }) => escapeHtml(text);
renderer.link = function ({ href, title, tokens }) {
  const label = this.parser.parseInline(tokens);
  const safeHref = safeLink(href);
  const titleAttribute = title ? ` title="${escapeHtml(title)}"` : "";
  return `<a href="${escapeHtml(safeHref)}"${titleAttribute} target="_blank" rel="noreferrer">${label}</a>`;
};

renderer.image = ({ href, title, text }) => {
  const titleAttribute = title ? ` title="${escapeHtml(title)}"` : "";
  return `<img src="${escapeHtml(href)}" alt="${escapeHtml(text)}"${titleAttribute}>`;
};

const markdown = new Marked({ breaks: true, gfm: true, renderer });
const themes: DiffsThemeNames[] = [
  "pierre-light", "pierre-light-soft", "pierre-dark", "pierre-dark-soft",
];

/**
 * Markdown to HTML with raw HTML escaped and links restricted to http(s) and mailto. The DOM pass
 * in `renderMarkdown` additionally strips any element or attribute outside a small allowlist.
 */
export function markdownHtml(source: string) {
  return markdown.parse(source, { async: false });
}

/**
 * Renders sanitized Markdown into `container`. Pass `highlight: false` while text is still
 * streaming: highlighting is the expensive part and re-runs on every render.
 */
export async function renderMarkdown(
  container: HTMLElement,
  source: string,
  themeName: Exclude<SyntaxTheme, "system">,
  { highlight = true, placeholder = "*Nothing to preview yet.*", imageSource }: {
    highlight?: boolean;
    placeholder?: string;
    /** Maps an image destination to a URL the page may load, or null to show only its alt text. */
    imageSource?: (destination: string) => string | null;
  } = {},
) {
  const template = document.createElement("template");
  template.innerHTML = markdownHtml(source || placeholder);
  sanitize(template.content, imageSource);
  container.replaceChildren(template.content);
  for (const image of container.querySelectorAll("img")) {
    image.addEventListener("error", () => {
      const missing = document.createElement("span");
      missing.className = "image-missing";
      missing.textContent = image.alt ? `Image not available: ${image.alt}` : "Image not available";
      image.replaceWith(missing);
    }, { once: true });
  }
  if (!highlight) return;

  const codeBlocks = [...container.querySelectorAll<HTMLElement>("pre > code")];
  await Promise.all(codeBlocks.map(async (code) => {
    const pre = code.parentElement;
    if (!pre) return;
    const language = code.className.match(/(?:^|\s)language-([^\s]+)/)?.[1] ?? "text";
    try {
      const highlighter = await getSharedHighlighter({
        themes,
        langs: [language as SupportedLanguages],
      });
      const highlighted = highlighter.codeToHtml(code.textContent ?? "", {
        lang: language,
        theme: themeName,
      });
      const highlightedTemplate = document.createElement("template");
      highlightedTemplate.innerHTML = highlighted;
      const replacement = highlightedTemplate.content.firstElementChild;
      if (replacement) pre.replaceWith(replacement);
    } catch {
      code.className = "language-text";
    }
  }));
}

function sanitize(fragment: DocumentFragment, imageSource?: (destination: string) => string | null) {
  const allowed = new Set([
    "A", "BLOCKQUOTE", "BR", "CODE", "DEL", "EM", "H1", "H2", "H3", "H4", "H5", "H6",
    "HR", "LI", "OL", "P", "PRE", "STRONG", "TABLE", "TBODY", "TD", "TH", "THEAD", "TR", "UL",
  ]);
  for (const element of [...fragment.querySelectorAll<HTMLElement>("*")]) {
    if (element.tagName === "IMG") {
      const source = imageSource?.(element.getAttribute("src") ?? "");
      if (!source) {
        element.replaceWith(document.createTextNode(element.getAttribute("alt") ?? ""));
        continue;
      }
      const alt = element.getAttribute("alt") ?? "";
      const title = element.getAttribute("title");
      for (const attribute of [...element.attributes]) element.removeAttribute(attribute.name);
      element.setAttribute("src", source);
      element.setAttribute("alt", alt);
      element.setAttribute("loading", "lazy");
      if (title) element.setAttribute("title", title);
      continue;
    }
    if (!allowed.has(element.tagName)) {
      element.replaceWith(document.createTextNode(element.textContent ?? ""));
      continue;
    }
    for (const attribute of [...element.attributes]) {
      const keepLinkAttribute = element.tagName === "A"
        && ["href", "title", "target", "rel"].includes(attribute.name);
      const keepCodeLanguage = element.tagName === "CODE"
        && attribute.name === "class"
        && attribute.value.startsWith("language-");
      if (!keepLinkAttribute && !keepCodeLanguage) element.removeAttribute(attribute.name);
    }
  }
}

function safeLink(value: string) {
  try {
    const url = new URL(value, globalThis.location?.href ?? "http://localhost/");
    if (["http:", "https:", "mailto:"].includes(url.protocol)) return value;
  } catch {
    // Invalid links render as inert anchors.
  }
  return "#";
}

function escapeHtml(value: string) {
  return value.replace(/[&<>'"]/g, (character) => ({
    "&": "&amp;", "<": "&lt;", ">": "&gt;", "'": "&#39;", '"': "&quot;",
  })[character] ?? character);
}
